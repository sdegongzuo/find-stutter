//! find-stutter 主入口（ADR-0001 决策 1 / 5）。
//!
//! 一条命令收口：
//! - **无参数（及 `run`）= GUI**：悬浮窗 + 自动确保服务（UAC 系统弹出）；
//! - **子命令 = CLI**：面向 agent 的 JSON 查询六件套 + export（CSV）+ upgrade。
//!
//! Stats 子命令已按 ADR-0001 删除（被 `events --from <今天零点>` 覆盖）。

fn main() -> anyhow::Result<()> {
    // 日志由 find_stutter_ui::run() 内部 init（用 try_init 容忍重复），
    // 这里不再 init，否则 lib.rs 二次 init 会 panic。
    let cli = find_stutter::parse_args();

    match find_stutter::dispatch(&cli)? {
        find_stutter::DispatchOutcome::LaunchGui => {
            // release 下启动 GUI 前解除控制台关联（不弹黑框）；debug 保留控制台便于看日志。
            // 只对 GUI 分支生效，CLI 子命令仍需控制台输出，不受影响。
            find_stutter_ui::window::hide_console_for_gui();
            find_stutter_ui::run()
        }
        find_stutter::DispatchOutcome::Done => Ok(()),
    }
}
