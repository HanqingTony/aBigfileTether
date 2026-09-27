//! abigfiletether（tether）—— 非侵入式大文件影子追踪库。
//!
//! Git 只追踪影子元数据，真实文件原位不动、字节永不进 Git 对象库。
//! 设计规格见仓库 `docs/DESIGN.md`。

pub mod apply;
pub mod config;
pub mod git;
pub mod hash;
pub mod model;
pub mod propagate;
pub mod reorg;
pub mod scan;
pub mod shadow;
pub mod sync;
pub mod transport;
pub mod walk;
