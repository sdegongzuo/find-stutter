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
    SWP_NOSIZE, SWP_NOZORDER, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
};

use crate::Overlay;

/// 设置/取消点击穿透
pub fn set_click_through_for(window: &Overlay, enable: bool) -> Result<(), String> {
    let hwnd = extract_hwnd(window).ok_or("cannot extract HWND from Slint window")?;
    apply_click_through(hwnd, enable);
    Ok(())
}

fn extract_hwnd(window: &Overlay) -> Option<HWND> {
    extract_hwnd_from(window.window())
}

/// 从任意 Slint 组件提取 HWND（Overlay / ProcessList / Taskbar 通用）。
pub fn extract_hwnd_from(win: &slint::Window) -> Option<HWND> {
    let handle = win.window_handle();
    use raw_window_handle::RawWindowHandle;
    let raw = handle.window_handle().ok()?;
    match raw.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut _)),
        _ => None,
    }
}

/// release 模式下隐藏 GUI 启动时的控制台黑框；debug 保留（方便看日志）。
///
/// 实现：`FreeConsole()` 解除当前进程与控制台的关联。选它而不是
/// `ShowWindow(GetConsoleWindow(), SW_HIDE)` 的原因：从 cmd 启动时
/// ShowWindow 会把**父 cmd 窗口**一起隐藏掉；FreeConsole 只影响本进程，
/// cmd 窗口不受影响。
///
/// 注意：仅主 GUI 入口（crates/bin 无子命令分支）在 release 下调用；
/// CLI 子命令（export / stats）需要控制台输出，**不得**调用本函数。
#[cfg(windows)]
pub fn hide_console_for_gui() {
    if !cfg!(debug_assertions) {
        unsafe {
            let _ = windows::Win32::System::Console::FreeConsole();
        }
    }
}

#[cfg(not(windows))]
pub fn hide_console_for_gui() {}

/// 让窗口不出现在 **Windows 系统任务栏**（工具窗口样式 `WS_EX_TOOLWINDOW`）。
///
/// 悬浮窗 / 进程详情页 / 任务栏窗口都适用：设置后窗口不占系统任务栏按钮，
/// 也不会出现在 Alt-Tab 切换列表里。
///
/// ## 为什么是「ensure」而不是「set once」
///
/// winit（Slint 的 Windows backend）在窗口 show / 状态变化时会**重新计算扩展
/// 样式**（`update_ex_style`），把我们手动设置的位清掉，且默认会加回
/// `WS_EX_APPWINDOW`（0x40000，强制显示在任务栏）。实测：show() 后立即设置
/// 无效（ExStyle 仍是 0x40118）。
///
/// 因此本函数做两件事：
/// 1. 置位 `WS_EX_TOOLWINDOW`（0x80）
/// 2. 清除 `WS_EX_APPWINDOW`（0x40000）
///
/// 调用方应**周期性重复调用**（Overlay 的 1Hz tick 守护三个窗口），
/// 确保 winit 覆盖后 1 秒内自动恢复。函数幂等，无样式变化时不重绘。
pub fn ensure_tool_window_for(win: &slint::Window) {
    if let Some(hwnd) = extract_hwnd_from(win) {
        ensure_tool_window(hwnd);
    }
}

/// 设置 `WS_EX_TOOLWINDOW` + 清除 `WS_EX_APPWINDOW`（纯位运算，可单测）。
fn ensure_tool_window(hwnd: HWND) {
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let tool = WS_EX_TOOLWINDOW.0 as isize;
        let appwin = WS_EX_APPWINDOW.0 as isize;
        let new_style = (style | tool) & !appwin;
        if new_style != style {
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
}

/// 右键菜单项 ID（TrackPopupMenu 返回的命令码）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeMenuCmd {
    /// 暂停/恢复监控
    TogglePause = 1,
    /// 点击穿透
    ToggleClickThrough = 2,
    /// 退出
    Quit = 3,
    /// 显示进程详情列表（P2）
    ProcessList = 4,
}

