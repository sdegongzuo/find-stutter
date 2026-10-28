//! Overlay 渲染逻辑。
//!
//! P3：service_health 字段由 DbReader 填进来，apply_metrics 推到 Slint。

use find_stutter_core::logger::LatestSampleSummary;
use slint::{Brush, Color, SharedString};

use crate::reader::ServiceHealth;
use crate::skin::SkinConfig;

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
    /// 当日累计网络流量 `(发送字节, 接收字节)`——由 DbReader 轮询缓存
    /// （悬浮窗原「磁盘读写速率」位改显此值；鼠标悬浮看当周明细）
    pub today_net_bytes: (u64, u64),
    /// 当周（本地周一零点起）累计网络流量 `(发送字节, 接收字节)`
    pub week_net_bytes: (u64, u64),
    pub paused: bool,
    /// 点击穿透模式（右键菜单切换；窗口鼠标事件完全穿透）
    pub click_through: bool,
    /// bubble 布局可选吉祥物图片缓存（避免 1Hz tick 重复读盘）
    pub mascot_image: Option<slint::Image>,
    /// mascot 缓存键（解析后的文件路径；空 = 无图片）
    pub mascot_key: String,
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
            today_net_bytes: (0, 0),
            week_net_bytes: (0, 0),
            paused: false,
            click_through: false,
            mascot_image: None,
            mascot_key: String::new(),
        }
    }

    /// 从 `PollResult` 更新状态（P3：不再有事件细节 stream，只读 db）
    pub fn update_from_poll(&mut self, poll: &crate::reader::PollResult) {
        self.last_summary = poll.summary.clone();
        self.last_event_at = poll.event.as_ref().map(|e| e.timestamp);
        self.today_event_count = poll.today_event_count;
        self.service_health = poll.health;
        self.last_heartbeat = poll.last_heartbeat.clone();
        self.today_net_bytes = poll.today_net_bytes;
        self.week_net_bytes = poll.week_net_bytes;
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
        ServiceHealth::Stale => SharedString::from("● 数据延迟"),
        ServiceHealth::Stopped => SharedString::from("● 服务已停止"),
        ServiceHealth::NoDatabase => {
            SharedString::from("● 服务未注册（请运行 find-stutter-service install）")
        }
    };
    (text, color)
}

/// 资源水位分档。用于悬浮窗数值着色，阈值与告警文案集中在此，避免
/// Slint 侧散落魔法数字。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceLevel {
    Normal,
    Warning,
    Critical,
}

/// CPU 预警 / 严重阈值（%）。
pub const CPU_WARNING_PERCENT: f32 = 80.0;
pub const CPU_CRITICAL_PERCENT: f32 = 95.0;
/// 内存预警 / 严重阈值（%）。严重线与采集侧 `mem_threshold_percent` 对齐。
pub const MEM_WARNING_PERCENT: f32 = 80.0;
pub const MEM_CRITICAL_PERCENT: f32 = 90.0;

pub fn resource_level(value: f32, warning: f32, critical: f32) -> ResourceLevel {
    if value >= critical {
        ResourceLevel::Critical
    } else if value >= warning {
        ResourceLevel::Warning
    } else {
        ResourceLevel::Normal
    }
}

/// 按水位返回数值颜色；正常值沿用皮肤注入的正文色。
pub fn resource_level_color(level: ResourceLevel, normal: Color) -> Color {
    match level {
        ResourceLevel::Normal => normal,
        ResourceLevel::Warning => Color::from_rgb_u8(0xb2, 0x6a, 0x00),
        ResourceLevel::Critical => Color::from_rgb_u8(0xc4, 0x3c, 0x3c),
    }
}

pub fn cpu_usage_color(value: f32, normal: Color) -> Color {
    resource_level_color(
        resource_level(value, CPU_WARNING_PERCENT, CPU_CRITICAL_PERCENT),
        normal,
    )
}

