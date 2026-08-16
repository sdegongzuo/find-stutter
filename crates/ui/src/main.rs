//! （已废弃的重复入口，待手动删除）
//!
//! ADR-0001 后 GUI 唯一入口收敛为 `crates/bin` 的 `find-stutter`（链接
//! `find_stutter_ui::run()`）；ui crate 的 Cargo.toml 已设 `autobins = false`，
//! **本文件不再参与构建**、不会产出 `find-stutter-ui.exe`。
//! 文件按「严禁删除」约定暂时保留，确认后可手动删除。
//!
//! 历史用途：直接 `cargo run -p find-stutter-ui` 时的独立 bin 入口
//! （release 用 windows_subsystem 隐藏控制台，debug 保留控制台看日志）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use find_stutter_ui::run;

fn main() -> anyhow::Result<()> {
    // 日志由 find_stutter_ui::run() 内部 init（用 try_init 容忍重复），
    // 这里不再 init，否则 lib.rs 二次 init 会 panic。
    run()
}
