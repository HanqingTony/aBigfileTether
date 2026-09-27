# aBigfileTether 设计文档 v0.1

非侵入式大文件"影子追踪"工具。Git 只存元数据，真实文件原位不动。
本仓库是**工具源码**仓库。

---

## 1. 定位与非目标

**是什么**：一个独立 CLI 工具。用 Git 仓库追踪一棵**同构的影子树**（每个真实文件一个
影子元数据文件），以 `size + blake3` 作为文件真实身份。工具提供两向显式同步：

- **扫描**：真实文件夹 → 仓库（把真实文件的增/删/改/移/复写成影子树变更并提交）。
- **应用**：仓库 → 真实文件夹（把影子树状态落成真实文件的增/删/改/移/复）。
- **传输**：在设备/仓库之间按内容身份搬运真实字节（自研协议，取代 rsync 清单）。

**非目标（红线）**：

- 绝不复制、搬移、软链真实文件；真实字节**永不进入 Git 对象库**。因此明确拒绝
  Git LFS / git-annex / DVC 的路线（它们都会把内容再拷一份或改软链）。
- 不做 Git 插件、不装 Git 钩子、不改变 Git 任何原生行为。用户可以完全正常地
  `git branch` / `git merge` / `git push`；本工具只按显式命令对齐两棵树。
- 不绑定设备。同一台设备可以管理多个文件夹（各自一个仓库）。

---

## 2. 核心概念

| 概念 | 定义 |
|---|---|
| **真实文件夹** | 被管理的目录（如 `~/zext`），含超大文件。工具从不修改它，除非显式 `apply`。 |
| **仓库** | 一个**外置**的普通 Git 仓库（位置任意，如 `~/zrepo/tether-zext`）。一个仓库 ↔ 一个真实文件夹，**不支持多根**。 |
| **影子文件** | 仓库中镜像真实树的文本文件（真实相对路径 + `.tether` 后缀），记录该文件的元数据。 |
| **身份** | `size + blake3(content)`。**路径只是位置**；移动/改名不改变身份。 |
| **证书** | 扫描时写入真实文件夹顶层的单文件 `TETHER.cert.toml`，含完整文件清单 + `root_hash`。 |
| **分支** | 仓库的普通 Git 分支 = 管理文件夹的某个**快照视图**。`main` 为全量；其它分支是子集。分支名不含设备名（仓库不绑定设备）。 |
| **base ref** | `refs/tether/base`，指向"真实文件夹当前所对应"的提交，用于 `apply` 的三方对齐。 |

**哈希策略（重要）**：扫描时先用**路径 + size 初筛**；路径与 size 都没变即视为未变，
**不重算哈希**。只有初筛发现变动（新增路径、消失路径、size 变化）的文件才做**全量
blake3**，再用 `size + hash` 确定真实身份。不做部分哈希，不掺时间（mtime 仅记录）。
已知取舍：同路径、同 size 但内容被改的文件不会被发现；后续可加 `--verify` 强制全量。

---

## 3. 仓库与文件布局

```
<repo>/                       # 外置普通 Git 仓库，位置任意
  .tetherignore               # 跟踪：本仓库是 tether 仓库的标记 + 排除规则（见 §4.4）
  mirrors/                    # 跟踪：影子树，镜像真实相对路径
    vllm/.../config.json.tether
    llamacpp/.../X.gguf.tether
  tether.toml                 # 不跟踪（gitignore）：本机专属（真实路径 + 对等端）
  .gitignore                  # 跟踪：至少含 tether.toml
~/zext/                       # 真实文件夹，除证书外一格不动
  TETHER.cert.toml            # 扫描产物：顶层证书
  ...真实大文件...
```

`tether.toml`（**gitignored，本机专属**）——真实路径因设备而异，绝不能入库：

```toml
# 本机管理哪个真实文件夹
path = "/home/tony/zext"

# 可选：本机标签（人读，不参与逻辑）
label = "zmain-hdd"

[peers]
# 对等端 = 对端的 tether 仓库（agent 会读取其 tether.toml 得到对端真实路径）
zmain = "tony@192.168.0.102:/home/tony/zrepo/tether-zext"

[transfer]
# 传输后端：system-ssh（默认）或 russh（需 `cargo build --features russh`）
backend = "system-ssh"
# key = "~/.ssh/id_ed25519"                    # russh 后端用的私钥
# remote_bin = "/home/tony/.local/bin/tether"  # 对端 tether 路径（默认走 PATH 的 tether）
```

---

## 4. 数据模型

统一使用 **TOML**（配置、影子、证书三处一致）。

### 4.1 影子文件 `<relpath>.tether`

