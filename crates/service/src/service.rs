//! 服务循环 + Windows service 入口。
//!
//! 设计：
//! - `run_foreground(config)`：在前台跑主循环（开发/调试 / `Run` 子命令用）。
//! - `run_scm()`：通过 `service_dispatcher::start` 让 SCM 调用我们
//!   （仅 Windows，且只在「已被 SCM 启动」时成功；非 SCM 上下文返回错误，
//!   提示用 `Run` 子命令）。
//!
//! 主循环：每秒 tick
//! 1. `Collector::collect()` 抓一次系统指标
//! 2. `Detector::analyze()` 检测卡顿
//! 3. `Logger::touch_heartbeat()` 写心跳（GUI 用此探活）
//! 4. `Logger::write_sample(&sample)` 缓冲写库
//! 5. 检测到事件 → `Logger::write_event(&event)`
//! 6. 每 10 ticks 调 `flush()` 刷盘
//! 7. 每 3600 ticks（约 1 小时）调 `cleanup()` 清理过期数据

use find_stutter_core::{Collector, Config, Detector, Logger};
use log::{error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[cfg(windows)]
use std::ffi::OsString;
#[cfg(windows)]
use windows_service::define_windows_service;
#[cfg(windows)]
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
    ServiceType,
};
#[cfg(windows)]
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
#[cfg(windows)]
use windows_service::service_dispatcher;

/// SCM 注册的服务名（也用于查询 / 启停）
pub const SERVICE_NAME: &str = "FindStutter";

/// SCM 显示名（用户友好）
pub const SERVICE_DISPLAY_NAME: &str = "Find Stutter Monitor";

static RUNNING: AtomicBool = AtomicBool::new(true);

/// 前台运行服务循环（开发 / 调试 / `Run` 子命令）。
///
/// 阻塞当前线程，按 `config.sampling.interval_ms` 周期采集。
pub fn run_foreground(config: Config) -> anyhow::Result<()> {
    info!(
        "find-stutter-service starting (interval={}ms, db={})",
        config.sampling.interval_ms, config.storage.db_path
    );

    let mut collector = Collector::new();
    let mut detector = Detector::new(&config.detection);
    let mut logger = Logger::new(&config.storage)?;

    // 启动时立即写一次心跳，让 GUI 一启动就能看到「服务在跑」
    if let Err(e) = logger.touch_heartbeat() {
        warn!("initial touch_heartbeat failed: {}", e);
    }

    let tick = Duration::from_millis(config.sampling.interval_ms);
    let mut count: u64 = 0;

    while RUNNING.load(Ordering::SeqCst) {
        count += 1;

        let sample = collector.collect();

        // 心跳：每 tick 一次，GUI 探活用
        if let Err(e) = logger.touch_heartbeat() {
            warn!("touch_heartbeat failed: {}", e);
        }

        if let Some(event) = detector.analyze(&sample) {
            info!(
                "stutter detected: {:?} — {}",
                event.severity,
                event.causes.join(", ")
            );
            if let Err(e) = logger.write_event(&event) {
                warn!("write_event failed: {}", e);
            }
        }

        if let Err(e) = logger.write_sample(&sample) {
            warn!("write_sample failed: {}", e);
        }

        if count % 10 == 0 {
            if let Err(e) = logger.flush() {
                warn!("flush failed: {}", e);
            }
        }
        if count % 3600 == 0 {
            if let Err(e) = logger.cleanup() {
                warn!("cleanup failed: {}", e);
            }
        }

        std::thread::sleep(tick);
    }

    // 退出前 flush + cleanup，保证数据不丢
    logger.flush()?;
    logger.cleanup()?;
    info!("find-stutter-service stopped");
    Ok(())
}

/// 通知主循环退出（Ctrl-C / SCM Stop 都会调它）
pub fn request_stop() {
    RUNNING.store(false, Ordering::SeqCst);
}

// =====================================================================
// Windows SCM 集成
// =====================================================================

#[cfg(windows)]
define_windows_service!(ffi_service_main, scm_service_entry);

#[cfg(windows)]
fn scm_service_entry(_arguments: Vec<OsString>) {
    if let Err(e) = scm_loop() {
        error!("SCM service entry failed: {}", e);
    }
}

