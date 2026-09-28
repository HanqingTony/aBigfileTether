//! 应用：仓库 → 真实文件夹（由 `Fs` 抽象）。默认 dry-run；`--prune` 才删。
//!
//! 移动/已在位在执行时按目标影子的 `content_hash` 全量校验；新增而真实缺失的留待 distribute。

use crate::fs::Fs;
use crate::git;
use crate::model::Shadow;
use crate::scan::BASE_REF;
use crate::shadow as shadowmod;
use crate::walk;
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 一次应用的结果摘要。
#[derive(Debug, Default, Serialize)]
pub struct ApplyReport {
    pub renamed: usize,
    pub removed: usize,
    pub satisfied: usize,
    pub conflicts: usize,
    pub need_pull: usize,
    pub prune_skipped: usize,
    pub dry_run: bool,
    pub target: String,
    pub applied: bool,
    pub details: Vec<String>,
}

/// 把仓库某目标状态落到 `fs`（真实文件夹）。
pub fn apply(
    repo: &Path,
    fs: &dyn Fs,
    to: Option<String>,
    prune: bool,
    dry_run: bool,
) -> Result<ApplyReport> {
    let base = git::rev_parse_opt(repo, BASE_REF)
        .context("缺少 refs/tether/base：请先 `tether scan` 建立基线")?;
    let target_ref = to.unwrap_or_else(|| "HEAD".to_string());
    let target = git::rev_parse(repo, &target_ref)?;

    let entries = git::diff_name_status(repo, &base, &target, shadowmod::MIRROR_DIR)?;
    let mut report = ApplyReport {
        dry_run,
        target: target_ref,
        ..Default::default()
    };

    for e in entries {
        match e.status {
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
                if fs.stat(&old_rel)?.is_none() {
                    report.conflicts += 1;
                    report
                        .details
                        .push(format!("冲突：待移动源不存在 {}", old_rel.display()));
                    continue;
                }
                if fs.stat(&new_rel)?.is_some() {
                    report.conflicts += 1;
                    report
                        .details
                        .push(format!("冲突：目标已存在 {}", new_rel.display()));
                    continue;
                }
                if dry_run {
                    report.details.push(format!(
                        "移动 {} -> {}（执行时按哈希校验）",
                        old_rel.display(),
                        new_rel.display()
                    ));
                } else {
                    let exp = target_shadow(repo, &target, &new_rel)?;
                    let actual = fs.hash(&old_rel)?;
                    if actual != exp.content_hash {
                        report.conflicts += 1;
                        report.details.push(format!(
                            "冲突：{} 内容哈希与影子不符，拒绝移动",
                            old_rel.display()
                        ));
                        continue;
                    }
                    fs.mv(&old_rel, &new_rel)?;
                }
                report.renamed += 1;
            }
            'A' => {
                let Some(new) = e.new else { continue };
                let Some(new_rel) = shadowmod::rel_from_shadow_str(&new) else {
                    continue;
                };
                let Some(size) = fs.stat(&new_rel)? else {
                    report.need_pull += 1;
                    report.details.push(format!(
                        "需 distribute：{} 真实缺失（apply 不造字节）",
                        new_rel.display()
                    ));
                    continue;
                };
                let exp = target_shadow(repo, &target, &new_rel)?;
                if size != exp.size {
                    report.conflicts += 1;
                    report.details.push(format!(
                        "冲突：{} 已存在但大小与影子不符",
                        new_rel.display()
                    ));
                } else if dry_run {
                    report.satisfied += 1;
                    report
                        .details
                        .push(format!("已在位：{}（执行时按哈希校验）", new_rel.display()));
                } else {
                    let actual = fs.hash(&new_rel)?;
                    if actual == exp.content_hash {
                        report.satisfied += 1;
                        report
                            .details
                            .push(format!("已在位：{}", new_rel.display()));
                    } else {
                        report.conflicts += 1;
                        report.details.push(format!(
                            "冲突：{} 已存在但内容与影子不符",
                            new_rel.display()
                        ));
                    }
                }
            }
            'M' => {
                let name = e.new.or(e.old).unwrap_or_default();
                report.conflicts += 1;
                report
                    .details
                    .push(format!("冲突：{} 内容与影子不符，拒绝覆盖", name));
            }
            'D' => {
                let Some(old) = e.old else { continue };
                let Some(old_rel) = shadowmod::rel_from_shadow_str(&old) else {
                    continue;
                };
                if prune {
                    report.details.push(format!("删除 {}", old_rel.display()));
                } else {
                    report.prune_skipped += 1;
                }
            }
            _ => {}
        }
    }

    // prune：以目标快照为准，清掉落 location 上的多余文件（幂等，与 base 无关）
    if prune {
        let names = git::ls_tree_names(repo, &target, shadowmod::MIRROR_DIR)?;
        let target_set: HashSet<PathBuf> = names
            .iter()
            .filter_map(|s| shadowmod::rel_from_shadow_str(s))
            .collect();
        let raw = fs.walk()?;
        for f in walk::filter(repo, fs.root(), raw)? {
            if !target_set.contains(&f.rel) {
                if !dry_run {
                    fs.rm(&f.rel)?;
                }
                report.removed += 1;
                report
                    .details
                    .push(format!("删除(prune) {}", f.rel.display()));
            }
        }
    }

    if !dry_run && report.conflicts == 0 && report.need_pull == 0 {
        git::update_ref(repo, BASE_REF, &target)?;
        report.applied = true;
    }
    Ok(report)
}

fn target_shadow(repo: &Path, target: &str, rel: &Path) -> Result<Shadow> {
    let git_path = format!(
        "{}/{}{}",
        shadowmod::MIRROR_DIR,
        rel.to_string_lossy().replace('\\', "/"),
        shadowmod::SHADOW_EXT
    );
    let text = git::show(repo, target, &git_path)?;
    toml::from_str(&text).context("解析目标影子失败")
}
