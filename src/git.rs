//! 系统 git 封装：读写都走系统 `git`，与用户既有 Git 行为完全一致。
//!
//! 不引入 libgit2、不挂钩子、不改 Git 行为（见 `docs/DESIGN.md` §1）。
//! 所有命令通过 `git -C <repo>` 执行，避免依赖进程当前目录。

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// 执行 git 并返回 stdout；失败时把 stderr 一并报出。
pub fn run(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("无法执行 git {args:?}"))?;
    if !out.status.success() {
        bail!(
            "git {args:?} 失败（退出码 {:?}）：{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 执行 git，仅关心是否成功；成功返回 stdout，失败返回 None。
pub fn try_run(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

/// 当前目录所属仓库的顶层目录（`git rev-parse --show-toplevel`）。
pub fn toplevel(cwd: &Path) -> Result<PathBuf> {
    let s = run(cwd, &["rev-parse", "--show-toplevel"])
        .context("当前目录不在任何 git 仓库内（先 git init 或 tether init）")?;
    Ok(PathBuf::from(s.trim()))
}

/// 目录是否已是一个 git 仓库。
pub fn is_repo(dir: &Path) -> bool {
    try_run(dir, &["rev-parse", "--git-dir"]).is_some()
}

/// `git init`（已存在仓库则无操作）。
pub fn init(dir: &Path) -> Result<()> {
    if !is_repo(dir) {
        run(dir, &["init"])?;
    }
    Ok(())
}

/// 暂存路径（对应 `git add --all -- <pathspec>`）。
pub fn add_all(repo: &Path, pathspec: &str) -> Result<()> {
    run(repo, &["add", "--all", "--", pathspec])?;
    Ok(())
}

/// 是否存在已暂存但未提交的改动。
pub fn has_staged_changes(repo: &Path) -> bool {
    // --quiet 有差异时退出码 1
    try_run(repo, &["diff", "--cached", "--quiet"]).is_none()
}

/// 提交暂存内容；无改动时返回 false（不报错）。
pub fn commit(repo: &Path, message: &str) -> Result<bool> {
    if !has_staged_changes(repo) {
        return Ok(false);
    }
    run(repo, &["commit", "-m", message])?;
    Ok(true)
}

/// 解析 rev 为完整 sha；rev 不存在返回 None。
pub fn rev_parse_opt(repo: &Path, rev: &str) -> Option<String> {
    try_run(repo, &["rev-parse", "--verify", "--quiet", rev]).map(|s| s.trim().to_string())
}

/// 解析 rev 为完整 sha；不存在则报错。
pub fn rev_parse(repo: &Path, rev: &str) -> Result<String> {
    rev_parse_opt(repo, rev).with_context(|| format!("无法解析 rev：{rev}"))
}

/// 两个 rev 的最近共同祖先（`git merge-base`）。
pub fn merge_base(repo: &Path, a: &str, b: &str) -> Result<String> {
    let out = run(repo, &["merge-base", a, b])?;
    Ok(out.trim().to_string())
}

/// 更新引用（`git update-ref <name> <to>`）。
pub fn update_ref(repo: &Path, name: &str, to: &str) -> Result<()> {
    run(repo, &["update-ref", name, to])?;
    Ok(())
}

/// 取指定 rev 下某路径的 blob 内容（`git show <rev>:<path>`）。
pub fn show(repo: &Path, rev: &str, path: &str) -> Result<String> {
    run(repo, &["show", &format!("{rev}:{path}")])
}

/// 一条 diff 记录（带重命名检测）。
#[derive(Debug, Clone)]
pub struct DiffEntry {
    /// 状态字符：A/M/D/R
    pub status: char,
    /// 旧路径（R 时有）
    pub old: Option<String>,
    /// 新路径（A/M/R 时有；D 时为 None）
    pub new: Option<String>,
}

/// 列出某 rev 下匹配 pathspec 的所有文件名（`git ls-tree -r -z --name-only`）。
/// 用 `-z` 避免 Git 对特殊字符文件名的引号转义。
pub fn ls_tree_names(repo: &Path, rev: &str, pathspec: &str) -> Result<Vec<String>> {
    let out = run(
        repo,
        &["ls-tree", "-r", "-z", "--name-only", rev, "--", pathspec],
    )?;
    Ok(out
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

/// `git diff -M --name-status <from> <to> -- <pathspec>`。
///
/// 输出形如：`M\tpath`、`A\tpath`、`D\tpath`、`R100\told\tnew`。
pub fn diff_name_status(
    repo: &Path,
    from: &str,
    to: &str,
    pathspec: &str,
) -> Result<Vec<DiffEntry>> {
    let out = run(
        repo,
        &["diff", "-M", "--name-status", from, to, "--", pathspec],
    )?;
    let mut entries = Vec::new();
    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        let status = cols[0].chars().next().unwrap_or('?');
        match status {
            'R' | 'C' if cols.len() >= 3 => {
                entries.push(DiffEntry {
                    status,
                    old: Some(cols[1].to_string()),
                    new: Some(cols[2].to_string()),
                });
            }
            'A' if cols.len() >= 2 => {
                entries.push(DiffEntry {
                    status,
                    old: None,
                    new: Some(cols[1].to_string()),
                });
            }
            'D' if cols.len() >= 2 => {
                entries.push(DiffEntry {
                    status,
                    old: Some(cols[1].to_string()),
                    new: None,
                });
            }
            'M' | 'T' if cols.len() >= 2 => {
                entries.push(DiffEntry {
                    status,
                    old: Some(cols[1].to_string()),
                    new: Some(cols[1].to_string()),
                });
            }
            _ => {}
        }
    }
    Ok(entries)
}
