//! 盘点（stocktake）与零售（retail）：子分支 ↔ main 的**文件存在**差异与按需取入。
//!
//! - `stocktake`：列出 main 有而当前分支没有（可 retail）、当前分支有而 main 没有的文件；
//!   可限定某个文件夹。
//! - `retail`：把 main 中指定文件/文件夹的**影子**取入当前分支并提交（不传字节）；
//!   之后用户 `tether pull` 即可下载真实文件。

use crate::{git, shadow as shadowmod};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

fn norm(p: &str) -> String {
    p.trim_end_matches('/').replace('\\', "/")
}

fn rel_str(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// 解析 main 引用：显式 > `origin/master` > `master`。
pub fn resolve_main(repo: &Path, explicit: Option<&str>) -> Result<String> {
    if let Some(r) = explicit
        && !r.is_empty()
    {
        return git::rev_parse(repo, r);
    }
    for cand in ["origin/master", "master"] {
        if let Some(s) = git::rev_parse_opt(repo, cand) {
            return Ok(s);
        }
    }
    bail!("找不到 main 引用（默认 origin/master 或 master）；可用 --main <ref> 指定")
}

/// main 分支下的所有影子 git 路径（`mirrors/...`）。
fn main_shadows(repo: &Path, main_ref: &str) -> Result<Vec<String>> {
    git::ls_tree_names(repo, main_ref, shadowmod::MIRROR_DIR)
}

/// 盘点结果。
#[derive(Debug, Default, Serialize)]
pub struct StocktakeReport {
    pub main_ref: String,
    pub filter: Option<String>,
    /// main 有、当前分支没有（可 retail）。
    pub missing: Vec<String>,
    /// 当前分支有、main 没有。
    pub extra: Vec<String>,
}

/// 比较 main 与当前分支的影子集合，`dir` 限定文件夹（可选）。
pub fn stocktake(repo: &Path, main: Option<&str>, dir: Option<&str>) -> Result<StocktakeReport> {
    let main_ref = resolve_main(repo, main)?;
    let filter = dir.map(norm).filter(|s| !s.is_empty());

    let mut main_set: BTreeSet<String> = BTreeSet::new();
    for s in main_shadows(repo, &main_ref)? {
        if let Some(rel) = shadowmod::rel_from_shadow_str(&s) {
            main_set.insert(rel_str(&rel));
        }
    }
    let cur = shadowmod::load_index(repo)?;
    let cur_set: BTreeSet<String> = cur.keys().map(|r| rel_str(r)).collect();

    let in_filter = |p: &String| {
        filter
            .as_ref()
            .is_none_or(|f| p == f || p.starts_with(&format!("{f}/")))
    };

    let missing = main_set
        .iter()
        .filter(|p| !cur_set.contains(*p) && in_filter(p))
        .cloned()
        .collect();
    let extra = cur_set
        .iter()
        .filter(|p| !main_set.contains(*p) && in_filter(p))
        .cloned()
        .collect();

    Ok(StocktakeReport {
        main_ref,
        filter,
        missing,
        extra,
    })
}

/// 零售结果。
#[derive(Debug, Default, Serialize)]
pub struct RetailReport {
    pub main_ref: String,
    pub added: Vec<String>,
    pub skipped: Vec<String>,
    pub not_found: Vec<String>,
    pub committed: bool,
    pub commit: Option<String>,
}

/// 把 main 中 `paths`（文件或文件夹，可多个）的影子取入当前分支并提交。
pub fn retail(repo: &Path, main: Option<&str>, paths: &[String]) -> Result<RetailReport> {
    let main_ref = resolve_main(repo, main)?;
    let shadows = main_shadows(repo, &main_ref)?;
    let mut report = RetailReport {
        main_ref: main_ref.clone(),
        ..Default::default()
    };

    for raw in paths {
        let p = norm(raw);
        let mut matched = false;
        for s in &shadows {
            let Some(rel) = shadowmod::rel_from_shadow_str(s) else {
                continue;
            };
            let rel = rel_str(&rel);
            if rel == p || rel.starts_with(&format!("{p}/")) {
                matched = true;
                let dest = repo.join(s);
                if dest.exists() {
                    report.skipped.push(rel);
                    continue;
                }
                let text = git::show(repo, &main_ref, s)
                    .with_context(|| format!("读取 main 影子失败：{s}"))?;
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&dest, text)
                    .with_context(|| format!("写入影子失败：{}", dest.display()))?;
                report.added.push(rel);
            }
        }
        if !matched {
            report.not_found.push(p);
        }
    }

    if !report.added.is_empty() {
        git::add_all(repo, shadowmod::MIRROR_DIR)?;
        report.committed = git::commit(
            repo,
            &format!(
                "tether retail: 从 {} 取入 {} 项",
                main_ref,
                report.added.len()
            ),
        )?;
        if report.committed {
            report.commit = git::rev_parse_opt(repo, "HEAD");
        }
    }
    Ok(report)
}