/// 在鼠标位置弹出 Windows 原生右键菜单（`TrackPopupMenu`）。
///
/// - `x_logical` / `y_logical`：Slint 窗口内的**逻辑**坐标（来自 TouchArea
///   pointer-event 的 mouse-x/mouse-y）；内部换算成**屏幕物理坐标**后弹出，
///   因此菜单可以超出悬浮窗区域（不受 Slint 窗口大小限制）。
/// - `paused`：当前暂停状态，决定菜单项显示「暂停监控」还是「恢复监控」。
///
/// 返回用户选择的菜单项；用户点击空白处 / 按 Esc 返回 `None`。
///
/// 注意：`TrackPopupMenu` 是**模态阻塞**调用（TPM_RETURNCMD 等用户选择后
/// 才返回），需在 UI 线程调用；Slint callback 里调用即可。
pub fn show_context_menu(
    window: &Overlay,
    x_logical: f32,
    y_logical: f32,
    paused: bool,
) -> Option<NativeMenuCmd> {
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, TrackPopupMenu, MF_STRING, TPM_NONOTIFY,
        TPM_RETURNCMD, TPM_RIGHTBUTTON,
    };

    let hwnd = extract_hwnd(window)?;
    let scale = window.window().scale_factor();

    unsafe {
        let hmenu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(e) => {
                log::warn!("CreatePopupMenu 失败: {}", e);
                return None;
            }
        };

        let toggle_text = if paused { "恢复监控" } else { "暂停监控" };
        let _ = AppendMenuW(hmenu, MF_STRING, NativeMenuCmd::TogglePause as usize, wide(toggle_text));
        let _ = AppendMenuW(
            hmenu,
            MF_STRING,
            NativeMenuCmd::ToggleClickThrough as usize,
            wide("点击穿透"),
        );
        let _ = AppendMenuW(
            hmenu,
            MF_STRING,
            NativeMenuCmd::ProcessList as usize,
            wide("进程详情"),
        );
        let _ = AppendMenuW(hmenu, MF_STRING, NativeMenuCmd::Quit as usize, wide("退出"));

        // 屏幕坐标 = 窗口物理位置 + 逻辑坐标 × 缩放
        let pos = window.window().position();
        let screen_x = pos.x + (x_logical * scale) as i32;
        let screen_y = pos.y + (y_logical * scale) as i32;

        let cmd = TrackPopupMenu(
            hmenu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            screen_x,
            screen_y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(hmenu);
        // TrackPopupMenu 的模态循环会"吞掉"鼠标按键弹起事件（up 都发给了
        // 菜单窗口），悬浮窗收不到 → Slint TouchArea 残留 pressed 状态 →
        // 菜单关闭后移动鼠标被误判为拖动（悬浮窗跟着鼠标走）。
        // 这里释放捕获 + 合成一次右键弹起，复位 winit/Slint 的鼠标状态机。
        reset_mouse_state_after_menu(hwnd);

        match cmd.0 as u32 {
            1 => Some(NativeMenuCmd::TogglePause),
            2 => Some(NativeMenuCmd::ToggleClickThrough),
            3 => Some(NativeMenuCmd::Quit),
            4 => Some(NativeMenuCmd::ProcessList),
            _ => None,
        }
    }
}

/// 进程详情列表行右键菜单命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowMenuCmd {
    /// 打开进程可执行文件所在位置（资源管理器打开并选中该文件）
    OpenLocation = 1,
    /// 停止进程
    Kill = 2,
}

/// 鼠标右键虚拟键码（`GetAsyncKeyState` 检测右键按下 / 释放用）。
/// 模块级常量：`show_row_menu_once` 与 `wait_rbutton_release` 共用。
pub const VK_RBUTTON: i32 = 0x02;

/// 行右键菜单单次弹出的结果（用单一枚举让「关闭方式」只能与命令互斥，
/// 编译器保证不出现 `(None, Command)` 这类非法组合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowMenuOutcome {
    /// 用户选择了命令（打开位置 / 停止进程）
    Command(RowMenuCmd),
    /// 菜单被**右键**点击外部关闭（用户在别处又按了右键 → 可能想切换进程，
    /// 调用方应命中测试鼠标新位置，若在另一行则重新弹出）
    Switch,
    /// 左键点击外部 / Esc 关闭（结束菜单流程）
    Cancelled,
}

