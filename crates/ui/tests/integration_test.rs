//! find-stutter-ui 集成测试
//!
//! P3：覆盖 reader 健康检测 + 皮肤配置 + OverlayState 默认值。
//!
//! 不再测试 `format_bytes/format_rate/parse_color`（P3 重构中由 Slint typed
//! setter 替代了原本的字符串格式化函数）。

use find_stutter_ui::overlay::{self, OverlayState};
use find_stutter_ui::reader::{DbReader, ServiceHealth};
use find_stutter_ui::skin::SkinConfig;

// ========== 皮肤配置（保留 P2 的）==========

#[test]
fn skin_default_dimensions() {
    let skin = SkinConfig::default();
    assert_eq!(skin.width, 360.0);
    assert_eq!(skin.height, 78.0);
    assert_eq!(skin.font_size, 13.0);
    assert_eq!(skin.border_radius, 8.0);
}

#[test]
fn skin_default_colors() {
    let skin = SkinConfig::default();
    assert_eq!(skin.background_color, "#FFFFFF");
    assert_eq!(skin.border_color, "#C0C0C8");
    assert_eq!(skin.upload_color, "#2E7D32");
    assert_eq!(skin.download_color, "#1565C0");
    assert_eq!(skin.cpu_color, "#37474F");
    assert_eq!(skin.memory_color, "#6A1B9A");
    assert_eq!(skin.gpu_color, "#00695C");
    assert_eq!(skin.disk_color, "#AD1457");
    assert_eq!(skin.label_color, "#546E7A");
}

#[test]
fn skin_load_nonexistent_returns_default() {
    let skin = SkinConfig::load("nonexistent_skin_12345");
    assert_eq!(skin.width, 360.0);
    assert_eq!(skin.height, 78.0);
}

#[test]
fn skin_toml_parse_from_string() {
    let content = "width = 300.0\nheight = 100.0\nfont_size = 15.0\nupload_color = \"00FF00\"\n";
    let parsed: SkinConfig = toml::from_str(content).unwrap();
    assert_eq!(parsed.width, 300.0);
    assert_eq!(parsed.height, 100.0);
    assert_eq!(parsed.font_size, 15.0);
    assert_eq!(parsed.upload_color, "00FF00");
    // 未指定字段用默认值
    assert_eq!(parsed.background_color, "#FFFFFF");
}

// ========== OverlayState（P3 新字段）==========

#[test]
fn overlay_state_defaults() {
    let state = OverlayState::new(SkinConfig::default());
    assert_eq!(state.today_event_count, 0);
    assert_eq!(state.service_health, ServiceHealth::NoDatabase);
    assert!(state.last_summary.is_none());
    assert!(state.last_event_at.is_none());
    assert!(state.last_heartbeat.is_none());
    assert!(!state.paused);
}

#[test]
fn overlay_state_clone() {
    let mut state = OverlayState::new(SkinConfig::default());
    state.today_event_count = 5;
    state.paused = true;
    let cloned = state.clone();
    assert_eq!(cloned.today_event_count, 5);
    assert!(cloned.paused);
}

// ========== 服务健康格式化 ==========

#[test]
fn format_service_status_includes_chinese_label() {
    let (text, _) = overlay::format_service_status(ServiceHealth::Running);
    assert!(text.as_str().contains("服务运行中"));
    let (text, _) = overlay::format_service_status(ServiceHealth::Stale);
    assert!(text.as_str().contains("服务卡顿"));
    let (text, _) = overlay::format_service_status(ServiceHealth::Stopped);
    assert!(text.as_str().contains("服务已停止"));
    let (text, _) = overlay::format_service_status(ServiceHealth::NoDatabase);
    assert!(text.as_str().contains("未注册"));
}

// ========== DbReader 健康检测（端到端）==========

#[test]
fn reader_poll_no_database_path() {
    let reader = DbReader::new("D:/__definitely_missing__/nope.db");
    let r = reader.poll();
    assert_eq!(r.health, ServiceHealth::NoDatabase);
    assert!(r.summary.is_none());
    assert_eq!(r.today_event_count, 0);
}

#[test]
fn reader_poll_picks_up_running_service() {
    use find_stutter_core::logger::Logger;
    use find_stutter_core::{Sample, StorageConfig};
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db = std::env::temp_dir()
        .join(format!("fs_int_{}.db", nanos))
        .to_str()
        .unwrap()
        .to_string();
    let cfg = StorageConfig {
        db_path: db.clone(),
        retention_days: 30,
    };
    let mut logger = Logger::new(&cfg).unwrap();
    logger.touch_heartbeat().unwrap();
    let mut s = Sample::default();
    s.cpu_usage = 30.0;
    logger.write_sample(&s).unwrap();
    logger.flush().unwrap();
    drop(logger);

    let reader = DbReader::new(&db);
    let r = reader.poll();
    assert_eq!(r.health, ServiceHealth::Running);
    assert!(r.summary.is_some());
    assert!(r.last_heartbeat.is_some());
    assert_eq!(r.today_event_count, 0);

    std::fs::remove_file(&db).ok();
}

#[test]
fn reader_poll_stopped_when_no_heartbeat() {
    use find_stutter_core::logger::Logger;
    use find_stutter_core::{Sample, StorageConfig};
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db = std::env::temp_dir()
        .join(format!("fs_int_stopped_{}.db", nanos))
        .to_str()
        .unwrap()
        .to_string();
    let cfg = StorageConfig {
        db_path: db.clone(),
        retention_days: 30,
    };
    let mut logger = Logger::new(&cfg).unwrap();
    // 只写 sample，不写心跳
    let mut s = Sample::default();
    s.cpu_usage = 10.0;
    logger.write_sample(&s).unwrap();
    logger.flush().unwrap();
    drop(logger);

    let reader = DbReader::new(&db);
    let r = reader.poll();
    assert_eq!(r.health, ServiceHealth::Stopped);
    assert!(r.summary.is_some());
    assert!(r.last_heartbeat.is_none());

    std::fs::remove_file(&db).ok();
}
