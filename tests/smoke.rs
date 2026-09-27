//! 端到端测试：全部在临时目录里造真实文件夹 + 仓库，绝不触碰任何真实数据。

use abigfiletether::{apply, config, git, propagate, scan};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

struct Env {
    _tmp: TempDir,
    repo: PathBuf,
    real: PathBuf,
}

fn setup() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&real).unwrap();
    fs::create_dir_all(repo.join("mirrors")).unwrap();
    fs::create_dir_all(&repo).unwrap();
    git::init(&repo).unwrap();
    git::run(&repo, &["config", "user.email", "tether@test.local"]).unwrap();
    git::run(&repo, &["config", "user.name", "tether test"]).unwrap();
    config::ensure_gitignore(&repo).unwrap();
    config::ensure_ignore_file(&repo).unwrap();
    let cfg = config::LocalConfig {
        path: real.to_string_lossy().into_owned(),
        label: None,
        peers: Default::default(),
        transfer: Default::default(),
    };
    config::save(&repo, &cfg).unwrap();
    Env {
        _tmp: tmp,
        repo,
        real,
    }
}

fn write(path: &std::path::Path, data: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, data).unwrap();
}

#[test]
fn init_scan_creates_shadows_cert_and_base() {
    let e = setup();
    write(&e.real.join("a.bin"), b"hello");
    write(&e.real.join("dir/b.txt"), b"world");
    write(&e.real.join("wéird name [x].bin"), b"unicode");

    let r = scan::scan(&e.repo, &e.real, true).unwrap();
    assert_eq!(r.added, 3);
    assert_eq!(r.hashed, 3);
    assert!(r.committed);

    assert!(e.repo.join("mirrors/a.bin.tether").is_file());
    assert!(e.repo.join("mirrors/dir/b.txt.tether").is_file());
    // 空格/中文/方括号都能作为普通路径处理
    assert!(e.repo.join("mirrors/wéird name [x].bin.tether").is_file());
    // 证书写真实侧
    assert!(e.real.join("TETHER.cert.toml").is_file());

    // base == HEAD
    let base = git::rev_parse(&e.repo, scan::BASE_REF).unwrap();
    let head = git::rev_parse(&e.repo, "HEAD").unwrap();
    assert_eq!(base, head);
}

#[test]
fn rescan_detects_modified_moved_added_deleted() {
    let e = setup();
    write(&e.real.join("keep.bin"), b"aaaa");
    write(&e.real.join("moveme.bin"), b"bbbbbb");
    write(&e.real.join("del.bin"), b"cccc");
    scan::scan(&e.repo, &e.real, true).unwrap();

    // 修改（改大小）、移动、删除、新增
    write(&e.real.join("keep.bin"), b"aaaaaaaaaa"); // size 变
    fs::rename(e.real.join("moveme.bin"), e.real.join("moved.bin")).unwrap();
    fs::remove_file(e.real.join("del.bin")).unwrap();
    write(&e.real.join("new.bin"), b"dddddddd");

    let r = scan::scan(&e.repo, &e.real, true).unwrap();
    assert_eq!(r.modified, 1, "keep.bin 被修改");
    assert_eq!(r.moved, 1, "moveme -> moved");
    assert_eq!(r.added, 1, "new.bin 新增");
    assert_eq!(r.deleted, 1, "del.bin 删除");
    assert!(e.repo.join("mirrors/moved.bin.tether").is_file());
    assert!(!e.repo.join("mirrors/moveme.bin.tether").exists());
}

#[test]
fn scan_refuses_when_shadow_changes_unapplied() {
    let e = setup();
    write(&e.real.join("a.bin"), b"hello");
    scan::scan(&e.repo, &e.real, true).unwrap();

    // 模拟用户对仓库的 Git 改动（尚未 apply）
    fs::rename(
        e.repo.join("mirrors/a.bin.tether"),
        e.repo.join("mirrors/renamed.bin.tether"),
    )
    .unwrap();
    git::add_all(&e.repo, "mirrors").unwrap();
    git::commit(&e.repo, "user moved shadow").unwrap();

    let err = scan::scan(&e.repo, &e.real, true).unwrap_err();
    assert!(err.to_string().contains("未应用"), "应为未应用告警：{err}");
}

