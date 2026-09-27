//! 应用：仓库 → 真实文件夹（默认 dry-run，`--prune` 才删）。
//!
//! 见 `docs/DESIGN.md` §5.2。以 `refs/tether/base` 为基线，取
//! `base..target` 在镜像树上的差异，映射为真实文件夹操作。
//!
//! 原则：**绝不凭空造字节、绝不覆盖内容**。新增而真实缺失的条目留待 `pull`；
//! 移动/已在位在执行时按目标影子的 `content_hash` 做**全量校验**。

use crate::git;
use crate::hash;
use crate::model::Shadow;
use crate::scan::BASE_REF;
use crate::shadow as shadowmod;
use crate::walk;
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
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

/// 把仓库某目标状态落到真实文件夹。
pub fn apply(
    repo: &Path,
    real: &Path,
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
                let src = real.join(&old_rel);
                let dst = real.join(&new_rel);
                if !src.exists() {
                    report.conflicts += 1;
                    report
                        .details
                        .push(format!("冲突：待移动源不存在 {}", src.display()));
                    continue;
                }
                if dst.exists() {
                    report.conflicts += 1;
                    report
                        .details
                        .push(format!("冲突：目标已存在 {}", dst.display()));
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
                    let actual = hash::blake3_file(&src)?;
                    if actual != exp.content_hash {
                        report.conflicts += 1;
                        report.details.push(format!(
                            "冲突：{} 内容哈希与影子不符，拒绝移动",
                            old_rel.display()
                        ));
                        continue;
                    }
                    if let Some(p) = dst.parent() {
                        fs::create_dir_all(p)?;
                    }
                    fs::rename(&src, &dst).with_context(|| {
                        format!("移动失败：{} -> {}", src.display(), dst.display())
                    })?;
                }
                report.renamed += 1;
            }
            'A' => {
                let Some(new) = e.new else { continue };
                let Some(new_rel) = shadowmod::rel_from_shadow_str(&new) else {
                    continue;
                };
                let dst = real.join(&new_rel);
                if !dst.exists() {
                    report.need_pull += 1;
                    report.details.push(format!(
                        "需 pull：{} 真实缺失（apply 不造字节）",
                        new_rel.display()
                    ));
                    continue;
                }
                let exp = target_shadow(repo, &target, &new_rel)?;
                let size_ok = fs::metadata(&dst)
                    .map(|m| m.len() == exp.size)
                    .unwrap_or(false);
                if !size_ok {
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
                    let actual = hash::blake3_file(&dst)?;
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
                    // 由下方以"目标快照"为准的 extras 扫描统一删除
                    report.details.push(format!("删除 {}", old_rel.display()));
                } else {
                    report.prune_skipped += 1;
                }
            }
            _ => {}
        }
    }

    // ---------- prune：以目标快照为准，清掉真实侧多余文件（幂等，与 base 无关） ----------
    if prune {
        let names = git::ls_tree_names(repo, &target, shadowmod::MIRROR_DIR)?;
        let target_set: HashSet<PathBuf> = names
            .iter()
            .filter_map(|s| shadowmod::rel_from_shadow_str(s))
            .collect();
        for f in walk::collect(repo, real)? {
            if !target_set.contains(&f.rel) {
                if !dry_run {
                    fs::remove_file(real.join(&f.rel)).ok();
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

/// 读取 target 下某真实相对路径对应的影子。
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
