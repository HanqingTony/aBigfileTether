//! 对等传输：SSH/本地 字节管道 + 自研分帧协议 + 哈希感知搬运 + 断点续传。
//!
//! 见 `docs/DESIGN.md` §5.3。要点：
//! - 传输后端可插拔（`Connection`）：默认系统 `ssh`（`ssh <host> -- tether agent`，
//!   命令行不含任何用户路径），可选纯 Rust `russh`（`--features russh`）。
//! - 帧 = `u32` 长度前缀 + bincode 负载；`path` 一律长度前缀的**原始字节**。
//! - 大文件：按 offset 续传；未完成内容写 `.part`，校验通过后原子改名。
//!   续传时本地重读已存在前缀以恢复 blake3 状态（只读本地磁盘，不重传网络）。

use crate::{config, hash, shadow as shadowmod};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// 协议版本（v2：Get/Put 支持 offset 续传）。
pub const PROTO_VERSION: u32 = 2;

/// 客户端 → agent 请求。
#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Hello {
        version: u32,
        repo: Vec<u8>,
    },
    ListIndex,
    /// 查询真实文件与 `.part` 的状态（续传用）。
    Stat {
        path: Vec<u8>,
    },
    /// 从 `offset` 起读取文件；agent 回 `Started` 后紧跟裸字节。
    Get {
        path: Vec<u8>,
        offset: u64,
    },
    /// 从 `offset` 起写入；客户端随后发 `size - offset` 字节。
    Put {
        path: Vec<u8>,
        offset: u64,
        size: u64,
        hash: String,
    },
    Move {
        from: Vec<u8>,
        to: Vec<u8>,
    },
    Copy {
        from: Vec<u8>,
        to: Vec<u8>,
    },
    Delete {
        path: Vec<u8>,
    },
    Bye,
}

/// agent → 客户端响应。
#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    HelloAck {
        version: u32,
    },
    Index {
        entries: Vec<IndexEntry>,
    },
    Stat {
        real_exists: bool,
        real_size: u64,
        part_size: u64,
    },
    Started {
        size: u64,
        hash: String,
    },
    PutDone {
        hash: String,
    },
    Ok,
    Error {
        message: String,
    },
}

/// 索引项：真实相对路径（原始字节）+ 大小 + 哈希。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub path: Vec<u8>,
    pub size: u64,
    pub hash: String,
}

/// 对端文件状态（续传判断）。
#[derive(Debug, Clone)]
pub struct StatInfo {
    pub real_exists: bool,
    pub real_size: u64,
    pub part_size: u64,
}

// ---------- 路径 <-> 原始字节（跨平台） ----------

#[cfg(unix)]
pub fn path_to_bytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
pub fn path_to_bytes(p: &Path) -> Vec<u8> {
    p.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
pub fn bytes_to_path(b: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(b))
}

#[cfg(not(unix))]
pub fn bytes_to_path(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).into_owned())
}

/// 校验并转换对端传来的相对路径，拒绝绝对路径与 `..` 逃逸。
fn safe_rel(b: &[u8]) -> Result<PathBuf> {
    let p = bytes_to_path(b);
    if p.as_os_str().is_empty() || p.is_absolute() {
        bail!("非法路径：{}", p.display());
    }
    if p.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("路径越界：{}", p.display());
    }
    Ok(p)
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

/// 读取文件前缀 `len` 字节以恢复 blake3 状态（续传用）。
fn seeded_hasher(path: &Path, len: u64) -> Result<blake3::Hasher> {
    let mut hasher = blake3::Hasher::new();
    if len == 0 {
        return Ok(hasher);
    }
    let mut f = File::open(path).with_context(|| format!("打开 {} 失败", path.display()))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut remaining = len;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        remaining -= n as u64;
    }
    Ok(hasher)
}

// ---------- 分帧 ----------

fn write_frame<W: Write + ?Sized>(w: &mut W, bytes: &[u8]) -> Result<()> {
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()?;
    Ok(())
}

