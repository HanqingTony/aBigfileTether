//! 统一的文件系统抽象：`LocalFs`（std::fs）与 `RemoteFs`（ssh + 无状态 agent）。
//!
//! 方法用 `&self`，远端用 `Mutex<Agent>` 做内部可变——这样本地哈希仍可并行（`rayon`），
//! 远端则天然串行。`scan/apply/distribute/ingest` 都通过 `Fs` 操作真实文件夹。

use crate::config::{Location, TransferConfig};
use crate::transport::{self, Agent};
use anyhow::{Context, Result, bail};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

/// 一个真实文件条目。
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub rel: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
}

/// 真实文件夹的统一操作接口（`&self`，可跨线程共享）。
pub trait Fs: Send + Sync {
    fn root(&self) -> &Path;
    fn walk(&self) -> Result<Vec<FileEntry>>;
    fn hash(&self, rel: &Path) -> Result<String>;
    fn stat(&self, rel: &Path) -> Result<Option<u64>>;
    fn read_to(&self, rel: &Path, w: &mut dyn Write) -> Result<(u64, String)>;
    fn write_from(&self, rel: &Path, r: &mut dyn Read, size: u64, hash: &str) -> Result<()>;
    fn mv(&self, from: &Path, to: &Path) -> Result<()>;
    fn cp(&self, from: &Path, to: &Path) -> Result<()>;
    fn rm(&self, rel: &Path) -> Result<()>;
}

/// 按 `Location` 打开对应的 `Fs`。
pub fn open(location: &Location, transfer: &TransferConfig) -> Result<Box<dyn Fs>> {
    match location {
        Location::Local { root } => Ok(Box::new(LocalFs { root: root.clone() })),
        Location::Remote { host, root } => {
            let agent = Agent::connect(host, root, transfer)?;
            Ok(Box::new(RemoteFs {
                agent: Mutex::new(agent),
                root: root.clone(),
            }))
        }
    }
}

fn mtime_ns(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(_) => 0,
    }
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

// ---------- LocalFs ----------

pub struct LocalFs {
    pub root: PathBuf,
}

impl LocalFs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    fn full(&self, rel: &Path) -> PathBuf {
        self.root.join(rel)
    }
}

impl Fs for LocalFs {
    fn root(&self) -> &Path {
        &self.root
    }

    fn walk(&self) -> Result<Vec<FileEntry>> {
        let mut out = Vec::new();
        for e in WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !e.file_type().is_file() {
                continue;
            }
            let rel = match e.path().strip_prefix(&self.root) {
                Ok(r) => r.to_path_buf(),
                Err(_) => continue,
            };
            let md = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            out.push(FileEntry {
                rel,
                size: md.len(),
                mtime_ns: mtime_ns(md.modified().unwrap_or(UNIX_EPOCH)),
            });
        }
        Ok(out)
    }

    fn hash(&self, rel: &Path) -> Result<String> {
        crate::hash::blake3_file(&self.full(rel))
    }

    fn stat(&self, rel: &Path) -> Result<Option<u64>> {
        match fs::metadata(self.full(rel)) {
            Ok(m) => Ok(Some(m.len())),
            Err(_) => Ok(None),
        }
    }

    fn read_to(&self, rel: &Path, w: &mut dyn Write) -> Result<(u64, String)> {
        let p = self.full(rel);
        let mut f = File::open(&p).with_context(|| format!("打开 {} 失败", p.display()))?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0u64;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            w.write_all(&buf[..n])?;
            total += n as u64;
        }
        Ok((total, format!("blake3:{}", hasher.finalize().to_hex())))
    }

    fn write_from(&self, rel: &Path, r: &mut dyn Read, size: u64, hash: &str) -> Result<()> {
        let dst = self.full(rel);
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        let tmp = part_path(&dst);
        let file = File::create(&tmp)?;
        let mut hasher = blake3::Hasher::new();
        {
            let mut writer = BufWriter::new(file);
            let mut limited = r.take(size);
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = limited.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                writer.write_all(&buf[..n])?;
            }
            writer.flush()?;
        }
        let got = format!("blake3:{}", hasher.finalize().to_hex());
        if got != hash {
            fs::remove_file(&tmp).ok();
            bail!("接收哈希不符：{got} != {hash}");
        }
        if dst.exists() {
            fs::remove_file(&dst).ok();
        }
        fs::rename(&tmp, &dst)?;
        Ok(())
    }

    fn mv(&self, from: &Path, to: &Path) -> Result<()> {
        let src = self.full(from);
        let dst = self.full(to);
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        if dst.exists() {
            fs::remove_file(&dst).ok();
        }
        fs::rename(&src, &dst).with_context(|| format!("移动失败：{}", src.display()))
    }

    fn cp(&self, from: &Path, to: &Path) -> Result<()> {
        let src = self.full(from);
        let dst = self.full(to);
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        fs::copy(&src, &dst)?;
        Ok(())
    }

    fn rm(&self, rel: &Path) -> Result<()> {
        fs::remove_file(self.full(rel)).ok();
        Ok(())
    }
}

