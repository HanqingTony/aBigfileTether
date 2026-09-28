//! 扫描：真实文件夹（由 `Fs` 抽象，本地/远端）→ 仓库。
//!
//! 以「路径 + size」初筛，只有变动文件才全量 blake3；按哈希识别 move/copy；提交、写证书、
//! 更新 `refs/tether/base`。

use crate::fs::Fs;
use crate::git;
use crate::hash;
use crate::model::{CertEntry, Certificate, Shadow};
use crate::shadow as shadowmod;
use crate::walk;
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// 每分支的 base 引用前缀（`refs/tether/base/<branch>`）。
pub const BASE_REF_PREFIX: &str = "refs/tether/base";

/// 当前分支的 base 引用。
pub fn base_ref(repo: &Path) -> Result<String> {
    let b = git::current_branch(repo).unwrap_or_else(|_| "HEAD".to_string());
    Ok(format!("{BASE_REF_PREFIX}/{b}"))
}

/// 一次扫描的结果摘要。
#[derive(Debug, Default, Serialize)]
pub struct ScanReport {
    pub added: usize,
    pub modified: usize,
    pub copied: usize,
    pub deleted: usize,
    pub moved: usize,
    pub unchanged: usize,
    pub hashed: usize,
    pub moved_pairs: Vec<(String, String)>,
    pub committed: bool,
    pub commit: Option<String>,
}

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

