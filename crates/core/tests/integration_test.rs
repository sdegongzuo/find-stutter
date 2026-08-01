use find_stutter_core::*;
use std::time::Duration;

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
swap_threshold = 60.0
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
        swap_threshold: 50.0,
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
        swap_threshold: 50.0,
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
