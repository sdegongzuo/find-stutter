//! Overlay 数据层：纯函数 + 状态 + 把数据推送到 Slint 窗口。
//!
//! 这个模块没有任何 UI 框架依赖（除了 Slint），方便做单元测试。

use std::time::Instant;

use find_stutter_core::Sample;
use find_stutter_core::Severity;

use crate::skin::SkinConfig;

/// 字节数 → 易读字符串（`512 B` / `1.0 KB` / `1.0 MB` / `1.00 GB`）
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.2} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

/// 速率（字节/秒）→ 易读字符串（`512 B/s` / `1.0 KB/s` / …）
pub fn format_rate(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB/s", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB/s", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB/s", bytes as f64 / 1024.0)
    } else {
        format!("{} B/s", bytes)
    }
}

/// 闪烁边框颜色（按严重程度查表）
fn flash_color(sev: Severity) -> slint::Color {
    match sev {
        Severity::Critical => slint::Color::from_rgb_u8(255, 70, 70),
        Severity::Major => slint::Color::from_rgb_u8(255, 170, 40),
        Severity::Minor => slint::Color::from_rgb_u8(255, 200, 60),
    }
}

/// 把字符串里的 `#RRGGBB` 解析成 Slint 颜色
pub fn parse_color(hex: &str) -> slint::Color {
    let hex = hex.trim_start_matches('#');
    if hex.len() == 6 {
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
        slint::Color::from_rgb_u8(r, g, b)
    } else {
        slint::Color::from_rgb_u8(255, 255, 255)
    }
}

/// UI 端共享的悬浮窗状态
#[derive(Clone)]
pub struct OverlayState {
    pub sent_total: u64,
    pub recv_total: u64,
    pub stutter_count: u32,
    /// 最近一次触发闪烁的卡顿严重程度（仅 Major/Critical）
    pub last_stutter_severity: Option<Severity>,
    /// 闪烁提醒的截止时刻；当前时刻 < 该值时边框脉冲闪烁
    pub flash_until: Instant,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            sent_total: 0,
            recv_total: 0,
            stutter_count: 0,
            last_stutter_severity: None,
            flash_until: Instant::now(),
        }
    }
}

/// 把皮肤一次性应用到 Slint 窗口（启动时调用）
pub fn apply_skin(window: &crate::Overlay, skin: &SkinConfig) {
    window.set_bg_color(parse_color(&skin.background_color).into());
    window.set_border_color(parse_color(&skin.border_color).into());
    window.set_border_radius(skin.border_radius);
    window.set_font_size(skin.font_size as f32);
    window.set_upload_color(parse_color(&skin.upload_color).into());
    window.set_download_color(parse_color(&skin.download_color).into());
    window.set_cpu_color(parse_color(&skin.cpu_color).into());
    window.set_memory_color(parse_color(&skin.memory_color).into());
    window.set_gpu_color(parse_color(&skin.gpu_color).into());
    window.set_disk_color(parse_color(&skin.disk_color).into());
    window.set_label_color(parse_color(&skin.label_color).into());
}