fn read_frame<R: Read + ?Sized>(r: &mut R) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)
        .context("读取帧长度失败（对端可能退出）")?;
    let n = u32::from_le_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).context("读取帧负载失败")?;
    Ok(buf)
}

fn send<W: Write + ?Sized, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    let bytes = bincode::serialize(msg).context("序列化协议消息失败")?;
    write_frame(w, &bytes)
}

fn recv<R: Read + ?Sized, T: DeserializeOwned>(r: &mut R) -> Result<T> {
    let bytes = read_frame(r)?;
    bincode::deserialize(&bytes).context("反序列化协议消息失败")
}

/// 把 `reader` 的指定字节数写入 `writer`，同时喂给 `hasher`。
fn pump<R: Read + ?Sized, W: Write + ?Sized>(
    reader: &mut R,
    writer: &mut W,
    hasher: &mut blake3::Hasher,
    len: u64,
) -> Result<()> {
    let mut limited = reader.take(len);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = limited.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        writer.write_all(&buf[..n])?;
    }
    Ok(())
}

fn hex_digest(hasher: &blake3::Hasher) -> String {
    format!("blake3:{}", hasher.finalize().to_hex())
}

// ---------- 连接抽象 ----------

/// 传输连接：对端 agent 的字节管道（子进程 或 russh 通道）。
trait Connection: Send {
    fn reader(&mut self) -> &mut (dyn Read + Send);
    fn writer(&mut self) -> &mut (dyn Write + Send);
    /// 正常收尾（等待/断开）。
    fn finish(&mut self);
    /// 异常清理（Drop 时）。
    fn kill(&mut self);
}

/// 本地/系统 ssh：以子进程 stdio 作为管道。
struct ChildConnection {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl ChildConnection {
    fn spawn(cmd: &mut Command) -> Result<ChildConnection> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("启动对端进程失败")?;
        let stdin = child.stdin.take().context("stdin 不可用")?;
        let stdout = child.stdout.take().context("stdout 不可用")?;
        Ok(ChildConnection {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }
}

impl Connection for ChildConnection {
    fn reader(&mut self) -> &mut (dyn Read + Send) {
        &mut self.stdout
    }
    fn writer(&mut self) -> &mut (dyn Write + Send) {
        &mut self.stdin
    }
    fn finish(&mut self) {
        let _ = self.child.wait();
    }
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 对等端描述。
#[derive(Debug, Clone)]
pub enum Peer {
    /// 本地直接拉起 `tether agent`（冒烟测试用）。
    Local { bin: PathBuf, repo: PathBuf },
    /// 系统 ssh：`ssh <host> -- <remote_bin> agent`。
    Ssh {
        host: String,
        repo: String,
        remote_bin: String,
    },
    /// 纯 Rust SSH（需 `--features russh`）。
    Russh {
        host: String,
        user: String,
        port: u16,
        key: Option<PathBuf>,
        repo: String,
    },
}

impl Peer {
    /// 解析 `user@host:/abs/repo` 为系统 ssh 对等端；`local:/abs/repo` 为同机本地 agent。
    /// `remote_bin` 为对端上 tether 的路径（默认调用 `tether`）。
    pub fn parse(spec: &str, remote_bin: &str) -> Result<Peer> {
        if let Some(repo) = spec.strip_prefix("local:") {
            let bin = std::env::current_exe().context("无法获取当前可执行文件路径")?;
            return Ok(Peer::Local {
                bin,
                repo: PathBuf::from(repo),
            });
        }
        let (host, repo) = spec.split_once(':').with_context(|| {
            format!("对等端格式应为 user@host:/repo 或 local:/repo，得到：{spec}")
        })?;
        Ok(Peer::Ssh {
            host: host.to_string(),
            repo: repo.to_string(),
            remote_bin: remote_bin.to_string(),
        })
    }

