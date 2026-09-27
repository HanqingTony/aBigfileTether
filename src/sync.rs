//! 对等同步：`push`（本快照 → 对端）与 `pull`（对端 → 本地快照）。
//!
//! 见 `docs/DESIGN.md` §5.3。核心：以内容哈希为身份，对端已有同 hash 的字节时
//! 只做 `Move`/`Copy`，**零字节重传**；只传真正缺失的内容。

use crate::git;
use crate::scan::BASE_REF;
use crate::shadow as shadowmod;
use crate::transport::{Agent, Peer, bytes_to_path, path_to_bytes};
use crate::walk;
use anyhow::Result;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// 一次同步的结果摘要。
#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub satisfied: usize,
    pub moved: usize,
    pub copied: usize,
    pub local_copied: usize,
    pub put: usize,
    pub fetched: usize,
    pub deleted: usize,
    pub missing: usize,
    pub details: Vec<String>,
}

/// 路径 → (size, hash)。
type PathMap = HashMap<PathBuf, (u64, String)>;
/// hash → 该 hash 在对端的全部路径。
type HashIndex = HashMap<String, Vec<PathBuf>>;

fn peer_index(agent: &mut Agent) -> Result<(PathMap, HashIndex)> {
    let entries = agent.list_index()?;
    let mut by_path = HashMap::new();
    let mut by_hash: HashIndex = HashMap::new();
    for e in entries {
        let rel = bytes_to_path(&e.path);
        by_hash.entry(e.hash.clone()).or_default().push(rel.clone());
        by_path.insert(rel, (e.size, e.hash));
    }
    Ok((by_path, by_hash))
}

/// 把本地当前快照推给对端：对端缺失的字节才传输，已有同 hash 的只移动/复制。
pub fn push(repo: &Path, real: &Path, peer: &Peer, prune: bool) -> Result<SyncReport> {
    let our = shadowmod::load_index(repo)?;
    let mut agent = Agent::connect(peer)?;
    let (peer_map, peer_by_hash) = peer_index(&mut agent)?;

    let target_paths: HashSet<PathBuf> = our.keys().cloned().collect();
    let mut consumed: HashSet<PathBuf> = HashSet::new();
    let mut report = SyncReport::default();

    for (rel, sh) in &our {
        let relb = path_to_bytes(rel);
        if let Some((s, h)) = peer_map.get(rel)
            && *s == sh.size
            && h == &sh.content_hash
        {
            report.satisfied += 1;
            continue;
        }
        // 对端已有同 hash 的字节？
        let mut source: Option<PathBuf> = None;
        if let Some(cands) = peer_by_hash.get(&sh.content_hash) {
            for q in cands {
                if q != rel && !consumed.contains(q) {
                    source = Some(q.clone());
                    break;
                }
            }
        }
        match source {
            Some(q) => {
                let qb = path_to_bytes(&q);
                if target_paths.contains(&q) {
                    agent.copy_path(&qb, &relb)?;
                    report.copied += 1;
                    report
                        .details
                        .push(format!("copy {} -> {}", q.display(), rel.display()));
                } else {
                    agent.move_path(&qb, &relb)?;
                    report.moved += 1;
                    report
                        .details
                        .push(format!("move {} -> {}", q.display(), rel.display()));
                    consumed.insert(q);
                }
            }
            None => {
                agent.put(&relb, &real.join(rel), sh.size, &sh.content_hash)?;
                report.put += 1;
                report.details.push(format!("put {}", rel.display()));
            }
        }
    }

    if prune {
        for rel in peer_map.keys() {
            if !target_paths.contains(rel) {
                agent.delete(&path_to_bytes(rel))?;
                report.deleted += 1;
            }
        }
    }

    agent.close()?;
    Ok(report)
}

/// 从对端补齐本地当前快照缺失的字节。
pub fn pull(repo: &Path, real: &Path, peer: &Peer, prune: bool) -> Result<SyncReport> {
    let desired = shadowmod::load_index(repo)?;
    let mut agent = Agent::connect(peer)?;
    let (_peer_map, peer_by_hash) = peer_index(&mut agent)?;

    let target_paths: HashSet<PathBuf> = desired.keys().cloned().collect();
    let mut report = SyncReport::default();

    // 第一遍：已在位（size 命中）的路径，按 hash 记为"本地可用源"
    let mut local_by_hash: HashMap<String, PathBuf> = HashMap::new();
    for (rel, sh) in &desired {
        let dest = real.join(rel);
        if let Ok(md) = fs::metadata(&dest)
            && md.len() == sh.size
        {
            report.satisfied += 1;
            local_by_hash
                .entry(sh.content_hash.clone())
                .or_insert_with(|| rel.clone());
        }
    }

    // 第二遍：缺失项，优先本地同 hash 复制，否则从对端取
    for (rel, sh) in &desired {
        let dest = real.join(rel);
        if fs::metadata(&dest)
            .map(|m| m.len() == sh.size)
            .unwrap_or(false)
        {
            continue; // 第一遍已计入
        }
        if let Some(src_rel) = local_by_hash.get(&sh.content_hash).cloned() {
            let src = real.join(&src_rel);
            if let Some(p) = dest.parent() {
                fs::create_dir_all(p)?;
            }
            fs::copy(&src, &dest)?;
            report.local_copied += 1;
            report.details.push(format!(
                "本地复制 {} -> {}",
                src_rel.display(),
                rel.display()
            ));
            local_by_hash
                .entry(sh.content_hash.clone())
                .or_insert_with(|| rel.clone());
            continue;
        }
        match peer_by_hash.get(&sh.content_hash).and_then(|v| v.first()) {
            Some(q) => {
                agent.get(&path_to_bytes(q), &dest, &sh.content_hash)?;
                report.fetched += 1;
                report
                    .details
                    .push(format!("get {} -> {}", q.display(), rel.display()));
                local_by_hash
                    .entry(sh.content_hash.clone())
                    .or_insert_with(|| rel.clone());
            }
            None => {
                report.missing += 1;
                report.details.push(format!(
                    "对端缺少 hash {}（{}）",
                    sh.content_hash,
                    rel.display()
                ));
            }
        }
    }

    if prune {
        for f in walk::collect(repo, real)? {
            if !target_paths.contains(&f.rel) {
                fs::remove_file(real.join(&f.rel)).ok();
                report.deleted += 1;
            }
        }
    }

    agent.close()?;

    // 全部到位后，真实文件夹即等于 HEAD 快照，推进 base
    if report.missing == 0
        && let Some(head) = git::rev_parse_opt(repo, "HEAD")
    {
        git::update_ref(repo, BASE_REF, &head)?;
    }
    Ok(report)
}
