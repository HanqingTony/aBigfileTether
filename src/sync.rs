//! 中心模型的两个方向搬运：
//!
//! - `distribute`：把**当前分支快照**分发到该分支 `location`（真实文件夹，可远端）；
//!   字节来源为 **master 分支的 location**（全量存储）。哈希感知：已有同 hash 只 MOVE，
//!   缺的才传。
//! - `ingest`：把某分支 `location` 的**新增/变动**收进 **master**（影子 + 字节），
//!   删除不传播。
//!
//! 两者都只操作影子仓库（本地）与两个 `Fs`（source/dest），不做 git 分支检出。

use crate::fs::{self, Fs};
use crate::model::Shadow;
use crate::shadow as shadowmod;
use crate::walk;
use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs as stdfs;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// 一次搬运的摘要。
#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub satisfied: usize,
    pub moved: usize,
    pub copied: usize,
    pub put: usize,
    pub deleted: usize,
    pub added: usize,
    pub modified: usize,
    pub missing: usize,
    pub details: Vec<String>,
}

type Index = BTreeMap<PathBuf, (u64, i64, String)>;
type HashPair = (PathBuf, (u64, i64, String));

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}
fn fmt_ns(ns: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(ns as i128)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

/// 对某 `Fs` 全量哈希，建 `rel -> (size,mtime_ns,hash)` 索引。
fn hash_index(repo: &Path, fsv: &dyn Fs) -> Result<Index> {
    let raw = fsv.walk()?;
    let entries = walk::filter(repo, fsv.root(), raw)?;
    let pairs: Result<Vec<HashPair>> = entries
        .par_iter()
        .map(|e| {
            let h = fsv.hash(&e.rel)?;
            Ok((e.rel.clone(), (e.size, e.mtime_ns, h)))
        })
        .collect();
    Ok(pairs?.into_iter().collect())
}

/// master → 设备：把 `branch_index`（分支快照）落到 `dev_fs`，字节取自 `master_fs`。
pub fn distribute(
    repo: &Path,
    branch_index: &BTreeMap<PathBuf, Shadow>,
    dev_fs: &dyn Fs,
    master_fs: &dyn Fs,
    prune: bool,
) -> Result<SyncReport> {
    let dev = hash_index(repo, dev_fs)?;
    let mut by_hash: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for (rel, (_, _, h)) in &dev {
        by_hash.entry(h.clone()).or_default().push(rel.clone());
    }
    let mut consumed: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut report = SyncReport::default();

    for (rel, sh) in branch_index {
        let relb = rel.to_path_buf();
        if let Some((s, _, h)) = dev.get(rel)
            && *s == sh.size
            && h == &sh.content_hash
        {
            report.satisfied += 1;
            continue;
        }
        // 设备上已有同 hash？
        let mut source: Option<PathBuf> = None;
        if let Some(cands) = by_hash.get(&sh.content_hash) {
            for q in cands {
                if q != rel && !consumed.contains(q) {
                    source = Some(q.clone());
                    break;
                }
            }
        }
        match source {
            Some(q) => {
                if branch_index.contains_key(&q) {
                    dev_fs.cp(&q, &relb)?;
                    report.copied += 1;
                    report
                        .details
                        .push(format!("copy {} -> {}", q.display(), rel.display()));
                } else {
                    dev_fs.mv(&q, &relb)?;
                    consumed.insert(q.clone());
                    report.moved += 1;
                    report
                        .details
                        .push(format!("move {} -> {}", q.display(), rel.display()));
                }
            }
            None => {
                fs::copy_between(master_fs, rel, dev_fs, &relb, sh.size, &sh.content_hash)?;
                report.put += 1;
                report.details.push(format!("put {}", rel.display()));
            }
        }
    }

    if prune {
        for rel in dev.keys() {
            if !branch_index.contains_key(rel) {
                dev_fs.rm(rel)?;
                report.deleted += 1;
            }
        }
    }
    Ok(report)
}

/// 设备 → master：把 `dev_fs` 的新增/变动并入当前（master）工作树与 `master_fs`。删除不传播。
pub fn ingest(
    repo: &Path,
    dev_fs: &dyn Fs,
    master_fs: &dyn Fs,
    commit: bool,
) -> Result<SyncReport> {
    let dev = hash_index(repo, dev_fs)?;
    let master = shadowmod::load_index(repo)?;
    let master_by_hash: HashMap<String, Vec<PathBuf>> = {
        let mut m: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for (rel, s) in &master {
            m.entry(s.content_hash.clone())
                .or_default()
                .push(rel.clone());
        }
        m
    };
    let mut consumed: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut report = SyncReport::default();

    for (rel, (size, mtime_ns, hash)) in &dev {
        match master.get(rel) {
            Some(s) if &s.content_hash == hash => {
                report.satisfied += 1;
                continue;
            }
            Some(_) => {
                // 已存在但内容变化：用设备内容覆盖 master
                fs::copy_between(dev_fs, rel, master_fs, rel, *size, hash)?;
                let sh = Shadow {
                    content_hash: hash.clone(),
                    size: *size,
                    mtime: fmt_ns(*mtime_ns),
                    last_seen: now_rfc3339(),
                };
                shadowmod::write(&shadowmod::shadow_path(repo, rel), &sh)?;
                report.modified += 1;
                report.details.push(format!("update {}", rel.display()));
                continue;
            }
            None => {}
        }
        // master 上别处已有同 hash → 视作移动
        let mut moved_from: Option<PathBuf> = None;
        if let Some(cands) = master_by_hash.get(hash) {
            for q in cands {
                if q != rel && !consumed.contains(q) && master.contains_key(q) {
                    moved_from = Some(q.clone());
                    break;
                }
            }
        }
        match moved_from {
            Some(q) => {
                if master_fs.stat(&q)?.is_some() {
                    master_fs.mv(&q, rel)?;
                    let src = shadowmod::shadow_path(repo, &q);
                    let dst = shadowmod::shadow_path(repo, rel);
                    if let Some(p) = dst.parent() {
                        stdfs::create_dir_all(p)?;
                    }
                    stdfs::rename(&src, &dst).ok();
                    consumed.insert(q.clone());
                } else {
                    fs::copy_between(dev_fs, rel, master_fs, rel, *size, hash)?;
                }
                report.moved += 1;
                report
                    .details
                    .push(format!("move {} -> {}", q.display(), rel.display()));
            }
            None => {
                fs::copy_between(dev_fs, rel, master_fs, rel, *size, hash)?;
                let sh = Shadow {
                    content_hash: hash.clone(),
                    size: *size,
                    mtime: fmt_ns(*mtime_ns),
                    last_seen: now_rfc3339(),
                };
                shadowmod::write(&shadowmod::shadow_path(repo, rel), &sh)?;
                report.added += 1;
                report.details.push(format!("add {}", rel.display()));
            }
        }
    }

    if commit {
        crate::git::add_all(repo, shadowmod::MIRROR_DIR)?;
        crate::git::commit(
            repo,
            &format!(
                "tether ingest: +{} ~{} ->{}",
                report.added, report.modified, report.moved
            ),
        )?;
        if let Some(head) = crate::git::rev_parse_opt(repo, "HEAD") {
            crate::git::update_ref(repo, &crate::scan::base_ref(repo)?, &head)?;
        }
    }
    Ok(report)
}

/// 从某分支 ref 读取影子索引（不检出）。
pub fn branch_index(repo: &Path, branch: &str) -> Result<BTreeMap<PathBuf, Shadow>> {
    let names = crate::git::ls_tree_names(repo, branch, shadowmod::MIRROR_DIR)?;
    let mut out = BTreeMap::new();
    for name in names {
        let Some(rel) = shadowmod::rel_from_shadow_str(&name) else {
            continue;
        };
        let text = crate::git::show(repo, branch, &name)
            .with_context(|| format!("读取分支影子失败：{name}"))?;
        let sh: Shadow = toml::from_str(&text).context("解析分支影子失败")?;
        out.insert(rel, sh);
    }
    Ok(out)
}
