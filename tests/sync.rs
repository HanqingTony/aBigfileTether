//! 传输/同步端到端测试：用本地 `tether agent` 管道跑真实协议（等价 SSH 通道），
//! 全部在临时目录，绝不触碰真实数据。

use abigfiletether::transport::Peer;
use abigfiletether::{config, git, scan, sync};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

struct Env {
    _tmp: TempDir,
    repo: PathBuf,
    real: PathBuf,
}

fn init_repo(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    let real = root.join(format!("{name}-real"));
    let repo = root.join(format!("{name}-repo"));
    fs::create_dir_all(&real).unwrap();
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
    (repo, real)
}

fn setup(name: &str) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, real) = init_repo(tmp.path(), name);
    Env {
        _tmp: tmp,
        repo,
        real,
    }
}

fn write(path: &Path, data: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, data).unwrap();
}

fn local_peer(repo: &Path) -> Peer {
    Peer::Local {
        bin: PathBuf::from(env!("CARGO_BIN_EXE_tether")),
        repo: repo.to_path_buf(),
    }
}

#[test]
fn push_moves_by_hash_and_only_sends_missing_bytes() {
    // A：源快照
    let a = setup("a");
    write(&a.real.join("a.bin"), b"aaaa");
    write(&a.real.join("models/x.bin"), b"xxxxxxxx");
    write(&a.real.join("dup1.bin"), b"same");
    write(&a.real.join("dup2.bin"), b"same");
    scan::scan(&a.repo, &a.real, true).unwrap();

    // B：对端，已有同 hash 的 x.bin（但路径不同），且 a.bin 内容不同
    let b = setup("b");
    write(&b.real.join("stage/x.bin"), b"xxxxxxxx");
    write(&b.real.join("a.bin"), b"old");
    scan::scan(&b.repo, &b.real, true).unwrap();

    let r = sync::push(&a.repo, &a.real, &local_peer(&b.repo), false).unwrap();

    // models/x.bin 的字节已在 B（stage/x.bin），应只做 move，零传输
    assert!(r.moved >= 1, "应识别为 move：{r:?}");
    assert_eq!(r.put, 3, "a.bin + dup1 + dup2 需传输");

    // B 真实侧最终与 A 快照一致
    assert_eq!(fs::read(b.real.join("models/x.bin")).unwrap(), b"xxxxxxxx");
    assert!(!b.real.join("stage/x.bin").exists(), "源应已移动");
    assert_eq!(fs::read(b.real.join("a.bin")).unwrap(), b"aaaa");
    assert_eq!(fs::read(b.real.join("dup2.bin")).unwrap(), b"same");
}

#[test]
fn push_prune_removes_peer_extras() {
    let a = setup("a");
    write(&a.real.join("keep.bin"), b"keep");
    scan::scan(&a.repo, &a.real, true).unwrap();

    let b = setup("b");
    write(&b.real.join("keep.bin"), b"keep");
    write(&b.real.join("extra.bin"), b"extra");
    scan::scan(&b.repo, &b.real, true).unwrap();

    let r = sync::push(&a.repo, &a.real, &local_peer(&b.repo), true).unwrap();
    assert_eq!(r.deleted, 1);
    assert!(!b.real.join("extra.bin").exists());
    assert!(b.real.join("keep.bin").exists());
}

#[test]
fn pull_fetches_all_missing_from_peer() {
    // A：全量源
    let a = setup("a");
    write(&a.real.join("m/one.bin"), b"one");
    write(&a.real.join("m/two.bin"), b"twotwo");
    write(&a.real.join("three.bin"), b"three");
    scan::scan(&a.repo, &a.real, true).unwrap();

    // C：从 A 克隆影子仓库（拿到快照索引），真实文件夹为空
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("c-repo");
    let real = tmp.path().join("c-real");
    fs::create_dir_all(&real).unwrap();
    git::run(
        tmp.path(),
        &["clone", &a.repo.to_string_lossy(), &repo.to_string_lossy()],
    )
    .unwrap();
    let cfg = config::LocalConfig {
        path: real.to_string_lossy().into_owned(),
        label: None,
        peers: Default::default(),
        transfer: Default::default(),
    };
    config::save(&repo, &cfg).unwrap();

    let r = sync::pull(&repo, &real, &local_peer(&a.repo), false).unwrap();
    assert_eq!(r.fetched, 3);
    assert_eq!(r.missing, 0);
    assert_eq!(fs::read(real.join("m/one.bin")).unwrap(), b"one");
    assert_eq!(fs::read(real.join("three.bin")).unwrap(), b"three");

    // pull 完成后 base 应指向 HEAD
    assert_eq!(
        git::rev_parse(&repo, scan::BASE_REF).unwrap(),
        git::rev_parse(&repo, "HEAD").unwrap()
    );
}

