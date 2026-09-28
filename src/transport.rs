//! 传输：SSH 字节管道 + 自研分帧协议 + **无状态 agent（以 root 真实文件夹为根）**。
//!
//! 中心模型：程序只在母机一份；远端只运行同一个二进制的 `agent` 子命令（无仓库、无配置），
//! 由 `Hello{root}` 告知其服务的真实文件夹。远端可 `Walk/Hash`（因此新文件与路径变化都能靠
//! 哈希识别），也可 `Get/Put/Move/Copy/Delete`。
//!
//! 帧 = `u32` 长度前缀 + bincode 负载；`path` 一律长度前缀的**原始字节**。

use crate::{config::TransferConfig, hash};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// 协议版本（v3：无状态、root 为根，新增 Walk/Hash）。
pub const PROTO_VERSION: u32 = 3;

/// 客户端 → agent 请求。
#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Hello {
        version: u32,
        root: Vec<u8>,
    },
    /// 列出 root 下所有文件（相对路径、大小、mtime_ns）。
    Walk,
    /// 计算某文件 blake3。
    Hash {
        path: Vec<u8>,
    },
    Stat {
        path: Vec<u8>,
    },
    Get {
        path: Vec<u8>,
        offset: u64,
    },
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
    Files {
        entries: Vec<FileEntryWire>,
    },
    Hash {
        hash: String,
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

/// Walk 的一条记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntryWire {
    pub path: Vec<u8>,
    pub size: u64,
    pub mtime_ns: i64,
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

/// 校验并转换相对路径，拒绝绝对路径与 `..` 逃逸。
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

pub fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

fn mtime_ns(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(_) => 0,
    }
}

/// 读取文件前缀以恢复 blake3（续传用）。
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

trait Connection: Send {
    fn reader(&mut self) -> &mut (dyn Read + Send);
    fn writer(&mut self) -> &mut (dyn Write + Send);
    fn finish(&mut self);
    fn kill(&mut self);
}

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

// ---------- 客户端 ----------

/// 已连接的远端 agent（无状态，以 root 为根）。
pub struct Agent {
    conn: Box<dyn Connection>,
    root: PathBuf,
}

