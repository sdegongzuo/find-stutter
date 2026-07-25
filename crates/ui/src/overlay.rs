use crate::skin::SkinConfig;
use egui::{Color32, FontId, RichText, Ui};

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

#[derive(Clone)]
pub struct OverlayState {
    pub sent_total: u64,
    pub recv_total: u64,
    pub stutter_count: u32,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self { sent_total: 0, recv_total: 0, stutter_count: 0 }
    }
}

fn label(ui: &mut Ui, text: &str, color: Color32, font_size: f32) {
    ui.label(RichText::new(text).color(color).font(FontId::proportional(font_size)));
}

pub fn render_compact(ui: &mut Ui, sample: &find_stutter_core::Sample, skin: &SkinConfig) {
    ui.horizontal(|ui| {
        label(ui, &format!("↑ {}", format_rate(sample.net_sent_bps)), skin.upload_color(), skin.font_size);
        ui.add_space(12.0);
        label(ui, &format!("↓ {}", format_rate(sample.net_recv_bps)), skin.download_color(), skin.font_size);
    });
    ui.horizontal(|ui| {
        label(ui, &format!("CPU: {:.1}%", sample.cpu_usage), skin.cpu_color(), skin.font_size);
        ui.add_space(12.0);
        label(ui, &format!("内存: {:.1}%", sample.mem_usage_percent), skin.memory_color(), skin.font_size);
    });
    let gpu_text = sample.gpu_usage.map(|g| format!("GPU: {:.1}%", g)).unwrap_or_else(|| "GPU: --".into());
    let disk_text = format!("硬盘: R {} / W {}", format_rate(sample.disk_read_bps), format_rate(sample.disk_write_bps));
    ui.horizontal(|ui| {
        label(ui, &gpu_text, skin.gpu_color(), skin.font_size);
        ui.add_space(12.0);
        label(ui, &disk_text, skin.disk_color(), skin.font_size);
    });
}

pub fn render_detail(
    ui: &mut Ui,
    sample: &find_stutter_core::Sample,
    state: &OverlayState,
    skin: &SkinConfig,
) {
    ui.separator();

    let daily_total = state.sent_total + state.recv_total;
    label(ui, &format!("今日流量: {}", format_bytes(daily_total)), skin.label_color(), skin.font_size - 2.0);

    ui.horizontal(|ui| {
        let freq = sample.cpu_freq_mhz.map(|f| format!(" @ {:.2} GHz", f / 1000.0)).unwrap_or_default();
        label(ui, &format!("CPU: {:.1}%{}", sample.cpu_usage, freq), skin.cpu_color(), skin.font_size - 2.0);
    });

    label(
        ui,
        &format!("内存: {:.2} GB / {:.2} GB", sample.mem_used_mb as f64 / 1024.0, sample.mem_total_mb as f64 / 1024.0),
        skin.memory_color(), skin.font_size - 2.0,
    );

    if let Some(gpu) = sample.gpu_usage {
        label(ui, &format!("GPU: {:.1}%", gpu), skin.gpu_color(), skin.font_size - 2.0);
    }

    label(
        ui,
        &format!("硬盘: R: {} W: {}", format_rate(sample.disk_read_bps), format_rate(sample.disk_write_bps)),
        skin.disk_color(), skin.font_size - 2.0,
    );

    if let Some(temp) = sample.cpu_temp {
        label(ui, &format!("CPU 温度: {:.0}°C", temp), skin.cpu_color(), skin.font_size - 2.0);
    }

    ui.separator();

    label(ui, &format!("今日卡顿: {} 次", state.stutter_count), skin.label_color(), skin.font_size - 2.0);
    label(ui, &format!("进程: {} | 线程: {}", sample.process_count, sample.thread_count), skin.label_color(), skin.font_size - 2.0);
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
    fn overlay_state_default() {
        let state = OverlayState::default();
        assert_eq!(state.sent_total, 0);
        assert_eq!(state.recv_total, 0);
        assert_eq!(state.stutter_count, 0);
    }

    #[test]
    fn overlay_state_clone() {
        let state = OverlayState { sent_total: 100, recv_total: 200, stutter_count: 3 };
        let cloned = state.clone();
        assert_eq!(cloned.sent_total, 100);
        assert_eq!(cloned.recv_total, 200);
        assert_eq!(cloned.stutter_count, 3);
    }
}
