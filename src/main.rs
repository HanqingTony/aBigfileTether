//! tether CLI（中心模型）：一个仓库，分支携带各自 `location`；命令作用于当前分支的位置。

use abigfiletether::fs as tfs;
use abigfiletether::{apply, config, git, inventory, propagate, reorg, scan, sync, transport};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::env;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "tether",
    version,
    about = "非侵入式大文件影子追踪工具（中心仓库 + retail 模型）",
    long_about = "一个 Git 仓库（母机）保存所有分支的影子元数据；每个分支的 tether.toml 声明该视图\n\
对应的真实文件夹位置（local:/path 或 [user@]host:/path，相对母机视角）。命令作用于当前分支的位置，\n\
真实字节经 hash 感知搬运（已有同 hash 零重传）。",
    arg_required_else_help = true,
    after_help = "示例:\n  \
tether init local:/mnt/data/zext\n  \
tether scan --yes\n  \
tether stocktake comfyui/models/loras\n  \
tether retail comfyui/models/vae/trellis\n  \
tether distribute --branch zlapwsl\n  \
tether ingest --branch zlapwsl"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 初始化：建立仓库/当前分支，写入跟踪的 tether.toml（含 location）
    Init {
        /// 位置：`local:/path` 或 `[user@]host:/path`
        location: String,
        /// 仓库路径，缺省当前目录
        #[arg(long, value_name = "REPO")]
        repo: Option<PathBuf>,
    },
    /// 扫描当前分支 location → 影子（默认预演，--yes 落盘）
    Scan {
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        log_json: bool,
    },
    /// 应用当前分支影子 → location（默认 dry-run）
    Apply {
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        prune: bool,
        #[arg(long)]
        yes: bool,
    },
    /// 展示 location 与影子树的差异（不落盘）
    Status,
    /// 按已知映射整理影子（--real 同时移动 location 上的真实文件），**零哈希**
    Reorg {
        #[arg(long, value_name = "FILE")]
        map: PathBuf,
        #[arg(long)]
        real: bool,
        #[arg(long)]
        yes: bool,
    },
    /// 盘点：当前分支与 main 的文件存在差异（可限定文件夹）
    Stocktake {
        #[arg(value_name = "DIR")]
        dir: Option<String>,
        #[arg(long, value_name = "REF")]
        main: Option<String>,
    },
    /// 零售：把 main 中指定文件/文件夹的影子取入当前分支（再 distribute 取字节）
    Retail {
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<String>,
        #[arg(long, value_name = "REF")]
        main: Option<String>,
    },
    /// 把某分支的 A/M/R 并回当前分支，**丢弃删除**
    Propagate {
        #[arg(long, value_name = "REF")]
        from: String,
        #[arg(long, value_name = "REF")]
        onto: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// 分发：把 <branch> 快照落到其 location（字节取自 <from> 的 location，默认 HEAD）
    ///
    /// 在母机执行；center → device。哈希感知：设备已有同 hash 只 MOVE，缺的才传。
    Distribute {
        /// 设备分支（其 tether.toml 给出设备位置）
        #[arg(long, value_name = "REF")]
        branch: String,
        /// 字节来源分支，缺省 HEAD（母机全量）
        #[arg(long, value_name = "REF")]
        from: Option<String>,
        /// 删除设备上快照外的文件
        #[arg(long)]
        prune: bool,
    },
    /// 收回：把 <branch> location 的新增/变动并入当前母机（master 影子 + 字节），删除不传播
    ///
    /// 在母机（master 已检出）执行；device → center。
    Ingest {
        #[arg(long, value_name = "REF")]
        branch: String,
        /// 母机引用（当前检出的分支），缺省 HEAD
        #[arg(long, value_name = "REF")]
        onto: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// 由索引生成证书，或 `--verify` 全量校验
    Cert {
        #[arg(long)]
        verify: bool,
    },
    /// 远端 agent：被 ssh 调用，stdout 协议（内部；无状态，以 Hello.root 为根）
    Agent,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("[ERROR] {e:#}");
        std::process::exit(1);
    }
}

