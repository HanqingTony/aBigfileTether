//! 内容哈希：blake3 全量哈希。
//!
//! 仅对扫描初筛（路径 + size）判定为变动的文件计算；不做部分哈希、不掺时间。
//! 见 `docs/DESIGN.md` §5.1。

use anyhow::{Context, Result};
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// 流式读取整个文件并对内容做 blake3，返回 `blake3:<hex>`。
pub fn blake3_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("打开文件失败：{}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("读取文件失败：{}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

/// 对任意字节串做 blake3（用于证书 root_hash）。
pub fn blake3_bytes(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}
