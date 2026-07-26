//! find-stutter-service — Windows 后台服务。
//!
//! 把原「GUI 内嵌 Collector」拆为独立 Windows 服务：
//! - 周期采集系统指标（CPU / 内存 / 磁盘 / 网络 / GPU / 温度）
//! - 卡顿检测 → 写入 `stutter.db`
//! - 每 tick 写心跳（`service_heartbeat` 表），让 GUI 探活
//!
//! 模块：
//! - [`cli`]       命令行参数（clap derive）
//! - [`service`]   服务循环 + windows-service 入口
//! - [`install`]   SCM 注册 / 卸载 / 启停 / 状态查询

pub mod cli;
pub mod install;
pub mod service;

pub use cli::{Cli, Commands};
pub use install::{ServiceStatusInfo, ScmError, ScmResult};
pub use service::{request_stop, run_foreground, run_scm, SERVICE_DISPLAY_NAME, SERVICE_NAME};
