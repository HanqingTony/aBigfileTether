//! 影子文件读写与镜像树索引（`<relpath>.tether`，TOML）。
//!
//! 见 `docs/DESIGN.md` §3、§4.1。

use crate::model::Shadow;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// 仓库内镜像目录名。
pub const MIRROR_DIR: &str = "mirrors";
/// 影子文件后缀。
pub const SHADOW_EXT: &str = ".tether";
/// 真实文件夹顶层证书文件名。
pub const CERT_NAME: &str = "TETHER.cert.toml";

/// 真实相对路径 → 仓库内影子文件绝对路径。
pub fn shadow_path(repo: &Path, rel: &Path) -> PathBuf {
    let mut p = repo.join(MIRROR_DIR).join(rel);
    let mut name = p.file_name().map(|s| s.to_os_string()).unwrap_or_default();
    name.push(SHADOW_EXT);
    p.set_file_name(name);
    p
}

/// 仓库内影子路径字符串（git 输出，形如 `mirrors/a/b.tether`）→ 真实相对路径。
pub fn rel_from_shadow_str(s: &str) -> Option<PathBuf> {
    let s = s.strip_prefix(&format!("{MIRROR_DIR}/"))?;
    let s = s.strip_suffix(SHADOW_EXT)?;
    Some(PathBuf::from(s))
}

/// 读取单个影子文件。
pub fn read(path: &Path) -> Result<Shadow> {
    let text =
        fs::read_to_string(path).with_context(|| format!("读取影子失败：{}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("解析影子 TOML 失败：{}", path.display()))
}

/// 写入单个影子文件（自动建父目录）。
pub fn write(path: &Path, shadow: &Shadow) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败：{}", parent.display()))?;
    }
    let text = toml::to_string_pretty(shadow).context("序列化影子 TOML 失败")?;
    fs::write(path, text).with_context(|| format!("写入影子失败：{}", path.display()))
}

/// 从镜像工作区建立 `真实相对路径 -> Shadow` 索引。
pub fn load_index(repo: &Path) -> Result<BTreeMap<PathBuf, Shadow>> {
    let mirror = repo.join(MIRROR_DIR);
    let mut index = BTreeMap::new();
    if !mirror.is_dir() {
        return Ok(index);
    }
    for entry in WalkDir::new(&mirror).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !name.ends_with(SHADOW_EXT) {
            continue;
        }
        let rel = match path.strip_prefix(&mirror) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // 去掉 .tether 后缀得到真实相对路径
        let rel = PathBuf::from(rel.to_string_lossy().trim_end_matches(SHADOW_EXT));
        index.insert(rel, read(path)?);
    }
    Ok(index)
}

/// 自底向上删除空目录（不动 `mirrors` 本身）。
pub fn prune_empty_dirs(repo: &Path, from: &Path) {
    let mirror = repo.join(MIRROR_DIR);
    let mut cur = from.to_path_buf();
    while cur.starts_with(&mirror) && cur != mirror {
        match fs::read_dir(&cur) {
            Ok(mut it) => {
                if it.next().is_some() {
                    break;
                }
            }
            Err(_) => break,
        }
        if fs::remove_dir(&cur).is_err() {
            break;
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => break,
        }
    }
}