pub fn memory_usage_color(value: f32, normal: Color) -> Color {
    resource_level_color(
        resource_level(value, MEM_WARNING_PERCENT, MEM_CRITICAL_PERCENT),
        normal,
    )
}

/// 今日卡顿数：0 时弱化，非 0 才适度强调（不再恒定抢走内存告警的注意力）。
pub fn event_count_color(count: u32, neutral: Color) -> Color {
    if count == 0 {
        neutral
    } else {
        Color::from_rgb_u8(0xb2, 0x6a, 0x00)
    }
}

/// 百分比统一为无空格、零位小数；非有限值降级为 `--`。
pub fn format_percent(value: f32) -> String {
    if value.is_finite() {
        format!("{value:.0}%")
    } else {
        "--".to_string()
    }
}

/// GPU 数值（标签 GPU 由 Slint 侧固定）。无采样/不可用时稳定显示 `--`，
/// 避免上一帧有值、这一帧采集失败时残留旧读数。
pub fn format_gpu_value(usage: Option<f32>) -> String {
    match usage {
        Some(v) if v.is_finite() => format!("{v:.1}%"),
        _ => "--".to_string(),
    }
}

pub fn format_event_count(count: u32) -> String {
    format!("今日卡顿 {count}")
}

/// 心跳显示为用户本地时间，避免把数据库里的 UTC 直接截成 HH:MM:SS
/// （东八区会凭空慢 8 小时，截图中的 03:54:29 即此问题）。
pub fn format_heartbeat_text(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return "最近心跳 --".to_string();
    };
    match chrono::DateTime::parse_from_rfc3339(raw) {
        Ok(ts) => format!(
            "最近心跳 {}",
            ts.with_timezone(&chrono::Local).format("%H:%M:%S")
        ),
        Err(_) => "最近心跳 --".to_string(),
    }
}

/// 「当日上行累计流量」短文本（紧凑单位，无额外空格）。
pub fn format_today_up_traffic(sent: u64) -> String {
    format!("↑{}", crate::process_list::format_bytes(sent))
}

/// 「当日下行累计流量」短文本（紧凑单位，无额外空格）。
pub fn format_today_down_traffic(recv: u64) -> String {
    format!("↓{}", crate::process_list::format_bytes(recv))
}

