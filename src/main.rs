//! tether CLI 入口。子命令见 `docs/DESIGN.md` §6。

use abigfiletether::{apply, config, git, scan, sync, transport};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "tether", version, about = "非侵入式大文件影子追踪工具")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 初始化：绑定仓库与真实文件夹，写入本地 gitignored 配置
    Init {
        /// 被管理的真实文件夹路径
        path: PathBuf,
        /// 仓库路径，缺省为当前目录
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// 扫描真实文件夹 → 仓库（默认预演，--yes 落盘）
    Scan {
        /// 不询问，直接落盘提交
        #[arg(long)]
        yes: bool,
        /// 结构化 JSON 日志
        #[arg(long)]
        log_json: bool,
    },
    /// 应用仓库 → 真实文件夹（默认 dry-run）
    Apply {
        /// 目标提交/分支，缺省为 HEAD
        #[arg(long)]
        to: Option<String>,
        /// 删除基线外的真实文件
        #[arg(long)]
        prune: bool,
        /// 只打印计划（默认行为）
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// 展示真实树与影子树的差异（不落盘）
    Status,
    /// 由当前索引生成证书，或全量校验
    Cert {
        /// 全量重算哈希并与索引/证书比对
        #[arg(long)]
        verify: bool,
    },
    /// 从对等端拉取本快照缺失的字节
    Pull {
        /// 对等端：tether.toml 中的名字，或 user@host:/repo
        #[arg(long)]
        from: String,
        /// 删除快照外的真实文件
        #[arg(long)]
        prune: bool,
    },
    /// 把本快照推给对等端（哈希感知，只传新字节）
    Push {
        /// 对等端：tether.toml 中的名字，或 user@host:/repo
        #[arg(long)]
        to: String,
        /// 删除对等端快照外的文件
        #[arg(long)]
        prune: bool,
    },
    /// 远端 agent：被 ssh 调用，走 stdio 协议（内部使用）
    Agent {
        /// 仓库路径；缺省由协议 Hello 携带
        #[arg(long)]
        repo: Option<PathBuf>,
    },
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

fn load_repo() -> Result<(PathBuf, config::LocalConfig)> {
    let repo = git::toplevel(&cwd()?)?;
    let cfg = config::load(&repo)?;
    Ok((repo, cfg))
}

/// 解析对等端：优先查 `tether.toml` 的 `[peers]`，否则按 `user@host:/repo` 解析。
fn resolve_peer(cfg: &config::LocalConfig, name: &str) -> Result<transport::Peer> {
    let spec = cfg.peers.get(name).map(String::as_str).unwrap_or(name);
    transport::Peer::parse(spec)
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init { path, repo } => {
            let repo = match repo {
                Some(r) => r,
                None => cwd()?,
            };
            let real = if path.is_absolute() {
                path
            } else {
                cwd()?.join(path)
            };
            let real = real
                .canonicalize()
                .context("真实文件夹不存在：请先创建后再 init（本工具不会创建真实文件）")?;
            fs::create_dir_all(&repo)?;
            git::init(&repo)?;
            config::ensure_gitignore(&repo)?;
            config::ensure_ignore_file(&repo)?;
            let cfg = config::LocalConfig {
                path: real.to_string_lossy().into_owned(),
                label: None,
                peers: Default::default(),
            };
            config::save(&repo, &cfg)?;
            println!("[INFO] 仓库 {}", repo.display());
            println!("[INFO] 管理真实文件夹 {}", real.display());
            println!("[INFO] 下一步：tether scan --yes");
            Ok(())
        }
        Command::Scan { yes, log_json } => {
            let (repo, cfg) = load_repo()?;
            let report = scan::scan(&repo, &cfg.resolve_real(&repo), yes)?;
            print_scan(&report, yes, log_json)?;
            Ok(())
        }
        Command::Status => {
            let (repo, cfg) = load_repo()?;
            let report = scan::scan(&repo, &cfg.resolve_real(&repo), false)?;
            print_scan(&report, false, false)?;
            Ok(())
        }
        Command::Apply { to, prune, dry_run } => {
            let (repo, cfg) = load_repo()?;
            let report = apply::apply(&repo, &cfg.resolve_real(&repo), to, prune, dry_run)?;
            print_apply(&report);
            Ok(())
        }
        Command::Cert { verify } => {
            let (repo, cfg) = load_repo()?;
            let real = cfg.resolve_real(&repo);
            if verify {
                let r = scan::verify(&repo, &real)?;
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
                let n = scan::write_cert_from_index(&repo, &real)?;
                println!("[INFO] 已写证书 {}（{n} 项）", real.display());
            }
            Ok(())
        }
        Command::Pull { from, prune } => {
            let (repo, cfg) = load_repo()?;
            let peer = resolve_peer(&cfg, &from)?;
            let report = sync::pull(&repo, &cfg.resolve_real(&repo), &peer, prune)?;
            print_sync("pull", &report);
            Ok(())
        }
        Command::Push { to, prune } => {
            let (repo, cfg) = load_repo()?;
            let peer = resolve_peer(&cfg, &to)?;
            let report = sync::push(&repo, &cfg.resolve_real(&repo), &peer, prune)?;
            print_sync("push", &report);
            Ok(())
        }
        Command::Agent { repo } => transport::run_agent(repo),
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
        "[INFO] 移动 {}  删除 {}  已在位 {}  冲突 {}  待pull {}  保留(未开prune) {}",
        r.renamed, r.removed, r.satisfied, r.conflicts, r.need_pull, r.prune_skipped
    );
    if r.dry_run {
        println!("[INFO] 预演（--dry-run）");
    } else if r.applied {
        println!("[INFO] 已应用，base 更新到 {}", r.target);
    } else {
        println!("[INFO] 未完全应用（存在冲突或待 pull），base 未更新");
    }
}

fn print_sync(op: &str, r: &sync::SyncReport) {
    for d in &r.details {
        println!("   {d}");
    }
    println!(
        "[INFO] {op}: 已在位 {}  移动 {}  复制 {}  传输 {}  拉取 {}  删除 {}  缺失 {}",
        r.satisfied, r.moved, r.copied, r.put, r.fetched, r.deleted, r.missing
    );
}
