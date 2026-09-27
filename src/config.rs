//! 本地配置 `tether.toml`（gitignored，本机专属）与 `.tetherignore`。
//!
//! 见 `docs/DESIGN.md` §3、§4.3、§4.4。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// 本机专属配置文件名（必须 gitignored）。
pub const CONFIG_NAME: &str = "tether.toml";
/// 排除清单文件名（跟踪；同时作为"本仓库是 tether 仓库"的标记）。
pub const IGNORE_NAME: &str = ".tetherignore";
/// 仓库自带 .gitignore 名。
pub const GITIGNORE_NAME: &str = ".gitignore";

/// 传输后端配置（`[transfer]`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransferConfig {
    /// 后端：`system-ssh`（默认）或 `russh`（需 `--features russh`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// russh 后端的私钥路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// 对端上 tether 的路径（默认 `tether`）。当对端不在默认 PATH 时用绝对路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_bin: Option<String>,
}

/// 本机专属配置；`path` 与 `peers` 随机器不同，故整个文件 gitignored。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalConfig {
    /// 被管理的真实文件夹路径。
    pub path: String,
    /// 人读标签（不参与逻辑）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// 对等端：名字 -> `user@host:/path`。
    #[serde(default)]
    pub peers: BTreeMap<String, String>,
    /// 传输后端（缺省 system-ssh）。
    #[serde(default)]
    pub transfer: TransferConfig,
}

impl LocalConfig {
    /// 真实文件夹绝对路径。
    pub fn path_buf(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }

    /// 相对路径相对仓库根解析，并尽量规范化（解析软链）。
    pub fn resolve_real(&self, repo: &Path) -> PathBuf {
        let p = self.path_buf();
        let p = if p.is_absolute() { p } else { repo.join(p) };
        p.canonicalize().unwrap_or(p)
    }
}

/// 载入 `tether.toml`。
pub fn load(repo: &Path) -> Result<LocalConfig> {
    let p = repo.join(CONFIG_NAME);
    let text = fs::read_to_string(&p)
        .with_context(|| format!("缺少本机配置（{}）；先运行 `tether init`", p.display()))?;
    toml::from_str(&text).with_context(|| format!("解析配置失败：{}", p.display()))
}

/// 写入 `tether.toml`。
pub fn save(repo: &Path, cfg: &LocalConfig) -> Result<()> {
    let text = toml::to_string_pretty(cfg).context("序列化本机配置失败")?;
    fs::write(repo.join(CONFIG_NAME), text)
        .with_context(|| format!("写入配置失败：{}", repo.display()))
}

/// 确保 `.gitignore` 含 `tether.toml`（幂等）。
pub fn ensure_gitignore(repo: &Path) -> Result<()> {
    let p = repo.join(GITIGNORE_NAME);
    let mut text = fs::read_to_string(&p).unwrap_or_default();
    let has = text
        .lines()
        .any(|l| l.trim() == CONFIG_NAME || l.trim() == "/tether.toml");
    if !has {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("# 本机专属配置（真实文件夹路径 / 对等端），绝不入库\n");
        text.push_str(CONFIG_NAME);
        text.push('\n');
        fs::write(&p, text).with_context(|| format!("写入 {} 失败", p.display()))?;
    }
    Ok(())
}

/// 创建 `.tetherignore`（若不存在，写入带说明的模板；幂等）。
pub fn ensure_ignore_file(repo: &Path) -> Result<()> {
    let p = repo.join(IGNORE_NAME);
    if p.exists() {
        return Ok(());
    }
    let template = "\
# .tetherignore —— 真实文件夹中【不纳入影子】的排除清单（类似 .gitignore）。
# 每行一个 glob 模式；以 # 开头为注释。包含关系由 Git 分支表达，此处只做排除。
# 内置始终排除：TETHER.cert.toml、*.part、.git/
";
    fs::write(&p, template).with_context(|| format!("写入 {} 失败", p.display()))
}