// ---------- RemoteFs ----------

pub struct RemoteFs {
    agent: Mutex<Agent>,
    root: PathBuf,
}

impl Fs for RemoteFs {
    fn root(&self) -> &Path {
        &self.root
    }

    fn walk(&self) -> Result<Vec<FileEntry>> {
        let mut a = self.agent.lock().unwrap();
        Ok(a.walk()?
            .into_iter()
            .map(|e| FileEntry {
                rel: transport::bytes_to_path(&e.path),
                size: e.size,
                mtime_ns: e.mtime_ns,
            })
            .collect())
    }

    fn hash(&self, rel: &Path) -> Result<String> {
        self.agent
            .lock()
            .unwrap()
            .hash(&transport::path_to_bytes(rel))
    }

    fn stat(&self, rel: &Path) -> Result<Option<u64>> {
        let (exists, size, _) = self
            .agent
            .lock()
            .unwrap()
            .stat(&transport::path_to_bytes(rel))?;
        Ok(if exists { Some(size) } else { None })
    }

    fn read_to(&self, rel: &Path, w: &mut dyn Write) -> Result<(u64, String)> {
        self.agent
            .lock()
            .unwrap()
            .read_to(&transport::path_to_bytes(rel), w)
    }

    fn write_from(&self, rel: &Path, r: &mut dyn Read, size: u64, hash: &str) -> Result<()> {
        self.agent
            .lock()
            .unwrap()
            .write_from(&transport::path_to_bytes(rel), r, size, hash)
    }

    fn mv(&self, from: &Path, to: &Path) -> Result<()> {
        self.agent.lock().unwrap().move_path(
            &transport::path_to_bytes(from),
            &transport::path_to_bytes(to),
        )
    }

    fn cp(&self, from: &Path, to: &Path) -> Result<()> {
        self.agent.lock().unwrap().copy_path(
            &transport::path_to_bytes(from),
            &transport::path_to_bytes(to),
        )
    }

    fn rm(&self, rel: &Path) -> Result<()> {
        self.agent
            .lock()
            .unwrap()
            .delete(&transport::path_to_bytes(rel))
    }
}

// ---------- 两个 Fs 之间的流式搬运 ----------

struct ChanWriter(std::sync::mpsc::SyncSender<Vec<u8>>);
impl Write for ChanWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "接收端已退出"))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ChanReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}
impl Read for ChanReader {
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

/// 把 `src` 上的 `srel` 流式搬到 `dst` 的 `drel`（不落本地临时盘）。
pub fn copy_between(
    src: &dyn Fs,
    srel: &Path,
    dst: &dyn Fs,
    drel: &Path,
    size: u64,
    hash: &str,
) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(32);
    let mut writer = ChanWriter(tx);
    let mut reader = ChanReader {
        rx,
        buf: Vec::new(),
        pos: 0,
    };
    std::thread::scope(|s| -> Result<()> {
        let h = s.spawn(move || src.read_to(srel, &mut writer));
        let r = dst.write_from(drel, &mut reader, size, hash);
        let src_res = h.join().map_err(|_| anyhow::anyhow!("搬运线程 panic"))?;
        r?;
        src_res?;
        Ok(())
    })
}
