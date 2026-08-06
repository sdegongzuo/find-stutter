//! find-stutter-ui 可执行入口
//!
//! 主要的 `find-stutter` CLI 见 `crates/bin`；这个 bin 仅在直接 cargo run 该 crate 时使用。
//!
//! ## 控制台窗口策略
//!
//! - **release**：Windows 子系统（`windows_subsystem = "windows"`），启动不弹黑框命令行窗口。
//!   注意：无控制台时 `env_logger` 的 stderr 输出不可见，release 日志默认丢弃。
//! - **debug**：默认控制台子系统，`cargo run` 可看日志。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use find_stutter_ui::run;

fn main() -> anyhow::Result<()> {
    // 日志由 find_stutter_ui::run() 内部 init（用 try_init 容忍重复），
    // 这里不再 init，否则 lib.rs 二次 init 会 panic。
    run()
}