impl Agent {
    /// 连接远端并握手，告知其服务的 root。
    pub fn connect(host: &str, root: &Path, transfer: &TransferConfig) -> Result<Agent> {
        let remote_bin = transfer
            .remote_bin
            .clone()
            .unwrap_or_else(|| "tether".to_string());
        let mut conn = open_connection(host, &remote_bin, transfer)?;
        send(
            conn.writer(),
            &Request::Hello {
                version: PROTO_VERSION,
                root: path_to_bytes(root),
            },
        )?;
        let mut agent = Agent {
            conn,
            root: root.to_path_buf(),
        };
        match recv::<_, Response>(agent.conn.reader())? {
            Response::HelloAck { version } if version == PROTO_VERSION => Ok(agent),
            Response::HelloAck { version } => bail!("协议版本不一致：对端 {version}"),
            Response::Error { message } => bail!("对端握手失败：{message}"),
            other => bail!("握手异常响应：{other:?}"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
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

    pub fn walk(&mut self) -> Result<Vec<FileEntryWire>> {
        self.send(&Request::Walk)?;
        match self.response()? {
            Response::Files { entries } => Ok(entries),
            Response::Error { message } => bail!("对端错误：{message}"),
            other => bail!("非预期响应：{other:?}"),
        }
    }

    pub fn hash(&mut self, path: &[u8]) -> Result<String> {
        self.send(&Request::Hash {
            path: path.to_vec(),
        })?;
        match self.response()? {
            Response::Hash { hash } => Ok(hash),
            Response::Error { message } => bail!("对端错误：{message}"),
            other => bail!("非预期响应：{other:?}"),
        }
    }

    pub fn stat(&mut self, path: &[u8]) -> Result<(bool, u64, u64)> {
        self.send(&Request::Stat {
            path: path.to_vec(),
        })?;
        match self.response()? {
            Response::Stat {
                real_exists,
                real_size,
                part_size,
            } => Ok((real_exists, real_size, part_size)),
            Response::Error { message } => bail!("对端错误：{message}"),
            other => bail!("非预期响应：{other:?}"),
        }
    }

    /// 从 `offset` 起流式取回文件写入 `writer`，返回完整 (size, hash)。
    pub fn read_to(
        &mut self,
        path: &[u8],
        writer: &mut dyn Write,
        offset: u64,
    ) -> Result<(u64, String)> {
        self.send(&Request::Get {
            path: path.to_vec(),
            offset,
        })?;
        let (size, hash) = match self.response()? {
            Response::Started { size, hash } => (size, hash),
            Response::Error { message } => bail!("对端错误：{message}"),
            other => bail!("非预期响应：{other:?}"),
        };
        let mut limited = self.conn.reader().take(size - offset);
        io::copy(&mut limited, writer).context("接收字节失败")?;
        Ok((size, hash))
    }

    /// 从 `offset` 起把 `reader` 流式写入远端文件（续传），agent 校验完整哈希。
    pub fn write_from(
        &mut self,
        path: &[u8],
        reader: &mut dyn Read,
        offset: u64,
        size: u64,
        hash: &str,
    ) -> Result<()> {
        self.send(&Request::Put {
            path: path.to_vec(),
            offset,
            size,
            hash: hash.to_string(),
        })?;
        io::copy(&mut reader.take(size - offset), self.conn.writer()).context("发送字节失败")?;
        self.conn.writer().flush()?;
        match self.response()? {
            Response::PutDone { .. } => Ok(()),
            Response::Error { message } => bail!("对端错误：{message}"),
            other => bail!("非预期响应：{other:?}"),
        }
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

fn open_connection(
    host: &str,
    remote_bin: &str,
    transfer: &TransferConfig,
) -> Result<Box<dyn Connection>> {
    match transfer.backend.as_deref().unwrap_or("system-ssh") {
        "system-ssh" | "ssh" => Ok(Box::new(ChildConnection::spawn(
            Command::new("ssh")
                .arg("-o")
                .arg("BatchMode=yes")
                .arg(host)
                .arg("--")
                .arg(remote_bin)
                .arg("agent"),
        )?)),
        "russh" => {
            #[cfg(feature = "russh")]
            {
                let key = transfer
                    .key
                    .clone()
                    .context("russh 后端需要 [transfer] key（私钥路径）")?;
                let (user, host_only) = match host.split_once('@') {
                    Some((u, h)) => (u.to_string(), h.to_string()),
                    None => ("root".to_string(), host.to_string()),
                };
                Ok(Box::new(russh_conn::connect(
                    &host_only,
                    22,
                    &user,
                    std::path::Path::new(&key),
                    &format!("{remote_bin} agent"),
                )?))
            }
            #[cfg(not(feature = "russh"))]
            {
                let _ = host;
                bail!(
                    "此构建未启用 russh 特性：用 `cargo build --features russh` 重编，或把 [transfer] backend 设为 system-ssh"
                )
            }
        }
        other => bail!("未知 [transfer] backend：{other}（system-ssh | russh）"),
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
                    Err(_) => return Ok(0),
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
            let h = session
                .best_supported_rsa_hash()
                .await
                .ok()
                .flatten()
                .flatten();
            let auth = match session
                .authenticate_publickey(
                    &user,
                    russh::keys::PrivateKeyWithHashAlg::new(Arc::new(kp), h),
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
                        if channel.data(&bytes[..]).await.is_err() { break; }
                    }
                    Some(msg) = channel.wait() => {
                        match msg {
                            russh::ChannelMsg::Data { ref data } => {
                                if from_tx.send(data.to_vec()).is_err() { break; }
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

// ---------- agent 服务端（无状态） ----------

/// 远端 agent：收到 `Hello{root}` 后，对 root 提供 Walk/Hash/Get/Put/Move/Copy/Delete。
pub fn run_agent() -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut r = BufReader::new(stdin.lock());
    let mut w = stdout.lock();

    let root = match recv::<_, Request>(&mut r)? {
        Request::Hello { version, root } => {
            if version != PROTO_VERSION {
                send(
                    &mut w,
                    &Response::Error {
                        message: format!("协议版本不一致：客户端 {version}"),
                    },
                )?;
                bail!("协议版本不一致");
            }
            bytes_to_path(&root)
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
    if !root.is_dir() {
        send(
            &mut w,
            &Response::Error {
                message: format!("root 不存在或不是目录：{}", root.display()),
            },
        )?;
        bail!("root 不存在：{}", root.display());
    }
    send(
        &mut w,
        &Response::HelloAck {
            version: PROTO_VERSION,
        },
    )?;

    loop {
        let req: Request = match recv(&mut r) {
            Ok(req) => req,
            Err(_) => break,
        };
        if matches!(req, Request::Bye) {
            break;
        }
        let result: Result<()> = (|| match req {
            Request::Walk => {
                let mut entries = Vec::new();
                for e in walkdir::WalkDir::new(&root)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    if !e.file_type().is_file() {
                        continue;
                    }
                    let rel = match e.path().strip_prefix(&root) {
                        Ok(r) => r,
                        Err(_) => continue,
                    };
                    let md = match e.metadata() {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    entries.push(FileEntryWire {
                        path: path_to_bytes(rel),
                        size: md.len(),
                        mtime_ns: mtime_ns(md.modified().unwrap_or(UNIX_EPOCH)),
                    });
                }
                send(&mut w, &Response::Files { entries })?;
                Ok(())
            }
            Request::Hash { path } => {
                let src = root.join(safe_rel(&path)?);
                let digest = hash::blake3_file(&src)?;
                send(&mut w, &Response::Hash { hash: digest })?;
                Ok(())
            }
            Request::Stat { path } => {
                let dst = root.join(safe_rel(&path)?);
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
                let src = root.join(safe_rel(&path)?);
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
                let digest = hash::blake3_file(&src)?;
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
                let dst = root.join(safe_rel(&path)?);
                if let Some(p) = dst.parent() {
                    fs::create_dir_all(p)?;
                }
                let tmp = part_path(&dst);
                let cur = fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
                if offset != 0 && cur != offset {
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
                let src = root.join(safe_rel(&from)?);
                let dst = root.join(safe_rel(&to)?);
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
                let src = root.join(safe_rel(&from)?);
                let dst = root.join(safe_rel(&to)?);
                if let Some(p) = dst.parent() {
                    fs::create_dir_all(p)?;
                }
                fs::copy(&src, &dst)?;
                send(&mut w, &Response::Ok)?;
                Ok(())
            }
            Request::Delete { path } => {
                let dst = root.join(safe_rel(&path)?);
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
            Request::Bye => Ok(()),
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
