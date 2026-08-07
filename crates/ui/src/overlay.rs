//! Overlay 渲染逻辑。
//!
//! P3：service_health 字段由 DbReader 填进来，apply_metrics 推到 Slint。

use find_stutter_core::logger::LatestSampleSummary;
use slint::{Brush, Color, SharedString};

use crate::reader::ServiceHealth;
use crate::skin::SkinConfig;

/// 状态条配置
pub const HEALTH_BAR_HEIGHT: f32 = 22.0;

#[derive(Debug, Clone)]
pub struct OverlayState {
    pub skin: SkinConfig,
    pub last_summary: Option<LatestSampleSummary>,
    pub last_event_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 上一次已弹通知的事件时间戳（P2：用于事件去重，防止重复弹 toast）
    pub last_notified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub today_event_count: u32,
    pub service_health: ServiceHealth,
    pub last_heartbeat: Option<String>,
    pub paused: bool,
    /// 点击穿透模式（右键菜单切换；窗口鼠标事件完全穿透）
    pub click_through: bool,
}

impl OverlayState {
    pub fn new(skin: SkinConfig) -> Self {
        Self {
            skin,
            last_summary: None,
            last_event_at: None,
            last_notified_at: None,
            today_event_count: 0,
            service_health: ServiceHealth::NoDatabase,
            last_heartbeat: None,
            paused: false,
            click_through: false,
        }
    }

    /// 从 `PollResult` 更新状态（P3：不再有事件细节 stream，只读 db）
    pub fn update_from_poll(&mut self, poll: &crate::reader::PollResult) {
        self.last_summary = poll.summary.clone();
        self.last_event_at = poll.event.as_ref().map(|e| e.timestamp);
        self.today_event_count = poll.today_event_count;
        self.service_health = poll.health;
        self.last_heartbeat = poll.last_heartbeat.clone();
    }
}

/// 格式化服务健康状态为 (text, color) 给 Slint 显示。
///
/// 颜色：
/// - Running     → 绿色 (#3fa055)
/// - Stale       → 黄色 (#c4a82e)
/// - Stopped     → 红色 (#c44c4c)
/// - NoDatabase  → 红色 (#c44c4c)
pub fn format_service_status(health: ServiceHealth) -> (SharedString, Brush) {
    let color = Brush::SolidColor(match health {
        ServiceHealth::Running => Color::from_rgb_u8(0x3f, 0xa0, 0x55),
        ServiceHealth::Stale => Color::from_rgb_u8(0xc4, 0xa8, 0x2e),
        ServiceHealth::Stopped | ServiceHealth::NoDatabase => Color::from_rgb_u8(0xc4, 0x4c, 0x4c),
    });
    let text = match health {
        ServiceHealth::Running => SharedString::from("● 服务运行中"),
        ServiceHealth::Stale => SharedString::from("● 服务卡顿"),
        ServiceHealth::Stopped => SharedString::from("● 服务已停止"),
        ServiceHealth::NoDatabase => {
            SharedString::from("● 服务未注册（请运行 find-stutter-service install）")
        }
    };
    (text, color)
}

/// 把 bps 速率格式化为自适应单位的可读字符串：数字变大时自动升级
/// B/s → KB/s → MB/s → GB/s，保持显示紧凑（数字部分右对齐 5 字符）。
pub fn format_bps(bps: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let v = bps as f64;
    if v >= GB {
        format!("{:>5.1} GB/s", v / GB)
    } else if v >= MB {
        format!("{:>5.1} MB/s", v / MB)
    } else if v >= KB {
        format!("{:>5.1} KB/s", v / KB)
    } else {
        format!("{:>5} B/s", bps)
    }
}

/// 解析 `#RRGGBB` / `#RRGGBBAA` / `RRGGBB` 颜色字符串为 `slint::Color`。
/// 解析失败返回 `None`（调用方 fallback 到默认色）。
///
/// 历史上此函数存在于 overlay.rs，P3 重构时被删除；皮肤接线需要它，已恢复。
pub fn parse_color(s: &str) -> Option<Color> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 && hex.len() != 8 {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    let r = ((v >> 16) & 0xFF) as u8;
    let g = ((v >> 8) & 0xFF) as u8;
    let b = (v & 0xFF) as u8;
    if hex.len() == 8 {
        let a = ((v >> 24) & 0xFF) as u8;
        Some(Color::from_argb_u8(a, r, g, b))
    } else {
        Some(Color::from_rgb_u8(r, g, b))
    }
}

