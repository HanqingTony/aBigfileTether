//! 真实文件夹遍历 + 排除规则（`.tetherignore` + 内置）。
//!
//! 扫描与 `apply --prune` 共用，保证两侧对"哪些文件算数"的判断一致。

use crate::{config, shadow as shadowmod};
use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

/// 真实文件夹中的一个文件。
pub struct RealFile {
    /// 相对真实根的路径。
    pub rel: PathBuf,
    pub size: u64,
    pub mtime: SystemTime,
}

/// 内置排除：证书自身、传输中间文件、VCS 元数据。
fn builtin_ignored(rel: &Path) -> bool {
    let top_level = rel.parent().is_none_or(|p| p.as_os_str().is_empty());
    if top_level && rel.file_name().is_some_and(|n| n == shadowmod::CERT_NAME) {
        return true;
    }
    if rel
        .file_name()
        .is_some_and(|n| n.to_string_lossy().ends_with(".part"))
    {
        return true;
    }
    if rel.components().any(|c| c.as_os_str() == ".git") {
        return true;
    }
    false
}

/// 由仓库内 `.tetherignore` 构建匹配器。
fn build_matcher(repo: &Path, real: &Path) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new(real);
    let p = repo.join(config::IGNORE_NAME);
    if p.is_file() {
        let text = fs::read_to_string(&p).with_context(|| format!("读取 {} 失败", p.display()))?;
        for line in text.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            builder
                .add_line(None, t)
                .with_context(|| format!("非法 .tetherignore 规则：{t}"))?;
        }
    }
    builder.build().context("构建 .tetherignore 匹配器失败")
}

/// 收集真实文件夹内所有纳入管理的文件（符号链接、目录一概跳过）。
pub fn collect(repo: &Path, real: &Path) -> Result<Vec<RealFile>> {
    let matcher = build_matcher(repo, real)?;
    let mut out = Vec::new();
    let walker = WalkDir::new(real)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let rel = e.path().strip_prefix(real).unwrap_or(e.path());
            if rel.as_os_str().is_empty() {
                return true; // 根目录
            }
            if builtin_ignored(rel) {
                return false;
            }
            let is_dir = e.file_type().is_dir();
            !matcher
                .matched_path_or_any_parents(e.path(), is_dir)
                .is_ignore()
        });
    for entry in walker {
        let entry = entry.with_context(|| format!("遍历 {} 失败", real.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(real)
            .context("相对路径计算失败")?
            .to_path_buf();
        let md = entry.metadata().context("读取文件元数据失败")?;
        out.push(RealFile {
            rel,
            size: md.len(),
            mtime: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    Ok(out)
}
