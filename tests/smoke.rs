//! 端到端测试（中心模型）：临时目录，绝不触碰真实数据。

use abigfiletether::config::BranchConfig;
use abigfiletether::fs::LocalFs;
use abigfiletether::{apply, config, git, inventory, propagate, reorg, scan};
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
    fs::create_dir_all(&repo).unwrap();
    git::init(&repo).unwrap();
    git::run(&repo, &["config", "user.email", "tether@test.local"]).unwrap();
    git::run(&repo, &["config", "user.name", "tether test"]).unwrap();
    config::ensure_ignore_file(&repo).unwrap();
    config::save(
        &repo,
        &BranchConfig {
            location: format!("local:{}", real.display()),
            label: None,
            transfer: Default::default(),
        },
    )
    .unwrap();
    Env {
        _tmp: tmp,
        repo,
        real,
    }
}

fn lfs(e: &Env) -> LocalFs {
    LocalFs::new(e.real.clone())
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

    let r = scan::scan(&e.repo, &lfs(&e), true).unwrap();
    assert_eq!(r.added, 3);
    assert_eq!(r.hashed, 3);
    assert!(r.committed);

    assert!(e.repo.join("mirrors/a.bin.tether").is_file());
    assert!(e.repo.join("mirrors/dir/b.txt.tether").is_file());
    assert!(e.repo.join("mirrors/wéird name [x].bin.tether").is_file());
    assert!(e.real.join("TETHER.cert.toml").is_file());

    let base = git::rev_parse(&e.repo, &scan::base_ref(&e.repo).unwrap()).unwrap();
    let head = git::rev_parse(&e.repo, "HEAD").unwrap();
    assert_eq!(base, head);
}

#[test]
fn rescan_detects_modified_moved_added_deleted() {
    let e = setup();
    write(&e.real.join("keep.bin"), b"aaaa");
    write(&e.real.join("moveme.bin"), b"bbbbbb");
    write(&e.real.join("del.bin"), b"cccc");
    scan::scan(&e.repo, &lfs(&e), true).unwrap();

    write(&e.real.join("keep.bin"), b"aaaaaaaaaa");
    fs::rename(e.real.join("moveme.bin"), e.real.join("moved.bin")).unwrap();
    fs::remove_file(e.real.join("del.bin")).unwrap();
    write(&e.real.join("new.bin"), b"dddddddd");

    let r = scan::scan(&e.repo, &lfs(&e), true).unwrap();
    assert_eq!(r.modified, 1);
    assert_eq!(r.moved, 1);
    assert_eq!(r.added, 1);
    assert_eq!(r.deleted, 1);
    assert!(e.repo.join("mirrors/moved.bin.tether").is_file());
}

#[test]
fn scan_refuses_when_shadow_changes_unapplied() {
    let e = setup();
    write(&e.real.join("a.bin"), b"hello");
    scan::scan(&e.repo, &lfs(&e), true).unwrap();

    fs::rename(
        e.repo.join("mirrors/a.bin.tether"),
        e.repo.join("mirrors/renamed.bin.tether"),
    )
    .unwrap();
    git::add_all(&e.repo, "mirrors").unwrap();
    git::commit(&e.repo, "user moved shadow").unwrap();

    let err = scan::scan(&e.repo, &lfs(&e), true).unwrap_err();
    assert!(err.to_string().contains("未应用"), "{err}");
}

#[test]
fn apply_renames_real_file_from_git_move() {
    let e = setup();
    write(&e.real.join("dir/b.txt"), b"world");
    scan::scan(&e.repo, &lfs(&e), true).unwrap();

    git::run(
        &e.repo,
        &["mv", "mirrors/dir/b.txt.tether", "mirrors/c.txt.tether"],
    )
    .unwrap();
    git::commit(&e.repo, "move shadow").unwrap();

    let dry = apply::apply(&e.repo, &lfs(&e), None, false, true).unwrap();
    assert_eq!(dry.renamed, 1);
    assert!(e.real.join("dir/b.txt").exists());

    let r = apply::apply(&e.repo, &lfs(&e), None, false, false).unwrap();
    assert_eq!(r.renamed, 1);
    assert!(r.applied);
    assert!(e.real.join("c.txt").is_file());
    assert!(!e.real.join("dir/b.txt").exists());
}

