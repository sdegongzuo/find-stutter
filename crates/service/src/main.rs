//! find-stutter-service CLI 入口。
//!
//! 子命令：
//!   run         前台运行服务循环（开发/调试，不注册 SCM）
//!   install     注册为 Windows 服务
//!   uninstall   从 SCM 卸载
//!   start       启动已注册的服务
//!   stop        停止已运行的服务
//!   status      打印服务状态

use clap::Parser;
use find_stutter_core::Config;
use find_stutter_service::{cli::Cli, cli::Commands, install, service, ServiceStatusInfo};

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Run => {
            // Ctrl-C 也让循环退出
            let _ = ctrlc::set_handler(|| {
                log::info!("Ctrl-C received, stopping...");
                service::request_stop();
            });
            let config = Config::load(&cli.config).unwrap_or_else(|e| {
                log::warn!("config load failed ({}), using defaults", e);
                Config::default()
            });
            service::run_foreground(config)
        }

        Commands::Install => {
            install::install().map_err(anyhow::Error::from)?;
            println!("Service installed: {}", service::SERVICE_NAME);
            Ok(())
        }

        Commands::Uninstall => {
            install::uninstall().map_err(anyhow::Error::from)?;
            println!("Service uninstalled: {}", service::SERVICE_NAME);
            Ok(())
        }

        Commands::Start => {
            install::start().map_err(anyhow::Error::from)?;
            println!("Service start requested");
            Ok(())
        }

        Commands::Stop => {
            install::stop().map_err(anyhow::Error::from)?;
            println!("Service stop requested");
            Ok(())
        }

        Commands::Status => {
            let s = install::status().map_err(anyhow::Error::from)?;
            println!(
                "{} ({})",
                install::status_to_string(&s),
                service::SERVICE_NAME
            );
            // 退出码：服务在跑 = 0，否则非 0（便于脚本判断）
            let code = if matches!(s, ServiceStatusInfo::Running) { 0 } else { 1 };
            // 直接退出，跳过 result 包装
            std::process::exit(code);
        }
    }
}
