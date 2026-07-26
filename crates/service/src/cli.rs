//! CLI 定义：clap derive。
//!
//! 子命令：
//! - `run`         前台运行服务循环（开发/调试用，不注册 SCM）
//! - `install`     注册 Windows 服务（需管理员）
//! - `uninstall`   卸载 Windows 服务
//! - `start`       启动已注册的服务
//! - `stop`        停止已注册的服务
//! - `status`      查询服务状态（Stopped / StartPending / Running / ...）

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "find-stutter-service",
    about = "find-stutter 后台采集服务（Windows service）",
    long_about = "在后台持续采集系统指标并写入 stutter.db。\n\
                  GUI（find-stutter-overlay）只读此数据库，不做采集。"
)]
pub struct Cli {
    /// 配置文件路径（默认 ./config.toml）
    #[arg(long, global = true, default_value = "config.toml")]
    pub config: String,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// 前台运行服务循环（开发/调试；不注册为 Windows 服务）
    Run,

    /// 注册为 Windows 服务（需管理员权限；写入 SCM）
    Install,

    /// 从 SCM 卸载 Windows 服务
    Uninstall,

    /// 启动已注册的 Windows 服务
    Start,

    /// 停止已运行的 Windows 服务
    Stop,

    /// 打印服务当前状态
    Status,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_run_subcommand() {
        let cli = Cli::try_parse_from(["find-stutter-service", "run"]).unwrap();
        assert!(matches!(cli.command, Commands::Run));
        assert_eq!(cli.config, "config.toml");
    }

    #[test]
    fn parse_install_subcommand() {
        let cli = Cli::try_parse_from(["find-stutter-service", "install"]).unwrap();
        assert!(matches!(cli.command, Commands::Install));
    }

    #[test]
    fn parse_uninstall_subcommand() {
        let cli = Cli::try_parse_from(["find-stutter-service", "uninstall"]).unwrap();
        assert!(matches!(cli.command, Commands::Uninstall));
    }

    #[test]
    fn parse_start_subcommand() {
        let cli = Cli::try_parse_from(["find-stutter-service", "start"]).unwrap();
        assert!(matches!(cli.command, Commands::Start));
    }

    #[test]
    fn parse_stop_subcommand() {
        let cli = Cli::try_parse_from(["find-stutter-service", "stop"]).unwrap();
        assert!(matches!(cli.command, Commands::Stop));
    }

    #[test]
    fn parse_status_subcommand() {
        let cli = Cli::try_parse_from(["find-stutter-service", "status"]).unwrap();
        assert!(matches!(cli.command, Commands::Status));
    }

    #[test]
    fn parse_with_custom_config() {
        let cli = Cli::try_parse_from([
            "find-stutter-service",
            "--config",
            "D:/etc/find-stutter.toml",
            "run",
        ])
        .unwrap();
        assert_eq!(cli.config, "D:/etc/find-stutter.toml");
        assert!(matches!(cli.command, Commands::Run));
    }

    #[test]
    fn parse_missing_subcommand_fails() {
        let result = Cli::try_parse_from(["find-stutter-service"]);
        assert!(result.is_err(), "必须提供子命令");
    }

    #[test]
    fn parse_unknown_subcommand_fails() {
        let result = Cli::try_parse_from(["find-stutter-service", "explode"]);
        assert!(result.is_err());
    }
}
