//! 数据模型：影子文件、证书、本地配置。
//!
//! 字段与 schema 见 `docs/DESIGN.md` §4。统一使用 TOML。

use serde::{Deserialize, Serialize};

/// 影子文件 `<relpath>.tether`（见 DESIGN §4.1）。
///
/// `mtime` 仅记录、不参与身份判断；身份 = `size + content_hash`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shadow {
    /// 完整 blake3，形如 `blake3:<hex>`。
    pub content_hash: String,
    pub size: u64,
    /// 记录用时间戳（RFC3339）。
    pub mtime: String,
    /// 该影子最后一次被写入的时间（RFC3339）；仅新建/修改时更新。
    pub last_seen: String,
}

/// 证书 `TETHER.cert.toml`（见 DESIGN §4.2）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Certificate {
    pub schema: u32,
    pub scanned_at: String,
    pub count: u64,
    pub total_size: u64,
    /// 对排序后的 `(path, size, content_hash)` 再哈希。
    pub root_hash: String,
    #[serde(default, rename = "files")]
    pub files: Vec<CertEntry>,
}

/// 证书中的一条文件清单项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CertEntry {
    pub path: String,
    pub size: u64,
    pub content_hash: String,
}