fn cwd() -> Result<PathBuf> {
    env::current_dir().context("无法获取当前目录")
}
fn toplevel() -> Result<PathBuf> {
    git::toplevel(&cwd()?)
}
fn load_cfg(repo: &std::path::Path) -> Result<config::BranchConfig> {
    config::load(repo)
}
fn open_fs(cfg: &config::BranchConfig) -> Result<Box<dyn tfs::Fs>> {
    let loc = cfg.location()?;
    tfs::open(&loc, &cfg.transfer)
}
fn open_rev_fs(repo: &std::path::Path, rev: &str) -> Result<Box<dyn tfs::Fs>> {
    let cfg = config::load_at(repo, rev)?;
    let loc = cfg.location()?;
    tfs::open(&loc, &cfg.transfer)
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init { location, repo } => {
            let repo = match repo {
                Some(r) => r,
                None => cwd()?,
            };
            std::fs::create_dir_all(&repo)?;
            git::init(&repo)?;
            config::ensure_ignore_file(&repo)?;
            let cfg = config::BranchConfig {
                location,
                label: None,
                transfer: Default::default(),
            };
            config::save(&repo, &cfg)?;
            println!("[INFO] 仓库 {}", repo.display());
            println!("[INFO] 当前分支 location = {}", cfg.location);
            println!("[INFO] 下一步：tether scan --yes");
            Ok(())
        }
        Command::Scan { yes, log_json } => {
            let repo = toplevel()?;
            let cfg = load_cfg(&repo)?;
            let f = open_fs(&cfg)?;
            let report = scan::scan(&repo, &*f, yes)?;
            print_scan(&report, yes, log_json)?;
            Ok(())
        }
        Command::Status => {
            let repo = toplevel()?;
            let cfg = load_cfg(&repo)?;
            let f = open_fs(&cfg)?;
            let report = scan::scan(&repo, &*f, false)?;
            print_scan(&report, false, false)?;
            Ok(())
        }
        Command::Apply { to, prune, yes } => {
            let repo = toplevel()?;
            let cfg = load_cfg(&repo)?;
            let f = open_fs(&cfg)?;
            let report = apply::apply(&repo, &*f, to, prune, !yes)?;
            print_apply(&report);
            Ok(())
        }
        Command::Reorg { map, real, yes } => {
            let repo = toplevel()?;
            if real {
                let cfg = load_cfg(&repo)?;
                let f = open_fs(&cfg)?;
                let report = reorg::reorg(&repo, Some(&*f), &map, !yes)?;
                print_reorg(&report);
            } else {
                let report = reorg::reorg(&repo, None, &map, !yes)?;
                print_reorg(&report);
            }
            Ok(())
        }
        Command::Stocktake { dir, main } => {
            let repo = toplevel()?;
            print_stocktake(&inventory::stocktake(
                &repo,
                main.as_deref(),
                dir.as_deref(),
            )?);
            Ok(())
        }
        Command::Retail { paths, main } => {
            let repo = toplevel()?;
            print_retail(&inventory::retail(&repo, main.as_deref(), &paths)?);
            Ok(())
        }
        Command::Propagate { from, onto, yes } => {
            let repo = toplevel()?;
            print_propagate(&propagate::propagate(&repo, &from, onto, !yes)?);
            Ok(())
        }
        Command::Distribute {
            branch,
            from,
            prune,
        } => {
            let repo = toplevel()?;
            let src_rev = from.unwrap_or_else(|| "HEAD".to_string());
            let master_fs = open_rev_fs(&repo, &src_rev)?;
            let dev_fs = open_rev_fs(&repo, &branch)?;
            let idx = sync::branch_index(&repo, &branch)?;
            let report = sync::distribute(&repo, &idx, &*dev_fs, &*master_fs, prune)?;
            print_sync("distribute", &report);
            Ok(())
        }
        Command::Ingest { branch, onto, yes } => {
            let repo = toplevel()?;
            let dev_fs = open_rev_fs(&repo, &branch)?;
            let onto_rev = onto.unwrap_or_else(|| "HEAD".to_string());
            let master_fs = open_rev_fs(&repo, &onto_rev)?;
            let report = sync::ingest(&repo, &*dev_fs, &*master_fs, yes)?;
            print_sync("ingest", &report);
            Ok(())
        }
        Command::Cert { verify } => {
            let repo = toplevel()?;
            let cfg = load_cfg(&repo)?;
            let f = open_fs(&cfg)?;
            if verify {
                let r = scan::verify(&repo, &*f)?;
                println!(
                    "[INFO] 校验 {} 项：一致 {}，不一致 {}",
                    r.checked,
                    r.ok,
                    r.mismatches.len()
                );
                for m in &r.mismatches {
                    println!("   不一致 {m}");
                }
                println!("[INFO] {}", r.cert_message);
                if !r.mismatches.is_empty() || !r.cert_ok {
                    anyhow::bail!("校验未通过");
                }
            } else {
                let n = scan::write_cert_from_index(&repo, &*f)?;
                println!("[INFO] 已写证书（{n} 项）");
            }
            Ok(())
        }
        Command::Agent => transport::run_agent(),
    }
}

