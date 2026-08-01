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

/// 把 message 追加到 binary 同目录的 `find-stutter-service.diag.log`（用于 SCM 启动调试）
///
/// SCM 启动 service 时没有 console，stderr 看不到。我们把所有关键节点 + panic
/// 写到 diag log，便于排查"service 启动后几秒就 Stopped"类问题。
fn diag_log(msg: &str) {
    use std::io::Write;
    let dir = match std::env::current_exe() {
        Ok(me) => me.parent().map(|p| p.to_path_buf()),
        Err(_) => None,
    };
    let path = match dir {
        Some(d) => d.join("find-stutter-service.diag.log"),
        None => return,
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(
            f,
            "[{}] [{}] {}",
            chrono::Utc::now().to_rfc3339(),
            std::process::id(),
            msg
        );
    }
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    diag_log(&format!("main() entered, args={:?}", std::env::args().collect::<Vec<_>>()));

    // panic hook：把 panic 写 diag log
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };
        let loc = if let Some(l) = info.location() {
            format!(" ({})", l)
        } else {
            String::new()
        };
        diag_log(&format!("PANIC: {}{}", msg, loc));
        prev_hook(info);
    }));

    let cli = Cli::parse();
    diag_log(&format!("subcommand: {:?}", cli.command));

    match cli.command {
        None => {
            // 无子命令 = 被 SCM 拉起的 Windows 服务模式。
            // SCM 启动服务时 launch_arguments 为空，不会传任何参数。
            // （此前用必填 subcommand，SCM 无参启动直接 clap 报错退出，
            //   服务永远起不来 —— 表现为 GUI 显示「服务已停止」。）
            diag_log("No subcommand: entering SCM service mode");
            service::run_scm()
        }
        Some(Commands::Run) => {
            diag_log("Run: setting up ctrl-c handler");
            // Ctrl-C 也让循环退出
            let _ = ctrlc::set_handler(|| {
                log::info!("Ctrl-C received, stopping...");
                service::request_stop();
            });
            let config = Config::load(&cli.config).unwrap_or_else(|e| {
                log::warn!("config load failed ({}), using defaults", e);
                diag_log(&format!("Run: config load failed: {}", e));
                Config::default()
            });
            diag_log(&format!(
                "Run: config loaded, db={}, interval={}ms",
                config.storage.db_path, config.sampling.interval_ms
            ));
            let r = service::run_foreground(config);
            diag_log(&format!("Run: foreground returned: {:?}", r));
            r
        }

        Some(Commands::Install) => {
            diag_log("Install: start");
            install::install().map_err(anyhow::Error::from)?;
            println!("Service installed: {}", service::SERVICE_NAME);
            diag_log("Install: success");
            Ok(())
        }

        Some(Commands::Uninstall) => {
            install::uninstall().map_err(anyhow::Error::from)?;
            println!("Service uninstalled: {}", service::SERVICE_NAME);
            Ok(())
        }

        Some(Commands::Start) => {
            install::start().map_err(anyhow::Error::from)?;
            println!("Service start requested");
            Ok(())
        }

        Some(Commands::Stop) => {
            install::stop().map_err(anyhow::Error::from)?;
            println!("Service stop requested");
            Ok(())
        }

        Some(Commands::InstallStart) => {
            // 给 GUI 端 UAC 一次提权完成 install + start
            // install 内部已包含「已注册时 stop + start」逻辑，
            // 所以这里不用再调 start。
            install::install().map_err(anyhow::Error::from)?;
            println!("Service installed and started: {}", service::SERVICE_NAME);
            Ok(())
        }

        Some(Commands::Status) => {
            let s = install::status().map_err(anyhow::Error::from)?;
            println!(
                "{} ({})",
                install::status_to_string(&s),
                service::SERVICE_NAME
            );
            // 退出码协议（GUI 端 auto_start 会读这个）：
            //   0 = Running（服务在跑）
            //   1 = Stopped（已注册但未跑 / Pending / Paused 等「存在但不可用」状态）
            //   2 = NotFound（服务未注册，需要 install + runas 提权）
            //   3 = Error（其他 SCM 错误）
            let code = match s {
                ServiceStatusInfo::Running => 0,
                ServiceStatusInfo::NotFound => 2,
                ServiceStatusInfo::Stopped
                | ServiceStatusInfo::StartPending
                | ServiceStatusInfo::StopPending
                | ServiceStatusInfo::ContinuePending
                | ServiceStatusInfo::PausePending
                | ServiceStatusInfo::Paused => 1,
            };
            // 直接退出，跳过 result 包装
            std::process::exit(code);
        }
    }
}
