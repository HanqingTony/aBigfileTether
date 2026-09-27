//! 对等传输：SSH/本地 字节管道 + 自研分帧协议 + 哈希感知搬运。
//!
//! 见 `docs/DESIGN.md` §5.3。要点：
//! - `ssh <host> -- tether agent`（**命令行不带任何用户路径**），随后走本协议；
//!   首个 `Hello` 消息里携带仓库路径，彻底规避 shell 转义问题。
//! - 帧 = `u32` 长度前缀 + bincode 负载；`path` 一律长度前缀的**原始字节**。
//! - 大文件：已知大小的裸流传输，收发两端流式算 blake3 校验，写 `.part` 后原子改名。

use crate::{config, hash, shadow as shadowmod};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// 协议版本。
pub const PROTO_VERSION: u32 = 1;

/// 客户端 → agent 请求。
#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Hello {
        version: u32,
        repo: Vec<u8>,
    },
    ListIndex,
    Get {
        path: Vec<u8>,
    },
    Put {
        path: Vec<u8>,
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
    HelloAck { version: u32 },
    Index { entries: Vec<IndexEntry> },
    Started { size: u64, hash: String },
    PutDone { hash: String },
    Ok,
    Error { message: String },
}

/// 索引项：真实相对路径（原始字节）+ 大小 + 哈希。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub path: Vec<u8>,
    pub size: u64,
    pub hash: String,
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

// ---------- 分帧 ----------

fn write_frame<W: Write>(w: &mut W, bytes: &[u8]) -> Result<()> {
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()?;
    Ok(())
}

fn read_frame<R: Read>(r: &mut R) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)
        .context("读取帧长度失败（对端可能退出）")?;
    let n = u32::from_le_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).context("读取帧负载失败")?;
    Ok(buf)
}

fn send<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    let bytes = bincode::serialize(msg).context("序列化协议消息失败")?;
    write_frame(w, &bytes)
}

fn recv<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T> {
    let bytes = read_frame(r)?;
    bincode::deserialize(&bytes).context("反序列化协议消息失败")
}

