//! P2：伪任务栏窗口（任务栏嵌入的轻量实现）。
//!
//! 用 Slint 的第二个窗口（`Taskbar` 组件）实现一个横向窄条：
//! - 无边框、半透明、置顶（与 Overlay 一致）
//! - 默认定位在**工作区底部中央**（避开真正的系统任务栏）
//! - 可拖动：用户拖到任务栏空白处即完成「嵌入」
//! - tick 里由 [`apply_taskbar_metrics`] 同步指标
//!
//! 说明：这是 PLAN §3.5 的 Phase 1「伪任务栏窗口」方案，
//! 不做 DeskBand 注入（Win7/10/11 兼容性成本高、沙箱无法验证）。
//!
//! 开关：`config.toml [ui] taskbar = true`（默认 false）。

use slint::ComponentHandle;

use crate::reader::ServiceHealth;
use crate::OverlayState;

/// 任务栏窗口句柄：持有 Slint Taskbar 组件，防止被 drop。
pub struct TaskbarWindow {
    ui: crate::Taskbar,
}

impl TaskbarWindow {
    /// 创建任务栏窗口并显示。
    pub fn show() -> anyhow::Result<Self> {
        let ui = crate::Taskbar::new()?;
        let weak = ui.as_weak();
        ui.on_drag_moved(move |dx, dy| {
            use slint::PhysicalPosition;
            if let Some(ui) = weak.upgrade() {
                let window = ui.window();
                let scale = window.scale_factor();
                let pos = window.position();
                let new_x = pos.x + (dx as f32 * scale) as i32;
                let new_y = pos.y + (dy as f32 * scale) as i32;
                window.set_position(PhysicalPosition::new(new_x, new_y));
            }
        });
        ui.show()?;
        // 任务栏窗口也不显示在 Windows 系统任务栏（工具窗口样式，
        // 避免在系统任务栏上再占一个按钮，与「伪任务栏」设计一致）
        crate::window::ensure_tool_window_for(ui.window());
        // winit 在 show 后会重算扩展样式（覆盖 WS_EX_TOOLWINDOW / 加回
        // WS_EX_APPWINDOW），延迟 500ms 再补一次；长期由 Overlay 的
        // 1Hz tick 守护（lib.rs）。
        let weak = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
            if let Some(ui) = weak.upgrade() {
                crate::window::ensure_tool_window_for(ui.window());
            }
        });
        position_at_bottom_center(&ui);
        Ok(Self { ui })
    }

    /// 底层 Slint 窗口（供 tick 守护重新设置任务栏样式）。
    pub fn window(&self) -> &slint::Window {
        self.ui.window()
    }

    /// 把 OverlayState 的最新指标推给任务栏窗口。
    pub fn apply(&self, state: &OverlayState) {
        apply_taskbar_metrics(&self.ui, state);
    }
}

/// 把任务栏窗口定位到工作区底部中央。
///
/// 用 `SystemParametersInfoW(SPI_GETWORKAREA)` 拿工作区（排除系统任务栏），
/// 窗口宽度由 slint 定义（340px），横向居中、贴底。
#[cfg(windows)]
fn position_at_bottom_center(ui: &crate::Taskbar) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    unsafe {
        let mut rect = windows::Win32::Foundation::RECT::default();
        let ok = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut rect as *mut _ as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        if ok.is_ok() {
            let window = ui.window();
            let scale = window.scale_factor();
            let w_logical = 340.0;
            let h_logical = 28.0;
            let w_phys = (w_logical * scale) as i32;
            let h_phys = (h_logical * scale) as i32;
            let x = rect.right - w_phys - ((rect.right - rect.left - w_phys) / 2);
            let y = rect.bottom - h_phys;
            // 屏幕坐标 → 窗口坐标（物理像素）
            use slint::PhysicalPosition;
            window.set_position(PhysicalPosition::new(x, y));
        }
    }
}

