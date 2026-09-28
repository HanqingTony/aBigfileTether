//! 排除规则（`.tetherignore` + 内置）：对 `Fs::walk` 得到的条目做过滤。

use crate::{config, fs::FileEntry, shadow as shadowmod};
use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::fs;
use std::path::Path;

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

fn build_matcher(repo: &Path, root: &Path) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
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

/// 过滤 `entries`（相对 `root`），保留纳入管理的文件。
pub fn filter(repo: &Path, root: &Path, entries: Vec<FileEntry>) -> Result<Vec<FileEntry>> {
    let matcher = build_matcher(repo, root)?;
    let mut out = Vec::new();
    for e in entries {
        if builtin_ignored(&e.rel) {
            continue;
        }
        if matcher
            .matched_path_or_any_parents(root.join(&e.rel), false)
            .is_ignore()
        {
            continue;
        }
        out.push(e);
    }
    Ok(out)
}