```toml
content_hash = "blake3:af1349b9f5f9a1a6..."   # 完整 blake3，身份主键
size         = 34038894550
mtime        = "2026-06-17T03:03:00+08:00"    # 仅记录，不参与判断
last_seen    = "2026-09-27T14:00:00+08:00"
```

不存 `original_path`（路径即位置，改名 = Git rename）。

### 4.2 证书 `TETHER.cert.toml`

```toml
schema      = 1
scanned_at  = "2026-09-27T14:00:00+08:00"
count       = 335
total_size  = 1234567890123
root_hash   = "blake3:..."     # 对排序后的 (path, size, content_hash) 再哈希
[[files]]
path = "vllm/.../config.json"
size = 1234
content_hash = "blake3:..."
# ...完整清单...
```

### 4.3 本地配置 `tether.toml`

见 §3。`path` 与 `peers` 都随设备/机器不同，故整个文件 gitignored。

### 4.4 `.tetherignore`（需向你解释的设计点）

这是一个**类似 `.gitignore` 的排除清单**（每行一个 glob "模式"，如 `*.part`、
`tmp/**`、`.chunkcache/`），作用域是**真实文件夹里哪些文件不纳入影子**。与旧
`zext.include` 的区别：

- 旧系统用**包含清单**（`+ 行`）描述"要什么"，于是必须逐级写祖先目录、末尾放
  `- *` 哨兵、还要手工转义 `[ ] * ?` —— 这是主要痛点。
- 新系统**包含关系由分支表达**（分支里有影子就是纳入），`.tetherignore` 只做
  **少量排除**（临时文件、证书自身、传输中间文件）。排除远简单于包含，且用成熟的
  `ignore` crate 处理 Git-ignore 语义，无需自写转义/祖先行/哨兵。

`mode` 一词在本项目仅指这些 glob 模式；若你不想引入模式语言，可退化为"仅内置排除
（证书、`.part`、`.tether` 附属文件），不做用户自定义排除"。

---

## 5. 算法

### 5.1 扫描（真实 → 仓库）

```
prev   = 读 mirrors 树 → { relpath -> (size, content_hash) }
real   = 遍历真实文件夹，应用 .tetherignore 与内置排除（证书、.part 等）
changed = []
for f in real:
    if prev[f.path].size == f.size:      # 路径 + size 初筛命中 → 未变
        carry_forward(f)                # 不读内容，不重算哈希
    else:
        changed.push(f)                 # 需要全量哈希

for f in changed:
    f.hash = blake3(f)                  # 全量哈希
new_index = { f.path -> (f.size, f.hash) }   # 未变的沿用旧记录
old_by_hash = group(prev by hash)

# 身份分类（只看路径与哈希，不看时间）
旧路径消失 & 新路径出现同 hash          -> move     (git mv)
旧路径仍在 & 新路径出现同 hash          -> copy     (新增影子，同 hash)
同路径 hash 变                          -> modified
新路径 hash 谁都没有                    -> add
旧路径消失 & hash 谁都没有              -> delete

→ 用系统 git 落盘：git mv / git rm / 写影子 / git add，然后一次提交
→ 写证书 TETHER.cert.toml 到真实根
→ refs/tether/base 指向新提交
```

### 5.2 应用（仓库 → 真实）

```
base   = refs/tether/base
target = HEAD(或 --to <commit/branch>)
diff   = tree_diff(base, target)

对 diff 中每个条目映射为真实文件操作：
  add       -> 真实文件不存在则跳过（字节需靠 pull；apply 不凭空造字节）
               若真实已存在且 hash 匹配则只更新 base
  move      -> 真实 rename（执行前校验 hash）
  modified  -> 报冲突（真实内容与目标不符），不硬覆盖
  delete    -> 默认不删；--prune 时删除

默认 dry-run 打印计划；--yes 执行；成功后 refs/tether/base = target
```

### 5.3 传输（自研协议，取代 rsync）

**通道**：传输后端可插拔（`Connection` 抽象）。默认系统 `ssh`：客户端连接对端 sshd，
执行固定远端命令 `ssh -o BatchMode=yes <host> -- tether agent`（**命令行不含任何用户
路径**）；仓库路径由首个 `Hello` 消息经协议传给 agent，agent 再读自己的 `tether.toml`
得到真实路径。这样彻底规避 shell 转义问题。可选纯 Rust `russh` 后端（`--features
russh` + `[transfer] backend="russh"`），以私钥认证，行为/日志同样可控。
**对端需在 PATH 上有 `tether`。**

**分帧**：二进制帧（`postcard`/`bincode`），其中 `path` 是长度前缀的原始字节
（`Vec<u8>`），天然支持空格 / 中文 / `[` / 任意字节，无需转义。

**哈希感知搬运**（移动不全量拷贝的关键）：两端都能按内容哈希索引自己的真实树。