#[test]
fn apply_prune_removes_extras_independent_of_base() {
    let e = setup();
    write(&e.real.join("a.bin"), b"aaaa");
    write(&e.real.join("b.bin"), b"bbbb");
    scan::scan(&e.repo, &e.real, true).unwrap();

    // 用户在仓库里删掉 a 的影子并提交
    git::run(&e.repo, &["rm", "-q", "mirrors/a.bin.tether"]).unwrap();
    git::commit(&e.repo, "drop a").unwrap();

    // 不带 prune：不动真实文件（base 仍会推进）
    let r1 = apply::apply(&e.repo, &e.real, None, false, false).unwrap();
    assert_eq!(r1.prune_skipped, 1);
    assert!(e.real.join("a.bin").exists());

    // 之后 --prune 仍应删掉它（以目标快照为准，不依赖 base 差异）
    let r2 = apply::apply(&e.repo, &e.real, None, true, false).unwrap();
    assert!(r2.removed >= 1);
    assert!(!e.real.join("a.bin").exists());

    // 真实侧凭空多出的文件也应被 prune 清掉
    write(&e.real.join("stray.bin"), b"zzz");
    apply::apply(&e.repo, &e.real, None, true, false).unwrap();
    assert!(!e.real.join("stray.bin").exists());
}

#[test]
fn propagate_applies_adds_and_moves_without_deletions() {
    let e = setup();
    write(&e.real.join("a.bin"), b"aaaa");
    write(&e.real.join("b.bin"), b"bbbb");
    write(&e.real.join("c.bin"), b"cccc");
    scan::scan(&e.repo, &e.real, true).unwrap(); // master: a,b,c

    // 造剪枝子分支：只有 a，且 a 被改名
    git::run(&e.repo, &["checkout", "-q", "-b", "dev"]).unwrap();
    git::run(
        &e.repo,
        &["rm", "-q", "mirrors/b.bin.tether", "mirrors/c.bin.tether"],
    )
    .unwrap();
    git::run(
        &e.repo,
        &["mv", "mirrors/a.bin.tether", "mirrors/a2.bin.tether"],
    )
    .unwrap();
    git::commit(&e.repo, "dev subset").unwrap();

    git::run(&e.repo, &["checkout", "-q", "master"]).unwrap();

    // propagate dev -> master：移动 a->a2，新增/删除不传播（b、c 保留）
    let r = propagate::propagate(&e.repo, "dev", None, false).unwrap();
    assert_eq!(r.moved, 1, "{r:?}");
    assert!(r.skipped_deleted >= 2, "删除不传播：{r:?}");
    assert!(e.repo.join("mirrors/a2.bin.tether").is_file());
    assert!(e.repo.join("mirrors/b.bin.tether").is_file(), "b 不应被删");
    assert!(e.repo.join("mirrors/c.bin.tether").is_file(), "c 不应被删");
}

#[test]
fn apply_renames_real_file_from_git_move() {
    let e = setup();
    write(&e.real.join("dir/b.txt"), b"world");
    scan::scan(&e.repo, &e.real, true).unwrap();

    // 用户在仓库里移动影子并提交（Git 侧改动，真实侧未动）
    git::run(
        &e.repo,
        &["mv", "mirrors/dir/b.txt.tether", "mirrors/c.txt.tether"],
    )
    .unwrap();
    git::commit(&e.repo, "move shadow").unwrap();

    // dry-run：只出计划
    let dry = apply::apply(&e.repo, &e.real, None, false, true).unwrap();
    assert_eq!(dry.renamed, 1);
    assert!(
        e.real.join("dir/b.txt").exists(),
        "dry-run 不应改动真实文件"
    );

    // 执行
    let r = apply::apply(&e.repo, &e.real, None, false, false).unwrap();
    assert_eq!(r.renamed, 1);
    assert!(r.applied);
    assert!(e.real.join("c.txt").is_file());
    assert!(!e.real.join("dir/b.txt").exists());

    // base 跟上
    let base = git::rev_parse(&e.repo, scan::BASE_REF).unwrap();
    let head = git::rev_parse(&e.repo, "HEAD").unwrap();
    assert_eq!(base, head);
}
