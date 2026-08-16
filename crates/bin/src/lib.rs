//! find-stutter 入口 crate（ADR-0001 决策 1：bin 只做分发）。
//!
//! - 无参数（及 `run`）= 启动 GUI：`find_stutter_ui::run()`（内部 auto_start
//!   自动确保服务在跑，UAC 由系统弹出）；
//! - 子命令 = 转发 CLI：`events` / `samples` / `analysis` / `config` /
//!   `status` / `process` / `export` / `upgrade`。
//!
//! 本 crate 不承载任何查询逻辑——clap 定义、JSON 输出、升级编排全部在
//! `find-stutter-cli`（便于独立测试、不被 GUI 依赖树拖累）。

pub use find_stutter_cli::{
    dispatch, parse_args, try_parse_args_from, Cli, Commands, DispatchOutcome,
};