#[cfg(not(windows))]
fn position_at_bottom_center(_ui: &crate::Taskbar) {}

/// 把 OverlayState 推到 Taskbar Slint 组件。
pub fn apply_taskbar_metrics(ui: &crate::Taskbar, state: &OverlayState) {
    use slint::{Brush, SharedString};

    // 皮肤注入
    let skin = &state.skin;
    ui.set_skin_bg(Brush::SolidColor(
        crate::overlay::parse_color(&skin.background_color)
            .unwrap_or(slint::Color::from_rgb_u8(0xf5, 0xf5, 0xf7)),
    ));
    ui.set_cpu_color(
        crate::overlay::parse_color(&skin.cpu_color)
            .unwrap_or(slint::Color::from_rgb_u8(0x37, 0x47, 0x4f)),
    );
    ui.set_mem_color(
        crate::overlay::parse_color(&skin.memory_color)
            .unwrap_or(slint::Color::from_rgb_u8(0x6a, 0x1b, 0x9a)),
    );
    ui.set_gpu_color(
        crate::overlay::parse_color(&skin.gpu_color)
            .unwrap_or(slint::Color::from_rgb_u8(0x00, 0x69, 0x5c)),
    );
    ui.set_net_color(
        crate::overlay::parse_color(&skin.download_color)
            .unwrap_or(slint::Color::from_rgb_u8(0x15, 0x65, 0xc0)),
    );
    ui.set_disk_color(
        crate::overlay::parse_color(&skin.disk_color)
            .unwrap_or(slint::Color::from_rgb_u8(0xad, 0x14, 0x57)),
    );
    ui.set_event_color(
        crate::overlay::parse_color(&skin.label_color)
            .unwrap_or(slint::Color::from_rgb_u8(0x54, 0x6e, 0x7a)),
    );
    ui.set_text_size(skin.font_size as f32);

    // 指标
    if let Some(s) = &state.last_summary {
        ui.set_cpu_text(SharedString::from(format!("CPU {:5.1}%", s.cpu_usage)));
        ui.set_mem_text(SharedString::from(format!(
            "内存 {:4.1}%",
            s.mem_usage_percent
        )));
        ui.set_gpu_text(SharedString::from(
            s.gpu_usage
                .map(|g| format!("GPU {:5.1}%", g))
                .unwrap_or_else(|| "GPU --".into()),
        ));
        ui.set_net_text(SharedString::from(format!(
            "↑{:>3}K ↓{:>3}K",
            s.net_sent_bps / 1024,
            s.net_recv_bps / 1024
        )));
        ui.set_disk_text(SharedString::from(format!(
            "R{:>3}K W{:>3}K",
            s.disk_read_bps / 1024,
            s.disk_write_bps / 1024
        )));
    }

    // 事件计数
    ui.set_event_text(SharedString::from(format!("卡顿 {}", state.today_event_count)));

    // 服务健康
    let (text, color) = crate::overlay::format_service_status(state.service_health);
    ui.set_service_text(text);
    ui.set_service_color(color);

    let _ = ServiceHealth::Running; // 保证类型引用（apply 用不到判别，仅为语义完整性）
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::OverlayState;
    use crate::skin::SkinConfig;

    /// 验证：任务栏窗口状态来自 OverlayState（字段读路径不 panic）
    #[test]
    fn taskbar_metrics_reads_state() {
        let state = OverlayState::new(SkinConfig::default());
        // apply_taskbar_metrics 需要 Slint 实例，无法在无窗口环境跑；
        // 这里只验证状态构造 + 皮肤读取路径可用
        assert!(state.last_summary.is_none());
        assert_eq!(state.today_event_count, 0);
    }

    /// 验证：format_service_status 与任务栏共用（颜色/文本契约一致）
    #[test]
    fn service_status_compatible_with_taskbar() {
        let (text, _) = crate::overlay::format_service_status(ServiceHealth::Running);
        assert!(text.as_str().contains("运行中"));
    }
}
