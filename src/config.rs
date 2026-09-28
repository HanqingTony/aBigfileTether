//! 分支配置 `tether.toml`（**跟踪文件，随分支走**）与 `.tetherignore`。
//!
//! `location` 声明该分支对应的真实文件夹位置，**相对运行 tether 的设备（母机）视角**：
//! - `local:/mnt/data/zext`
//! - `tony@192.168.0.101:/mnt/b/zext`
//!
//! `git checkout <分支>` 会切换该文件，于是"位置"随分支自然改变。

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 分支配置文件名（**跟踪入库**）。
pub const CONFIG_NAME: &str = "tether.toml";
/// 排除清单文件名（跟踪）。
pub const IGNORE_NAME: &str = ".tetherignore";
/// 仓库 .gitignore 名。
pub const GITIGNORE_NAME: &str = ".gitignore";

/// 传输相关设置（远端访问）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransferConfig {
    /// 后端：`system-ssh`（默认）或 `russh`（需 `--features russh`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// russh 私钥路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// 远端上 tether 的路径（默认 `tether`，走 PATH）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_bin: Option<String>,
}

/// 分支配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchConfig {
    /// 真实文件夹位置：`local:/abs/path` 或 `[user@]host:/abs/path`。
    pub location: String,
    /// 人读标签。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// 传输设置。
    #[serde(default)]
    pub transfer: TransferConfig,
}

/// 解析后的位置。
#[derive(Debug, Clone)]
pub enum Location {
    /// 本机路径。
    Local { root: PathBuf },
    /// 远端主机上的路径（经 ssh + stateless agent 访问）。
    Remote { host: String, root: PathBuf },
}

impl Location {
    /// 解析 `local:/path` 或 `[user@]host:/path`。
    pub fn parse(spec: &str) -> Result<Location> {
        if let Some(p) = spec.strip_prefix("local:") {
            return Ok(Location::Local {
                root: PathBuf::from(p),
            });
        }
        let (host, path) = spec.split_once(':').with_context(|| {
            format!("location 格式应为 local:/path 或 [user@]host:/path，得到：{spec}")
        })?;
        if host.is_empty() || path.is_empty() {
            bail!("location 非法：{spec}");
        }
        Ok(Location::Remote {
            host: host.to_string(),
            root: PathBuf::from(path),
        })
    }

    pub fn root(&self) -> &Path {
        match self {
            Location::Local { root } | Location::Remote { root, .. } => root,
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Location::Remote { .. })
    }
}

impl BranchConfig {
    pub fn location(&self) -> Result<Location> {
        Location::parse(&self.location)
    }
}

/// 载入 `tether.toml`（跟踪文件）。
pub fn load(repo: &Path) -> Result<BranchConfig> {
    let p = repo.join(CONFIG_NAME);
    let text = fs::read_to_string(&p).with_context(|| {
        format!(
            "缺少分支配置（{}）；用 `tether init` 建立，或 `git checkout` 到含该文件的分支",
            p.display()
        )
    })?;
    toml::from_str(&text).with_context(|| format!("解析配置失败：{}", p.display()))
}

/// 写入 `tether.toml`。
pub fn save(repo: &Path, cfg: &BranchConfig) -> Result<()> {
    let text = toml::to_string_pretty(cfg).context("序列化配置失败")?;
    fs::write(repo.join(CONFIG_NAME), text)
        .with_context(|| format!("写入配置失败：{}", repo.display()))
}

/// 读取某分支/提交下的 `tether.toml`（不检出）。
pub fn load_at(repo: &Path, rev: &str) -> Result<BranchConfig> {
    let text = crate::git::show(repo, rev, CONFIG_NAME)
        .with_context(|| format!("{rev} 缺少分支配置 {CONFIG_NAME}"))?;
    toml::from_str(&text).with_context(|| format!("解析 {rev}:{CONFIG_NAME} 失败"))
}

/// 创建 `.tetherignore`（若不存在；幂等）。
pub fn ensure_ignore_file(repo: &Path) -> Result<()> {
    let p = repo.join(IGNORE_NAME);
    if p.exists() {
        return Ok(());
    }
    let template = "\
# .tetherignore —— 真实文件夹中【不纳入影子】的排除清单（类似 .gitignore）。
# 每行一个 glob 模式；以 # 开头为注释。包含关系由分支表达，此处只做排除。
# 内置始终排除：TETHER.cert.toml、*.part、.git/
";
    fs::write(&p, template).with_context(|| format!("写入 {} 失败", p.display()))
}
