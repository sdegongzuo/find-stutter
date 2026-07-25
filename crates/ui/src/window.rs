//! Windows 原生窗口控制：置顶、点击穿透、HWND 缓存。
//!
//! 通过 `slint::Window::window_handle()` + `raw-window-handle` 提取 HWND，
//! 再调用 Win32 API 设置/读取 `WS_EX_TRANSPARENT` 扩展样式。
//!
//! 设计要点：
//! - 拖动改用 Slint 的 TouchArea + set_position()（仅在用户实际拖动期间更新，不是每帧），
//!   避免 egui 那种「每帧 OuterPosition」更新导致透明窗重影/闪烁
//! - 点击穿透通过切换 `WS_EX_TRANSPARENT` 实现，开启后窗口鼠标事件完全穿透
//! - Slint 创建的窗口默认有 `WS_EX_TOPMOST`（由 `always-on-top: true` 触发）

use raw_window_handle::HasWindowHandle;
use slint::ComponentHandle;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, GWL_EXSTYLE, SetWindowLongPtrW, SetWindowPos, SWP_FRAMECHANGED, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, WS_EX_TRANSPARENT,
};

use crate::Overlay;

/// 设置/取消点击穿透
pub fn set_click_through_for(window: &Overlay, enable: bool) -> Result<(), String> {
    let hwnd = extract_hwnd(window).ok_or("cannot extract HWND from Slint window")?;
    apply_click_through(hwnd, enable);
    Ok(())
}

fn extract_hwnd(window: &Overlay) -> Option<HWND> {
    let slint_window = window.window();
    let handle = slint_window.window_handle();
    use raw_window_handle::RawWindowHandle;
    let raw = handle.window_handle().ok()?;
    match raw.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut _)),
        _ => None,
    }
}

fn apply_click_through(hwnd: HWND, enable: bool) {
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let flag = WS_EX_TRANSPARENT.0 as isize;
        let new_style = if enable { style | flag } else { style & !flag };
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style);
        // 触发重绘使样式生效
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
        );
    }
}

#[cfg(test)]
mod tests {
    // 纯函数逻辑（无 HWND 也能跑的部分）已在 overlay.rs / skin.rs 中覆盖。
    // 本模块强依赖 Slint Window 实例，无法在无窗口环境测试。
}