/// 弹一次行右键菜单（顶部标题行 + 「打开文件所在的位置」+「停止进程」），
/// **不循环**。返回 [`RowMenuOutcome`]：
///
/// - `Command(cmd)`：用户选择了菜单项
/// - `Switch`：菜单被**右键**点击外部关闭 → 连续右键切换流程
/// - `Cancelled`：左键/Esc 关闭，流程结束
///
/// 弹出位置用 `GetCursorPos()` 取**鼠标当前屏幕坐标**（右键按下瞬间光标就在
/// 目标行上），不依赖 Slint 坐标换算——`mouse-x` 是行内局部坐标（丢 ListView
/// 偏移），`absolute-position` 是 item 布局位置（ListView 池化复用 + 绑定缓存，
/// 连续右键不同行可能拿到旧值），两者都不可靠。
///
/// 右键关闭检测：`TrackPopupMenu` 前先调 `GetAsyncKeyState(VK_RBUTTON)` 清除
/// 历史位（低位 1 = 自上次调用以来被按下过），模态结束后再读一次：
/// 模态期间若发生右键按下，低位必为 1（user32 菜单循环走 GetKeyState，
/// 不清除 GetAsyncKeyState 的历史位）。
pub fn show_row_menu_once(
    window: &slint::Window,
    pid: i32,
    name: &str,
) -> RowMenuOutcome {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, TrackPopupMenu, MF_GRAYED,
        MF_SEPARATOR, MF_STRING, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    };

    let Some(hwnd) = extract_hwnd_from(window) else {
        return RowMenuOutcome::Cancelled;
    };

    unsafe {
        let hmenu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(e) => {
                log::warn!("CreatePopupMenu 失败: {}", e);
                return RowMenuOutcome::Cancelled;
            }
        };
        // 顶部标题行：显示 `{name} (PID {pid})`，灰色不可点击
        let title = format!("{} (PID {})", name, pid);
        let _ = AppendMenuW(hmenu, MF_GRAYED | MF_STRING, 0, wide(&title));
        // 分隔线
        let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, windows::core::PCWSTR(std::ptr::null()));
        // 服务条目（pid<=0）没有可定位的可执行文件 → 置灰「打开文件所在的位置」
        let locate_flags = if pid <= 0 { MF_GRAYED } else { MF_STRING };
        let _ = AppendMenuW(
            hmenu,
            locate_flags,
            RowMenuCmd::OpenLocation as usize,
            wide("打开文件所在的位置"),
        );
        let _ = AppendMenuW(hmenu, MF_STRING, RowMenuCmd::Kill as usize, wide("停止进程"));

        // 鼠标当前位置（物理屏幕像素）作为菜单弹出点
        let mut pt = windows::Win32::Foundation::POINT::default();
        let _ = GetCursorPos(&mut pt);

        // 清除 GetAsyncKeyState 历史位：确保下面的低位检测只反映模态期间
        let _ = GetAsyncKeyState(VK_RBUTTON);
        let cmd = TrackPopupMenu(
            hmenu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        // 模态期间是否发生过右键按下（历史位，低位 1）
        let right_clicked_away = GetAsyncKeyState(VK_RBUTTON) & 0x0001 != 0;
        let _ = DestroyMenu(hmenu);
        // TrackPopupMenu 的模态循环会"吞掉"up 事件 → 复位 winit/Slint 鼠标状态机
        reset_mouse_state_after_menu(hwnd);

        match cmd.0 as u32 {
            1 => RowMenuOutcome::Command(RowMenuCmd::OpenLocation),
            2 => RowMenuOutcome::Command(RowMenuCmd::Kill),
            _ if right_clicked_away => RowMenuOutcome::Switch,
            _ => RowMenuOutcome::Cancelled,
        }
    }
}

