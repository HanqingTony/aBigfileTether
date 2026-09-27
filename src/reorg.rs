//! 重排：按**已知路径映射**移动影子（以及可选真实）文件，**不计算哈希**。
//!
//! 用途：批量整理目录结构时，身份已由影子记录，只需按映射改名——比"先动真实再扫描"
//! （会为每个新路径重新哈希）快得多。
//!
//! 映射文件每行 `旧相对路径|新相对路径`（相对真实根）。默认只动影子仓库；`--real`
//! 同时移动真实文件。幂等：目标已存在则跳过。

use crate::git;
use crate::scan::BASE_REF;
use crate::shadow as shadowmod;
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::path::Path;

/// 一次重排的结果摘要。
#[derive(Debug, Default, Serialize)]
pub struct ReorgReport {
    pub moved: usize,
    pub real_moved: usize,
    pub skipped: usize,
    pub missing: usize,
    pub dry_run: bool,
    pub do_real: bool,
    pub committed: bool,
    pub commit: Option<String>,
    pub details: Vec<String>,
}

/// 解析映射文件为 `(旧, 新)` 列表。
pub fn load_map(path: &Path) -> Result<Vec<(String, String)>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("读取映射失败：{}", path.display()))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (old, new) = line
            .split_once('|')
            .with_context(|| format!("映射行格式应为 旧|新：{line}"))?;
        out.push((old.to_string(), new.to_string()));
    }
    Ok(out)
}

/// 执行重排。`do_real` 为真时同步移动真实文件。
pub fn reorg(
    repo: &Path,
    real: &Path,
    map_path: &Path,
    do_real: bool,
    dry_run: bool,
) -> Result<ReorgReport> {
    let map = load_map(map_path)?;
    let mut report = ReorgReport {
        dry_run,
        do_real,
        ..Default::default()
    };
    for (old, new) in &map {
        // 影子
        let so = shadowmod::shadow_path(repo, Path::new(old));
        let sn = shadowmod::shadow_path(repo, Path::new(new));
        if sn.exists() {
            report.skipped += 1;
        } else if so.exists() {
            if !dry_run {
                if let Some(p) = sn.parent() {
                    fs::create_dir_all(p)?;
                }
                fs::rename(&so, &sn).with_context(|| {
                    format!("移动影子失败：{} -> {}", so.display(), sn.display())
                })?;
                if let Some(p) = so.parent() {
                    shadowmod::prune_empty_dirs(repo, p);
                }
            }
            report.moved += 1;
        } else {
            report.missing += 1;
        }
        // 真实
        if do_real {
            let ro = real.join(old);
            let rn = real.join(new);
            if rn.exists() {
                report.skipped += 1;
            } else if ro.exists() {
                if !dry_run {
                    if let Some(p) = rn.parent() {
                        fs::create_dir_all(p)?;
                    }
                    fs::rename(&ro, &rn)
                        .with_context(|| format!("移动真实文件失败：{}", ro.display()))?;
                }
                report.real_moved += 1;
            } else {
                report.missing += 1;
            }
        }
    }
    if !dry_run {
        git::add_all(repo, shadowmod::MIRROR_DIR)?;
        report.committed = git::commit(repo, "tether reorg: 按映射整理目录")?;
        if let Some(head) = git::rev_parse_opt(repo, "HEAD") {
            git::update_ref(repo, BASE_REF, &head)?;
        }
        if report.committed {
            report.commit = git::rev_parse_opt(repo, "HEAD");
        }
    }
    Ok(report)
}