/// 把 OverlayState 推到 Slint Overlay 实例。
pub fn apply_metrics(ui: &crate::Overlay, state: &OverlayState) {
    // 0) 皮肤：颜色 / 字号 / 尺寸 / 边框 注入（修复：P3 后皮肤名存实亡，从未接线）
    //
    // 主窗口（Overlay）的视觉风格与进程详情页（ProcessList）对齐：
    //   - 背景色：白底（skin.background_color，默认 #F5F5F7 的 skin.toml 已统一为 #ffffff）
    //   - 边框：1px，颜色由 skin.border_color 注入（默认 #C0C0C8 ≈ ProcessList 的 #c0c0c0）
    //   - 圆角：由 skin.border_radius 注入（默认 8px，与 ProcessList 一致）
    //   - 指标文字：统一深色 #1e1e2e（与 ProcessList 文本色一致；不再用 skin 字段的
    //     Material 800 彩色 — skin 字段保留供将来扩展，不破坏皮肤系统）
    //   - 卡顿计数：保留红色 #c44c4c 作为视觉重点（今日卡顿:N）
    //   - 心跳时间：灰色 #8a8a92（弱化的辅助信息）
    let skin = &state.skin;
    let text_color = Color::from_rgb_u8(0x1e, 0x1e, 0x2e); // 与 ProcessList 文本色一致
    let event_red = Color::from_rgb_u8(0xc4, 0x4c, 0x4c); // 卡顿计数红色（醒目）
    let hb_gray = Color::from_rgb_u8(0x8a, 0x8a, 0x92); // 心跳时间灰色（弱化）

    ui.set_skin_width(skin.width as f32);
    ui.set_skin_height(skin.height as f32);
    ui.set_text_size(skin.font_size as f32);
    ui.set_skin_bg(Brush::SolidColor(
        parse_color(&skin.background_color).unwrap_or(Color::from_rgb_u8(0xff, 0xff, 0xff)),
    ));
    ui.set_skin_border_color(
        parse_color(&skin.border_color).unwrap_or(Color::from_rgb_u8(0xc0, 0xc0, 0xc0)),
    );
    ui.set_skin_border_radius(skin.border_radius as f32);
    ui.set_cpu_color(text_color);
    ui.set_mem_color(text_color);
    ui.set_event_color(event_red);
    ui.set_gpu_color(text_color);
    ui.set_net_color(text_color);
    ui.set_disk_color(text_color);
    ui.set_hb_color(hb_gray);

    // 1) 服务健康条
    let (text, color) = format_service_status(state.service_health);
    ui.set_service_status(text);
    ui.set_service_status_color(color);
    ui.set_service_bar_height(HEALTH_BAR_HEIGHT);

    // 2) 暂停按钮在服务停止时禁用
    let pause_enabled = matches!(state.service_health, ServiceHealth::Running);
    ui.set_pause_enabled(pause_enabled);

    // 3) 指标（仅在 Running 时更新，Stale/Stopped 时保留最后值）
    if let Some(s) = &state.last_summary {
        ui.set_cpu_text(SharedString::from(format!("CPU {:5.1}%", s.cpu_usage)));
        ui.set_mem_text(SharedString::from(format!(
            "内存 {:4.1}%",
            s.mem_usage_percent
        )));
        ui.set_net_send(SharedString::from(format!("↑ {}", format_bps(s.net_sent_bps))));
        ui.set_net_recv(SharedString::from(format!("↓ {}", format_bps(s.net_recv_bps))));
        ui.set_disk_read(SharedString::from(format!("R {}", format_bps(s.disk_read_bps))));
        ui.set_disk_write(SharedString::from(format!("W {}", format_bps(s.disk_write_bps))));
        if let Some(g) = s.gpu_usage {
            ui.set_gpu_text(SharedString::from(format!("GPU {:5.1}%", g)));
        }
        // 温度已从悬浮窗移除（采集/入库保留，仅不显示）
    }

    // 4) 今日事件数
    ui.set_event_count(SharedString::from(format!(
        "今日卡顿: {}",
        state.today_event_count
    )));

    // 5) 上次心跳时间（调试用）
    if let Some(hb) = &state.last_heartbeat {
        let display = if hb.len() >= 19 { &hb[11..19] } else { hb };
        ui.set_last_heartbeat(SharedString::from(format!("心跳: {}", display)));
    } else {
        ui.set_last_heartbeat(SharedString::from(""));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把 Brush 转 RGB 元组（仅 SolidColor 颜色；非 solid 视为 0）
    fn brush_to_rgb(brush: Brush) -> (u8, u8, u8) {
        match brush {
            Brush::SolidColor(c) => (c.red(), c.green(), c.blue()),
            _ => (0, 0, 0),
        }
    }

    #[test]
    fn format_service_status_running() {
        let (text, _) = format_service_status(ServiceHealth::Running);
        assert_eq!(text.as_str(), "● 服务运行中");
    }

    #[test]
    fn format_service_status_stale() {
        let (text, _) = format_service_status(ServiceHealth::Stale);
        assert_eq!(text.as_str(), "● 服务卡顿");
    }

    #[test]
    fn format_service_status_stopped() {
        let (text, _) = format_service_status(ServiceHealth::Stopped);
        assert_eq!(text.as_str(), "● 服务已停止");
    }

    #[test]
    fn format_service_status_no_database() {
        let (text, _) = format_service_status(ServiceHealth::NoDatabase);
        assert!(text.as_str().contains("未注册"));
    }

    #[test]
    fn format_bps_auto_upgrades_unit() {
        // B/s 档：< 1024 不升级
        assert_eq!(format_bps(0), "    0 B/s");
        assert_eq!(format_bps(512), "  512 B/s");
        assert_eq!(format_bps(1023), " 1023 B/s");
        // KB/s 档：>= 1024 且 < 1024²
        assert_eq!(format_bps(1024), "  1.0 KB/s");
        assert_eq!(format_bps(1536), "  1.5 KB/s");
        assert_eq!(format_bps(1024 * 1024 - 1), "1024.0 KB/s");
        // MB/s 档：>= 1024² 且 < 1024³
        assert_eq!(format_bps(1024 * 1024), "  1.0 MB/s");
        assert_eq!(format_bps(5 * 1024 * 1024), "  5.0 MB/s");
        // GB/s 档：>= 1024³
        assert_eq!(format_bps(3 * 1024 * 1024 * 1024), "  3.0 GB/s");
    }

    #[test]
    fn format_bps_monotonic() {
        // 更大速率不会导致数字宽度爆炸（始终升级单位）
        assert!(format_bps(1024).contains("KB/s"));
        assert!(format_bps(1024 * 1024).contains("MB/s"));
        assert!(format_bps(1024 * 1024 * 1024).contains("GB/s"));
        assert!(format_bps(10 * 1024 * 1024 * 1024).contains("GB/s"));
    }

    #[test]
    fn format_service_status_colors_distinct() {
        let (_, brush_running) = format_service_status(ServiceHealth::Running);
        let (_, brush_stale) = format_service_status(ServiceHealth::Stale);
        let (_, brush_stopped) = format_service_status(ServiceHealth::Stopped);
        let (_, brush_nodb) = format_service_status(ServiceHealth::NoDatabase);

        // running 绿色 / stale 黄色 / stopped+nodb 红色
        let (r_running, g_running, _) = brush_to_rgb(brush_running);
        let (r_stale, g_stale, _) = brush_to_rgb(brush_stale);
        let (r_stopped, g_stopped, _) = brush_to_rgb(brush_stopped);
        let (r_nodb, g_nodb, _) = brush_to_rgb(brush_nodb);

        // running 偏绿（g > r）
        assert!(g_running > r_running, "running 应该偏绿: r={},g={}", r_running, g_running);
        // stale 偏黄（r ≈ g）
        assert!(r_stale > 150 && g_stale > 100, "stale 应该偏黄: r={},g={}", r_stale, g_stale);
        // stopped/nodb 偏红（r > g > b）
        assert!(r_stopped > g_stopped, "stopped 应该偏红: r={},g={}", r_stopped, g_stopped);
        // stopped 和 nodb 同色
        assert_eq!((r_stopped, g_stopped), (r_nodb, g_nodb));
    }

    #[test]
    fn overlay_state_new_defaults() {
        let s = OverlayState::new(SkinConfig::load("default"));
        assert_eq!(s.service_health, ServiceHealth::NoDatabase);
        assert!(s.last_summary.is_none());
        assert!(!s.paused);
        assert_eq!(s.today_event_count, 0);
    }

    #[test]
    fn overlay_state_update_from_poll() {
        let mut s = OverlayState::new(SkinConfig::load("default"));
        let poll = crate::reader::PollResult {
            summary: Some(LatestSampleSummary {
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                cpu_usage: 50.0,
                mem_usage_percent: 65.0,
                mem_available_mb: 4096,
                net_sent_bps: 0,
                net_recv_bps: 0,
                disk_read_bps: 0,
                disk_write_bps: 0,
                gpu_usage: None,
                cpu_temp: None,
            }),
            event: None,
            health: ServiceHealth::Running,
            today_event_count: 7,
            last_heartbeat: Some("2026-01-01T00:00:00Z".to_string()),
        };
        s.update_from_poll(&poll);
        assert_eq!(s.service_health, ServiceHealth::Running);
        assert_eq!(s.today_event_count, 7);
        assert!(s.last_summary.is_some());
        assert!(s.last_heartbeat.is_some());
    }

    /// 验证：服务停止时暂停按钮应禁用
    #[test]
    fn pause_enabled_only_when_running() {
        for h in [
            ServiceHealth::Running,
            ServiceHealth::Stale,
            ServiceHealth::Stopped,
            ServiceHealth::NoDatabase,
        ] {
            let enabled = matches!(h, ServiceHealth::Running);
            if h == ServiceHealth::Running {
                assert!(enabled);
            } else {
                assert!(!enabled);
            }
        }
    }

    /// 验证：HEALTH_BAR_HEIGHT 在合理范围
    #[test]
    fn health_bar_height_constant() {
        assert!(HEALTH_BAR_HEIGHT > 0.0);
        assert!(HEALTH_BAR_HEIGHT < 100.0);
    }

    // ===== parse_color（皮肤接线恢复）=====

    #[test]
    fn parse_color_rgb_6digit() {
        let c = parse_color("#F9E2AF").unwrap();
        assert_eq!((c.red(), c.green(), c.blue()), (0xF9, 0xE2, 0xAF));
    }

    #[test]
    fn parse_color_without_hash() {
        let c = parse_color("89B4FA").unwrap();
        assert_eq!((c.red(), c.green(), c.blue()), (0x89, 0xB4, 0xFA));
    }

    #[test]
    fn parse_color_argb_8digit() {
        let c = parse_color("#80FF0000").unwrap(); // 半透明红
        assert_eq!((c.red(), c.green(), c.blue(), c.alpha()), (0xFF, 0, 0, 0x80));
    }

    #[test]
    fn parse_color_invalid_lengths() {
        assert!(parse_color("#FFF").is_none());
        assert!(parse_color("#FF").is_none());
        assert!(parse_color("").is_none());
    }

    #[test]
    fn parse_color_invalid_hex() {
        assert!(parse_color("#GGGGGG").is_none());
        assert!(parse_color("#12345Z").is_none());
    }

    /// 验证：暂停默认 false
    #[test]
    fn paused_default_is_false() {
        let s = OverlayState::new(SkinConfig::load("default"));
        assert!(!s.paused);
    }

    /// 验证：click_through 默认关闭（右键菜单切换前的初始状态）
    #[test]
    fn click_through_default_off() {
        let s = OverlayState::new(SkinConfig::load("default"));
        assert!(!s.click_through);
    }
}
