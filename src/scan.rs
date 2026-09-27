//! 扫描：真实文件夹 → 仓库（增/删/改/移/复）。
//!
//! 算法见 `docs/DESIGN.md` §5.1：
//! 1. 以**路径 + size** 初筛，未变的文件不重算哈希；
//! 2. 变动文件做**全量 blake3**，再按 `size + hash` 判定身份；
//! 3. 按哈希匹配识别 move / copy，落成 Git 变更并提交；
//! 4. 写证书、更新 `refs/tether/base`。

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
use std::time::SystemTime;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// 记录"真实文件夹当前对应提交"的本地引用。
pub const BASE_REF: &str = "refs/tether/base";

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

fn fmt_time(t: SystemTime) -> String {
    OffsetDateTime::from(t)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// 扫描主入口。`commit=false` 时只计算并返回计划（dry-run）。
pub fn scan(repo: &Path, real: &Path, commit: bool) -> Result<ScanReport> {
    if !real.is_dir() {
        bail!("真实文件夹不存在或不是目录：{}", real.display());
    }

    // 仓库存在未应用的影子变更时，禁止扫描（否则会覆盖用户尚未 apply 的 Git 改动）。
    let head = git::rev_parse_opt(repo, "HEAD");
    if let (Some(base), Some(head)) = (git::rev_parse_opt(repo, BASE_REF), head.as_ref())
        && &base != head
    {
        bail!(
            "仓库存在未应用的影子变更（refs/tether/base != HEAD）：\
             先 `tether apply`，或 `git reset --hard` 后再扫描"
        );
    }

    let prev = shadowmod::load_index(repo)?;

    // ---------- 遍历真实文件夹 ----------
    let mut real_files: BTreeMap<PathBuf, (u64, SystemTime)> = BTreeMap::new();
    for f in walk::collect(repo, real)? {
        real_files.insert(f.rel, (f.size, f.mtime));
    }

    // ---------- 初筛：只有变动文件才全量哈希 ----------
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
            let h = hash::blake3_file(&real.join(rel))?;
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

    // ---------- 身份分类 ----------
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

    // ---------- 落盘到镜像树 ----------
    // 1) 移动：整文件改名，内容不动（保持 Git rename 语义）
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
    // 2) 删除
    for rel in prev.keys() {
        if !real_set.contains(rel) && !consumed.contains(rel) {
            let p = shadowmod::shadow_path(repo, rel);
            fs::remove_file(&p).ok();
            if let Some(parent) = p.parent() {
                shadowmod::prune_empty_dirs(repo, parent);
            }
        }
    }
    // 3) 新增 / 复制 / 修改：写影子（跳过已由移动落地的目标）
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
        let (size, mtime) = real_files[rel];
        let s = Shadow {
            content_hash: h.clone(),
            size,
            mtime: fmt_time(mtime),
            last_seen: now_rfc3339(),
        };
        shadowmod::write(&shadowmod::shadow_path(repo, rel), &s)?;
    }

    // ---------- Git 提交（连同脚手架文件，确保 .tetherignore/.gitignore 入库） ----------
    let mut args: Vec<&str> = vec!["add", "--all", "--"];
    args.push(crate::config::IGNORE_NAME);
    args.push(crate::config::GITIGNORE_NAME);
    if !new_hash.is_empty() {
        args.push(shadowmod::MIRROR_DIR);
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

    // ---------- 证书 + base ----------
    write_cert(real, &new_hash)?;
    if let Some(head) = git::rev_parse_opt(repo, "HEAD") {
        git::update_ref(repo, BASE_REF, &head)?;
    }
    Ok(report)
}

/// 依据索引写出证书（不重算哈希）。`cert` 子命令使用。
pub fn write_cert_from_index(repo: &Path, real: &Path) -> Result<u64> {
    let index = shadowmod::load_index(repo)?;
    let mut content: BTreeMap<PathBuf, String> = BTreeMap::new();
    for (rel, s) in index {
        content.insert(rel, s.content_hash);
    }
    write_cert(real, &content)
}

/// 写证书：`root_hash = blake3(排序后 path\0size\0hash\n)`。
fn write_cert(real: &Path, new_hash: &BTreeMap<PathBuf, String>) -> Result<u64> {
    let mut entries: Vec<CertEntry> = Vec::new();
    let mut root_input = String::new();
    let mut total: u64 = 0;
    for (rel, h) in new_hash {
        let size = real_file_size(real, rel);
        total += size;
        let p = rel.to_string_lossy().replace('\\', "/");
        root_input.push_str(&format!("{p}\0{size}\0{h}\n"));
        entries.push(CertEntry {
            path: p,
            size,
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
    fs::write(real.join(shadowmod::CERT_NAME), text)
        .with_context(|| format!("写证书失败：{}", real.display()))?;
    Ok(cert.count)
}

fn real_file_size(real: &Path, rel: &Path) -> u64 {
    fs::metadata(real.join(rel)).map(|m| m.len()).unwrap_or(0)
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
pub fn verify(repo: &Path, real: &Path) -> Result<VerifyReport> {
    let index = shadowmod::load_index(repo)?;
    let mut report = VerifyReport::default();

    for (rel, s) in &index {
        report.checked += 1;
        let p = real.join(rel);
        if !p.is_file() {
            report
                .mismatches
                .push(format!("{}：真实文件缺失", rel.display()));
            continue;
        }
        let actual = hash::blake3_file(&p)?;
        if actual == s.content_hash {
            report.ok += 1;
        } else {
            report
                .mismatches
                .push(format!("{}：{actual} != {}", rel.display(), s.content_hash));
        }
    }

    // 由索引重算 root_hash，与证书比对
    let mut root_input = String::new();
    for (rel, s) in &index {
        let p = rel.to_string_lossy().replace('\\', "/");
        root_input.push_str(&format!("{p}\0{}\0{}\n", s.size, s.content_hash));
    }
    let root = hash::blake3_bytes(root_input.as_bytes());
    let cert_path = real.join(shadowmod::CERT_NAME);
    if cert_path.is_file() {
        let cert: Certificate =
            toml::from_str(&fs::read_to_string(&cert_path)?).context("解析证书失败")?;
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
