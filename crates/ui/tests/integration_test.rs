//! find-stutter-ui 集成测试
//!
//! 覆盖：纯函数（格式化/颜色解析）、皮肤配置、OverlayState 状态默认值。

use find_stutter_ui::overlay::{self, OverlayState};
use find_stutter_ui::skin::SkinConfig;
use find_stutter_core::Severity;
use std::time::Instant;

// ========== Overlay 格式化 ==========

#[test]
fn format_bytes_zero() {
    assert_eq!(overlay::format_bytes(0), "0 B");
}

#[test]
fn format_bytes_one_kb() {
    assert_eq!(overlay::format_bytes(1024), "1.0 KB");
}

#[test]
fn format_bytes_one_mb() {
    assert_eq!(overlay::format_bytes(1_048_576), "1.0 MB");
}

#[test]
fn format_bytes_one_gb() {
    assert_eq!(overlay::format_bytes(1_073_741_824), "1.00 GB");
}

#[test]
fn format_bytes_large_values() {
    assert_eq!(overlay::format_bytes(5_368_709_120), "5.00 GB");
}

#[test]
fn format_rate_zero() {
    assert_eq!(overlay::format_rate(0), "0 B/s");
}

#[test]
fn format_rate_one_kbps() {
    assert_eq!(overlay::format_rate(1024), "1.0 KB/s");
}

#[test]
fn format_rate_one_mbps() {
    assert_eq!(overlay::format_rate(1_048_576), "1.0 MB/s");
}

#[test]
fn format_rate_one_gbps() {
    assert_eq!(overlay::format_rate(1_073_741_824), "1.0 GB/s");
}

#[test]
fn format_rate_partial_values() {
    let result = overlay::format_rate(1536);
    assert!(result.contains("1.5"));
    assert!(result.contains("KB/s"));
}

// ========== 颜色解析 ==========

#[test]
fn parse_color_red() {
    let c = overlay::parse_color("#FF0000");
    assert_eq!(c.red(), 255);
    assert_eq!(c.green(), 0);
    assert_eq!(c.blue(), 0);
}

#[test]
fn parse_color_green() {
    let c = overlay::parse_color("#00FF00");
    assert_eq!(c.red(), 0);
    assert_eq!(c.green(), 255);
    assert_eq!(c.blue(), 0);
}

#[test]
fn parse_color_blue() {
    let c = overlay::parse_color("#0000FF");
    assert_eq!(c.red(), 0);
    assert_eq!(c.green(), 0);
    assert_eq!(c.blue(), 255);
}

#[test]
fn parse_color_no_hash() {
    let c = overlay::parse_color("FF00FF");
    assert_eq!(c.red(), 255);
    assert_eq!(c.green(), 0);
    assert_eq!(c.blue(), 255);
}

#[test]
fn parse_color_invalid_returns_white() {
    let c = overlay::parse_color("#XYZ");
    assert_eq!(c.red(), 255);
    assert_eq!(c.green(), 255);
    assert_eq!(c.blue(), 255);
}

// ========== 皮肤 ==========

#[test]
fn skin_default_dimensions() {
    let skin = SkinConfig::default();
    assert_eq!(skin.width, 260.0);
    assert_eq!(skin.height, 80.0);
    assert_eq!(skin.font_size, 13.0);
    assert_eq!(skin.border_radius, 8.0);
}

#[test]
fn skin_default_colors() {
    let skin = SkinConfig::default();
    assert_eq!(skin.background_color, "#1E1E2E");
    assert_eq!(skin.border_color, "#45475A");
    assert_eq!(skin.upload_color, "#A6E3A1");
    assert_eq!(skin.download_color, "#89B4FA");
    assert_eq!(skin.cpu_color, "#F9E2AF");
    assert_eq!(skin.memory_color, "#F38BA8");
    assert_eq!(skin.gpu_color, "#CBA6F7");
    assert_eq!(skin.disk_color, "#94E2D5");
    assert_eq!(skin.label_color, "#BAC2DE");
}

#[test]
fn skin_custom_colors() {
    let mut skin = SkinConfig::default();
    skin.upload_color = "FF0000".into();
    skin.download_color = "00FF00".into();

    let upload = overlay::parse_color(&skin.upload_color);
    assert_eq!(upload.red(), 255);
    assert_eq!(upload.green(), 0);
    assert_eq!(upload.blue(), 0);

    let download = overlay::parse_color(&skin.download_color);
    assert_eq!(download.red(), 0);
    assert_eq!(download.green(), 255);
    assert_eq!(download.blue(), 0);
}

#[test]
fn skin_load_nonexistent_returns_default() {
    let skin = SkinConfig::load("nonexistent_skin_12345");
    assert_eq!(skin.width, 260.0);
    assert_eq!(skin.height, 80.0);
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
    assert_eq!(parsed.background_color, "#1E1E2E");
}

// ========== OverlayState ==========

#[test]
fn overlay_state_defaults() {
    let state = OverlayState::default();
    assert_eq!(state.sent_total, 0);
    assert_eq!(state.recv_total, 0);
    assert_eq!(state.stutter_count, 0);
    assert!(state.last_stutter_severity.is_none());
}

#[test]
fn overlay_state_clone() {
    let state = OverlayState {
        sent_total: 1024,
        recv_total: 2048,
        stutter_count: 3,
        last_stutter_severity: Some(Severity::Major),
        flash_until: Instant::now(),
    };
    let cloned = state.clone();
    assert_eq!(cloned.sent_total, 1024);
    assert_eq!(cloned.recv_total, 2048);
    assert_eq!(cloned.stutter_count, 3);
    assert_eq!(cloned.last_stutter_severity, Some(Severity::Major));
}