- push 前先向对端 `HAVE(hash, size)`；对端若在**别的路径**已有同 hash → 只发
  `MOVE`，**零字节传输**；否则才 `PUT`。
- 大文件：流式传输 + **断点续传**（按字节 offset）；未完成内容写 `.part`，校验通过
  后原子改名；收发两端流式算 blake3 校验。续传时本地重读已存在前缀以恢复哈希状态
  （只读本地磁盘，不重传网络）。数十 GB 文件可断可续。（并行/分块并发传输暂未做。）

**统一日志**：本地与远端 agent 发出同一套结构化事件（`INFO/WARN/PROGRESS/ERROR`
+ `key=value`），客户端统一渲染；`--log-json` 可机读。这是取代 rsync 日志的主要动机。

### 5.4 设备工作流（示例）

```
首次:      git clone <repo> → git checkout <该设备视图分支>
           写 tether.toml(path=~/zext)
           tether pull --from zmain      # 仅拉本分支影子清单对应的字节
新增模型:  下载 → tether scan → git commit
整理:      改名 / 合并文件夹 → tether scan   # 影子 git mv，零字节复制
汇总总库:  git checkout main → git merge <视图分支> → git push
           tether push --to zmain          # 仅传对端没有的 hash；已有则 MOVE
```

---

## 6. CLI 草案

```
tether init <real> [--repo <repo>]       # 初始化仓库/本地配置
tether scan [--yes] [--log-json]         # 真实 → 仓库（默认展示计划）
tether apply [--to <ref>] [--prune] [--yes]  # 仓库 → 真实（默认 dry-run）
tether status                            # 展示真实与影子树的差异
tether reorg --map <file> [--real] [--yes]   # 按已知映射移动影子（可选真实），零哈希
tether propagate --from <ref> [--onto <ref>] [--yes]  # 并回 A/M/R，丢弃 D
tether cert [--verify]                   # 生成/校验证书与 root_hash
tether pull --from <peer> [--prune]      # 从对端补齐本快照缺失的字节
tether push --to <peer> [--prune]        # 把本快照推给对端（哈希感知，只传新字节）
tether agent [--repo <repo>]             # 远端被 ssh 调用，走 stdio 协议（内部）
```

所有命令支持 `-h/--help`。

**整理目录结构时用 `reorg`（元数据优先、零哈希），不要"先动真实再 scan"**——后者会为每个新
路径重新哈希全部内容。`reorg` 按已知映射 `git mv` 影子（`--real` 同时移动真实文件）。

---

## 7. 旧系统（`zscript/deploy/*/z-zext`）坑对照

| 旧坑 | 新设计如何消掉 |
|---|---|
| rsync 模式语言 + 手工转义 `[ ] * ?` | 路径即文件系统路径，Git 存树；传输走原始字节帧 |
| 祖先目录逐级 `+ dir/`、`- *` 哨兵、插入顺序 | Git 树天然表达层级；包含关系由分支表达 |
| 无内容身份，路径即一切 | `size + blake3` 为身份，移动只改影子路径 |
| 改名/移动 = 删旧行 + 加新行 → 全量重传 | 哈希索引匹配，可零字节 `MOVE` |
| 设备子集 = 多份硬编码 include 清单 | 普通 Git 分支 |
| 路径硬编码进脚本（`$HOME/zext`、`/mnt/data/zext`） | gitignored 本地 `tether.toml` |
| 预检/进度解析 rsync 输出，filter 下不准 | 自研传输 + 统一结构化日志 |
| GNU `find -printf`，不跨平台 | Rust 跨平台，含 Windows |
| 传输按 size/mtime 判等，无内容校验 | 收发两端 blake3 校验 |
| 删除语义散在 checkout/fetch `--delete` | 集中在 `apply --prune` / `pull --prune` |

---

## 8. 工具链与依赖（拟）

- Rust，edition 2024（本机 rustc 1.98）。
- 依赖候选：`clap`(CLI)、`blake3`(哈希)、`serde`+`toml`(模型)、`walkdir`(遍历)、
  `ignore`(排除规则)、`rayon`(并行哈希)、`anyhow`(错误)、`bincode`(协议帧)；
  `russh`+`tokio` 为**可选**（`--features russh`）的纯 Rust SSH 后端。
- 跨平台：无 inode、无权限位、无 POSIX 专属路径假设；路径按 OS 原生字节处理。

## 9. 待定 / 后续

- 并行传输、单文件分块并发（当前为单流按 offset 续传）。
- `apply` 遇到"真实与目标不一致"的冲突策略细化。
- 多对等端、限速。
- 未来作为整个资料库的底层基建时的扩展点（保留设计余地，但当前不做多根）。