    /// 解析 `[user@]host:/abs/repo` 为 russh 对等端。
    pub fn russh(spec: &str, key: Option<PathBuf>) -> Result<Peer> {
        let (userhost, repo) = spec
            .split_once(':')
            .with_context(|| format!("对等端格式应为 user@host:/repo，得到：{spec}"))?;
        let (user, host) = match userhost.split_once('@') {
            Some((u, h)) => (u.to_string(), h.to_string()),
            None => ("root".to_string(), userhost.to_string()),
        };
        Ok(Peer::Russh {
            host,
            user,
            port: 22,
            key,
            repo: repo.to_string(),
        })
    }

    fn repo_bytes(&self) -> Vec<u8> {
        match self {
            Peer::Local { repo, .. } => path_to_bytes(repo),
            Peer::Ssh { repo, .. } | Peer::Russh { repo, .. } => repo.as_bytes().to_vec(),
        }
    }

    fn connect(&self) -> Result<Box<dyn Connection>> {
        match self {
            Peer::Local { bin, .. } => Ok(Box::new(ChildConnection::spawn(
                Command::new(bin).arg("agent"),
            )?)),
            Peer::Ssh {
                host, remote_bin, ..
            } => Ok(Box::new(ChildConnection::spawn(
                Command::new("ssh")
                    .arg("-o")
                    .arg("BatchMode=yes")
                    .arg(host)
                    .arg("--")
                    .arg(remote_bin)
                    .arg("agent"),
            )?)),
            Peer::Russh {
                host,
                user,
                port,
                key,
                ..
            } => {
                #[cfg(feature = "russh")]
                {
                    let key = key
                        .clone()
                        .context("russh 后端需要 [transfer] key（私钥路径）")?;
                    Ok(Box::new(russh_conn::connect(
                        host,
                        *port,
                        user,
                        &key,
                        "tether agent",
                    )?))
                }
                #[cfg(not(feature = "russh"))]
                {
                    let _ = (host, user, port, key);
                    bail!(
                        "此构建未启用 russh 特性：用 `cargo build --features russh` 重编，\
                         或把 [transfer] backend 设为 system-ssh"
                    )
                }
            }
        }
    }
}

// ---------- 客户端 ----------

/// 已连接的 agent 客户端。
pub struct Agent {
    conn: Box<dyn Connection>,
}

impl Agent {
    /// 连接对等端并完成握手。
    pub fn connect(peer: &Peer) -> Result<Agent> {
        let mut conn = peer.connect()?;
        send(
            conn.writer(),
            &Request::Hello {
                version: PROTO_VERSION,
                repo: peer.repo_bytes(),
            },
        )?;
        let mut agent = Agent { conn };
        match agent.response()? {
            Response::HelloAck { version } if version == PROTO_VERSION => Ok(agent),
            Response::HelloAck { version } => bail!("协议版本不一致：对端 {version}"),
            Response::Error { message } => bail!("对端握手失败：{message}"),
            other => bail!("握手异常响应：{other:?}"),
        }
    }

    fn send(&mut self, req: &Request) -> Result<()> {
        send(self.conn.writer(), req)
    }

    fn response(&mut self) -> Result<Response> {
        recv(self.conn.reader())
    }

    fn expect_ok(&mut self) -> Result<()> {
        match self.response()? {
            Response::Ok => Ok(()),
            Response::Error { message } => bail!("对端错误：{message}"),
            other => bail!("对端非预期响应：{other:?}"),
        }
    }

    /// 拉取对端索引（路径→size,hash）。
    pub fn list_index(&mut self) -> Result<Vec<IndexEntry>> {
        self.send(&Request::ListIndex)?;
        match self.response()? {
            Response::Index { entries } => Ok(entries),
            Response::Error { message } => bail!("对端错误：{message}"),
            other => bail!("非预期响应：{other:?}"),
        }
    }

    /// 查询对端某路径的真实文件/`.part` 状态。
    pub fn stat(&mut self, path: &[u8]) -> Result<StatInfo> {
        self.send(&Request::Stat {
            path: path.to_vec(),
        })?;
        match self.response()? {
            Response::Stat {
                real_exists,
                real_size,
                part_size,
            } => Ok(StatInfo {
                real_exists,
                real_size,
                part_size,
            }),
            Response::Error { message } => bail!("对端错误：{message}"),
            other => bail!("非预期响应：{other:?}"),
        }
    }

