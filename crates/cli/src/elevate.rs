//! 薄 re-export 层（ADR-0001）。
//!
//! UAC 提权封装已下沉到 `find_stutter_core::elevate`（与 ui 的 auto_start
//! 共用同一实现）。本 crate 的 `upgrade` 模块经 `crate::elevate::...` 引用。
//!
//! 注意：CLI 中唯一允许提权的路径是 `upgrade` 子命令（ADR-0001 决策 6 对
//! 决策 4「CLI 不做提权控制」的明确例外），查询类子命令不得调用。

pub use find_stutter_core::elevate::*;
