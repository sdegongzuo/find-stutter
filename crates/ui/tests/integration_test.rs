use find_stutter_ui::overlay;
use find_stutter_ui::skin::SkinConfig;

// ========== Overlay Format Tests ==========

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
    assert_eq!(overlay::format_bytes(1048576), "1.0 MB");
}

#[test]
fn format_bytes_one_gb() {
    assert_eq!(overlay::format_bytes(1073741824), "1.00 GB");
}

#[test]
fn format_bytes_large_values() {
    assert_eq!(overlay::format_bytes(5368709120), "5.00 GB");
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
    assert_eq!(overlay::format_rate(1048576), "1.0 MB/s");
}

#[test]
fn format_rate_one_gbps() {
    assert_eq!(overlay::format_rate(1073741824), "1.0 GB/s");
}

#[test]
fn format_rate_partial_values() {
    let result = overlay::format_rate(1536);
    assert!(result.contains("1.5"));
    assert!(result.contains("KB/s"));
}

// ========== Skin Tests ==========

#[test]
fn skin_default_dimensions() {
    let skin = SkinConfig::default();
    assert_eq!(skin.width, 260.0);
    assert_eq!(skin.height, 80.0);
    assert_eq!(skin.font_size, 13.0);
    assert_eq!(skin.border_radius, 8.0);
}

#[test]
fn skin_color_parsing() {
    let skin = SkinConfig::default();
    let upload = skin.upload_color();
    assert!(upload.r() > 100);
    assert!(upload.g() > 200);

    let cpu = skin.cpu_color();
    assert!(cpu.r() > 200);
    assert!(cpu.g() > 200);
}

#[test]
fn skin_custom_colors() {
    let mut skin = SkinConfig::default();
    skin.upload_color = "FF0000".into();
    skin.download_color = "00FF00".into();

    let upload = skin.upload_color();
    assert_eq!(upload.r(), 255);
    assert_eq!(upload.g(), 0);
    assert_eq!(upload.b(), 0);

    let download = skin.download_color();
    assert_eq!(download.r(), 0);
    assert_eq!(download.g(), 255);
    assert_eq!(download.b(), 0);
}

#[test]
fn skin_load_nonexistent_returns_default() {
    let skin = find_stutter_ui::skin::load_skin("nonexistent_skin_12345");
    assert_eq!(skin.width, 260.0);
    assert_eq!(skin.height, 80.0);
}

#[test]
fn skin_toml_parse_from_file() {
    let tmp = std::env::temp_dir().join("find_stutter_test_skin.toml");
    let content = "width = 300.0\nheight = 100.0\nfont_size = 15.0\nupload_color = \"00FF00\"\n";
    std::fs::write(&tmp, content).unwrap();
    let skin = find_stutter_ui::skin::load_skin(tmp.to_str().unwrap());
    // load_skin expects skins/{name}/skin.toml pattern, so direct file won't work
    // But we can test toml parsing directly
    let parsed: SkinConfig = toml::from_str(content).unwrap();
    assert_eq!(parsed.width, 300.0);
    assert_eq!(parsed.height, 100.0);
    assert_eq!(parsed.font_size, 15.0);
    assert_eq!(parsed.upload_color, "00FF00");
    std::fs::remove_file(tmp).ok();
}

// ========== OverlayState Tests ==========

#[test]
fn overlay_state_defaults() {
    let state = overlay::OverlayState::default();
    assert_eq!(state.sent_total, 0);
    assert_eq!(state.recv_total, 0);
    assert_eq!(state.stutter_count, 0);
}

#[test]
fn overlay_state_clone() {
    let state = overlay::OverlayState {
        sent_total: 1024,
        recv_total: 2048,
        stutter_count: 3,
    };
    let cloned = state.clone();
    assert_eq!(cloned.sent_total, 1024);
    assert_eq!(cloned.recv_total, 2048);
    assert_eq!(cloned.stutter_count, 3);
}
