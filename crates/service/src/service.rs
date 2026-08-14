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

use find_stutter_core::software_root_cause::{
    enrich_software_causes, is_whitelisted_win_event, merge_software_causes,
};
use find_stutter_core::win32::{read_windows_events, snapshot_process_modules};
use find_stutter_core::types::{ProcessBrief, ProcessModule, WindowsEventRecord};
use find_stutter_core::{Collector, Config, Detector, Logger, StackSampler};
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

/// 追加一行到 binary 同目录的 `find-stutter-service.diag.log`（用于 SCM 启动调试）
pub fn diag_log(msg: &str) {
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

/// 前台运行服务循环（开发 / 调试 / `Run` 子命令）。
///
/// 阻塞当前线程，按 `config.sampling.interval_ms` 周期采集。
pub fn run_foreground(config: Config) -> anyhow::Result<()> {
    info!(
        "find-stutter-service starting (interval={}ms, db={})",
        config.sampling.interval_ms, config.storage.db_path
    );
    diag_log(&format!(
        "run_foreground: starting, db={}, interval={}ms",
        config.storage.db_path, config.sampling.interval_ms
    ));

    diag_log("run_foreground: creating Collector");
    let mut collector = Collector::new();
    diag_log("run_foreground: Collector created");
    let mut detector = Detector::new(&config.detection);
    diag_log("run_foreground: Detector created");
    let mut logger = match Logger::new(&config.storage) {
        Ok(l) => {
            diag_log(&format!("run_foreground: Logger created, db={}", config.storage.db_path));
            l
        }
        Err(e) => {
            diag_log(&format!("run_foreground: Logger::new FAILED: {}", e));
            return Err(e);
        }
    };

    // F-RC14-d：ETW 调用栈采样器（初始化失败自动静默降级，不影响采集热路径）。
    // 采样窗口在独立后台线程执行（PRD §F-RC14-d / 验收 687：绝不阻塞采集热路径）：
    // 主循环只把 (event_id, culprits) 塞进通道即返回，worker 线程完成采样后补写库。
    diag_log("run_foreground: creating StackSampler + background ETW worker");
    let stack_sampler = StackSampler::new();
    diag_log(&format!("run_foreground: StackSampler created, enabled={}", stack_sampler.enabled()));
    let (etw_tx, etw_rx) = std::sync::mpsc::channel::<(i64, Vec<ProcessBrief>)>();
    let worker_db = config.storage.db_path.clone();
    std::thread::Builder::new()
        .name("etw-worker".into())
        .spawn(move || {
            // 后台线程独占 StackSampler：慢速采样与落库都在这里，每次事件限频一轮
            while let Ok((event_id, culprits)) = etw_rx.recv() {
                let samples = stack_sampler.sample(&culprits);
                if samples.is_empty() {
                    continue;
                }
                if let Err(e) = Logger::write_stack_samples(&worker_db, event_id, &samples) {
                    warn!("ETW worker: write_stack_samples failed: {}", e);
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("ETW worker thread spawn failed: {}", e))?;
    diag_log("run_foreground: ETW worker started");

    // 启动时立即写一次心跳，让 GUI 一启动就能看到「服务在跑」
    if let Err(e) = logger.touch_heartbeat() {
        warn!("initial touch_heartbeat failed: {}", e);
        diag_log(&format!("run_foreground: initial touch_heartbeat failed: {}", e));
    } else {
        diag_log("run_foreground: initial touch_heartbeat ok");
    }

    let tick = Duration::from_millis(config.sampling.interval_ms);
    let mut count: u64 = 0;
    // F-RC14-a 方案 B：累积卡顿窗口内各 pid 的句柄数采样序列，供句柄「趋势」判定
    // （绝对值高但无增长 = 中性提示，持续增长 = 真泄漏）。事件结束后清空。
    let mut handle_history: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();

    while RUNNING.load(Ordering::SeqCst) {
        count += 1;
        if count == 1 {
            diag_log("run_foreground: entering main loop, count=1");
        }

        // 心跳：每 tick 一次，GUI 探活用。
        // 注意：必须在 collect() 之前写——首次 collect() 要初始化
        // WMI/COM（慢通道），可能耗时数秒；若先 collect 再写心跳，
        // GUI 启动后的前几秒会误判为 Stopped/Stale。
        if let Err(e) = logger.touch_heartbeat() {
            warn!("touch_heartbeat failed: {}", e);
            if count <= 3 {
                diag_log(&format!("run_foreground: tick={} touch_heartbeat failed: {}", count, e));
            }
        }

        // 非卡顿时跳过 top_processes 构建（collect_with(false)），
        // 卡顿进行中/刚结束一帧时才构建（detector.needs_process_snapshot()）。
        let sample = collector.collect_with(detector.needs_process_snapshot());

        // 累积本帧 top 进程句柄数（卡顿窗口内多次采样，供句柄趋势判定）
        for p in &sample.top_processes {
            handle_history.entry(p.pid).or_default().push(p.handle_count.unwrap_or(0));
        }

        if let Some(event) = detector.analyze(&sample) {
            let culprits = event.culprits.clone();
            let onset_secs = event.onset_ts.map(|ms| ms / 1000).unwrap_or_else(|| {
                event.timestamp.timestamp()
            });
            // F-RC14-b/c/d：卡顿触发后（限频）回溯事件日志 / 模块 / 调用栈
            let now_secs = chrono::Utc::now().timestamp();
            let since = onset_secs - 30;
            let win_events: Vec<WindowsEventRecord> = read_windows_events(since, now_secs)
                .into_iter()
                .filter(is_whitelisted_win_event)
                .collect();
            let mut modules: Vec<ProcessModule> = Vec::new();
            for c in &culprits {
                modules.extend(snapshot_process_modules(c.pid, &c.name));
            }
            let sw = enrich_software_causes(
                &culprits,
                &win_events,
                &handle_history,
                config.detection.handle_leak_threshold,
                config.detection.handle_leak_growth_threshold,
                config.detection.gdi_leak_threshold,
            );
            let merged = merge_software_causes(event, sw);
            info!(
                "stutter detected: {:?} — {}",
                merged.severity,
                merged.causes.join(", ")
            );
            match logger.write_event(&merged) {
                Ok(event_id) => {
                    // F-RC14-d：后台 ETW worker 异步采样并补写 stack_samples（不阻塞热路径）
                    if etw_tx.send((event_id, culprits)).is_err() {
                        warn!("ETW worker 通道已关闭，跳过本次调用栈采样");
                    }
                    if let Err(e) = logger.write_software_root_cause_data(
                        event_id,
                        &modules,
                        &win_events,
                        &Vec::new(),
                    ) {
                        warn!("write_software_root_cause_data failed: {}", e);
                    }
                }
                Err(e) => warn!("write_event failed: {}", e),
            }
            // 事件已结束：清空句柄历史，避免跨事件污染下一轮趋势判定
            handle_history.clear();
        }

        if let Err(e) = logger.write_sample(&sample) {
            warn!("write_sample failed: {}", e);
            if count <= 3 {
                diag_log(&format!("run_foreground: tick={} write_sample failed: {}", count, e));
            }
        }

        if count % 10 == 0 {
            if let Err(e) = logger.flush() {
                warn!("flush failed: {}", e);
                diag_log(&format!("run_foreground: tick={} flush failed: {}", count, e));
            }
        }
        if count % 3600 == 0 {
            if let Err(e) = logger.cleanup() {
                warn!("cleanup failed: {}", e);
            }
        }

        std::thread::sleep(tick);
    }

    diag_log(&format!("run_foreground: exiting main loop, count={}", count));

    // 退出前 flush + cleanup，保证数据不丢
    logger.flush()?;
    logger.cleanup()?;
    info!("find-stutter-service stopped");
    diag_log("run_foreground: cleanup done, returning Ok");
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
    diag_log("scm_service_entry: entered");
    if let Err(e) = scm_loop() {
        error!("SCM service entry failed: {}", e);
        diag_log(&format!("scm_service_entry: FAILED: {}", e));
    }
}

/// SCM 入口：被 SCM 启动时调用。
/// 注册 control handler → 把状态推到 Running → 跑主循环 → Stop 时退出。
#[cfg(windows)]
fn scm_loop() -> anyhow::Result<()> {
    diag_log("scm_loop: entering");
    let handler = service_control_handler::register(SERVICE_NAME, move |control_event| {
        diag_log(&format!("scm_loop: control_event = {:?}", control_event));
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
        diag_log(&format!("scm_loop: config load failed: {}", e));
        Config::default()
    });
    diag_log(&format!(
        "scm_loop: config loaded, db={}, interval={}ms",
        config.storage.db_path, config.sampling.interval_ms
    ));

    let result = run_foreground(config);
    diag_log(&format!("scm_loop: run_foreground returned: {:?}", result));

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
            event_retention_days: 30,
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
            event_retention_days: 30,
        };

        let mut logger = Logger::new(&config).unwrap();
        let mut s = Sample::default();
        s.cpu_usage = 42.5;
        s.mem_usage_percent = 58.3;
        s.mem_available_mb = 8192;
        s.net_sent_bps = 1024;
        logger.write_sample(&s).unwrap();
        logger.flush().unwrap();

        let summary = logger.latest_sample_summary().unwrap();
        assert!(summary.is_some());
        let s = summary.unwrap();
        assert!((s.cpu_usage - 42.5).abs() < 0.01);
        assert!((s.mem_usage_percent - 58.3).abs() < 0.01);
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