fn print_scan(report: &scan::ScanReport, committed: bool, log_json: bool) -> Result<()> {
    if log_json {
        println!("{}", serde_json::to_string(report)?);
        return Ok(());
    }
    println!(
        "[INFO] 未变 {}  修改 {}  移动 {}  复制 {}  新增 {}  删除 {}（全量哈希 {} 个）",
        report.unchanged,
        report.modified,
        report.moved,
        report.copied,
        report.added,
        report.deleted,
        report.hashed
    );
    for (a, b) in &report.moved_pairs {
        println!("   移动 {a} -> {b}");
    }
    if committed {
        match &report.commit {
            Some(c) => println!("[INFO] 已提交 {}", &c[..c.len().min(12)]),
            None => println!("[INFO] 无变更，未提交"),
        }
    } else {
        println!("[INFO] 预演（未落盘）；加 --yes 执行");
    }
    Ok(())
}

fn print_apply(r: &apply::ApplyReport) {
    for d in &r.details {
        println!("   {d}");
    }
    println!(
        "[INFO] 移动 {}  删除 {}  已在位 {}  冲突 {}  待distribute {}  保留(未开prune) {}",
        r.renamed, r.removed, r.satisfied, r.conflicts, r.need_pull, r.prune_skipped
    );
    if r.dry_run {
        println!("[INFO] 预演（未改动真实文件）；加 --yes 执行");
    } else if r.applied {
        println!("[INFO] 已应用，base 更新到 {}", r.target);
    } else {
        println!("[INFO] 未完全应用（存在冲突或待 distribute），base 未更新");
    }
}

fn print_reorg(r: &reorg::ReorgReport) {
    println!(
        "[INFO] reorg: 影子移动 {}  跳过 {}  缺失 {}{}",
        r.moved,
        r.skipped,
        r.missing,
        if r.do_real {
            format!("  真实移动 {}", r.real_moved)
        } else {
            String::new()
        }
    );
    if r.dry_run {
        println!("[INFO] 预演（未落盘）；加 --yes 执行");
    } else if r.committed {
        match &r.commit {
            Some(c) => println!("[INFO] 已提交 {}", &c[..c.len().min(12)]),
            None => println!("[INFO] 无变更，未提交"),
        }
    }
}

fn print_stocktake(r: &inventory::StocktakeReport) {
    let f = r.filter.as_deref().unwrap_or("(全部)");
    println!(
        "[INFO] stocktake vs {}（范围 {}）：可 retail {} 项，多出 {} 项",
        &r.main_ref[..r.main_ref.len().min(12)],
        f,
        r.missing.len(),
        r.extra.len()
    );
    for p in &r.missing {
        println!("  + {p}");
    }
    for p in &r.extra {
        println!("  - {p}");
    }
    if r.missing.is_empty() && r.extra.is_empty() {
        println!("  （无差异）");
    }
}

fn print_retail(r: &inventory::RetailReport) {
    for p in &r.added {
        println!("  取入 {p}");
    }
    for p in &r.skipped {
        println!("  已有跳过 {p}");
    }
    for p in &r.not_found {
        println!("  未找到 {p}");
    }
    println!(
        "[INFO] retail vs {}：取入 {}  已有 {}  未找到 {}",
        &r.main_ref[..r.main_ref.len().min(12)],
        r.added.len(),
        r.skipped.len(),
        r.not_found.len()
    );
    if r.committed {
        println!("[INFO] 已提交；下一步 `tether distribute --branch <本分支>` 下载字节");
    }
}

fn print_propagate(r: &propagate::PropagateReport) {
    for d in &r.details {
        println!("   {d}");
    }
    println!(
        "[INFO] propagate {} -> {}: 新增 {}  更新 {}  移动 {}  丢弃删除 {}  已存在跳过 {}  冲突 {}",
        r.from,
        r.onto,
        r.added,
        r.updated,
        r.moved,
        r.skipped_deleted,
        r.skipped_existing,
        r.conflicts
    );
    if r.dry_run {
        println!("[INFO] 预演（未落盘）；加 --yes 执行");
    } else if r.committed {
        match &r.commit {
            Some(c) => println!("[INFO] 已提交 {}", &c[..c.len().min(12)]),
            None => println!("[INFO] 无变更，未提交"),
        }
    }
}

fn print_sync(op: &str, r: &sync::SyncReport) {
    for d in &r.details {
        println!("   {d}");
    }
    println!(
        "[INFO] {op}: 已在位 {}  移动 {}  复制 {}  传输 {}  新增 {}  修改 {}  删除 {}  缺失 {}",
        r.satisfied, r.moved, r.copied, r.put, r.added, r.modified, r.deleted, r.missing
    );
}