/// 边写边算 blake3 的写入器。
struct HashingWriter<W: Write> {
    inner: W,
    hasher: blake3::Hasher,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
        }
    }
    fn digest(&self) -> String {
        format!("blake3:{}", self.hasher.finalize().to_hex())
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// ---------- 对等端描述 ----------

/// 对等端：生产用 SSH，本地用于测试。
#[derive(Debug, Clone)]
pub enum Peer {
    /// 本地直接拉起 `tether agent`（冒烟测试用）。
    Local { bin: PathBuf, repo: PathBuf },
    /// 远程：`ssh <host> -- tether agent`，仓库路径经协议 Hello 传递。
    Ssh { host: String, repo: String },
}

impl Peer {
    /// 解析 `user@host:/abs/repo`。
    pub fn parse(spec: &str) -> Result<Peer> {
        let (host, repo) = spec
            .split_once(':')
            .with_context(|| format!("对等端格式应为 user@host:/repo，得到：{spec}"))?;
        Ok(Peer::Ssh {
            host: host.to_string(),
            repo: repo.to_string(),
        })
    }

    fn repo_bytes(&self) -> Vec<u8> {
        match self {
            Peer::Local { repo, .. } => path_to_bytes(repo),
            Peer::Ssh { repo, .. } => repo.as_bytes().to_vec(),
        }
    }

    fn spawn(&self) -> Result<Child> {
        match self {
            Peer::Local { bin, .. } => Command::new(bin)
                .arg("agent")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .with_context(|| format!("无法启动本地 agent：{}", bin.display())),
            Peer::Ssh { host, .. } => Command::new("ssh")
                .args(["-o", "BatchMode=yes", host, "--", "tether", "agent"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .with_context(|| format!("无法 ssh 到对等端：{host}")),
        }
    }
}

// ---------- 客户端 ----------

/// 已连接的 agent 客户端。
pub struct Agent {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Agent {
    /// 连接对等端并完成握手。
    pub fn connect(peer: &Peer) -> Result<Agent> {
        let mut child = peer.spawn()?;
        let stdin = child.stdin.take().context("agent stdin 不可用")?;
        let stdout = child.stdout.take().context("agent stdout 不可用")?;
        let mut agent = Agent {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };
        send(
            &mut agent.stdin,
            &Request::Hello {
                version: PROTO_VERSION,
                repo: peer.repo_bytes(),
            },
        )?;
        match agent.response()? {
            Response::HelloAck { version } if version == PROTO_VERSION => Ok(agent),
            Response::HelloAck { version } => bail!("协议版本不一致：对端 {version}"),
            Response::Error { message } => bail!("对端握手失败：{message}"),
            other => bail!("握手异常响应：{other:?}"),
        }
    }

    fn send(&mut self, req: &Request) -> Result<()> {
        send(&mut self.stdin, req)
    }

    fn response(&mut self) -> Result<Response> {
        recv(&mut self.stdout)
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

    /// 从对端某路径拉取文件到本地 `dest`，校验哈希后原子改名。
    pub fn get(&mut self, remote: &[u8], dest: &Path, expected_hash: &str) -> Result<()> {
        self.send(&Request::Get {
            path: remote.to_vec(),
        })?;
        let (size, hash) = match self.response()? {
            Response::Started { size, hash } => (size, hash),
            Response::Error { message } => bail!("对端错误：{message}"),
            other => bail!("非预期响应：{other:?}"),
        };
        if hash != expected_hash {
            bail!("对端源文件哈希不符：{hash} != {expected_hash}");
        }
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p)?;
        }
        let tmp = part_path(dest);
        let file = File::create(&tmp).with_context(|| format!("创建 {} 失败", tmp.display()))?;
        let mut hw = HashingWriter::new(BufWriter::new(file));
        let mut limited = self.stdout.by_ref().take(size);
        io::copy(&mut limited, &mut hw).context("接收文件字节失败")?;
        hw.flush()?;
        let got = hw.digest();
        drop(hw);
        if got != expected_hash {
            fs::remove_file(&tmp).ok();
            bail!("接收内容哈希不符：{got} != {expected_hash}");
        }
        if dest.exists() {
            fs::remove_file(dest).ok();
        }
        fs::rename(&tmp, dest).with_context(|| format!("落盘 {} 失败", dest.display()))?;
        Ok(())
    }

    /// 把本地文件推给对端某路径。
    pub fn put(&mut self, remote: &[u8], local: &Path, size: u64, hash: &str) -> Result<()> {
        self.send(&Request::Put {
            path: remote.to_vec(),
            size,
            hash: hash.to_string(),
        })?;
        let mut f = File::open(local).with_context(|| format!("打开 {} 失败", local.display()))?;
        io::copy(
            &mut std::io::Read::by_ref(&mut f).take(size),
            &mut self.stdin,
        )
        .context("发送文件字节失败")?;
        self.stdin.flush()?;
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

    /// 发送 Bye 并等待退出。
    pub fn close(mut self) -> Result<()> {
        send(&mut self.stdin, &Request::Bye)?;
        let _ = self.child.wait();
        Ok(())
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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

    loop {
        let req: Request = match recv(&mut r) {
            Ok(req) => req,
            Err(_) => break, // 客户端断开
        };
        // Bye 必须跳出整个循环（在闭包里 `return` 只会退出闭包，会死锁）
        if matches!(req, Request::Bye) {
            break;
        }
        let result: Result<()> = (|| {
            match req {
                Request::ListIndex => {
                    let index = shadowmod::load_index(&repo)?;
                    let entries = index
                        .into_iter()
                        .map(|(rel, s)| IndexEntry {
                            path: path_to_bytes(&rel),
                            size: s.size,
                            hash: s.content_hash,
                        })
                        .collect();
                    send(&mut w, &Response::Index { entries })?;
                }
                Request::Get { path } => {
                    let src = real.join(safe_rel(&path)?);
                    let md = fs::metadata(&src)
                        .with_context(|| format!("对端缺少文件：{}", src.display()))?;
                    let digest = hash::blake3_file(&src)?;
                    send(
                        &mut w,
                        &Response::Started {
                            size: md.len(),
                            hash: digest,
                        },
                    )?;
                    let mut f = File::open(&src)?;
                    io::copy(&mut std::io::Read::by_ref(&mut f).take(md.len()), &mut w)?;
                    w.flush()?;
                }
                Request::Put { path, size, hash } => {
                    let dest = real.join(safe_rel(&path)?);
                    if let Some(p) = dest.parent() {
                        fs::create_dir_all(p)?;
                    }
                    let tmp = part_path(&dest);
                    let file = File::create(&tmp)?;
                    let mut hw = HashingWriter::new(BufWriter::new(file));
                    let mut limited = r.by_ref().take(size);
                    io::copy(&mut limited, &mut hw)?;
                    hw.flush()?;
                    let got = hw.digest();
                    drop(hw);
                    if got != hash {
                        fs::remove_file(&tmp).ok();
                        send(
                            &mut w,
                            &Response::Error {
                                message: format!("接收哈希不符：{got} != {hash}"),
                            },
                        )?;
                    } else {
                        if dest.exists() {
                            fs::remove_file(&dest).ok();
                        }
                        fs::rename(&tmp, &dest)?;
                        send(&mut w, &Response::PutDone { hash: got })?;
                    }
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
                }
                Request::Copy { from, to } => {
                    let src = real.join(safe_rel(&from)?);
                    let dst = real.join(safe_rel(&to)?);
                    if let Some(p) = dst.parent() {
                        fs::create_dir_all(p)?;
                    }
                    fs::copy(&src, &dst)?;
                    send(&mut w, &Response::Ok)?;
                }
                Request::Delete { path } => {
                    let dst = real.join(safe_rel(&path)?);
                    fs::remove_file(&dst).ok();
                    send(&mut w, &Response::Ok)?;
                }
                Request::Hello { .. } => {
                    send(
                        &mut w,
                        &Response::Error {
                            message: "重复 Hello".to_string(),
                        },
                    )?;
                }
                Request::Bye => {} // 已在循环顶部处理
            }
            Ok(())
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