/// 把最新的 `Sample` + `OverlayState` 推送到 Slint 窗口属性。
///
/// 由 1Hz 定时器调用。这里只计算文本与闪烁参数，不持有任何锁。
pub fn apply_metrics(window: &crate::Overlay, sample: &Sample, state: &OverlayState, _skin: &SkinConfig) {
    // 紧凑视图
    window.set_upload(format!("↑ {}", format_rate(sample.net_sent_bps)).into());
    window.set_download(format!("↓ {}", format_rate(sample.net_recv_bps)).into());
    window.set_cpu(format!("CPU: {:.1}%", sample.cpu_usage).into());
    window.set_memory(format!("内存: {:.1}%", sample.mem_usage_percent).into());

    let gpu_text = sample
        .gpu_usage
        .map(|g| format!("GPU: {:.1}%", g))
        .unwrap_or_else(|| "GPU: --".to_string());
    window.set_gpu(gpu_text.into());
    window.set_disk(format!(
        "硬盘: R {} / W {}",
        format_rate(sample.disk_read_bps),
        format_rate(sample.disk_write_bps)
    ).into());

    // 详情面板（展开时由 .slint 按条件渲染）
    let daily_total = state.sent_total + state.recv_total;
    window.set_detail_daily(format!("今日流量: {}", format_bytes(daily_total)).into());

    let freq = sample
        .cpu_freq_mhz
        .map(|f| format!(" @ {:.2} GHz", f / 1000.0))
        .unwrap_or_default();
    window.set_detail_cpu(format!("CPU: {:.1}%{}", sample.cpu_usage, freq).into());

    window.set_detail_memory(format!(
        "内存: {:.2} GB / {:.2} GB",
        sample.mem_used_mb as f64 / 1024.0,
        sample.mem_total_mb as f64 / 1024.0
    ).into());

    let has_gpu = sample.gpu_usage.is_some();
    window.set_show_gpu_detail(has_gpu);
    if let Some(gpu) = sample.gpu_usage {
        window.set_detail_gpu(format!("GPU: {:.1}%", gpu).into());
    }

    window.set_detail_disk(format!(
        "硬盘: R: {} W: {}",
        format_rate(sample.disk_read_bps),
        format_rate(sample.disk_write_bps)
    ).into());

    let has_temp = sample.cpu_temp.is_some();
    window.set_show_temp_detail(has_temp);
    if let Some(temp) = sample.cpu_temp {
        window.set_detail_temp(format!("CPU 温度: {:.0}°C", temp).into());
    }

    window.set_detail_count(format!("今日卡顿: {} 次", state.stutter_count).into());
    window.set_detail_proc(format!(
        "进程: {} | 线程: {}",
        sample.process_count, sample.thread_count
    ).into());

    // 闪烁边框：仅在 flash_until 之内生效
    if Instant::now() < state.flash_until {
        if let Some(sev) = state.last_stutter_severity {
            // 用 0.5~1.0 的脉动 alpha（避免每帧重新计算 sin，外部用 1Hz tick 近似）
            let now = Instant::now();
            let phase = (now.elapsed().as_secs_f32() * 6.0).sin().abs();
            let alpha = 0.55 + 0.45 * phase;
            let color = flash_color(sev);
            window.set_border_flash(slint::Brush::SolidColor(color));
            window.set_border_flash_alpha(alpha);
        }
    } else if window.get_border_flash_alpha() != 0.0 {
        window.set_border_flash_alpha(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn format_bytes_below_kb() {
        assert_eq!(format_bytes(512), "512 B");
    }

    #[test]
    fn format_bytes_one_kb() {
        assert_eq!(format_bytes(1024), "1.0 KB");
    }

    #[test]
    fn format_bytes_one_mb() {
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
    }

    #[test]
    fn format_bytes_one_gb() {
        assert_eq!(format_bytes(1_073_741_824), "1.00 GB");
    }

    #[test]
    fn format_bytes_fractional_kb() {
        assert_eq!(format_bytes(1536), "1.5 KB");
    }

    #[test]
    fn format_bytes_fractional_mb() {
        assert_eq!(format_bytes(1_572_864), "1.5 MB");
    }

    #[test]
    fn format_rate_zero() {
        assert_eq!(format_rate(0), "0 B/s");
    }

    #[test]
    fn format_rate_one_kb() {
        assert_eq!(format_rate(1024), "1.0 KB/s");
    }

    #[test]
    fn format_rate_one_mb() {
        assert_eq!(format_rate(1_048_576), "1.0 MB/s");
    }

    #[test]
    fn format_rate_one_gb() {
        assert_eq!(format_rate(1_073_741_824), "1.0 GB/s");
    }

    #[test]
    fn format_rate_fractional() {
        assert_eq!(format_rate(1536), "1.5 KB/s");
    }

    #[test]
    fn parse_color_red() {
        let c = parse_color("#FF0000");
        assert_eq!(c.red(), 255);
        assert_eq!(c.green(), 0);
        assert_eq!(c.blue(), 0);
    }

    #[test]
    fn parse_color_invalid_returns_white() {
        let c = parse_color("#XYZ");
        assert_eq!(c.red(), 255);
        assert_eq!(c.green(), 255);
        assert_eq!(c.blue(), 255);
    }

    #[test]
    fn parse_color_no_hash_prefix() {
        let c = parse_color("FF00FF");
        assert_eq!(c.red(), 255);
        assert_eq!(c.green(), 0);
        assert_eq!(c.blue(), 255);
    }

    #[test]
    fn overlay_state_default() {
        let state = OverlayState::default();
        assert_eq!(state.sent_total, 0);
        assert_eq!(state.recv_total, 0);
        assert_eq!(state.stutter_count, 0);
        assert!(state.last_stutter_severity.is_none());
    }

    #[test]
    fn overlay_state_clone() {
        let state = OverlayState {
            sent_total: 100,
            recv_total: 200,
            stutter_count: 3,
            last_stutter_severity: Some(Severity::Major),
            flash_until: Instant::now(),
        };
        let cloned = state.clone();
        assert_eq!(cloned.sent_total, 100);
        assert_eq!(cloned.recv_total, 200);
        assert_eq!(cloned.stutter_count, 3);
        assert_eq!(cloned.last_stutter_severity, Some(Severity::Major));
    }
}
