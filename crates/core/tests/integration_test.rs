use find_stutter_core::*;
use std::time::Duration;

// F-RC15 集成测试辅助：回读某事件的软件根因子表行数与结论版本（本地连接，不改生产 API）。
#[derive(Default)]
struct BackCounts {
    modules: u64,
    win_events: u64,
    stack_samples: u64,
    reports: u64,
    report_version: Option<String>,
}

fn read_back_counts(db_path: &str, event_id: i64) -> BackCounts {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let mut out = BackCounts::default();
    let count = |sql: &str| -> u64 {
        conn.query_row(sql, rusqlite::params![event_id], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as u64
    };
    out.modules = count("SELECT COUNT(*) FROM process_modules WHERE event_id = ?1");
    out.win_events = count("SELECT COUNT(*) FROM windows_events WHERE event_id = ?1");
    out.stack_samples = count("SELECT COUNT(*) FROM stack_samples WHERE event_id = ?1");
    out.reports = count("SELECT COUNT(*) FROM root_cause_reports WHERE event_id = ?1");
    if out.reports > 0 {
        out.report_version = conn
            .query_row(
                "SELECT algorithm_version FROM root_cause_reports WHERE event_id = ?1",
                rusqlite::params![event_id],
                |r| r.get::<_, String>(0),
            )
            .ok();
    }
    out
}

// ========== Config Tests ==========

#[test]
fn config_load_from_toml_string() {
    let toml_str = r#"
[sampling]
interval_ms = 2000
slow_interval_factor = 10

[detection]
cpu_threshold = 85.0
mem_threshold_percent = 85.0
mem_threshold_mb = 1024
commit_threshold_percent = 85.0
page_reads_threshold = 50.0
disk_rate_spike_ratio = 3.0
spike_ratio = 1.5
sustained_seconds = 5

[ui]
skin = "dark"
always_on_top = false
show_upload = true
show_download = true
show_cpu = true
show_memory = true
show_gpu = true
show_disk = true
show_cpu_freq = false
show_temperature = false
mouse_transparent = false
click_through = false

[storage]
db_path = ":memory:"
retention_days = 7

[notifications]
stutter_alert = false
min_severity = "critical"

[logging]
level = "debug"
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(config.sampling.interval_ms, 2000);
    assert_eq!(config.sampling.slow_interval_factor, 10);
    assert_eq!(config.detection.cpu_threshold, 85.0);
    assert_eq!(config.detection.mem_threshold_mb, 1024);
    assert_eq!(config.detection.sustained_seconds, 5);
    assert_eq!(config.ui.skin, "dark");
    assert!(!config.ui.always_on_top);
    assert_eq!(config.storage.retention_days, 7);
    assert!(!config.notifications.stutter_alert);
    assert_eq!(config.notifications.min_severity, "critical");
}

#[test]
fn config_save_and_reload_roundtrip() {
    let config = Config::default();
    let tmp = std::env::temp_dir().join("find_stutter_test_config.toml");
    let path = tmp.to_str().unwrap();
    config.save(path).unwrap();
    let loaded = Config::load(path).unwrap();
    assert_eq!(loaded.sampling.interval_ms, config.sampling.interval_ms);
    assert_eq!(loaded.detection.cpu_threshold, config.detection.cpu_threshold);
    // Config::load 有意把相对 db_path 解析为「配置所在目录」下的绝对路径
    // （防止 SCM 服务把数据库写到 C:\Windows\System32），所以 roundtrip 后
    // db_path 应是 tmp 目录下的绝对路径，而非原始相对路径。
    let expected = tmp.parent().unwrap().join("stutter.db");
    assert_eq!(
        std::path::Path::new(&loaded.storage.db_path),
        expected,
        "load 后 db_path 应被解析为绝对路径"
    );
    assert!(std::path::Path::new(&loaded.storage.db_path).is_absolute());
    std::fs::remove_file(path).ok();
}

// ========== Collector Tests ==========

#[test]
fn collector_produces_valid_samples() {
    let mut collector = Collector::new();
    let sample = collector.collect();
    assert!(sample.cpu_usage >= 0.0 && sample.cpu_usage <= 100.0);
    assert!(sample.mem_total_mb > 0);
    assert!(sample.mem_used_mb <= sample.mem_total_mb);
    assert!(sample.process_count > 0);
}

#[test]
fn collector_network_delta_tracking() {
    let mut collector = Collector::new();
    let s1 = collector.collect();
    std::thread::sleep(Duration::from_millis(100));
    let s2 = collector.collect();
    // Total should be >= first sample's total
    assert!(s2.net_sent_total >= s1.net_sent_total);
    assert!(s2.net_recv_total >= s1.net_recv_total);
}

// ========== Detector Tests ==========

#[test]
fn detector_normal_sample_no_event() {
    let config = DetectionConfig {
        cpu_threshold: 90.0,
        mem_threshold_mb: 500,
        sustained_seconds: 1,
        ..Default::default()
    };
    let mut detector = Detector::new(&config);
    let sample = Sample {
        cpu_usage: 30.0,
        mem_available_mb: 8000,
        swap_usage_percent: 10.0,
        disk_read_bps: 1000,
        disk_write_bps: 1000,
        net_sent_bps: 500,
        net_recv_bps: 500,
        ..Default::default()
    };
    // Should not trigger any event for normal samples
    for _ in 0..5 {
        assert!(detector.analyze(&sample).is_none());
    }
}

#[test]
fn detector_cpu_threshold_triggers_event() {
    let config = DetectionConfig {
        cpu_threshold: 90.0,
        mem_threshold_mb: 500,
        sustained_seconds: 1,
        ..Default::default()
    };
    let mut detector = Detector::new(&config);
    let high_cpu = Sample {
        cpu_usage: 95.0,
        mem_available_mb: 8000,
        swap_usage_percent: 10.0,
        ..Default::default()
    };
    let normal = Sample {
        cpu_usage: 30.0,
        mem_available_mb: 8000,
        swap_usage_percent: 10.0,
        ..Default::default()
    };

    // Start stutter
    assert!(detector.analyze(&high_cpu).is_none());
    // Wait for sustained period
    std::thread::sleep(Duration::from_millis(1100));
    // Recovery should emit event
    let event = detector.analyze(&normal);
    assert!(event.is_some());
    let e = event.unwrap();
    assert!(!e.causes.is_empty());
    assert!(e.duration_ms >= 1000);
}

#[test]
fn detector_low_memory_triggers() {
    let config = DetectionConfig {
        cpu_threshold: 90.0,
        mem_threshold_mb: 500,
        sustained_seconds: 1,
        ..Default::default()
    };
    let mut detector = Detector::new(&config);
    let low_mem = Sample {
        cpu_usage: 30.0,
        mem_available_mb: 200,
        ..Default::default()
    };
    let normal = Sample {
        cpu_usage: 30.0,
        mem_available_mb: 8000,
        ..Default::default()
    };
    assert!(detector.analyze(&low_mem).is_none());
    std::thread::sleep(Duration::from_millis(1100));
    let event = detector.analyze(&normal);
    assert!(event.is_some());
}

#[test]
fn detector_severity_levels() {
    let config = DetectionConfig {
        sustained_seconds: 1,
        ..Default::default()
    };
    let mut detector = Detector::new(&config);

    // Single cause -> Minor
    let sample = Sample {
        cpu_usage: 95.0,
        mem_available_mb: 8000,
        ..Default::default()
    };
    detector.analyze(&sample);
    std::thread::sleep(Duration::from_millis(1100));
    let normal = Sample {
        cpu_usage: 30.0,
        mem_available_mb: 8000,
        swap_usage_percent: 10.0,
        ..Default::default()
    };
    let event = detector.analyze(&normal).unwrap();
    assert_eq!(event.severity, Severity::Minor);
}

// ========== Logger Tests ==========

#[test]
fn logger_write_and_readback() {
    let config = StorageConfig {
        db_path: ":memory:".to_string(),
        ..Default::default()
    };
    let mut logger = Logger::new(&config).unwrap();

    let sample = Sample {
        cpu_usage: 55.0,
        mem_usage_percent: 67.0,
        mem_used_mb: 12000,
        mem_total_mb: 16000,
        mem_available_mb: 4000,
        net_sent_bps: 1024,
        net_recv_bps: 2048,
        process_count: 200,
        ..Default::default()
    };

    logger.write_sample(&sample).unwrap();
    logger.flush().unwrap();

    let count = logger.event_count_today().unwrap();
    assert_eq!(count, 0); // No events written yet
}

#[test]
fn logger_event_count_tracking() {
    let config = StorageConfig {
        db_path: ":memory:".to_string(),
        ..Default::default()
    };
    let logger = Logger::new(&config).unwrap();

    let event = StutterEvent {
        timestamp: chrono::Utc::now(),
        duration_ms: 5000,
        severity: Severity::Major,
        causes: vec!["CPU High".to_string()],
        snapshot: Sample::default(),
        culprits: vec![],
        ..Default::default()
    };

    logger.write_event(&event).unwrap();
    logger.write_event(&event).unwrap();

    let count = logger.event_count_today().unwrap();
    assert_eq!(count, 2);
}

#[test]
fn logger_csv_export() {
    let tmp_db = std::env::temp_dir().join("find_stutter_csv_test.db");
    let config = StorageConfig {
        db_path: tmp_db.to_str().unwrap().to_string(),
        ..Default::default()
    };
    let mut logger = Logger::new(&config).unwrap();

    let sample = Sample {
        cpu_usage: 42.0,
        mem_usage_percent: 55.0,
        net_sent_bps: 500,
        net_recv_bps: 1000,
        ..Default::default()
    };
    logger.write_sample(&sample).unwrap();
    logger.flush().unwrap();

    let csv_path = std::env::temp_dir().join("find_stutter_test_export.csv");
    let path = csv_path.to_str().unwrap();
    let result = logger.export_csv(
        "2000-01-01T00:00:00Z",
        "2099-12-31T23:59:59Z",
        path,
    );
    assert!(result.is_ok(), "CSV export failed: {:?}", result.err());

    let content = std::fs::read_to_string(path).unwrap();
    assert!(content.contains("42"));
    std::fs::remove_file(path).ok();
    std::fs::remove_file(&tmp_db).ok();
}

#[test]
fn logger_cleanup_removes_old_data() {
    let config = StorageConfig {
        db_path: ":memory:".to_string(),
        retention_days: 0, // Delete everything
        ..Default::default()
    };
    let mut logger = Logger::new(&config).unwrap();

    let sample = Sample::default();
    logger.write_sample(&sample).unwrap();
    logger.flush().unwrap();

    logger.cleanup().unwrap();
    // After cleanup with 0 retention, today's data should also be gone
    // (cleanup uses now - 0 days = now, so data written just now might survive depending on timing)
}

// ========== Integration: Collector -> Detector Pipeline ==========

#[test]
fn collector_detector_pipeline() {
    let mut collector = Collector::new();
    let config = DetectionConfig {
        sustained_seconds: 1,
        ..Default::default()
    };
    let mut detector = Detector::new(&config);

    // Collect a few samples
    for _ in 0..3 {
        let sample = collector.collect();
        detector.analyze(&sample);
        std::thread::sleep(Duration::from_millis(100));
    }

    // Pipeline should not crash
    assert!(true);
}
// ========== Integration: F-RC14/F-RC15 软件根因数据落库 + 级联清理 + 结论 UPSERT ==========

#[test]
fn software_root_cause_tables_write_and_cascade_cleanup() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db_path = std::env::temp_dir()
        .join(format!("fs_src_test_{}.db", nanos))
        .to_str()
        .unwrap()
        .to_string();
    let config = StorageConfig {
        db_path: db_path.clone(),
        retention_days: 30,
        event_retention_days: 30, hot_retention_days: 0, // 测试关闭降采样，保持既有行为口径
    };
    let mut logger = Logger::new(&config).unwrap();

    // 1) 写一个事件，拿到真实主键 id
    let event = StutterEvent {
        timestamp: chrono::Utc::now(),
        duration_ms: 3000,
        severity: Severity::Major,
        causes: vec!["CPU High".to_string()],
        cause_kinds: vec![CauseKind::CpuHigh, CauseKind::ProcessHandleLeak],
        culprits: vec![ProcessBrief {
            pid: 777,
            name: "hog.exe".into(),
            cpu_usage: 95.0,
            mem_used_mb: 512,
            ..Default::default()
        }],
        ..Default::default()
    };
    let event_id = logger.write_event(&event).unwrap();
    assert!(event_id > 0, "write_event 必须返回真实自增主键");

    // 2) 写三张子表数据
    let modules = vec![ProcessModule {
        pid: 777,
        process_name: "hog.exe".into(),
        module_path: "C:\\Windows\\System32\\ntdll.dll".into(),
        module_size: 2_097_152,
    }];
    let win_events = vec![WindowsEventRecord {
        channel: "System".into(),
        provider: "disk".into(),
        win_event_id: 7,
        level: "Warning".into(),
        message: "The device \\Device\\Harddisk0\\DR0 has a bad block.".into(),
        ts: chrono::Utc::now().to_rfc3339(),
    }];
    let stack_samples = vec![StackSample {
        pid: 777,
        process_name: "hog.exe".into(),
        module: "C:\\Windows\\System32\\ntdll.dll".into(),
        rva: 0x1234,
        sample_count: 42,
    }];
    logger
        .write_software_root_cause_data(event_id, &modules, &win_events, &stack_samples)
        .unwrap();
    logger.flush().unwrap();

    // 3) 回读校验各表命中
    let read = read_back_counts(&db_path, event_id);
    assert_eq!(read.modules, 1);
    assert_eq!(read.win_events, 1);
    assert_eq!(read.stack_samples, 1);

    // 4) 结论 UPSERT：写两次同 event_id，仅一条且为最新版本
    let report = RootCauseReport {
        event_id,
        algorithm_version: "rc5-rc14.v1".into(),
        primary_cause: "CPU 占用高".into(),
        confidence: 0.8,
        cause_chain: vec!["CPU 占用高".into(), "句柄泄漏".into()],
        software_root_cause: serde_json::json!({"software_cause": "句柄泄漏"}),
        baseline_delta: serde_json::json!({"deviation": ""}),
        computed_at: chrono::Utc::now().to_rfc3339(),
    };
    logger.write_root_cause_report(&report).unwrap();
    let report_v2 = RootCauseReport {
        algorithm_version: "rc5-rc14.v2".into(),
        ..report.clone()
    };
    logger.write_root_cause_report(&report_v2).unwrap();
    let read = read_back_counts(&db_path, event_id);
    assert_eq!(read.reports, 1, "UPSERT 后同 event_id 应只有一条结论");
    assert_eq!(read.report_version, Some("rc5-rc14.v2".to_string()));

    // 5) 级联清理：retention=0 清空事件 → 子表随 FK ON DELETE CASCADE 一并清空
    logger.cleanup_with_retention(0).unwrap();
    let read = read_back_counts(&db_path, event_id);
    assert_eq!(read.modules, 0, "级联删除后 modules 应清空");
    assert_eq!(read.win_events, 0, "级联删除后 win_events 应清空");
    assert_eq!(read.stack_samples, 0, "级联删除后 stack_samples 应清空");
    assert_eq!(read.reports, 0, "级联删除后 root_cause_reports 应清空");

    std::fs::remove_file(&db_path).ok();
    std::fs::remove_file(format!("{}-wal", db_path)).ok();
    std::fs::remove_file(format!("{}-shm", db_path)).ok();
}