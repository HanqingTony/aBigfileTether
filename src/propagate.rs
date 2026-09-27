//! 传播：把某分支相对共同祖先的 **A/M/R** 应用到当前分支，**丢弃 D（删除）**。
//!
//! 用途：子仓库/子分支是剪枝视图（相对 main 有大量"未包含"= D）。直接把这种分支
//! `git merge` 进 main 会把这些 D 也带过去、删掉 main 的文件。`propagate` 只取
//! 真正的 新增/移动/修改，不传播删除，因此可以安全地把子分支的增量并回 main。
//!
//! 只在**影子仓库**层操作（不触碰真实文件夹）；真实侧随后用 `tether apply` 落地
//! （新增条目若本地无字节，再 `tether pull`）。

use crate::{git, shadow as shadowmod};
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

/// 一次传播的结果摘要。
#[derive(Debug, Default, Serialize)]
pub struct PropagateReport {
    pub moved: usize,
    pub added: usize,
    pub updated: usize,
    pub skipped_deleted: usize,
    pub skipped_existing: usize,
    pub conflicts: usize,
    pub dry_run: bool,
    pub from: String,
    pub onto: String,
    pub committed: bool,
    pub commit: Option<String>,
    pub details: Vec<String>,
}

/// 把 `from` 相对 `merge-base(onto, from)` 的 A/M/R 应用到 `onto`（默认 HEAD）。
pub fn propagate(
    repo: &Path,
    from: &str,
    onto: Option<String>,
    dry_run: bool,
) -> Result<PropagateReport> {
    let onto_ref = onto.unwrap_or_else(|| "HEAD".to_string());
    let onto_sha = git::rev_parse(repo, &onto_ref)?;
    let from_sha = git::rev_parse(repo, from)?;
    let base = git::merge_base(repo, &onto_sha, &from_sha)?;

    let entries = git::diff_name_status(repo, &base, &from_sha, shadowmod::MIRROR_DIR)?;
    let mut report = PropagateReport {
        dry_run,
        from: from.to_string(),
        onto: onto_ref,
        ..Default::default()
    };

    for e in entries {
        match e.status {
            // 删除一律不传播
            'D' => report.skipped_deleted += 1,
            'R' => {
                let (Some(old), Some(new)) = (e.old, e.new) else {
                    continue;
                };
                let (Some(old_rel), Some(new_rel)) = (
                    shadowmod::rel_from_shadow_str(&old),
                    shadowmod::rel_from_shadow_str(&new),
                ) else {
                    continue;
                };
                let dst = shadowmod::shadow_path(repo, &new_rel);
                if dst.exists() {
                    report.skipped_existing += 1;
                    continue;
                }
                let src = shadowmod::shadow_path(repo, &old_rel);
                if dry_run {
                    report.details.push(format!(
                        "移动 {} -> {}",
                        old_rel.display(),
                        new_rel.display()
                    ));
                } else if src.exists() {
                    if let Some(p) = dst.parent() {
                        fs::create_dir_all(p)?;
                    }
                    fs::rename(&src, &dst)?;
                    if let Some(p) = src.parent() {
                        shadowmod::prune_empty_dirs(repo, p);
                    }
                } else {
                    // 源不在本地（例如 main 里本就没有）：按 from 的 blob 直接写入
                    write_blob(repo, &from_sha, &new, &dst)?;
                }
                report.moved += 1;
            }
            'A' => {
                let Some(new) = e.new else { continue };
                let Some(new_rel) = shadowmod::rel_from_shadow_str(&new) else {
                    continue;
                };
                let dst = shadowmod::shadow_path(repo, &new_rel);
                if dst.exists() {
                    report.skipped_existing += 1;
                    continue;
                }
                if dry_run {
                    report.details.push(format!("新增 {}", new_rel.display()));
                } else {
                    write_blob(repo, &from_sha, &new, &dst)?;
                }
                report.added += 1;
            }
            'M' | 'T' => {
                let Some(path) = e.new.or(e.old) else {
                    continue;
                };
                let Some(rel) = shadowmod::rel_from_shadow_str(&path) else {
                    continue;
                };
                let dst = shadowmod::shadow_path(repo, &rel);
                if !dry_run {
                    write_blob(repo, &from_sha, &path, &dst)?;
                }
                report.updated += 1;
                report.details.push(format!("更新 {}", rel.display()));
            }
            _ => {}
        }
    }

    if !dry_run {
        git::add_all(repo, shadowmod::MIRROR_DIR)?;
        let msg = format!(
            "tether propagate: from {} -> {} (+{} ~{} ->{} ; 丢弃删除 {} )",
            from, report.onto, report.added, report.updated, report.moved, report.skipped_deleted
        );
        report.committed = git::commit(repo, &msg)?;
        if report.committed {
            report.commit = git::rev_parse_opt(repo, "HEAD");
        }
    }
    Ok(report)
}

/// 把 `from_sha` 下某影子 blob 写到工作区 `dst`。
fn write_blob(repo: &Path, from_sha: &str, git_path: &str, dst: &Path) -> Result<()> {
    let text = git::show(repo, from_sha, git_path).context("读取影子 blob 失败")?;
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p)?;
    }
    fs::write(dst, text).with_context(|| format!("写入影子失败：{}", dst.display()))
}

/// 便于测试：影子 git 路径 → 真实相对路径。
pub fn shadow_git_path_to_rel(s: &str) -> Option<PathBuf> {
    shadowmod::rel_from_shadow_str(s)
}