#[test]
fn verify_detects_same_size_content_change() {
    let e = setup("a");
    write(&e.real.join("a.bin"), b"aaaa");
    scan::scan(&e.repo, &e.real, true).unwrap();

    let ok = scan::verify(&e.repo, &e.real).unwrap();
    assert_eq!(ok.checked, 1);
    assert_eq!(ok.mismatches.len(), 0);
    assert!(ok.cert_ok);

    // 同大小、不同内容：扫描初筛会漏掉，但 verify 全量哈希应抓到
    write(&e.real.join("a.bin"), b"bbbb");
    let bad = scan::verify(&e.repo, &e.real).unwrap();
    assert_eq!(bad.mismatches.len(), 1, "应为 1 处不一致：{bad:?}");
}

/// 克隆一个已有快照的仓库到新目录，返回 (repo, real)。
fn clone_with_real(
    source: &Env,
    tag: &str,
    real_files: &[(&str, &[u8])],
) -> (TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join(format!("{tag}-repo"));
    let real = tmp.path().join(format!("{tag}-real"));
    fs::create_dir_all(&real).unwrap();
    git::run(
        tmp.path(),
        &[
            "clone",
            &source.repo.to_string_lossy(),
            &repo.to_string_lossy(),
        ],
    )
    .unwrap();
    config::save(
        &repo,
        &config::LocalConfig {
            path: real.to_string_lossy().into_owned(),
            label: None,
            peers: Default::default(),
            transfer: Default::default(),
        },
    )
    .unwrap();
    for (rel, data) in real_files {
        write(&real.join(rel), data);
    }
    (tmp, repo, real)
}

#[test]
fn get_resumes_from_existing_part() {
    let a = setup("a");
    write(&a.real.join("resume.bin"), b"0123456789");
    scan::scan(&a.repo, &a.real, true).unwrap();

    // 预置本地 .part 前 4 字节，pull 应从 offset=4 续传
    let (_tmp, repo, real) = clone_with_real(&a, "c", &[("resume.bin.part", b"0123")]);
    let r = sync::pull(&repo, &real, &local_peer(&a.repo), false).unwrap();

    assert_eq!(r.fetched, 1);
    assert_eq!(fs::read(real.join("resume.bin")).unwrap(), b"0123456789");
    assert!(!real.join("resume.bin.part").exists(), ".part 应已改名");
}

#[test]
fn put_resumes_from_existing_part() {
    let a = setup("a");
    write(&a.real.join("big.bin"), b"ABCDEFGHIJ");
    scan::scan(&a.repo, &a.real, true).unwrap();

    // 对端未扫描，但预置 .part 前 5 字节；push 应从 offset=5 续传
    let b = setup("b");
    write(&b.real.join("big.bin.part"), b"ABCDE");

    let r = sync::push(&a.repo, &a.real, &local_peer(&b.repo), false).unwrap();
    assert_eq!(r.put, 1);
    assert_eq!(fs::read(b.real.join("big.bin")).unwrap(), b"ABCDEFGHIJ");
    assert!(!b.real.join("big.bin.part").exists(), ".part 应已改名");
}

#[test]
fn pull_uses_local_copy_for_duplicate_hash() {
    let a = setup("a");
    // 两个路径同内容（同 hash）
    write(&a.real.join("p1.bin"), b"same");
    write(&a.real.join("p2.bin"), b"same");
    scan::scan(&a.repo, &a.real, true).unwrap();

    // C：p1 已在位，p2 缺失 → p2 应从本地 p1 复制，不走网络
    let (_tmp, repo, real) = clone_with_real(&a, "c", &[("p1.bin", b"same")]);
    let r = sync::pull(&repo, &real, &local_peer(&a.repo), false).unwrap();

    assert_eq!(r.local_copied, 1, "应本地复制：{r:?}");
    assert_eq!(r.fetched, 0, "不应从对端取：{r:?}");
    assert_eq!(fs::read(real.join("p2.bin")).unwrap(), b"same");
}