/// SCM 入口：被 SCM 启动时调用。
/// 注册 control handler → 把状态推到 Running → 跑主循环 → Stop 时退出。
#[cfg(windows)]
fn scm_loop() -> anyhow::Result<()> {
    let handler = service_control_handler::register(SERVICE_NAME, move |control_event| {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                info!("SCM Stop/Shutdown received");
                request_stop();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    })?;

    handler.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    let config = Config::load("config.toml").unwrap_or_else(|e| {
        warn!("config load failed ({}), using defaults", e);
        Config::default()
    });

    let result = run_foreground(config);

    // 把状态推到 Stopped
    handler.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: if result.is_ok() {
            ServiceExitCode::NO_ERROR
        } else {
            ServiceExitCode::ServiceSpecific(1)
        },
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    result
}

/// 入口：SCM 启动我们。
///
/// 只有在「被 SCM 启动」（即作为服务跑）的上下文中才能成功；
/// 终端里直接 `find-stutter-service.exe run` 调到这里会失败，
/// 因此失败时 fallback 到 `run_foreground`（便于开发 / 调试）。
#[cfg(windows)]
pub fn run_scm() -> anyhow::Result<()> {
    match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => Ok(()),
        Err(_) => {
            // 不是 SCM 上下文：fallback 到前台运行
            info!("Not in SCM context, falling back to foreground");
            let config = Config::load("config.toml").unwrap_or_default();
            run_foreground(config)
        }
    }
}

#[cfg(not(windows))]
pub fn run_scm() -> anyhow::Result<()> {
    Err(anyhow::anyhow!(
        "Windows service mode is only available on Windows; use `run` instead"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use find_stutter_core::{Sample, StorageConfig};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// 验证心跳写入 + 最近心跳读取（与 UI 端探活逻辑对齐）
    #[test]
    fn touch_and_read_heartbeat_roundtrip() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let db_path = std::env::temp_dir()
            .join(format!("fs_service_hb_{}.db", nanos))
            .to_str()
            .unwrap()
            .to_string();
        let config = StorageConfig {
            db_path: db_path.clone(),
            retention_days: 30,
        };

        let logger = Logger::new(&config).unwrap();
        // 新建后应无心跳
        assert!(logger.latest_heartbeat().unwrap().is_none());

        logger.touch_heartbeat().unwrap();
        let hb = logger.latest_heartbeat().unwrap();
        assert!(hb.is_some(), "心跳写入后必须能读到");

        std::fs::remove_file(&db_path).ok();
    }

    /// 验证：tick 一次后 latest_sample_summary 立即可读（GUI 启动后立刻有数据）
    #[test]
    fn write_sample_then_read_summary() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let db_path = std::env::temp_dir()
            .join(format!("fs_service_sum_{}.db", nanos))
            .to_str()
            .unwrap()
            .to_string();
        let config = StorageConfig {
            db_path: db_path.clone(),
            retention_days: 30,
        };

        let mut logger = Logger::new(&config).unwrap();
        let mut s = Sample::default();
        s.cpu_usage = 42.5;
        s.mem_available_mb = 8192;
        s.net_sent_bps = 1024;
        logger.write_sample(&s).unwrap();
        logger.flush().unwrap();

        let summary = logger.latest_sample_summary().unwrap();
        assert!(summary.is_some());
        let s = summary.unwrap();
        assert!((s.cpu_usage - 42.5).abs() < 0.01);
        assert_eq!(s.mem_available_mb, 8192);
        assert_eq!(s.net_sent_bps, 1024);

        std::fs::remove_file(&db_path).ok();
    }

    /// 验证：服务名常量
    #[test]
    fn service_name_is_stable() {
        // GUI 端可能依赖此名做日志提示，必须稳定
        assert_eq!(SERVICE_NAME, "FindStutter");
        assert!(!SERVICE_DISPLAY_NAME.is_empty());
    }

    /// 验证：request_stop 能让循环退出
    #[test]
    fn request_stop_sets_flag() {
        // RUNNING 初始为 true
        request_stop(); // 把它设 false
        assert!(!RUNNING.load(Ordering::SeqCst));
        // 恢复 true 以免影响其它测试
        RUNNING.store(true, Ordering::SeqCst);
    }
}