    /// 从对端拉取文件到本地 `dest`，支持断点续传，校验哈希后原子改名。
    pub fn get(&mut self, remote: &[u8], dest: &Path, expected_hash: &str) -> Result<()> {
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p)?;
        }
        let tmp = part_path(dest);
        // 最多重试一次：首次尝试续传，失败则从 0 重来。
        for attempt in 0..2 {
            let offset = if attempt == 0 {
                fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0)
            } else {
                0
            };
            self.send(&Request::Get {
                path: remote.to_vec(),
                offset,
            })?;
            let (size, hash) = match self.response()? {
                Response::Started { size, hash } => (size, hash),
                Response::Error { message } => {
                    if attempt == 0 {
                        fs::remove_file(&tmp).ok();
                        continue;
                    }
                    bail!("对端错误：{message}");
                }
                other => bail!("非预期响应：{other:?}"),
            };
            if hash != expected_hash {
                bail!("对端源文件哈希不符：{hash} != {expected_hash}");
            }
            let mut hasher = if offset > 0 {
                seeded_hasher(&tmp, offset)?
            } else {
                blake3::Hasher::new()
            };
            let file = if offset > 0 {
                OpenOptions::new().append(true).open(&tmp)?
            } else {
                File::create(&tmp)?
            };
            let mut writer = BufWriter::new(file);
            pump(self.conn.reader(), &mut writer, &mut hasher, size - offset)?;
            writer.flush()?;
            drop(writer);
            let got = hex_digest(&hasher);
            if got != expected_hash {
                fs::remove_file(&tmp).ok();
                if attempt == 0 {
                    continue;
                }
                bail!("接收内容哈希不符：{got} != {expected_hash}");
            }
            if dest.exists() {
                fs::remove_file(dest).ok();
            }
            fs::rename(&tmp, dest).with_context(|| format!("落盘 {} 失败", dest.display()))?;
            return Ok(());
        }
        bail!(
            "获取失败（重试后仍失败）：{}",
            bytes_to_path(remote).display()
        )
    }

    /// 把本地文件推给对端某路径，支持断点续传。
    pub fn put(&mut self, remote: &[u8], local: &Path, size: u64, hash: &str) -> Result<()> {
        for attempt in 0..2 {
            let offset = if attempt == 0 {
                let st = self.stat(remote)?;
                if st.part_size <= size {
                    st.part_size
                } else {
                    0
                }
            } else {
                0
            };
            self.send(&Request::Put {
                path: remote.to_vec(),
                offset,
                size,
                hash: hash.to_string(),
            })?;
            let mut f =
                File::open(local).with_context(|| format!("打开 {} 失败", local.display()))?;
            if offset > 0 {
                f.seek(SeekFrom::Start(offset))?;
            }
            io::copy(
                &mut std::io::Read::by_ref(&mut f).take(size - offset),
                self.conn.writer(),
            )
            .context("发送文件字节失败")?;
            self.conn.writer().flush()?;
            match self.response()? {
                Response::PutDone { .. } => return Ok(()),
                Response::Error { message } => {
                    if attempt == 0 {
                        continue;
                    }
                    bail!("对端错误：{message}");
                }
                other => bail!("非预期响应：{other:?}"),
            }
        }
        bail!("推送失败（重试后仍失败）")
    }

    pub fn move_path(&mut self, from: &[u8], to: &[u8]) -> Result<()> {
        self.send(&Request::Move {
            from: from.to_vec(),
            to: to.to_vec(),
        })?;
        self.expect_ok()
    }

    pub fn copy_path(&mut self, from: &[u8], to: &[u8]) -> Result<()> {
        self.send(&Request::Copy {
            from: from.to_vec(),
            to: to.to_vec(),
        })?;
        self.expect_ok()
    }

