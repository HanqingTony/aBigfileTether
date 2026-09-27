//! tether CLI 入口。子命令见 `docs/DESIGN.md` §6。

use abigfiletether::{apply, config, git, propagate, reorg, scan, sync, transport};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "tether",
    version,
    about = "非侵入式大文件影子追踪工具",
    long_about = "tether 用普通 Git 仓库追踪大文件的影子元数据：真实文件原位不动，\
字节永不进入 Git 对象库。\n\n\
身份 = size + blake3；路径只是位置，移动/改名靠内容哈希识别。\
真实侧的任何改动都只在显式命令下发生（scan 只读、apply/pull/push 才动）。",
    arg_required_else_help = true,
    after_help = "示例:\n  \
tether init ~/zext --repo ~/zrepo/tether-zext\n  \
tether scan --yes\n  \
tether status\n  \
tether apply --yes\n  \
tether cert --verify\n  \
tether push --to zmain\n\n\
配置文件（gitignored，仓库根 tether.toml）:\n  \
path  = 本机真实文件夹；[peers] 为对等端。\n\n\
对等端写法:\n  \
user@host:/peer-repo（对端需在 PATH 上有 tether；推送元数据用普通 git push）。"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 初始化：绑定仓库与真实文件夹
    ///
    /// 建立（或接入）一个普通 Git 仓库，写入本机专属、被 gitignore 的 tether.toml，
    /// 并生成 .tetherignore（排除清单 + 仓库标记）。不创建、不移动任何真实文件；
    /// 真实路径会解析软链后记录。
    #[command(after_help = "示例:\n  tether init ~/zext --repo ~/zrepo/tether-zext")]
    Init {
        /// 被管理的真实文件夹路径
        #[arg(value_name = "REAL")]
        path: PathBuf,
        /// 仓库路径，缺省为当前目录
        #[arg(long, value_name = "REPO")]
        repo: Option<PathBuf>,
    },
    /// 扫描真实文件夹 → 仓库（默认预演，--yes 落盘）
    ///
    /// 以「路径 + size」初筛，只有变动文件才做全量 blake3；按内容哈希识别移动/复制，
    /// 落成 Git 变更并提交，随后写证书（真实侧顶层 TETHER.cert.toml）并推进
    /// refs/tether/base。库存在未应用的影子变更时会拒绝扫描。
    #[command(
        after_help = "示例:\n  tether scan          # 预演\n  tether scan --yes    # 落盘提交"
    )]
    Scan {
        /// 不询问，直接落盘提交
        #[arg(long)]
        yes: bool,
        /// 结构化 JSON 日志
        #[arg(long)]
        log_json: bool,
    },
    /// 应用仓库 → 真实文件夹（默认 dry-run）
    ///
    /// 取 refs/tether/base..target 的镜像差异落地：移动按目标影子哈希校验后改名；
    /// 新增而真实缺失的条目留待 pull（绝不凭空造字节）；修改一律报冲突、不覆盖。
    /// --prune 以目标快照为准删除真实侧多余文件。默认只预演。
    #[command(
        after_help = "示例:\n  tether apply            # 预演\n  tether apply --yes      # 执行\n  tether apply --to main --prune --yes"
    )]
    Apply {
        /// 目标提交/分支，缺省为 HEAD
        #[arg(long, value_name = "REF")]
        to: Option<String>,
        /// 删除目标快照外的真实文件
        #[arg(long)]
        prune: bool,
        /// 执行（缺省仅预演）
        #[arg(long)]
        yes: bool,
    },
    /// 展示真实树与影子树的差异（不落盘）
    ///
    /// 等价于一次不带 --yes 的扫描，只列出 未变/修改/移动/复制/新增/删除。
    Status,
    /// 把某分支的 新增/移动/修改 并回当前分支，**丢弃删除**
    ///
    /// 用于把剪枝子分支的增量安全并回 main：取 merge-base..from 的 A/M/R，不传播
    /// D，因此在影子层不会删掉 main 的文件。随后在目标真实文件夹用 apply 落地。
    #[command(
        after_help = "示例:\n  tether propagate --from zlapwsl            # 预演\n  tether propagate --from zlapwsl --yes      # 落到当前分支\n  tether propagate --from zlapwsl --onto master --yes"
    )]
    Propagate {
        /// 来源分支/提交
        #[arg(long, value_name = "REF")]
        from: String,
        /// 目标分支/提交，缺省为 HEAD
        #[arg(long, value_name = "REF")]
        onto: Option<String>,
        /// 执行（缺省仅预演）
        #[arg(long)]
        yes: bool,
    },
    /// 按已知路径映射整理影子（可选真实）文件，**不计算哈希**
    ///
    /// 映射文件每行 `旧相对路径|新相对路径`（相对真实根）。整理目录结构时用它，
    /// 比"先动真实再扫描"快得多。默认只动影子仓库；--real 同时移动真实文件。
    #[command(
        after_help = "示例:\n  tether reorg --map plan.txt            # 预演（只影子）\n  tether reorg --map plan.txt --yes      # 落地影子\n  tether reorg --map plan.txt --real --yes  # 同时移动真实文件"
    )]
    Reorg {
        /// 映射文件（每行 `旧|新`，相对真实根）
        #[arg(long, value_name = "FILE")]
        map: PathBuf,
        /// 同时移动真实文件
        #[arg(long)]
        real: bool,
        /// 执行（缺省仅预演）
        #[arg(long)]
        yes: bool,
    },
    /// 由索引生成证书，或全量校验
    ///
    /// 默认由当前镜像索引写证书（不重算哈希）。--verify 重算所有真实文件的 blake3，
    /// 与索引及证书 root_hash 比对，任一处不一致则退出码 1。
    #[command(
        after_help = "示例:\n  tether cert            # 写证书\n  tether cert --verify   # 全量校验"
    )]
    Cert {
        /// 全量重算哈希并与索引/证书比对
        #[arg(long)]
        verify: bool,
    },
    /// 从对等端拉取本快照缺失的字节
    ///
    /// 对端 = tether.toml 的 [peers] 名，或 user@host:/peer-repo。按内容哈希取数，
    /// 支持断点续传；本地别处已有同 hash 时本地复制、不下载。
    /// --prune 删除快照外的真实文件。
    #[command(
        after_help = "示例:\n  tether pull --from zmain\n  tether pull --from tony@nas:/home/tony/zrepo/tether-zext --prune"
    )]
    Pull {
        /// 对等端：名字或 user@host:/repo
        #[arg(long, value_name = "PEER")]
        from: String,
        /// 删除快照外的真实文件
        #[arg(long)]
        prune: bool,
    },
    /// 把本快照推给对等端（哈希感知，只传新字节）
    ///
    /// 对端已有同 hash 的字节只做移动/复制（零重传），缺失的才传输；支持断点续传。
    /// --prune 删除对端快照外的文件。元数据的发布请另用 git push。
    #[command(after_help = "示例:\n  tether push --to zmain\n  tether push --to zmain --prune")]
    Push {
        /// 对等端：名字或 user@host:/repo
        #[arg(long, value_name = "PEER")]
        to: String,
        /// 删除对等端快照外的文件
        #[arg(long)]
        prune: bool,
    },
    /// 远端 agent：被 ssh 调用，走 stdio 协议（内部使用）
    ///
    /// 由 `ssh <host> -- tether agent` 调起，仓库路径由协议 Hello 携带，
    /// 通常无需手工调用；--repo 仅供本地调试。
    Agent {
        /// 仓库路径；缺省由协议 Hello 携带
        #[arg(long, value_name = "REPO")]
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
/// 后端由 `[transfer] backend` 选择（system-ssh 默认，russh 可选）。
fn resolve_peer(cfg: &config::LocalConfig, name: &str) -> Result<transport::Peer> {
    let spec = cfg.peers.get(name).map(String::as_str).unwrap_or(name);
    let remote_bin = cfg
        .transfer
        .remote_bin
        .clone()
        .unwrap_or_else(|| "tether".to_string());
    match cfg.transfer.backend.as_deref().unwrap_or("system-ssh") {
        "system-ssh" | "ssh" => transport::Peer::parse(spec, &remote_bin),
        "russh" => transport::Peer::russh(spec, cfg.transfer.key.as_ref().map(PathBuf::from)),
        other => anyhow::bail!("未知 [transfer] backend：{other}（system-ssh | russh）"),
    }
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
                transfer: Default::default(),
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
        Command::Propagate { from, onto, yes } => {
            let repo = git::toplevel(&cwd()?)?;
            let report = propagate::propagate(&repo, &from, onto, !yes)?;
            print_propagate(&report);
            Ok(())
        }
        Command::Reorg { map, real, yes } => {
            let (repo, cfg) = load_repo()?;
            let report = reorg::reorg(&repo, &cfg.resolve_real(&repo), &map, real, !yes)?;
            print_reorg(&report);
            Ok(())
        }
        Command::Apply { to, prune, yes } => {
            let (repo, cfg) = load_repo()?;
            let report = apply::apply(&repo, &cfg.resolve_real(&repo), to, prune, !yes)?;
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
        println!("[INFO] 预演（未改动真实文件）；加 --yes 执行");
    } else if r.applied {
        println!("[INFO] 已应用，base 更新到 {}", r.target);
    } else {
        println!("[INFO] 未完全应用（存在冲突或待 pull），base 未更新");
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

fn print_sync(op: &str, r: &sync::SyncReport) {
    for d in &r.details {
        println!("   {d}");
    }
    println!(
        "[INFO] {op}: 已在位 {}  移动 {}  复制 {}  本地复制 {}  传输 {}  拉取 {}  删除 {}  缺失 {}",
        r.satisfied, r.moved, r.copied, r.local_copied, r.put, r.fetched, r.deleted, r.missing
    );
}
