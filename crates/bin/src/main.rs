//! find-stutter 主入口（ADR-0001 决策 1 / 5）。
//!
//! 一条命令收口：
//! - **无参数（及 `run`）= GUI**：悬浮窗 + 自动确保服务（UAC 系统弹出）；
//! - **子命令 = CLI**：面向 agent 的 JSON 查询六件套 + export（CSV）+ upgrade。
//!
//! Stats 子命令已按 ADR-0001 删除（被 `events --from <今天零点>` 覆盖）。
//!
//! ## 为什么不闪黑框（2026-10-28）
//!
//! release 编成 **GUI 子系统**（下面的 `windows_subsystem` 属性）。控制台子系统
//! （默认）会在进程创建的瞬间就由 Windows 分配一个控制台窗口，
//! `hide_console_for_gui()` 里的 `FreeConsole` 只是事后销毁——用户已经看见闪一下了。
//! 改成 GUI 子系统后进程根本不分配控制台，黑框从源头消失。
//!
//! 代价：GUI 子系统不会自动继承父控制台，CLI 子命令（events / status / export /
//! upgrade）需要先 `AttachConsole(ATTACH_PARENT_PROCESS)` 才能打印，见下方
//! [`find_stutter_ui::window::attach_parent_console_for_cli`]。
//! 注意副作用：cmd 启动 GUI 子系统程序不会等它退出，输出会回到提示符之后
//! （管道 / 重定向场景不受影响，脚本里请用 `start /wait`）。
//!
//! debug 构建保持控制台子系统（开发时要看 panic / 日志）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// A/B 实验：mimalloc 全局分配器（--features mimalloc 启用，默认关）。
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    // CLI 子命令需要控制台输出：GUI 子系统下先挂回父控制台（已被重定向时不动）。
    // `run` / 无参数走 GUI 分支，不需要也不该挂控制台。
    let mut args = std::env::args_os().skip(1);
    if let Some(first) = args.next() {
        if first != "run" {
            find_stutter_ui::window::attach_parent_console_for_cli();
        }
    }

    // 日志由 find_stutter_ui::run() 内部 init（用 try_init 容忍重复），
    // 这里不再 init，否则 lib.rs 二次 init 会 panic。
    let cli = find_stutter::parse_args();

    match find_stutter::dispatch(&cli)? {
        find_stutter::DispatchOutcome::LaunchGui => {
            // debug 构建仍是控制台子系统，启动 GUI 前解除控制台关联（不弹黑框）；
            // release 已是 GUI 子系统，这里是幂等的空操作。
            find_stutter_ui::window::hide_console_for_gui();
            find_stutter_ui::run()
        }
        find_stutter::DispatchOutcome::Done => Ok(()),
    }
}