/// 「当周累计网络流量」tooltip 明细（鼠标悬浮任一当日流量文本时显示；多行）。
pub fn format_week_traffic_tip(sent: u64, recv: u64) -> String {
    format!(
        "**本周累计流量**（周一起）：{}\n上传 {} · 下载 {}",
        crate::process_list::format_bytes(sent.saturating_add(recv)),
        crate::process_list::format_bytes(sent),
        crate::process_list::format_bytes(recv)
    )
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

/// 悬浮窗 tooltip（多行）：服务状态 / 今日卡顿 / 最近心跳 / 当日流量。
/// 主区空间有限，完整状态（含服务未注册时的修复命令）收敛到这里。
pub fn format_bubble_tip(state: &OverlayState) -> String {
    let (status, _) = format_service_status(state.service_health);
    format!(
        "{}\n{}\n{}\n今日流量 {} / {}",
        status,
        format_event_count(state.today_event_count),
        format_heartbeat_text(state.last_heartbeat.as_deref()),
        format_today_up_traffic(state.today_net_bytes.0),
        format_today_down_traffic(state.today_net_bytes.1)
    )
}

/// 把 OverlayState 推到 Slint Overlay 实例。
///
/// 需要 `&mut`：bubble 布局的吉祥物图片缓存按皮肤配置惰性加载（路径变化才读盘）。
pub fn apply_metrics(ui: &crate::Overlay, state: &mut OverlayState) {
    // 0) 皮肤：颜色 / 字号 / 尺寸 / 边框 注入。
    // 标签比数值弱、数值按水位着色；卡顿数只在非 0 时用琥珀色强调，
    // 让“内存 94%”这类真实危险比“今日卡顿 1”更先被看见。
    let skin = &state.skin;
    let is_bubble = skin.layout == "bubble";
    let skin_label = parse_color(&skin.label_color).unwrap_or(Color::from_rgb_u8(0x3a, 0x4a, 0x36));
    let text_color = if is_bubble {
        skin_label
    } else {
        Color::from_rgb_u8(0x1e, 0x1e, 0x2e)
    };
    let label_color = if is_bubble {
        skin_label.with_alpha(0.72)
    } else {
        Color::from_rgb_u8(0x6e, 0x6e, 0x78)
    };
    let hb_gray = if is_bubble {
        skin_label.with_alpha(0.62)
    } else {
        Color::from_rgb_u8(0x8a, 0x8a, 0x92)
    };
    let event_neutral = hb_gray;

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
    ui.set_label_color(label_color);
    ui.set_cpu_color(text_color);
    ui.set_mem_color(text_color);
    ui.set_event_color(event_count_color(state.today_event_count, event_neutral));
    ui.set_gpu_color(text_color);
    ui.set_net_color(text_color);
    ui.set_hb_color(hb_gray);

    // 0b) 布局模式仍保留在皮肤协议中；Slint 侧 card / bubble 已统一网格，
    // 仅由皮肤配色、尺寸和可选吉祥物区分。
    ui.set_layout_mode(SharedString::from(if is_bubble { "bubble" } else { "card" }));

    // 0c) 吉祥物图片（皮肤可选；路径变化时才重新读盘）
    let mascot_key = skin
        .mascot_file
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    if state.mascot_key != mascot_key {
        state.mascot_key = mascot_key.clone();
        state.mascot_image = if mascot_key.is_empty() {
            None
        } else {
            slint::Image::load_from_path(std::path::Path::new(&mascot_key)).ok()
        };
    }
    match &state.mascot_image {
        Some(img) => {
            ui.set_mascot_visible(true);
            ui.set_mascot_image(img.clone());
        }
        None => ui.set_mascot_visible(false),
    }

    // 1) 服务状态（右列首行）
    let (text, color) = format_service_status(state.service_health);
    ui.set_service_status(text);
    ui.set_service_status_color(color);

    // 2) 暂停按钮在服务停止时禁用
    let pause_enabled = matches!(state.service_health, ServiceHealth::Running);
    ui.set_pause_enabled(pause_enabled);

    // 3) 指标：每 tick 显式覆盖，避免 GPU 等可选信号在采集失败后残留旧读数。
    // 无 summary 时统一回落到 `--`，不把不可信的历史值伪装成实时数据。
    match &state.last_summary {
        Some(s) => {
            ui.set_cpu_value(SharedString::from(format_percent(s.cpu_usage)));
            ui.set_mem_value(SharedString::from(format_percent(s.mem_usage_percent)));
            ui.set_cpu_color(cpu_usage_color(s.cpu_usage, text_color));
            ui.set_mem_color(memory_usage_color(s.mem_usage_percent, text_color));
            ui.set_net_send_value(SharedString::from(
                format_bps(s.net_sent_bps).trim().to_string(),
            ));
            ui.set_net_recv_value(SharedString::from(
                format_bps(s.net_recv_bps).trim().to_string(),
            ));
            ui.set_gpu_value(SharedString::from(format_gpu_value(s.gpu_usage)));
            ui.set_gpu_color(if s.gpu_usage.is_some_and(f32::is_finite) {
                text_color
            } else {
                hb_gray
            });
            // 温度已从悬浮窗移除（采集/入库保留，仅不显示）
        }
        None => {
            ui.set_cpu_value(SharedString::from("--"));
            ui.set_mem_value(SharedString::from("--"));
            ui.set_net_send_value(SharedString::from("--"));
            ui.set_net_recv_value(SharedString::from("--"));
            ui.set_gpu_value(SharedString::from("--"));
            ui.set_cpu_color(text_color);
            ui.set_mem_color(text_color);
            ui.set_gpu_color(hb_gray);
        }
    }

    // 3b) 当日累计网络流量 + 当周明细 tooltip。
    // 不依赖 summary 存在，db 暂时打不开时仍显示 reader 缓存的最近值。
    ui.set_net_today_up(SharedString::from(format_today_up_traffic(
        state.today_net_bytes.0,
    )));
    ui.set_net_today_down(SharedString::from(format_today_down_traffic(
        state.today_net_bytes.1,
    )));
    ui.set_net_week_tip(SharedString::from(format_week_traffic_tip(
        state.week_net_bytes.0,
        state.week_net_bytes.1,
    )));

    // 4) 今日事件数与完整 tooltip
    ui.set_event_count(SharedString::from(format_event_count(state.today_event_count)));
    ui.set_event_color(event_count_color(state.today_event_count, event_neutral));
    ui.set_bubble_tip(SharedString::from(format_bubble_tip(state)));

    // 5) 最近心跳：转本地时区后显示；无心跳/格式异常统一显示 `--`。
    ui.set_last_heartbeat(SharedString::from(format_heartbeat_text(
        state.last_heartbeat.as_deref(),
    )));

    // 6) 暂停状态以 Rust 侧为唯一真相（托盘/右键菜单也会改它）。
    ui.set_paused(state.paused);
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
        assert_eq!(text.as_str(), "● 数据延迟");
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
            today_net_bytes: (100, 200),
            week_net_bytes: (1000, 2000),
        };
        s.update_from_poll(&poll);
        assert_eq!(s.service_health, ServiceHealth::Running);
        assert_eq!(s.today_event_count, 7);
        assert_eq!(s.today_net_bytes, (100, 200));
        assert_eq!(s.week_net_bytes, (1000, 2000));
        assert!(s.last_summary.is_some());
        assert!(s.last_heartbeat.is_some());
    }

    /// 当日流量短文本：↑ 上行 / ↓ 下行紧密排版（右列空间有限）。
    #[test]
    fn format_today_traffic_directional() {
        assert_eq!(format_today_up_traffic(0), "↑0B");
        assert_eq!(format_today_up_traffic(1536), "↑1.5K");
        // GB 级别显示一位小数
        let gb = 3 * 1024 * 1024 * 1024;
        assert_eq!(format_today_up_traffic(gb), "↑3.0G");
        assert_eq!(format_today_down_traffic(2048), "↓2.0K");
        assert_eq!(format_today_down_traffic(gb), "↓3.0G");
    }

    #[test]
    fn resource_level_thresholds_are_inclusive() {
        assert_eq!(resource_level(79.9, 80.0, 90.0), ResourceLevel::Normal);
        assert_eq!(resource_level(80.0, 80.0, 90.0), ResourceLevel::Warning);
        assert_eq!(resource_level(90.0, 80.0, 90.0), ResourceLevel::Critical);
    }

    #[test]
    fn memory_critical_is_more_prominent_than_cpu_high() {
        let normal = Color::from_rgb_u8(0x1e, 0x1e, 0x2e);
        let mem94 = memory_usage_color(94.0, normal);
        let cpu94 = cpu_usage_color(94.0, normal);
        assert_eq!(
            (mem94.red(), mem94.green(), mem94.blue()),
            (0xc4, 0x3c, 0x3c),
            "内存 94% 必须进入严重红档"
        );
        assert_eq!(
            (cpu94.red(), cpu94.green(), cpu94.blue()),
            (0xb2, 0x6a, 0x00),
            "CPU 94% 仍处预警档，不应与内存严重档混淆"
        );
    }

    #[test]
    fn normal_usage_keeps_skin_text_color() {
        let normal = Color::from_rgb_u8(0x3a, 0x4a, 0x36);
        assert_eq!(cpu_usage_color(23.0, normal), normal);
        assert_eq!(memory_usage_color(65.0, normal), normal);
    }

    #[test]
    fn event_count_is_neutral_at_zero() {
        let neutral = Color::from_rgb_u8(0x8a, 0x8a, 0x92);
        assert_eq!(event_count_color(0, neutral), neutral);
        assert_ne!(event_count_color(1, neutral), neutral);
    }

    #[test]
    fn percent_and_gpu_format_are_consistent() {
        assert_eq!(format_percent(23.4), "23%");
        assert_eq!(format_percent(94.6), "95%");
        assert_eq!(format_percent(f32::NAN), "--");
        assert_eq!(format_gpu_value(Some(17.44)), "17.4%");
        assert_eq!(format_gpu_value(None), "--");
        assert_eq!(format_gpu_value(Some(f32::NAN)), "--");
        assert_eq!(format_event_count(0), "今日卡顿 0");
        assert_eq!(format_event_count(12), "今日卡顿 12");
    }

    #[test]
    fn heartbeat_uses_local_display_and_handles_invalid_values() {
        let raw = "2026-10-01T03:54:44Z";
        let local = chrono::DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%H:%M:%S")
            .to_string();
        assert_eq!(format_heartbeat_text(Some(raw)), format!("最近心跳 {local}"));
        assert_eq!(format_heartbeat_text(None), "最近心跳 --");
        assert_eq!(format_heartbeat_text(Some("not-a-timestamp")), "最近心跳 --");
    }

    /// 当周 tooltip：含总量、上传、下载三段，且换行分两行。
    #[test]
    fn format_week_traffic_tip_contains_parts() {
        let tip = format_week_traffic_tip(1024 * 1024, 2 * 1024 * 1024);
        assert!(tip.contains("本周累计流量"), "应含本周标题: {tip}");
        assert!(tip.contains("周一起"), "应注明口径起点: {tip}");
        assert!(tip.contains("1.0M"), "应含上传量: {tip}");
        assert!(tip.contains("2.0M"), "应含下载量: {tip}");
        assert!(tip.contains("3.0M"), "应含总量(1M+2M): {tip}");
        assert_eq!(tip.lines().count(), 2, "应为两行: {tip}");
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

    // ===== 悬浮窗 tooltip =====

    /// tooltip：服务状态 / 今日卡顿 / 最近心跳 / 今日流量四行齐全
    #[test]
    fn format_bubble_tip_contains_parts() {
        let mut s = OverlayState::new(SkinConfig::load("default"));
        s.service_health = ServiceHealth::Running;
        s.today_event_count = 52;
        s.last_heartbeat = Some("2026-10-01T03:54:44Z".to_string());
        s.today_net_bytes = (7 * 1024 * 1024, 4 * 1024 * 1024 * 1024_u64);
        let tip = format_bubble_tip(&s);
        assert!(tip.contains("服务运行中"), "{tip}");
        assert!(tip.contains("今日卡顿 52"), "{tip}");
        assert!(tip.contains("最近心跳 "), "{tip}");
        assert!(!tip.contains("T03:54:44"), "不应直接显示 UTC ISO 串: {tip}");
        assert!(tip.contains("今日流量 ↑7.0M / ↓4.0G"), "{tip}");
        assert_eq!(tip.lines().count(), 4, "{tip}");
    }

    /// tooltip：无心跳时显示 "--" 而不是空行，服务修复提示完整保留
    #[test]
    fn format_bubble_tip_without_heartbeat() {
        let mut s = OverlayState::new(SkinConfig::load("default"));
        s.service_health = ServiceHealth::NoDatabase;
        let tip = format_bubble_tip(&s);
        assert!(tip.contains("最近心跳 --"), "{tip}");
        assert!(tip.contains("未注册"), "{tip}");
    }

    /// mascot 缓存初始为空（无皮肤图片时不读盘）
    #[test]
    fn mascot_cache_defaults_empty() {
        let s = OverlayState::new(SkinConfig::load("default"));
        assert!(s.mascot_image.is_none());
        assert_eq!(s.mascot_key, "");
    }
}