/// 扫描主入口。`fs` 为当前分支 `location` 对应的文件系统。
pub fn scan(repo: &Path, fs: &dyn Fs, commit: bool) -> Result<ScanReport> {
    // 存在未应用的影子变更时禁止扫描
    let bref = base_ref(repo)?;
    let head = git::rev_parse_opt(repo, "HEAD");
    if let (Some(base), Some(head)) = (git::rev_parse_opt(repo, &bref), head.as_ref())
        && &base != head
    {
        bail!(
            "仓库存在未应用的影子变更（refs/tether/base != HEAD）：先 `tether apply`，或 `git reset --hard` 后再扫描"
        );
    }

    let prev = shadowmod::load_index(repo)?;
    let raw = fs.walk()?;
    let entries = walk::filter(repo, fs.root(), raw)?;

    let mut real_files: BTreeMap<PathBuf, (u64, i64)> = BTreeMap::new();
    for e in entries {
        real_files.insert(e.rel, (e.size, e.mtime_ns));
    }

    // 初筛：只有变动文件才全量哈希
    let mut needs_hash: Vec<PathBuf> = Vec::new();
    for (rel, (size, _)) in &real_files {
        match prev.get(rel) {
            Some(s) if s.size == *size => {}
            _ => needs_hash.push(rel.clone()),
        }
    }
    let total = needs_hash.len();
    let done = AtomicUsize::new(0);
    let hashed: Vec<(PathBuf, String)> = needs_hash
        .par_iter()
        .map(|rel| -> Result<(PathBuf, String)> {
            let h = fs.hash(rel)?;
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            eprintln!("[进度] 哈希 {n}/{total} {}", rel.display());
            Ok((rel.clone(), h))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut hash_map: HashMap<PathBuf, String> = hashed.into_iter().collect();

    let mut new_hash: BTreeMap<PathBuf, String> = BTreeMap::new();
    for (rel, (size, _)) in &real_files {
        if let Some(s) = prev.get(rel)
            && s.size == *size
        {
            new_hash.insert(rel.clone(), s.content_hash.clone());
            continue;
        }
        if let Some(h) = hash_map.remove(rel) {
            new_hash.insert(rel.clone(), h);
        }
    }

    // 身份分类
    let real_set: BTreeSet<PathBuf> = real_files.keys().cloned().collect();
    let mut old_by_hash: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for (rel, s) in &prev {
        old_by_hash
            .entry(s.content_hash.clone())
            .or_default()
            .push(rel.clone());
    }

    let mut report = ScanReport {
        hashed: needs_hash.len(),
        ..Default::default()
    };
    let mut consumed: HashSet<PathBuf> = HashSet::new();
    let mut moved_new: HashSet<PathBuf> = HashSet::new();
    let mut moved_pairs: Vec<(PathBuf, PathBuf)> = Vec::new();

    for (rel, h) in &new_hash {
        match prev.get(rel) {
            Some(s) => {
                if &s.content_hash == h {
                    report.unchanged += 1;
                } else {
                    report.modified += 1;
                }
            }
            None => {
                let mut found: Option<PathBuf> = None;
                if let Some(cands) = old_by_hash.get(h) {
                    for c in cands {
                        if !real_set.contains(c) && !consumed.contains(c) {
                            found = Some(c.clone());
                            break;
                        }
                    }
                }
                match found {
                    Some(old) => {
                        consumed.insert(old.clone());
                        moved_new.insert(rel.clone());
                        moved_pairs.push((old, rel.clone()));
                    }
                    None if old_by_hash.contains_key(h) => report.copied += 1,
                    None => report.added += 1,
                }
            }
        }
    }
    for rel in prev.keys() {
        if !real_set.contains(rel) && !consumed.contains(rel) {
            report.deleted += 1;
        }
    }
    report.moved = moved_pairs.len();
    report.moved_pairs = moved_pairs
        .iter()
        .map(|(a, b)| (a.display().to_string(), b.display().to_string()))
        .collect();

    if !commit {
        return Ok(report);
    }

    // 落盘到镜像树
    for (old, new) in &moved_pairs {
        let src = shadowmod::shadow_path(repo, old);
        let dst = shadowmod::shadow_path(repo, new);
        if dst.exists() {
            fs::remove_file(&dst).ok();
        }
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        fs::rename(&src, &dst)
            .with_context(|| format!("移动影子失败：{} -> {}", src.display(), dst.display()))?;
        if let Some(p) = src.parent() {
            shadowmod::prune_empty_dirs(repo, p);
        }
    }
    for rel in prev.keys() {
        if !real_set.contains(rel) && !consumed.contains(rel) {
            let p = shadowmod::shadow_path(repo, rel);
            fs::remove_file(&p).ok();
            if let Some(parent) = p.parent() {
                shadowmod::prune_empty_dirs(repo, parent);
            }
        }
    }
    for (rel, h) in &new_hash {
        if moved_new.contains(rel) {
            continue;
        }
        let need_write = match prev.get(rel) {
            Some(s) => &s.content_hash != h,
            None => true,
        };
        if !need_write {
            continue;
        }
        let (size, mtime_ns) = real_files[rel];
        let s = Shadow {
            content_hash: h.clone(),
            size,
            mtime: fmt_ns(mtime_ns),
            last_seen: now_rfc3339(),
        };
        shadowmod::write(&shadowmod::shadow_path(repo, rel), &s)?;
    }

    // Git 提交（连同脚手架/配置文件；只加存在的路径，避免 pathspec 不匹配）
    let mut args: Vec<&str> = vec!["add", "--all", "--"];
    if !new_hash.is_empty() {
        args.push(shadowmod::MIRROR_DIR);
    }
    if repo.join(crate::config::IGNORE_NAME).exists() {
        args.push(crate::config::IGNORE_NAME);
    }
    if repo.join(crate::config::GITIGNORE_NAME).exists() {
        args.push(crate::config::GITIGNORE_NAME);
    }
    if repo.join(crate::config::CONFIG_NAME).exists() {
        args.push(crate::config::CONFIG_NAME);
    }
    git::run(repo, &args)?;
    let msg = format!(
        "tether scan: +{} ~{} ->{} ={} -{}",
        report.added, report.modified, report.moved, report.copied, report.deleted
    );
    report.committed = git::commit(repo, &msg)?;
    if report.committed {
        report.commit = git::rev_parse_opt(repo, "HEAD");
    }

    write_cert_scan(fs, &new_hash, &real_files)?;
    if let Some(head) = git::rev_parse_opt(repo, "HEAD") {
        git::update_ref(repo, &bref, &head)?;
    }
    Ok(report)
}

/// 依据索引写出证书（不重算哈希）。
pub fn write_cert_from_index(repo: &Path, fs: &dyn Fs) -> Result<u64> {
    let index = shadowmod::load_index(repo)?;
    let mut content: BTreeMap<PathBuf, (u64, String)> = BTreeMap::new();
    for (rel, s) in index {
        content.insert(rel, (s.size, s.content_hash));
    }
    write_cert(fs, &content)
}

fn write_cert(fs: &dyn Fs, content: &BTreeMap<PathBuf, (u64, String)>) -> Result<u64> {
    let mut entries: Vec<CertEntry> = Vec::new();
    let mut root_input = String::new();
    let mut total: u64 = 0;
    for (rel, (size, h)) in content {
        total += size;
        let p = rel.to_string_lossy().replace('\\', "/");
        root_input.push_str(&format!("{p}\0{size}\0{h}\n"));
        entries.push(CertEntry {
            path: p,
            size: *size,
            content_hash: h.clone(),
        });
    }
    let cert = Certificate {
        schema: 1,
        scanned_at: now_rfc3339(),
        count: entries.len() as u64,
        total_size: total,
        root_hash: hash::blake3_bytes(root_input.as_bytes()),
        files: entries,
    };
    let text = toml::to_string_pretty(&cert).context("序列化证书失败")?;
    let bytes = text.as_bytes();
    let digest = hash::blake3_bytes(bytes);
    let mut rd: &[u8] = bytes;
    fs.write_from(
        Path::new(shadowmod::CERT_NAME),
        &mut rd,
        0,
        bytes.len() as u64,
        &digest,
    )?;
    Ok(cert.count)
}

/// 写证书（scan 用）：content = new_hash + 真实大小。
fn write_cert_scan(
    fs: &dyn Fs,
    new_hash: &BTreeMap<PathBuf, String>,
    real_files: &BTreeMap<PathBuf, (u64, i64)>,
) -> Result<u64> {
    let mut content: BTreeMap<PathBuf, (u64, String)> = BTreeMap::new();
    for (rel, h) in new_hash {
        let size = real_files.get(rel).map(|(s, _)| *s).unwrap_or(0);
        content.insert(rel.clone(), (size, h.clone()));
    }
    write_cert(fs, &content)
}

/// 校验结果（`cert --verify`）。
#[derive(Debug, Default, Serialize)]
pub struct VerifyReport {
    pub checked: usize,
    pub ok: usize,
    pub mismatches: Vec<String>,
    pub cert_ok: bool,
    pub cert_message: String,
}

/// 全量重算真实文件哈希，与索引及证书 `root_hash` 比对。
pub fn verify(repo: &Path, fs: &dyn Fs) -> Result<VerifyReport> {
    let index = shadowmod::load_index(repo)?;
    let mut report = VerifyReport::default();
    for (rel, s) in &index {
        report.checked += 1;
        match fs.stat(rel)? {
            None => report
                .mismatches
                .push(format!("{}：真实文件缺失", rel.display())),
            Some(_) => {
                let actual = fs.hash(rel)?;
                if actual == s.content_hash {
                    report.ok += 1;
                } else {
                    report.mismatches.push(format!(
                        "{}：{actual} != {}",
                        rel.display(),
                        s.content_hash
                    ));
                }
            }
        }
    }
    let mut root_input = String::new();
    for (rel, s) in &index {
        let p = rel.to_string_lossy().replace('\\', "/");
        root_input.push_str(&format!("{p}\0{}\0{}\n", s.size, s.content_hash));
    }
    let root = hash::blake3_bytes(root_input.as_bytes());
    if fs.stat(Path::new(shadowmod::CERT_NAME))?.is_some() {
        let mut buf = Vec::new();
        fs.read_to(Path::new(shadowmod::CERT_NAME), &mut buf, 0)?;
        let cert: Certificate =
            toml::from_str(&String::from_utf8_lossy(&buf)).context("解析证书失败")?;
        report.cert_ok = cert.root_hash == root;
        report.cert_message = if report.cert_ok {
            format!("证书 root_hash 一致（{root}）")
        } else {
            format!(
                "证书 root_hash 不一致：证书 {} != 重算 {root}",
                cert.root_hash
            )
        };
    } else {
        report.cert_ok = false;
        report.cert_message = "证书不存在（先 scan）".to_string();
    }
    Ok(report)
}