#[test]
fn apply_prune_removes_extras_independent_of_base() {
    let e = setup();
    write(&e.real.join("a.bin"), b"aaaa");
    write(&e.real.join("b.bin"), b"bbbb");
    scan::scan(&e.repo, &lfs(&e), true).unwrap();

    git::run(&e.repo, &["rm", "-q", "mirrors/a.bin.tether"]).unwrap();
    git::commit(&e.repo, "drop a").unwrap();

    let r1 = apply::apply(&e.repo, &lfs(&e), None, false, false).unwrap();
    assert_eq!(r1.prune_skipped, 1);
    assert!(e.real.join("a.bin").exists());

    let r2 = apply::apply(&e.repo, &lfs(&e), None, true, false).unwrap();
    assert!(r2.removed >= 1);
    assert!(!e.real.join("a.bin").exists());
}

#[test]
fn propagate_applies_adds_and_moves_without_deletions() {
    let e = setup();
    write(&e.real.join("a.bin"), b"aaaa");
    write(&e.real.join("b.bin"), b"bbbb");
    write(&e.real.join("c.bin"), b"cccc");
    scan::scan(&e.repo, &lfs(&e), true).unwrap();

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

    let r = propagate::propagate(&e.repo, "dev", None, false).unwrap();
    assert_eq!(r.moved, 1, "{r:?}");
    assert!(r.skipped_deleted >= 2, "{r:?}");
    assert!(e.repo.join("mirrors/a2.bin.tether").is_file());
    assert!(e.repo.join("mirrors/b.bin.tether").is_file());
    assert!(e.repo.join("mirrors/c.bin.tether").is_file());
}

#[test]
fn reorg_moves_shadow_and_real_without_hashing() {
    let e = setup();
    write(&e.real.join("dir/x.bin"), b"xxxx");
    scan::scan(&e.repo, &lfs(&e), true).unwrap();

    let map = e.real.join("..").join("map.txt");
    fs::write(&map, "dir/x.bin|sub/y.bin\n").unwrap();
    let r = reorg::reorg(&e.repo, Some(&lfs(&e)), &map, false).unwrap();
    assert_eq!(r.moved, 1);
    assert_eq!(r.real_moved, 1);
    assert!(e.real.join("sub/y.bin").is_file());
    assert!(e.repo.join("mirrors/sub/y.bin.tether").is_file());
}

#[test]
fn stocktake_and_retail_against_main() {
    let e = setup();
    write(&e.real.join("a.bin"), b"aaaa");
    write(&e.real.join("sub/b.bin"), b"bbbb");
    write(&e.real.join("sub/c.bin"), b"cccc");
    scan::scan(&e.repo, &lfs(&e), true).unwrap();

    git::run(&e.repo, &["checkout", "-q", "-b", "dev"]).unwrap();
    git::run(
        &e.repo,
        &[
            "rm",
            "-q",
            "mirrors/sub/b.bin.tether",
            "mirrors/sub/c.bin.tether",
        ],
    )
    .unwrap();
    git::commit(&e.repo, "dev subset").unwrap();

    let r = inventory::stocktake(&e.repo, None, None).unwrap();
    assert!(r.missing.contains(&"sub/b.bin".to_string()), "{r:?}");
    assert!(r.missing.contains(&"sub/c.bin".to_string()), "{r:?}");
    assert!(r.extra.is_empty());

    let r3 = inventory::retail(&e.repo, None, &["sub/b.bin".to_string()]).unwrap();
    assert_eq!(r3.added, vec!["sub/b.bin".to_string()]);
    assert!(e.repo.join("mirrors/sub/b.bin.tether").is_file());
    assert_eq!(
        inventory::stocktake(&e.repo, None, None).unwrap().missing,
        vec!["sub/c.bin".to_string()]
    );
    let r5 = inventory::retail(&e.repo, None, &["sub".to_string()]).unwrap();
    assert_eq!(r5.added, vec!["sub/c.bin".to_string()]);
}
