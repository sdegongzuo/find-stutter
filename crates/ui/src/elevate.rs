//! 薄 re-export 层（ADR-0001）。
//!
//! UAC 提权封装已下沉到 `find_stutter_core::elevate`（无界面依赖，
//! ui 的 auto_start 与 cli 的 upgrade 共用同一实现，避免两份同构漂移）。
//! 本文件保留 `crate::elevate::...` 的旧路径兼容：`auto_start.rs` 等
//! ui 内部引用无需改动。

pub use find_stutter_core::elevate::*;
