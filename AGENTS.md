# AGENTS.md

`abigfiletether` (working name **tether**) — a Rust CLI that manages huge files by
tracking only **shadow metadata** in a plain Git repo. Real files stay in place;
their bytes **never** enter the Git object store.

Canonical spec: **`docs/DESIGN.md`** — read it before changing behavior.

## Hard constraints (do NOT violate)

- **Non-intrusive / no byte duplication.** Never move, rename, copy, or symlink a
  real file, and never let real bytes enter the Git object store. This is *why*
  Git LFS / git-annex / DVC are rejected. Do not introduce LFS, annex-style
  symlinks, `.gitattributes` LFS filters, or any scheme that re-copies managed
  bytes. (This repo is not an LFS repo; keep it that way.)
- **Plain Git only.** The shadow repo is an ordinary repo. Do not add hooks,
  filters, plugins, or alter Git behavior. Users may freely `git branch/merge/push`.
  The tool only reads/commits shadow metadata on explicit commands.
- **Identity = `size + blake3(content)`.** Path is only a location. `mtime` is
  recorded but never used for decisions. No partial hashes. During scan, initial
  filter is **path + size**; only files whose path or size changed get hashed.
- **Cross-platform incl. Windows.** No inode, no Unix permission bits, no
  POSIX-only path assumptions. Paths are OS-native byte strings.
- **Central model.** ONE Git repo (on the mothership host) holds every branch's
  shadows. Each branch's **tracked** `tether.toml` carries its `location`
  (`local:/path` or `[user@]host:/path`, from the mothership's view); `git checkout`
  switches the location. The program lives once on the mothership; the remote
  `agent` is the same binary (stateless, rooted at `Hello.root`), placed inside the
  repo tree and **gitignored**.
- **`apply` never fabricates or overwrites real bytes**; it deletes nothing unless
  `--prune`. Byte transfer is only `distribute` (mothership→device) / `ingest`
  (device→mothership), hash-aware and resumable.

## Commands

- Build: `cargo build`
- Run: `cargo run -- <args>`
- Test: `cargo test`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format: `cargo fmt`
- Optional pure-Rust SSH backend: `cargo build --features russh`, then set
  `[transfer] backend = "russh"`. Default backend is system `ssh`.

## Layout

- `src/config.rs` — per-branch `tether.toml` (tracked) with `location`; `.tetherignore`.
- `src/fs.rs` — `Fs` abstraction: `LocalFs` / `RemoteFs` (over the agent).
- `src/scan.rs` — location→repo; `refs/tether/base` tracks the commit real matches.
- `src/apply.rs` — repo→location; `--prune` deletes extras vs the target snapshot.
- `src/reorg.rs` — apply a known path mapping to shadows (and `--real`); **zero hashing**.
  Use this to reorganize structure instead of moving real files then `scan`.
- `src/propagate.rs` — merge a branch's A/M/R onto another, **dropping D** (merge-safe).
- `src/inventory.rs` — `stocktake` (main↔branch existence diff) + `retail` (copy a
  file/folder's shadows from main into the current branch; bytes come via `distribute`).
- `src/transport.rs` — framed protocol + stateless `tether agent` (system `ssh` pipe).
- `src/sync.rs` — `distribute`/`ingest`; hash-aware MOVE/COPY, resumable.
- `tests/{smoke,sync}.rs` — end-to-end on temp dirs via `LocalFs`; never touch real data.

Tests use `LocalFs` directly (no subprocess/SSH); the agent wire protocol is not
covered by the test suite — validate it manually against a real host.

## Reference

- `docs/DESIGN.md` — canonical design (data model, algorithms, CLI).
- Predecessor to learn from (NOT to copy): `../zscript/deploy/{zmain,zlapwsl,zlapdeb}/z-zext`
  (rsync + `zext.include` manifest) and `../zscript/deploy/*/install/git-annex.sh`.
  Its pitfalls are catalogued in `docs/DESIGN.md` §7.
- `../zscript/AGENTS.md` — house conventions (Chinese comments, structured blocks,
  idempotent scripts). This repo follows the same spirit.

## Conventions

- Code comments in **Chinese** (matches the zscript house style); keep them minimal.
- Do not commit unless explicitly asked.
- Never commit secrets. The agent binary lives in the repo tree (`.tether/`) and is
  **gitignored**; `tether.toml` is **tracked** (branch config) and MUST be committed.

## Git workflow

- Working branch `main`.
- `origin` has two `pushurl`s (GitHub + Gitee): a plain `git push` publishes to
  BOTH; fetch comes from GitHub only. Restore config with
  `bash setup-dual-remote.sh`; verify with `git remote -v | grep push` (two lines).