    pub fn delete(&mut self, path: &[u8]) -> Result<()> {
        self.send(&Request::Delete {
            path: path.to_vec(),
        })?;
        self.expect_ok()
    }

    /// 发送 Bye 并等待退出。
    pub fn close(mut self) -> Result<()> {
        send(self.conn.writer(), &Request::Bye)?;
        self.conn.finish();
        Ok(())
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.conn.kill();
    }
}

// ---------- 纯 Rust SSH 后端（可选 feature） ----------

#[cfg(feature = "russh")]
mod russh_conn {
    use super::Connection;
    use anyhow::{Context, Result, anyhow, bail};
    use std::io::{self, Read, Write};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::mpsc as std_mpsc;
    use tokio::sync::mpsc as tokio_mpsc;

    struct Client;
    impl russh::client::Handler for Client {
        type Error = russh::Error;
        async fn check_server_key(
            &mut self,
            _server_public_key: &russh::keys::PublicKeyOrCertificate,
        ) -> std::result::Result<bool, Self::Error> {
            Ok(true)
        }
    }

    struct ChannelReader {
        rx: std_mpsc::Receiver<Vec<u8>>,
        buf: Vec<u8>,
        pos: usize,
    }

    impl Read for ChannelReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.buf.len() {
                match self.rx.recv() {
                    Ok(v) => {
                        self.buf = v;
                        self.pos = 0;
                    }
                    Err(_) => return Ok(0), // 远端结束
                }
            }
            if self.buf.is_empty() {
                return Ok(0);
            }
            let n = (self.buf.len() - self.pos).min(out.len());
            out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    struct ChannelWriter {
        tx: tokio_mpsc::UnboundedSender<Vec<u8>>,
    }

    impl Write for ChannelWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.tx
                .send(data.to_vec())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "远端已断开"))?;
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    pub struct RusshConnection {
        _rt: tokio::runtime::Runtime,
        reader: ChannelReader,
        writer: ChannelWriter,
    }

    /// 连接并执行 `command`，返回同步可用的字节管道。
    pub fn connect(
        host: &str,
        port: u16,
        user: &str,
        key: &Path,
        command: &str,
    ) -> Result<RusshConnection> {
        let (to_tx, mut to_rx) = tokio_mpsc::unbounded_channel::<Vec<u8>>();
        let (from_tx, from_rx) = std_mpsc::channel::<Vec<u8>>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()
            .context("创建 tokio 运行时失败")?;

        let host = host.to_string();
        let user = user.to_string();
        let key = key.to_path_buf();
        let command = command.to_string();
        rt.spawn(async move {
            let cfg = Arc::new(russh::client::Config::default());
            let mut session = match russh::client::connect(cfg, (host.as_str(), port), Client).await
            {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow!("SSH 连接失败：{e}")));
                    return;
                }
            };
            let kp = match russh::keys::load_secret_key(&key, None) {
                Ok(k) => k,
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow!("读取私钥失败：{e}")));
                    return;
                }
            };
            let hash = session
                .best_supported_rsa_hash()
                .await
                .ok()
                .flatten()
                .flatten();
            let auth = match session
                .authenticate_publickey(
                    &user,
                    russh::keys::PrivateKeyWithHashAlg::new(Arc::new(kp), hash),
                )
                .await
            {
                Ok(a) => a,
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow!("SSH 认证失败：{e}")));
                    return;
                }
            };
            if !auth.success() {
                let _ = ready_tx.send(Err(anyhow!("SSH 公钥认证被拒")));
                return;
            }
            let mut channel = match session.channel_open_session().await {
                Ok(c) => c,
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow!("打开通道失败：{e}")));
                    return;
                }
            };
            if let Err(e) = channel.exec(false, command).await {
                let _ = ready_tx.send(Err(anyhow!("执行远端命令失败：{e}")));
                return;
            }
            let _ = ready_tx.send(Ok(()));

            loop {
                tokio::select! {
                    Some(bytes) = to_rx.recv() => {
                        if channel.data(&bytes[..]).await.is_err() {
                            break;
                        }
                    }
                    Some(msg) = channel.wait() => {
                        match msg {
                            russh::ChannelMsg::Data { ref data } => {
                                if from_tx.send(data.to_vec()).is_err() {
                                    break;
                                }
                            }
                            russh::ChannelMsg::Eof
                            | russh::ChannelMsg::ExitStatus { .. }
                            | russh::ChannelMsg::Close => break,
                            _ => {}
                        }
                    }
                    else => break,
                }
            }
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "", "en")
                .await;
        });

        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => bail!("SSH 后端启动失败"),
        }
        Ok(RusshConnection {
            _rt: rt,
            reader: ChannelReader {
                rx: from_rx,
                buf: Vec::new(),
                pos: 0,
            },
            writer: ChannelWriter { tx: to_tx },
        })
    }

    impl Connection for RusshConnection {
        fn reader(&mut self) -> &mut (dyn Read + Send) {
            &mut self.reader
        }
        fn writer(&mut self) -> &mut (dyn Write + Send) {
            &mut self.writer
        }
        fn finish(&mut self) {}
        fn kill(&mut self) {}
    }
}