/// `TrackPopupMenu` 的模态消息循环会消费鼠标按键弹起事件，导致 Slint
/// TouchArea 残留 `pressed` 状态（菜单关闭后移动鼠标被误判为拖动）。
///
/// 菜单返回后调用：释放鼠标捕获 + 向窗口合成一次右键弹起事件，
/// 复位 winit/Slint 的鼠标状态机，使悬浮窗在菜单关闭后保持原地不动。
fn reset_mouse_state_after_menu(hwnd: HWND) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetCursorPos, PostMessageW, WM_RBUTTONUP,
    };
    unsafe {
        let _ = ReleaseCapture();
        // 取鼠标当前位置（屏幕坐标）→ 转窗口客户区坐标，作为合成事件的坐标，
        // 避免 winit 解析到 (0,0) 造成光标位置跳变
        let mut pt = windows::Win32::Foundation::POINT::default();
        if GetCursorPos(&mut pt).is_ok() {
            let _ = ScreenToClient(hwnd, &mut pt);
            // 光标可能在窗口外（ScreenToClient 得负坐标）→ clamp 进客户区，
            // 防止负值被 `as u32` 包装成超大 LPARAM
            let mut rc = windows::Win32::Foundation::RECT::default();
            if GetClientRect(hwnd, &mut rc).is_ok() {
                let cx = pt.x.clamp(0, rc.right.saturating_sub(1).max(0));
                let cy = pt.y.clamp(0, rc.bottom.saturating_sub(1).max(0));
                let lparam = (((cy as u32) & 0xffff) << 16) | ((cx as u32) & 0xffff);
                let _ = PostMessageW(Some(hwnd), WM_RBUTTONUP, WPARAM(0), LPARAM(lparam as isize));
            }
        }
    }
}

/// 等待用户松开右键（连续切换时，若用户按住右键移动，菜单关闭后先等释放再重弹）。
///
/// 最多等 1 秒，防止异常情况下死循环。返回时右键必定处于释放状态。
pub fn wait_rbutton_release() {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    let mut waited = 0u32;
    unsafe {
        while ((GetAsyncKeyState(VK_RBUTTON) as u16) & 0x8000) != 0 && waited < 100 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            waited += 1;
        }
    }
}

#[cfg(windows)]
fn wide(s: &str) -> windows::core::PCWSTR {
    // 需要一个存活到调用结束的缓冲区；这里用 Box 泄漏避免生命周期问题
    // （PCWSTR 只是指针，AppendMenuW 是同步调用，调用完即可丢弃）。
    // 由于 unsafe 生命周期约束，用静态 Vec 缓存最近一个字符串最稳妥。
    use std::sync::OnceLock;
    static CACHE: OnceLock<std::sync::Mutex<Vec<u16>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut buf = cache.lock().unwrap();
    buf.clear();
    buf.extend(s.encode_utf16());
    buf.push(0);
    windows::core::PCWSTR(buf.as_ptr())
}

