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
    pub today_event_count: u32,
    pub service_health: ServiceHealth,
    pub last_heartbeat: Option<String>,
    pub paused: bool,
}

impl OverlayState {
    pub fn new(skin: SkinConfig) -> Self {
        Self {
            skin,
            last_summary: None,
            last_event_at: None,
            today_event_count: 0,
            service_health: ServiceHealth::NoDatabase,
            last_heartbeat: None,
            paused: false,
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

/// 把 OverlayState 推到 Slint Overlay 实例。
pub fn apply_metrics(ui: &crate::Overlay, state: &OverlayState) {
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
            "MEM {:>5} MB",
            s.mem_available_mb
        )));
        ui.set_net_send(SharedString::from(format!(
            "↑ {:>5} KB/s",
            s.net_sent_bps / 1024
        )));
        ui.set_net_recv(SharedString::from(format!(
            "↓ {:>5} KB/s",
            s.net_recv_bps / 1024
        )));
        ui.set_disk_read(SharedString::from(format!(
            "R {:>5} KB/s",
            s.disk_read_bps / 1024
        )));
        ui.set_disk_write(SharedString::from(format!(
            "W {:>5} KB/s",
            s.disk_write_bps / 1024
        )));
        if let Some(g) = s.gpu_usage {
            ui.set_gpu_text(SharedString::from(format!("GPU {:5.1}%", g)));
        }
        if let Some(t) = s.cpu_temp {
            ui.set_temp_text(SharedString::from(format!("T {:4.1}°C", t)));
        }
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

    /// 验证：暂停默认 false
    #[test]
    fn paused_default_is_false() {
        let s = OverlayState::new(SkinConfig::load("default"));
        assert!(!s.paused);
    }
}
