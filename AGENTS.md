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
- **One repo ↔ one managed folder.** No multi-root. The machine-specific path
  lives in `tether.toml`, which MUST be gitignored.
- **`apply` never fabricates or overwrites real bytes**; it deletes nothing unless
  `--prune`. Real byte transfer is only `push`/`pull`.

## Commands

- Build: `cargo build`
- Run: `cargo run -- <args>`
- Test: `cargo test`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format: `cargo fmt`
- Optional pure-Rust SSH backend: `cargo build --features russh`, then set
  `[transfer] backend = "russh"`. Default backend is system `ssh`.

## Layout

- `src/git.rs` — all Git access shells out to system `git` (no libgit2, no hooks).
- `src/scan.rs` — real→repo; `refs/tether/base` tracks the commit real matches.
- `src/apply.rs` — repo→real; `--prune` deletes extras vs the target snapshot.
- `src/reorg.rs` — apply a known path mapping to shadows (and `--real`); **zero hashing**.
  Use this to reorganize structure instead of moving real files then `scan`.
- `src/propagate.rs` — merge a branch's A/M/R onto another, **dropping D** (merge-safe).
- `src/transport.rs` — framed protocol + `tether agent` (system `ssh` is the pipe).
- `src/sync.rs` — `push`/`pull`; hash-aware: MOVE/COPY instead of retransfer.
- `tests/{smoke,sync}.rs` — end-to-end on temp dirs; never touch real data.

Tests spawn the built binary as a local `tether agent`, so they exercise the real
wire protocol without SSH.

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
- Never commit secrets or machine-specific paths (`tether.toml` is gitignored).

## Git workflow

- Working branch `main`.
- `origin` has two `pushurl`s (GitHub + Gitee): a plain `git push` publishes to
  BOTH; fetch comes from GitHub only. Restore config with
  `bash setup-dual-remote.sh`; verify with `git remote -v | grep push` (two lines).
