//! 薄 re-export 层（ADR-0001）。
//!
//! 卡顿分析的聚合查询与根因纯函数（F-RC5~F-RC16）已下沉到
//! `find_stutter_core::analytics`，UI 与 CLI 共用同一份分析口径。
//! 本文件保留 `crate::analytics::...` 的旧路径兼容：UI 内部大量
//! `use crate::analytics::...` 引用无需改动，直接转发到 core。
//!
//! 若未来出现确有 GUI 依赖的分析辅助（如 slint 模型转换），应放在
//! ui 侧的其他模块，而不是把逻辑搬回这里。

pub use find_stutter_core::analytics::*;
