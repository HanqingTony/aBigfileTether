//! 中心模型：distribute（母机→设备）与 ingest（设备→母机）测试，全部 LocalFs。

use abigfiletether::config::BranchConfig;
use abigfiletether::fs::LocalFs;
use abigfiletether::{config, git, scan, sync};
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

fn dev_real(e: &Env, name: &str) -> PathBuf {
    let p = e._tmp.path().join(name);
    fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn distribute_transfers_missing_to_device() {
    let e = setup();
    write(&e.real.join("a.bin"), b"aaaa");
    write(&e.real.join("b.bin"), b"bbbb");
    scan::scan(&e.repo, &lfs(&e), true).unwrap();

    // 设备视图分支 dev：只含 a
    git::run(&e.repo, &["checkout", "-q", "-b", "dev"]).unwrap();
    git::run(&e.repo, &["rm", "-q", "mirrors/b.bin.tether"]).unwrap();
    git::commit(&e.repo, "dev subset").unwrap();
    git::run(&e.repo, &["checkout", "-q", "master"]).unwrap();

    let dr = dev_real(&e, "dev-real");
    let idx = sync::branch_index(&e.repo, "dev").unwrap();
    let r = sync::distribute(&e.repo, &idx, &LocalFs::new(&dr), &lfs(&e), false).unwrap();
    assert_eq!(r.put, 1, "{r:?}");
    assert_eq!(fs::read(dr.join("a.bin")).unwrap(), b"aaaa");
    assert!(!dr.join("b.bin").exists());
}

#[test]
fn ingest_adds_new_file_to_master() {
    let e = setup();
    write(&e.real.join("a.bin"), b"aaaa");
    scan::scan(&e.repo, &lfs(&e), true).unwrap();

    let dr = dev_real(&e, "dev-real");
    write(&dr.join("a.bin"), b"aaaa"); // 已有
    write(&dr.join("n.bin"), b"nnnn"); // 新

    let r = sync::ingest(&e.repo, &LocalFs::new(&dr), &lfs(&e), true).unwrap();
    assert_eq!(r.added, 1, "{r:?}");
    assert_eq!(fs::read(e.real.join("n.bin")).unwrap(), b"nnnn");
    assert!(e.repo.join("mirrors/n.bin.tether").is_file());
    // master 索引已含 n
    let idx = abigfiletether::shadow::load_index(&e.repo).unwrap();
    assert!(idx.contains_key(std::path::Path::new("n.bin")));
}

#[test]
fn ingest_recognizes_move_on_device() {
    let e = setup();
    write(&e.real.join("a.bin"), b"aaaa");
    scan::scan(&e.repo, &lfs(&e), true).unwrap();

    // 设备上把 a.bin 改名为 a2.bin（内容不变）
    let dr = dev_real(&e, "dev-real");
    write(&dr.join("a2.bin"), b"aaaa"); // 与 master 的 a.bin 同 hash

    let r = sync::ingest(&e.repo, &LocalFs::new(&dr), &lfs(&e), true).unwrap();
    assert_eq!(r.moved, 1, "{r:?}");
    assert!(e.real.join("a2.bin").is_file());
    assert!(!e.real.join("a.bin").exists());
    assert!(e.repo.join("mirrors/a2.bin.tether").is_file());
}