fn apply_click_through(hwnd: HWND, enable: bool) {
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new_style = toggle_transparent_style(style, enable);
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

/// 计算切换 `WS_EX_TRANSPARENT` 后的扩展样式（纯位运算，可单测）。
///
/// - `enable=true`  → 置位（穿透）
/// - `enable=false` → 清位（取消穿透）
fn toggle_transparent_style(style: isize, enable: bool) -> isize {
    let flag = WS_EX_TRANSPARENT.0 as isize;
    if enable {
        style | flag
    } else {
        style & !flag
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WS_EX_TRANSPARENT = 0x00000020
    const TRANSPARENT: isize = 0x20;

    #[test]
    fn enable_sets_bit() {
        let style = 0x1000; // 一些已有扩展样式
        let new = toggle_transparent_style(style, true);
        assert_eq!(new, style | TRANSPARENT);
        assert_ne!(new & TRANSPARENT, 0);
        // 不破坏原有 bit
        assert_ne!(new & 0x1000, 0);
    }

    #[test]
    fn disable_clears_bit() {
        let style = 0x1000 | TRANSPARENT;
        let new = toggle_transparent_style(style, false);
        assert_eq!(new & TRANSPARENT, 0);
        // 不破坏原有 bit
        assert_ne!(new & 0x1000, 0);
    }

    #[test]
    fn idempotent() {
        // 重复开启 / 重复关闭不改变结果
        let style = 0x20;
        assert_eq!(toggle_transparent_style(style, true), style);
        assert_eq!(toggle_transparent_style(0x1000, false), 0x1000);
    }

    #[test]
    fn zero_style_works() {
        assert_eq!(toggle_transparent_style(0, true), TRANSPARENT);
        assert_eq!(toggle_transparent_style(TRANSPARENT, false), 0);
    }

    // ===== WS_EX_TOOLWINDOW（不在系统任务栏显示）=====

    /// WS_EX_TOOLWINDOW = 0x00000080，WS_EX_APPWINDOW = 0x00040000
    const TOOLWINDOW: isize = 0x80;
    const APPWINDOW: isize = 0x40000;

    /// 模拟 ensure_tool_window 的位逻辑：置 TOOLWINDOW + 清 APPWINDOW + 保留其他
    #[test]
    fn tool_window_sets_bit_and_clears_appwindow() {
        let style = APPWINDOW | 0x1000; // winit 默认：APPWINDOW + 一些其他样式
        let new = (style | TOOLWINDOW) & !APPWINDOW;
        assert_ne!(new & TOOLWINDOW, 0);
        assert_eq!(new & APPWINDOW, 0);
        assert_ne!(new & 0x1000, 0); // 不破坏其他位
    }

    /// 已设置且无 APPWINDOW 时幂等（ensure 里 `new_style != style` 才改）
    #[test]
    fn tool_window_idempotent_when_already_clean() {
        let style = TOOLWINDOW | 0x1000; // 无 APPWINDOW
        let new = (style | TOOLWINDOW) & !APPWINDOW;
        assert_eq!(new, style);
    }

    /// 典型 winit 初始样式（0x40118）应判定为需要修改
    #[test]
    fn tool_window_detects_winit_default_needs_fix() {
        let style = 0x40118; // 实测 winit 默认：APPWINDOW|WINDOWEDGE|TOPMOST|ACCEPTFILES
        let new = (style | TOOLWINDOW) & !APPWINDOW;
        assert_ne!(new, style); // APPWINDOW 被清除 → 需要修改
        assert_eq!(new & APPWINDOW, 0);
        assert_ne!(new & TOOLWINDOW, 0);
        assert_eq!(new & 0x100, 0x100); // WINDOWEDGE 保留
    }

    // ===== NativeMenuCmd（右键原生菜单命令）=====

    /// 验证：命令码与 TrackPopupMenu 返回值映射一致（1/2/3/4）
    #[test]
    fn native_menu_cmd_ids_stable() {
        assert_eq!(NativeMenuCmd::TogglePause as u32, 1);
        assert_eq!(NativeMenuCmd::ToggleClickThrough as u32, 2);
        assert_eq!(NativeMenuCmd::Quit as u32, 3);
        assert_eq!(NativeMenuCmd::ProcessList as u32, 4);
    }

    #[test]
    fn native_menu_cmd_eq_and_debug() {
        assert_eq!(NativeMenuCmd::Quit, NativeMenuCmd::Quit);
        assert_ne!(NativeMenuCmd::TogglePause, NativeMenuCmd::ToggleClickThrough);
        assert!(!format!("{:?}", NativeMenuCmd::TogglePause).is_empty());
    }

    /// 验证：命令码到枚举的映射（show_context_menu 的返回逻辑）
    #[test]
    fn native_menu_cmd_from_trackpopup_value() {
        let map = |v: u32| match v {
            1 => Some(NativeMenuCmd::TogglePause),
            2 => Some(NativeMenuCmd::ToggleClickThrough),
            3 => Some(NativeMenuCmd::Quit),
            4 => Some(NativeMenuCmd::ProcessList),
            _ => None,
        };
        assert_eq!(map(1), Some(NativeMenuCmd::TogglePause));
        assert_eq!(map(2), Some(NativeMenuCmd::ToggleClickThrough));
        assert_eq!(map(3), Some(NativeMenuCmd::Quit));
        assert_eq!(map(4), Some(NativeMenuCmd::ProcessList));
        assert_eq!(map(0), None); // 用户取消
        assert_eq!(map(99), None);
    }

    // ===== RowMenuCmd（进程详情行右键菜单命令）=====

    /// 验证：命令码与 TrackPopupMenu 返回值映射一致（1/2）
    #[test]
    fn row_menu_cmd_ids_stable() {
        assert_eq!(RowMenuCmd::OpenLocation as u32, 1);
        assert_eq!(RowMenuCmd::Kill as u32, 2);
    }

    /// 验证：命令码到枚举的映射（show_row_menu 的返回逻辑）
    #[test]
    fn row_menu_cmd_from_trackpopup_value() {
        let map = |v: u32| match v {
            1 => Some(RowMenuCmd::OpenLocation),
            2 => Some(RowMenuCmd::Kill),
            _ => None,
        };
        assert_eq!(map(1), Some(RowMenuCmd::OpenLocation));
        assert_eq!(map(2), Some(RowMenuCmd::Kill));
        assert_eq!(map(0), None); // 用户取消
        assert_eq!(map(99), None);
    }
}