// ---------- agent 服务端 ----------

/// 远端 agent 主循环：从 stdio 读请求、操作真实文件夹、回响应。
///
/// `default_repo` 为 `--repo` 参数；若为空则用 Hello 里携带的仓库路径。
pub fn run_agent(default_repo: Option<PathBuf>) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut r = BufReader::new(stdin.lock());
    let mut w = stdout.lock();

    let repo = match recv::<_, Request>(&mut r)? {
        Request::Hello { version, repo } => {
            if version != PROTO_VERSION {
                send(
                    &mut w,
                    &Response::Error {
                        message: format!("协议版本不一致：客户端 {version}"),
                    },
                )?;
                bail!("协议版本不一致");
            }
            default_repo.unwrap_or_else(|| bytes_to_path(&repo))
        }
        other => {
            send(
                &mut w,
                &Response::Error {
                    message: "首个消息必须是 Hello".to_string(),
                },
            )?;
            bail!("首个消息不是 Hello：{other:?}");
        }
    };

    let cfg = config::load(&repo)
        .with_context(|| format!("agent 载入仓库配置失败：{}", repo.display()))?;
    let real = cfg.resolve_real(&repo);
    if !real.is_dir() {
        send(
            &mut w,
            &Response::Error {
                message: format!("真实文件夹不存在：{}", real.display()),
            },
        )?;
        bail!("真实文件夹不存在：{}", real.display());
    }
    send(
        &mut w,
        &Response::HelloAck {
            version: PROTO_VERSION,
        },
    )?;

    // 索引缓存：会话内不变。Get 用它取哈希，省掉"发送前先整读一遍"的重复读。
    let index = shadowmod::load_index(&repo)?;

    loop {
        let req: Request = match recv(&mut r) {
            Ok(req) => req,
            Err(_) => break, // 客户端断开
        };
        // Bye 必须跳出整个循环（在闭包里 `return` 只会退出闭包，会死锁）
        if matches!(req, Request::Bye) {
            break;
        }
        let result: Result<()> = (|| match req {
            Request::ListIndex => {
                let entries = index
                    .iter()
                    .map(|(rel, s)| IndexEntry {
                        path: path_to_bytes(rel),
                        size: s.size,
                        hash: s.content_hash.clone(),
                    })
                    .collect();
                send(&mut w, &Response::Index { entries })?;
                Ok(())
            }
            Request::Stat { path } => {
                let dst = real.join(safe_rel(&path)?);
                let (real_exists, real_size) = match fs::metadata(&dst) {
                    Ok(m) => (true, m.len()),
                    Err(_) => (false, 0),
                };
                let part_size = fs::metadata(part_path(&dst)).map(|m| m.len()).unwrap_or(0);
                send(
                    &mut w,
                    &Response::Stat {
                        real_exists,
                        real_size,
                        part_size,
                    },
                )?;
                Ok(())
            }
            Request::Get { path, offset } => {
                let rel = safe_rel(&path)?;
                let src = real.join(&rel);
                let md = fs::metadata(&src)
                    .with_context(|| format!("对端缺少文件：{}", src.display()))?;
                if offset > md.len() {
                    send(
                        &mut w,
                        &Response::Error {
                            message: format!("offset {offset} 越界（大小 {})", md.len()),
                        },
                    )?;
                    return Ok(());
                }
                let digest = match index.get(&rel) {
                    Some(s) => s.content_hash.clone(),
                    None => hash::blake3_file(&src)?,
                };
                send(
                    &mut w,
                    &Response::Started {
                        size: md.len(),
                        hash: digest,
                    },
                )?;
                let mut f = File::open(&src)?;
                f.seek(SeekFrom::Start(offset))?;
                io::copy(
                    &mut std::io::Read::by_ref(&mut f).take(md.len() - offset),
                    &mut w,
                )?;
                w.flush()?;
                Ok(())
            }
            Request::Put {
                path,
                offset,
                size,
                hash,
            } => {
                let dst = real.join(safe_rel(&path)?);
                if let Some(p) = dst.parent() {
                    fs::create_dir_all(p)?;
                }
                let tmp = part_path(&dst);
                let cur = fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
                if offset != 0 && cur != offset {
                    // 断点不一致：丢弃负载保持协议同步，让客户端从 0 重传
                    io::copy(&mut r.by_ref().take(size - offset), &mut io::sink())?;
                    send(
                        &mut w,
                        &Response::Error {
                            message: "断点不一致，请从 0 重传".to_string(),
                        },
                    )?;
                    return Ok(());
                }
                let mut hasher = if offset > 0 {
                    seeded_hasher(&tmp, offset)?
                } else {
                    blake3::Hasher::new()
                };
                let file = if offset > 0 {
                    OpenOptions::new().append(true).open(&tmp)?
                } else {
                    File::create(&tmp)?
                };
                let mut writer = BufWriter::new(file);
                pump(&mut r, &mut writer, &mut hasher, size - offset)?;
                writer.flush()?;
                drop(writer);
                let got = hex_digest(&hasher);
                if got != hash {
                    fs::remove_file(&tmp).ok();
                    send(
                        &mut w,
                        &Response::Error {
                            message: format!("接收哈希不符：{got} != {hash}"),
                        },
                    )?;
                } else {
                    if dst.exists() {
                        fs::remove_file(&dst).ok();
                    }
                    fs::rename(&tmp, &dst)?;
                    send(&mut w, &Response::PutDone { hash: got })?;
                }
                Ok(())
            }
            Request::Move { from, to } => {
                let src = real.join(safe_rel(&from)?);
                let dst = real.join(safe_rel(&to)?);
                if let Some(p) = dst.parent() {
                    fs::create_dir_all(p)?;
                }
                if dst.exists() {
                    fs::remove_file(&dst).ok();
                }
                fs::rename(&src, &dst)?;
                send(&mut w, &Response::Ok)?;
                Ok(())
            }
            Request::Copy { from, to } => {
                let src = real.join(safe_rel(&from)?);
                let dst = real.join(safe_rel(&to)?);
                if let Some(p) = dst.parent() {
                    fs::create_dir_all(p)?;
                }
                fs::copy(&src, &dst)?;
                send(&mut w, &Response::Ok)?;
                Ok(())
            }
            Request::Delete { path } => {
                let dst = real.join(safe_rel(&path)?);
                fs::remove_file(&dst).ok();
                send(&mut w, &Response::Ok)?;
                Ok(())
            }
            Request::Hello { .. } => {
                send(
                    &mut w,
                    &Response::Error {
                        message: "重复 Hello".to_string(),
                    },
                )?;
                Ok(())
            }
            Request::Bye => Ok(()), // 已在循环顶部处理
        })();

        if let Err(e) = result {
            let _ = send(
                &mut w,
                &Response::Error {
                    message: e.to_string(),
                },
            );
        }
    }
    Ok(())
}
