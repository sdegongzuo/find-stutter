//! Windows 原生窗口控制：置顶、点击穿透、HWND 缓存。
//!
//! 通过 `slint::Window::window_handle()` + `raw-window-handle` 提取 HWND，
//! 再调用 Win32 API 设置/读取 `WS_EX_TRANSPARENT` 扩展样式。
//!
//! 设计要点：
//! - 拖动改用 Slint 的 TouchArea + set_position()（仅在用户实际拖动期间更新，不是每帧），
//!   避免 egui 那种「每帧 OuterPosition」更新导致透明窗重影/闪烁
//! - 点击穿透通过切换 `WS_EX_TRANSPARENT` 实现，开启后窗口鼠标事件完全穿透
//! - Z 序分层刻意分两类：
//!   * **悬浮窗 / 伪任务栏**：`.slint` 里 `always-on-top: true` → 带 `WS_EX_TOPMOST`，
//!     永远压在所有普通窗口之上；
//!   * **进程详情页 / 卡顿分析页**：`always-on-top: false` → 普通窗口，点别的 APP
//!     就会被压到后面（符合正常使用习惯）。
//!   扩展样式另有两套：工具窗口（不进任务栏 / Alt-Tab）与普通应用窗口（进任务栏），
//!   见 `ensure_tool_window_for` / `ensure_app_window_for`。

use raw_window_handle::HasWindowHandle;
use slint::ComponentHandle;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW,
    GetWindowLongPtrW, GetWindowRect, GWL_EXSTYLE, GWL_STYLE, IsIconic,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, HWND_TOP,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SW_RESTORE, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCLIENT, HTLEFT, HTRIGHT, HTTOP,
    HTTOPLEFT, HTTOPRIGHT, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_GETMINMAXINFO, WM_NCHITTEST,
    WM_NCLBUTTONDOWN, WM_SYSCOMMAND, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
    WS_THICKFRAME, MINMAXINFO,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;

use crate::Overlay;

/// 「用户正在拖动窗口边框 / 标题栏」——Windows 原生模态尺寸循环是否进行中。
///
/// 由 [`native_resize_proc`] 在 `WM_ENTERSIZEMOVE` / `WM_EXITSIZEMOVE` 处维护。
/// 用途：拖拽期间窗口尺寸由用户手指驱动，任何定时重绘 / 几何对账都只是在
/// 同一线程上抢帧——只会把拖拽拖慢，不会让用户看到更正确的画面。
///
/// **作用域**：本标志只反映 *原生模态循环*（主窗口 / 进程详情页的边缘缩放，
/// 由 `native_resize_proc` 子类接管）。悬浮窗 / 伪任务栏走 Slint 自绘拖拽
/// （TouchArea + set_position），**不会**置位本标志——不要把它当成
/// 「进程内是否有任何拖拽」的总开关。
///
/// 进程级单一标志仍然够用：原生模态循环同一时刻只可能有一个（前台窗口独占
/// 鼠标捕获），且 Slint 自绘拖拽根本不读这个标志，互不干扰。
static SIZE_MOVE_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 进入原生拖拽循环的时刻（UNIX 毫秒时间戳；0 = 不在拖拽）。
/// 与 [`SIZE_MOVE_ACTIVE`] 成对维护，仅供 [`size_move_active`] 的超时兜底使用。
static SIZE_MOVE_STARTED_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `WM_EXITSIZEMOVE` 丢失兜底阈值。真实拖拽（移动 / 缩放一个窗口）不可能
/// 持续 30 秒；超过即认定退出消息已丢失，见 [`size_move_active`]。
const SIZE_MOVE_STALE_MS: u64 = 30_000;

/// 原生拖拽（移动 / 缩放）模态循环是否进行中。
///
/// 带**超时自愈**：`WM_EXITSIZEMOVE` 可能永远不送（拖拽中窗口被关闭、DPI
/// 变化时窗口重建、子类回调路径 panic），标志一旦卡在 true，所有依赖本函数
/// 提前返回的定时器（1Hz 主 tick、进程列表刷新、样式守护）会**永久停摆**，
/// 表现为整个程序假死。因此超过 [`SIZE_MOVE_STALE_MS`] 视为丢失并强制复位。
pub fn size_move_active() -> bool {
    if !SIZE_MOVE_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    let started = SIZE_MOVE_STARTED_MS.load(std::sync::atomic::Ordering::Relaxed);
    if started != 0 && unix_millis().saturating_sub(started) > SIZE_MOVE_STALE_MS {
        SIZE_MOVE_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
        SIZE_MOVE_STARTED_MS.store(0, std::sync::atomic::Ordering::Relaxed);
        log::warn!(
            "拖拽标志超过 {}s 未收到 WM_EXITSIZEMOVE，疑似丢失，强制复位（定时器恢复刷新）",
            SIZE_MOVE_STALE_MS / 1000
        );
        return false;
    }
    true
}

/// 当前 UNIX 时间（毫秒）。`SystemTime` 早于纪元等异常情况返回 0，
/// 此时超时兜底不生效（退化为旧行为），不影响正确性。
fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

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

/// 把 CLI 子命令的输出接回**父进程的控制台**（仅 GUI 子系统构建需要）。
///
/// release 编成 GUI 子系统后，进程启动时不会分配控制台，标准输出句柄是无效的，
/// 直接 `println!` 什么都不会出现。从 cmd / PowerShell 里跑子命令时，控制台属于
/// 父进程，因此 `AttachConsole(ATTACH_PARENT_PROCESS)` 就能把输出接上去。
///
/// **已被重定向时不动作**：stdout/stderr 若是有效的管道或文件句柄（脚本 `> out.txt`、
/// agent 抓取输出、CI），说明父进程已经安排好了落点，再去 Attach 反而会把日志写错地方。
///
/// 已知副作用：cmd 不会等待 GUI 子系统进程退出，交互式跑会出现"提示符先回来、
/// 输出后到"的错位观感；脚本里用 `start /wait` 或直接重定向即可。
#[cfg(windows)]
pub fn attach_parent_console_for_cli() {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    fn ok(h: windows::core::Result<HANDLE>) -> bool {
        match h {
            Ok(h) => !h.is_invalid(),
            Err(_) => false,
        }
    }

    unsafe {
        if ok(GetStdHandle(STD_OUTPUT_HANDLE)) || ok(GetStdHandle(STD_ERROR_HANDLE)) {
            return; // 已重定向（管道/文件），保持原样
        }
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
pub fn attach_parent_console_for_cli() {}

/// 让窗口不出现在 **Windows 系统任务栏**（工具窗口样式 `WS_EX_TOOLWINDOW`）。
///
/// 悬浮窗 / 伪任务栏窗口适用：设置后窗口不占系统任务栏按钮，也不会出现在
/// Alt-Tab 切换列表里。**进程详情页 / 卡顿分析页不要用这个**——它们是普通
/// 窗口，需要一个任务栏按钮作为切回入口，见 [`ensure_app_window_for`]。
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
/// 调用方应**周期性重复调用**（Overlay 的 1Hz tick 守护各窗口），
/// 确保 winit 覆盖后 1 秒内自动恢复。函数幂等，无样式变化时不重绘。
pub fn ensure_tool_window_for(win: &slint::Window) {
    if let Some(hwnd) = extract_hwnd_from(win) {
        ensure_tool_window(hwnd);
    }
}

/// 设置 `WS_EX_TOOLWINDOW` + 清除 `WS_EX_APPWINDOW`（纯位运算，可单测）。
fn ensure_tool_window(hwnd: HWND) {
    apply_ex_style(hwnd, toggle_tool_window_bits);
}

/// 让窗口成为**普通应用窗口**（有任务栏按钮、出现在 Alt-Tab 列表）。
///
/// 进程详情页 / 卡顿分析页用这个。与 [`ensure_tool_window_for`] 的差别只在
/// 扩展样式的两位方向：清 `WS_EX_TOOLWINDOW`、置 `WS_EX_APPWINDOW`。
///
/// 为什么主窗口要留在任务栏：主窗口**不置顶**（见下），点别的 APP 就会被压到
/// 后面；此时必须有地方能把它切回来——任务栏按钮 / Alt-Tab。若还做成工具窗口，
/// 窗口一被压下去就只能靠托盘菜单找回，等于半个"消失"。
///
/// 同样需要周期性重调：winit 在 show / 状态变化时会重算扩展样式。
pub fn ensure_app_window_for(win: &slint::Window) {
    if let Some(hwnd) = extract_hwnd_from(win) {
        ensure_app_window(hwnd);
    }
}

fn ensure_app_window(hwnd: HWND) {
    apply_ex_style(hwnd, toggle_app_window_bits);
}

/// 扩展样式的「读-改-写」外壳：只有位确实变化时才写回 + 触发一次重绘。
///
/// `f` 接收当前样式，返回目标样式（见 [`toggle_tool_window_bits`] /
/// [`toggle_app_window_bits`]）。
fn apply_ex_style(hwnd: HWND, f: fn(isize) -> isize) {
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new_style = f(style);
        if new_style != style {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style);
            // 触发重绘使样式生效（SWP_NOZORDER：只改样式，不动 Z 序）
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

/// 工具窗口位运算：置 TOOLWINDOW、清 APPWINDOW。
fn toggle_tool_window_bits(style: isize) -> isize {
    let tool = WS_EX_TOOLWINDOW.0 as isize;
    let appwin = WS_EX_APPWINDOW.0 as isize;
    (style | tool) & !appwin
}

/// 普通应用窗口位运算：置 APPWINDOW、清 TOOLWINDOW。
fn toggle_app_window_bits(style: isize) -> isize {
    let tool = WS_EX_TOOLWINDOW.0 as isize;
    let appwin = WS_EX_APPWINDOW.0 as isize;
    (style | appwin) & !tool
}

/// 把窗口带到最前并激活。
///
/// 只用于**用户显式要求打开 / 切回窗口**的路径（标题栏/托盘菜单/悬浮窗点击），
/// 不要放进定时刷新里——那会周期性抢前台，正是本次要修掉的行为。
///
/// 主窗口不置顶后，若它已被别的 APP 压到后面，`Slint::show()` 并不会把它翻到
/// 前面（窗口已可见时 show 基本是空操作）。因此复用已有窗口前必须显式前置。
///
/// `SetForegroundWindow` 受 Windows 前台锁限制：调用进程不是前台进程时会被拒绝
/// （返回 false）。这里的调用点都源自用户输入事件（点菜单/点悬浮窗），本进程
/// 持有前台输入权，正常情况下能成功；失败时窗口仍会被 `SetWindowPos(HWND_TOP)`
/// 抬到同层最前，只是没有键盘焦点，故只记 debug 日志。
pub fn bring_to_front(win: &slint::Window) {
    let Some(hwnd) = extract_hwnd_from(win) else {
        return;
    };
    unsafe {
        // 最小化状态直接 SetForegroundWindow 无效，先还原
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        if !SetForegroundWindow(hwnd).as_bool() {
            log::debug!("SetForegroundWindow 被前台锁拒绝（窗口已抬到最前但没有焦点）");
        }
    }
}

/// 一次性设置窗口矩形（**物理像素** x, y, w, h）：move + size 在同一次
/// SetWindowPos 调用内原子完成。
///
/// ## 为什么不用 Slint 的 set_position() + set_size()
///
/// 左/上边缘缩放需要同时改位置与尺寸；拆成两次 Slint 调用会在 Windows 上
/// 产生两个独立的窗口操作（两次 WM_WINDOWPOSCHANGED），拖动中表现为
/// 「先移后缩」的两段式跳动。原生 SetWindowPos 一步到位，窗口单帧到位。
///
/// winit 通过 WM_WINDOWPOSCHANGED 感知几何变化并同步内部状态、触发
/// Slint relayout，因此绕过 Slint API 直接改 HWND 是安全的。
///
/// HWND 提取失败（非 Windows 平台 / 异常路径）时退回 Slint 两次调用，
/// 牺牲原子性但不丢功能。
pub fn set_window_rect(win: &slint::Window, x: i32, y: i32, w: i32, h: i32) {
    #[cfg(windows)]
    if let Some(hwnd) = extract_hwnd_from(win) {
        // SAFETY：hwnd 来自当前进程的合法 Slint 窗口；仅改几何，不动 Z 序、
        // 不抢焦点（SWP_NOZORDER | SWP_NOACTIVATE）
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                x,
                y,
                w.max(1),
                h.max(1),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        return;
    }
    // 兜底：Slint API（非 Windows 或 HWND 提取失败）
    win.set_position(slint::PhysicalPosition::new(x, y));
    win.set_size(slint::PhysicalSize::new(w.max(1) as u32, h.max(1) as u32));
}

/// 读取原生窗口矩形（物理像素 x, y, w, h）。非 Windows / HWND 提取失败返回 None。
#[cfg(windows)]
pub fn native_rect(win: &slint::Window) -> Option<(i32, i32, i32, i32)> {
    let hwnd = extract_hwnd_from(win)?;
    let mut r = windows::Win32::Foundation::RECT::default();
    // SAFETY：RECT 为合法输出指针，GetWindowRect 仅写入该结构
    unsafe { GetWindowRect(hwnd, &mut r) }
        .ok()
        .map(|_| (r.left, r.top, r.right - r.left, r.bottom - r.top))
}

#[cfg(not(windows))]
pub fn native_rect(_win: &slint::Window) -> Option<(i32, i32, i32, i32)> {
    None
}

/// 单轴贴边吸附（纯函数，便于单测；单位均为物理像素）。
///
/// 窗口在该轴占用 `[pos, pos + size)`，工作区为 `[lo, hi)`：
/// - 距近端 `lo` 小于等于 `threshold` → 贴到 `lo`；
/// - 否则距远端 `hi - size` 小于等于 `threshold` → 贴到远端；
/// - 都不满足 / 窗口比工作区还大 / 阈值非正 → 保持原位置。
pub fn snap_axis(pos: i32, size: i32, lo: i32, hi: i32, threshold: i32) -> i32 {
    if threshold <= 0 {
        return pos;
    }
    if (pos - lo).abs() <= threshold {
        return lo;
    }
    let far = hi - size;
    if far >= lo && (far - pos).abs() <= threshold {
        return far;
    }
    pos
}

/// 松手贴边：把窗口吸附到**所在显示器**工作区最近的边缘（多屏安全）。
///
/// `threshold_logical` 为触发距离（逻辑像素，按窗口缩放换算物理像素）。
/// 返回是否发生了吸附（位置有变化）。
#[cfg(windows)]
pub fn snap_to_work_area(win: &slint::Window, threshold_logical: f32) -> bool {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };

    let Some(hwnd) = extract_hwnd_from(win) else {
        return false;
    };
    let Some((x, y, w, h)) = native_rect(win) else {
        return false;
    };
    let threshold = (threshold_logical * win.scale_factor()).round() as i32;
    if threshold <= 0 {
        return false;
    }

    let mut mi = MONITORINFO::default();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    // SAFETY：hwnd 为当前进程窗口句柄；mi 为合法输出结构，cbSize 已置
    let work = unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return false;
        }
        mi.rcWork
    };

    let nx = snap_axis(x, w, work.left, work.right, threshold);
    let ny = snap_axis(y, h, work.top, work.bottom, threshold);
    if nx == x && ny == y {
        return false;
    }
    set_window_rect(win, nx, ny, w, h);
    true
}

#[cfg(not(windows))]
pub fn snap_to_work_area(_win: &slint::Window, _threshold_logical: f32) -> bool {
    false
}

/// 窗口所在显示器的**工作区**（物理像素 `(left, top, right, bottom)`，
/// 已扣除任务栏）。多屏下取离窗口最近的那块（`MONITOR_DEFAULTTONEAREST`）。
///
/// HWND 不可提取时返回 `None`（非 Windows / 窗口尚未创建）。
#[cfg(windows)]
pub fn monitor_work_area_for_window(win: &slint::Window) -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };

    let hwnd = extract_hwnd_from(win)?;
    let mut mi = MONITORINFO::default();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    // SAFETY：mi 为合法输出结构且 cbSize 已置；只读显示器信息
    let ok = unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        GetMonitorInfoW(mon, &mut mi).as_bool()
    };
    if !ok {
        return None;
    }
    Some((mi.rcWork.left, mi.rcWork.top, mi.rcWork.right, mi.rcWork.bottom))
}

#[cfg(not(windows))]
pub fn monitor_work_area_for_window(_win: &slint::Window) -> Option<(i32, i32, i32, i32)> {
    None
}

/// 屏幕物理坐标 `(x, y)` 所在显示器的工作区（多屏 / 负坐标安全）。
///
/// 用于「子窗口跟随锚点摆放」：锚点窗口尚未移动时，`MonitorFromWindow`
/// 取的是锚点所在屏，这与用户期望一致。
#[cfg(windows)]
pub fn monitor_work_area_at(x: i32, y: i32) -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };

    let mut mi = MONITORINFO::default();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    // SAFETY：mi 为合法输出结构且 cbSize 已置；只读显示器信息
    let ok = unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        GetMonitorInfoW(mon, &mut mi).as_bool()
    };
    if !ok {
        return None;
    }
    Some((mi.rcWork.left, mi.rcWork.top, mi.rcWork.right, mi.rcWork.bottom))
}

#[cfg(not(windows))]
pub fn monitor_work_area_at(_x: i32, _y: i32) -> Option<(i32, i32, i32, i32)> {
    None
}

/// 光标所在显示器的工作区（物理像素）。无锚点窗口（如卡顿分析页）用它选屏。
#[cfg(windows)]
pub fn monitor_work_area_at_cursor() -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut p = POINT::default();
    // SAFETY：p 为合法输出结构；只读光标位置
    unsafe { GetCursorPos(&mut p).ok()? };
    monitor_work_area_at(p.x, p.y)
}

#[cfg(not(windows))]
pub fn monitor_work_area_at_cursor() -> Option<(i32, i32, i32, i32)> {
    None
}

/// 系统 DPI 缩放比（`GetDpiForSystem`，返回的是**主屏 / 系统级**缩放）。
/// 仅作 [`monitor_scale_at`] 失败时的兜底——多屏不同缩放时它是错的，
/// 不要在新代码里直接用它推算屏幕几何。
#[cfg(windows)]
fn system_scale() -> f32 {
    use windows::Win32::UI::HiDpi::GetDpiForSystem;

    // SAFETY：无参数纯查询
    let dpi = unsafe { GetDpiForSystem() };
    if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
}

#[cfg(not(windows))]
fn system_scale() -> f32 {
    1.0
}

/// 屏幕物理坐标 `(x, y)` 所在显示器的**有效 DPI 缩放比**（per-monitor）。
///
/// 为什么不用 `GetDpiForSystem`：多屏各自缩放时系统 DPI 只反映主屏，
/// 例如主屏 100% + 副屏 150%，光标在副屏时按系统 DPI 推算窗口物理尺寸
/// 会**低估 1/3**，居中位置偏移、夹取边界也按偏小尺寸计算，窗口实际仍越界。
/// `GetDpiForMonitor(MDT_EFFECTIVE_DPI)` 才是目标屏真实缩放。
#[cfg(windows)]
fn monitor_scale_at(x: i32, y: i32) -> Option<f32> {
    use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    // SAFETY：纯查询；MonitorFromPoint 失败时返回 None 由调用方兜底
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut dpi_x = 0u32;
        let mut dpi_y = 0u32;
        GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).ok()?;
        if dpi_x == 0 {
            return None;
        }
        Some(dpi_x as f32 / 96.0)
    }
}

#[cfg(not(windows))]
fn monitor_scale_at(_x: i32, _y: i32) -> Option<f32> {
    None
}

/// 推算弹窗在**显示之前**应有的矩形（物理像素）。
///
/// 为什么必须在 show 之前算：窗口一旦显示，winit 会先按默认位置画出一帧，
/// 之后再 SetWindowPos 移动，用户看到的就是「先露一截、再跳到完整位置」的闪动。
/// 把这个位置在 `show()` 之前交给 `Window::set_position`，窗口**创建时**就
/// 落在终态，首帧即终态（实测：show 之前设的位置会被采用；尺寸设了没用，
/// 会被 .slint 根组件声明的尺寸在窗口创建时覆盖，所以这里只算位置）。
///
/// 只依赖锚点窗口的原生矩形 + 目标窗口的**逻辑**尺寸，不碰目标窗口的 HWND
/// （此刻它还不存在）。返回 `None` 表示拿不到几何，调用方退回「show 后夹取」。
pub fn plan_popup_rect_near(
    anchor_win: &slint::Window,
    win_logical_w: f32,
    win_logical_h: f32,
    gap_logical: f32,
) -> Option<(i32, i32, i32, i32)> {
    let anchor = native_rect(anchor_win)?;
    let scale = anchor_win.scale_factor();
    let w = (win_logical_w * scale).round() as i32;
    let h = (win_logical_h * scale).round() as i32;
    if w <= 1 || h <= 1 {
        return None;
    }
    // 工作区取「锚点中心」所在显示器：目标窗口此刻可能还在别的屏上
    let (cx, cy) = (anchor.0 + anchor.2 / 2, anchor.1 + anchor.3 / 2);
    let work = monitor_work_area_at(cx, cy)?;
    let gap = (gap_logical * scale).round() as i32;
    Some(place_popup_rect(anchor, w, h, work, gap))
}

/// 推算无锚点窗口在**显示之前**应有的矩形：光标所在显示器工作区居中。
///
/// 尺寸换算用**光标所在屏**的有效 DPI（`monitor_scale_at`）：光标在 150%
/// 副屏而系统 100% 时，按系统 DPI 算出的物理尺寸会低估 1/3，窗口会越界。
/// 拿不到 per-monitor DPI 时退回系统 DPI（单屏场景两者一致，无损失）。
pub fn plan_centered_rect(
    win_logical_w: f32,
    win_logical_h: f32,
) -> Option<(i32, i32, i32, i32)> {
    let mut p = POINT::default();
    // SAFETY：p 为合法输出结构；只读光标位置
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p).ok()? };
    let work = monitor_work_area_at(p.x, p.y)?;
    let scale = monitor_scale_at(p.x, p.y).unwrap_or_else(system_scale);
    let w = (win_logical_w * scale).round() as i32;
    let h = (win_logical_h * scale).round() as i32;
    if w <= 1 || h <= 1 {
        return None;
    }
    let (l, t, r, b) = work;
    let x = l + ((r - l) - w) / 2;
    let y = t + ((b - t) - h) / 2;
    Some(clamp_rect_to_work(x, y, w, h, work))
}

/// 窗口隐藏后重新显示**之前**，把上次的位置夹回工作区（换屏 / 改分辨率后可能
/// 已越界）。同样只设位置：show 之后再挪会闪一帧。返回是否调整过。
pub fn clamp_position_before_show(win: &slint::Window) -> bool {
    let (Some((x, y, w, h)), Some(work)) = (
        native_rect(win),
        monitor_work_area_for_window(win),
    ) else {
        return false;
    };
    let (nx, ny, _nw, _nh) = clamp_rect_to_work(x, y, w, h, work);
    if (nx, ny) == (x, y) {
        return false;
    }
    win.set_position(slint::PhysicalPosition::new(nx, ny));
    true
}

/// 把矩形夹进工作区（纯函数，单位均为物理像素）：
/// - 尺寸超过工作区 → 缩到工作区大小（不小于 1px）；
/// - 位置越界 → 平移回工作区内（不改变尺寸）。
///
/// **注意**：这里的下限是 1px，不是 UI 的 resize 最小值（`RESIZE_MIN_W/H`，
/// 500×300 逻辑像素）。工作区比窗口还小的极端屏（超低分辨率 / 超大缩放）下，
/// 缩到工作区后内部布局会挤（列溢出），但仍保证整窗可见——可见性优先于
/// 布局美观，这是有意的取舍。
///
/// 返回 `(x, y, w, h)`；输入已在工作区内时原样返回。
pub fn clamp_rect_to_work(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    work: (i32, i32, i32, i32),
) -> (i32, i32, i32, i32) {
    let (l, t, r, b) = work;
    let work_w = (r - l).max(1);
    let work_h = (b - t).max(1);
    let nw = w.clamp(1, work_w);
    let nh = h.clamp(1, work_h);
    // 右/下夹取的上界可能小于左/上（窗口比工作区大，已被缩到等号），
    // 用 max(l)/max(t) 兜住，避免 clamp 的 min > max panic。
    let nx = x.clamp(l, (r - nw).max(l));
    let ny = y.clamp(t, (b - nh).max(t));
    (nx, ny, nw, nh)
}

/// 子窗口相对锚点窗口的摆放矩形（纯函数，单位均为物理像素）。
///
/// 优先放在锚点**下方** `gap` 处；下方空间不足时翻到**上方**；两边都放不下
/// （窗口比任一侧剩余空间都高）时挑**空间更大的一侧**——至少让锚点露出来，
/// 而不是整个盖住它；最后统一交给 [`clamp_rect_to_work`] 收尾。
///
/// `anchor`：锚点窗口矩形 `(x, y, w, h)`；`win_w`/`win_h`：子窗口尺寸。
pub fn place_popup_rect(
    anchor: (i32, i32, i32, i32),
    win_w: i32,
    win_h: i32,
    work: (i32, i32, i32, i32),
    gap: i32,
) -> (i32, i32, i32, i32) {
    let (ax, ay, _aw, ah) = anchor;
    let (_l, t, _r, b) = work;
    let below = ay + ah + gap;
    let above = ay - gap - win_h;
    let below_room = b - below; // 下方剩余可用高度
    let above_room = ay - gap - t; // 上方剩余可用高度
    let y = if below_room >= win_h {
        below
    } else if above_room >= win_h || above_room > below_room {
        // 上方放得下；或两边都放不下但上方空间更大——至少让锚点露出来
        above
    } else {
        below
    };
    clamp_rect_to_work(ax, y, win_w, win_h, work)
}

/// 让**已显示**的窗口完整落在所在显示器工作区内（尺寸超限则缩小、
/// 位置越界则平移），返回是否真的动过窗口。
///
/// 用于「窗口刚打开时靠近屏幕边缘被裁掉一部分」：位置按固定偏移算出来
/// （例如贴在悬浮窗下方），在屏幕右边/下边就会有一截跑到工作区之外。
/// 读取用的是原生矩形（`GetWindowRect`），因此对 Slint 侧几何是否同步
/// 不敏感。
pub fn fit_window_to_work(win: &slint::Window) -> bool {
    let Some((x, y, w, h)) = native_rect(win) else {
        return false;
    };
    // 窗口尚未真正落地（尺寸为 0/1）时不动，交给下一次重试
    if w <= 1 || h <= 1 {
        return false;
    }
    let Some(work) = monitor_work_area_for_window(win) else {
        return false;
    };
    let rect = clamp_rect_to_work(x, y, w, h, work);
    if rect == (x, y, w, h) {
        return false;
    }
    log::info!("窗口越出工作区 {:?}，夹回到 {:?}", (x, y, w, h), rect);
    set_window_rect(win, rect.0, rect.1, rect.2, rect.3);
    true
}

/// 把子窗口 `win` 摆在**锚点窗口** `anchor_win` 的下方（不足则上方），
/// 并整体夹进锚点所在显示器的工作区。`gap_logical` 为间距（逻辑像素）。
///
/// 返回 `false` 表示拿不到几何（HWND 未就绪 / 窗口尺寸异常），调用方
/// 可退回自算位置。
pub fn place_popup_near(
    win: &slint::Window,
    anchor_win: &slint::Window,
    gap_logical: f32,
) -> bool {
    let Some(anchor) = native_rect(anchor_win) else {
        return false;
    };
    // 子窗口尺寸优先取原生矩形（与 set_window_rect 同一口径）；HWND 尚未
    // 就绪时退回 Slint 的 physical size
    let (w, h) = match native_rect(win) {
        Some((_, _, w, h)) => (w, h),
        None => {
            let s = win.size();
            (s.width as i32, s.height as i32)
        }
    };
    if w <= 1 || h <= 1 {
        return false;
    }
    // 工作区取「锚点中心」所在显示器：子窗口此刻可能还在别的屏上
    let (cx, cy) = (anchor.0 + anchor.2 / 2, anchor.1 + anchor.3 / 2);
    let Some(work) = monitor_work_area_at(cx, cy) else {
        return false;
    };
    let gap = (gap_logical * anchor_win.scale_factor()).round() as i32;
    let rect = place_popup_rect(anchor, w, h, work, gap);
    set_window_rect(win, rect.0, rect.1, rect.2, rect.3);
    true
}


/// 比对 Slint 认知矩形与原生矩形是否脱节（纯函数，便于单测）。
/// 任一边不相等即视为脱节：slint 侧是「热区锚定布局」的真相源，
/// native 侧是「用户实际点到的窗口」，两者偏差 1px 都会让右/下热区错位。
fn geometry_diverged(
    slint: (i32, i32, i32, i32),
    native: (i32, i32, i32, i32),
) -> bool {
    slint != native
}

/// 检测并修复「Slint 布局几何 ≠ 原生窗口几何」的脱节，返回是否修复过。
///
/// ## 为什么需要
///
/// 边缘缩放写回走原生 SetWindowPos 原子完成 move+size，Slint 的新尺寸
/// 依赖 winit 处理随之送达的 WM_WINDOWPOSCHANGED 后回传给布局层。实测
/// （200% 缩放、连续拖拽）该回传偶发丢失：Slint 布局停留在旧尺寸，而
/// 原生窗口已是新尺寸——锚定 parent.width/height 的右/下热区随之落进
/// 窗口内部看不见的旧位置，用户按可见边缘毫无响应，且内容区与窗口
/// 边缘之间出现透明空带。左/上拖拽因锚定 (0,0) 不受影响，且会从 Slint
/// 基准重写整个矩形，客观上「治愈」脱节——这也是「右/下失灵、左/上
/// 正常」这一不对称症状的来源。
///
/// ## 做法
///
/// 读原生矩形（GetWindowRect，无边框工具窗口下即窗口矩形）与 Slint 的
/// position()/size() 逐项比对；不一致时经 Slint API（set_position +
/// set_size）写回原生值。走 Slint API 而非再次 SetWindowPos，是为了让
/// winit/Slint 内部状态链路整体重新同步。调用时机应在事件循环空闲时
/// （拖拽开始 / 写回后延迟一拍 / 定时器），避免在事件风暴中误判。
pub fn reconcile_geometry(win: &slint::Window) -> bool {
    let Some((nx, ny, nw, nh)) = native_rect(win) else {
        return false;
    };
    let pos = win.position();
    let size = win.size();
    let slint_rect = (pos.x, pos.y, size.width as i32, size.height as i32);
    if !geometry_diverged(slint_rect, (nx, ny, nw, nh)) {
        return false;
    }
    log::info!(
        "检测到 Slint 布局与原生窗口几何脱节: slint={:?} native={:?}，经 Slint API 重新对齐",
        slint_rect,
        (nx, ny, nw, nh)
    );
    win.set_position(slint::PhysicalPosition::new(nx, ny));
    win.set_size(slint::PhysicalSize::new(nw.max(1) as u32, nh.max(1) as u32));
    true
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
    /// 打开卡顿分析窗口（PRD M1 F6）
    Analysis = 5,
    /// 隐藏悬浮窗（恢复入口：左键单击托盘图标 / 托盘菜单「显示/隐藏悬浮窗」）
    HideOverlay = 6,
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
    allow_hide: bool,
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
        let _ = AppendMenuW(
            hmenu,
            MF_STRING,
            NativeMenuCmd::Analysis as usize,
            wide("卡顿分析"),
        );
        // 「隐藏悬浮窗」：仅在托盘可用时提供——托盘（左键单击 / 托盘菜单）
        // 是唯一的恢复入口，托盘创建失败时若还能隐藏窗口就再也找不回了。
        if allow_hide {
            let _ = AppendMenuW(
                hmenu,
                MF_STRING,
                NativeMenuCmd::HideOverlay as usize,
                wide("隐藏悬浮窗"),
            );
        }
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
            5 => Some(NativeMenuCmd::Analysis),
            6 => Some(NativeMenuCmd::HideOverlay),
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
    /// 结束进程树（连子进程一起终止）
    KillTree = 3,
    /// 查看进程详情（打开详情面板；与双击行等价）
    Detail = 4,
}

impl RowMenuCmd {
    /// 命令码 → 命令（`TrackPopupMenu` 的返回值）。
    ///
    /// 生产分支与单测共用这一份映射：此前单测里另写了一份同样的 `match`，
    /// 于是「加了新菜单项但忘了改生产分支」照样全绿（假阳性）。
    /// 返回 `None` 表示 `0`（点标题行/取消）或未知命令码。
    pub fn from_id(id: u32) -> Option<Self> {
        match id {
            1 => Some(Self::OpenLocation),
            2 => Some(Self::Kill),
            3 => Some(Self::KillTree),
            4 => Some(Self::Detail),
            _ => None,
        }
    }
}

/// 鼠标右键虚拟键码（`GetAsyncKeyState` 检测右键按下 / 释放用）。
/// 模块级常量：`show_row_menu_once` 与 `wait_rbutton_release` 共用。
pub const VK_RBUTTON: i32 = 0x02;

/// 行右键菜单单次弹出的结果（用单一枚举让「关闭方式」只能与命令互斥，
/// 编译器保证不出现 `(None, Command)` 这类非法组合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowMenuOutcome {
    /// 用户选择了命令（查看详情 / 打开位置 / 停止进程 / 结束进程树）
    Command(RowMenuCmd),
    /// 菜单被**右键**点击外部关闭（用户在别处又按了右键 → 可能想切换进程，
    /// 调用方应命中测试鼠标新位置，若在另一行则重新弹出）
    Switch,
    /// 左键点击外部 / Esc 关闭（结束菜单流程）
    Cancelled,
}

/// 弹一次行右键菜单（顶部标题行 + 「查看详情」+「打开文件所在的位置」+「停止进程」，
/// 有子进程时追加「结束进程树」），**不循环**。返回 [`RowMenuOutcome`]：
///
/// - `Command(cmd)`：用户选择了菜单项
/// - `Switch`：菜单被**右键**点击外部关闭 → 连续右键切换流程
/// - `Cancelled`：左键/Esc 关闭，流程结束
///
/// `kill_tree_enabled`：该进程是否存在可终止的后代（由调用方从进程快照判定）；
/// 为 `false` 时不显示「结束进程树」项。
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
    kill_tree_enabled: bool,
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
        // 服务明细行（pid=0）不是真实进程：详情/定位/终止全部置灰。
        // 「查看详情」此前无条件可点，点了会在 cache 里查不到 pid=0 的行，
        // 弹出标题为「PID 0 (PID 0)」、字段全空的空壳面板。
        let item_flags = if pid > 0 { MF_STRING } else { MF_GRAYED };
        // 查看详情（与双击行等价：弹出全窗详情面板）
        let _ = AppendMenuW(
            hmenu,
            item_flags,
            RowMenuCmd::Detail as usize,
            wide("查看详情"),
        );
        // 服务条目（pid<=0）没有可定位的可执行文件 → 置灰「打开文件所在的位置」
        let _ = AppendMenuW(
            hmenu,
            item_flags,
            RowMenuCmd::OpenLocation as usize,
            wide("打开文件所在的位置"),
        );
        let _ = AppendMenuW(
            hmenu,
            item_flags,
            RowMenuCmd::Kill as usize,
            wide("停止进程"),
        );
        // 结束进程树：连子进程一起终止（任务管理器同款入口）。
        // 仅当该进程确实有后代时才显示——无可杀子树时该入口无意义。
        if kill_tree_enabled && pid > 0 {
            let _ = AppendMenuW(
                hmenu,
                MF_STRING,
                RowMenuCmd::KillTree as usize,
                wide("结束进程树"),
            );
        }

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

        match RowMenuCmd::from_id(cmd.0 as u32) {
            Some(c) => RowMenuOutcome::Command(c),
            None if right_clicked_away => RowMenuOutcome::Switch,
            None => RowMenuOutcome::Cancelled,
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

// ===== 原生边缘缩放（WM_NCHITTEST 子类化，进程详情窗口专用）=====

/// 子类化 ID（同一 HWND 上多个子类时区分用；本窗口只有一个）
const NATIVE_RESIZE_SUBCLASS_ID: usize = 0x46_53; // "FS"

/// 边缘热区宽度（逻辑像素，与 slint 端 resize-margin 一致）
const NATIVE_RESIZE_EDGE_LOGICAL: i32 = 8;
/// 角落热区宽度（逻辑像素，与 slint 端右下角扁平热区一致）
const NATIVE_RESIZE_CORNER_LOGICAL: i32 = 24;

/// `WS_THICKFRAME`（= `WS_SIZEBOX`，0x00040000）数值化：尺寸边框样式位。
const WS_SIZEBOX_BIT: isize = WS_THICKFRAME.0 as isize;

/// 补上尺寸边框样式位（幂等）：**Windows 原生尺寸拖拽循环的前置条件**。
///
/// 只有带 `WS_THICKFRAME` 的窗口，`DefWindowProc` 收到
/// `WM_NCLBUTTONDOWN(HTLEFT/…)` 时才会进入系统级拖拽循环；缺这一位时
/// 命中码照样返回、光标照样是斜向箭头，但**拖不动**（2026-11-03 实测：
/// 进程详情窗口 style=0x16ca0000 无 0x40000，拖右边缘窗口纹丝不动；
/// 手工置位后同一位置拖拽立即生效）。
///
/// ## 为什么这一位会丢
///
/// 这一位由 winit 按 `WindowFlags::RESIZABLE` 生成，而 Slint 的 winit 后端把
/// 「根元素布局约束 min < max」当作可缩放判据（`window_is_resizable`）。
/// `ProcessList` 根节点写了固定 `width: 900px; height: 506px`，算出来 min == max，
/// 于是 winit 认为窗口不可缩放、样式里根本没有这位——**Slint 端声明的
/// `resize-margin` 热区在 Windows 上实际是死的**，我们只能自己补。
///
/// 只置位、不清位；winit 在 show / 约束变化时会整份重写 GWL_STYLE，补的位会被
/// 抹掉，故调用点分两处：安装子类时断言一次，**每次边缘按下时**再断言一次
/// （按下到 `DefWindowProc` 进入循环之间没有别的样式写入，这是最可靠的自愈点）。
fn ensure_sizebox_style(hwnd: HWND) {
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let new_style = with_sizebox_style(style);
        if new_style != style {
            SetWindowLongPtrW(hwnd, GWL_STYLE, new_style);
            log::info!("补齐 WS_THICKFRAME（原生边缘缩放前置样式位），style={:#x}", style);
        }
    }
}

/// 纯位运算：补 `WS_THICKFRAME`（幂等，不清任何既有位）。供单测锚定语义。
fn with_sizebox_style(style: isize) -> isize {
    style | WS_SIZEBOX_BIT
}

/// 按屏幕坐标（物理像素）判定命中区，返回 WM_NCHITTEST 应答码
/// （HTLEFT/HTRIGHT/…/HTCLIENT 的 Win32 数值）。纯函数便于单测。
/// `edge`/`corner` 为换算 DPI 后的物理像素热区宽度；角区优先于边区，
/// 且四角取 corner×corner 的更大抓取范围（Windows 通行交互）。
fn hit_test_zone(x: i32, y: i32, rect: (i32, i32, i32, i32), edge: i32, corner: i32) -> u32 {
    let (l, t, r, b) = rect;
    let in_left = x < l + edge;
    let in_right = x >= r - edge;
    let in_top = y < t + edge;
    let in_bottom = y >= b - edge;
    // 角区范围（corner ≥ edge，覆盖边的端头）
    let near_left = x < l + corner;
    let near_right = x >= r - corner;
    let near_top = y < t + corner;
    let near_bottom = y >= b - corner;

    if near_left && near_top {
        HTTOPLEFT
    } else if near_right && near_top {
        HTTOPRIGHT
    } else if near_left && near_bottom {
        HTBOTTOMLEFT
    } else if near_right && near_bottom {
        HTBOTTOMRIGHT
    } else if in_left {
        HTLEFT
    } else if in_right {
        HTRIGHT
    } else if in_top {
        HTTOP
    } else if in_bottom {
        HTBOTTOM
    } else {
        HTCLIENT
    }
}

/// 子类过程：接管边缘/角落的 WM_NCHITTEST（返回 HT_* 后 Windows 自己驱动
/// 原生尺寸拖拽循环，事件不再进入 Slint TouchArea），并施加最小/最大尺寸。
///
/// ## 为什么放弃 Slint TouchArea 方案
///
/// 原 TouchArea 方案的按压/拖动状态机在「窗口自身随拖拽连续变形」这一场景下
/// 实测不可靠（复现配方：首个拖拽后，重复按压同一热区大概率无响应，
/// resize-started 都不触发），且症状随操作序列非确定性地扩散到其他热区。
/// 原生命中测试由 Windows 直接驱动尺寸循环，不经过任何按压状态机，
/// 是无边框可缩放窗口的标准实现。
unsafe extern "system" fn native_resize_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    // 消息级耗时探针（`FIND_STUTTER_PROF=1` 开启）：窗口线程在这条消息上花了多久。
    //
    // 拖拽卡顿的定位靠这个：`SendMessageTimeout` 只能告诉你「线程这一拍被卡了」，
    // 卡在哪条消息上必须在这里看——是 WM_SIZE（重排 + 整窗重绘）、还是恰好撞上
    // 某个定时器回调被塞进模态循环派发。
    let prof = PROF_ENABLED.get_or_init(|| {
        std::env::var("FIND_STUTTER_PROF").as_deref() == Ok("1")
    });
    // 探针关闭时不取时间戳：本过程对每条窗口消息都会进来一次，几十纳秒
    // 也是白花的热路径开销（PROF_ENABLED 已缓存，t0 跟着条件化）。
    let t0 = if *prof { Some(std::time::Instant::now()) } else { None };
    // SAFETY：本回调由系统在窗口线程上调用；全部裸调用仅操作本窗口的
    // 矩形/尺寸信息，不跨线程共享可变状态
    let result = unsafe { native_resize_handle(hwnd, msg, wparam, lparam) };
    // 以下两类消息的耗时**等于整段拖拽时长**（它们是进入模态循环的入口，
    // 循环返回后消息才返回），必须排除，否则每次拖拽都刷一条「几秒」的假尖峰：
    // - `WM_NCLBUTTONDOWN`：我们直通 `DefWindowProcW` 的那条
    // - `WM_SYSCOMMAND`：`DefWindowProcW` 内部把按下翻译成 `SC_SIZE`/`SC_MOVE`
    //   后再次进入同一个循环
    if let Some(t0) = t0 {
        if msg != WM_NCLBUTTONDOWN && msg != WM_SYSCOMMAND {
            let ms = t0.elapsed().as_millis();
            if ms >= 50 {
                log::warn!("[PROF] 窗口线程 msg=0x{:04X} 处理 {}ms", msg, ms);
            }
        }
    }
    result
}

/// 探针开关缓存（每个进程只读一次环境变量）。
static PROF_ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// 子类过程的实际消息处理（拆出来是为了让 [`native_resize_proc`] 能在外层计时，
/// 不必在每个 `return` 分支上重复埋点）。
unsafe fn native_resize_handle(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
        WM_NCHITTEST => {
            // lParam = 屏幕物理坐标；多屏/负坐标按 i16 符号扩展
            let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            let mut rc = RECT::default();
            if GetWindowRect(hwnd, &mut rc).is_err() {
                return DefSubclassProc(hwnd, msg, wparam, lparam);
            }
            let dpi = GetDpiForWindow(hwnd).max(96);
            let edge = NATIVE_RESIZE_EDGE_LOGICAL * dpi as i32 / 96;
            let corner = NATIVE_RESIZE_CORNER_LOGICAL * dpi as i32 / 96;
            let ht = hit_test_zone(x, y, (rc.left, rc.top, rc.right, rc.bottom), edge, corner);
            if ht == HTCLIENT {
                DefSubclassProc(hwnd, msg, wparam, lparam)
            } else {
                LRESULT(ht as isize)
            }
        }
        WM_NCLBUTTONDOWN => {
            // 边缘/角落的按下：绕过 winit 直通 DefWindowProc——只有它会把
            // 尺寸类命中码转成原生尺寸拖拽模态循环（winit 会吞掉这条消息）
            let ht = wparam.0 as u32;
            if (HTLEFT..=HTBOTTOMRIGHT).contains(&ht) {
                // 进入循环前必须已带 WS_THICKFRAME（缺位时 DefWindowProc 直接
                // 忽略，表现为「光标能变、拖不动」）。winit 会重写样式，这里
                // 每次按下都自愈一次，幂等且只在缺位时写。
                ensure_sizebox_style(hwnd);
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        // 拖拽模态循环的进出：置位 / 清除「正在拖拽」标志。
        // 只做一次原子写，之后透传给 winit（它靠这两个消息维护内部状态，
        // 必须继续传递）。
        WM_ENTERSIZEMOVE => {
            SIZE_MOVE_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
            SIZE_MOVE_STARTED_MS.store(
                unix_millis(),
                std::sync::atomic::Ordering::Relaxed,
            );
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        WM_EXITSIZEMOVE => {
            SIZE_MOVE_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
            SIZE_MOVE_STARTED_MS.store(0, std::sync::atomic::Ordering::Relaxed);
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        WM_GETMINMAXINFO => {
            // 原生尺寸循环的 min/max 夹取（逻辑常量 × DPI → 物理像素）。
            //
            // ⚠ 这三个范围必须与 `overlay.slint` 里 `ProcessList` 根节点的
            // `min-width/min-height/max-width/max-height` 一致（那里是 Slint
            // 布局约束的真相源，winit 会据此 set_min/max_inner_size；这里
            // 拦在前面用同一组数值，保证约束传播偶尔滞后时也不会拖出怪尺寸）。
            let dpi = GetDpiForWindow(hwnd).max(96) as i32;
            let mmi = lparam.0 as *mut MINMAXINFO;
            (*mmi).ptMinTrackSize = POINT {
                x: 500 * dpi / 96,
                y: 300 * dpi / 96,
            };
            (*mmi).ptMaxTrackSize = POINT {
                x: 1200 * dpi / 96,
                y: 900 * dpi / 96,
            };
            LRESULT::default()
        }
        _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}

/// 给窗口安装原生边缘缩放命中测试（幂等：同 ID 重复安装只更新回调）。
/// 仅 Windows 生效；其余平台无边框缩放仍走 Slint 热区（若保留）。
///
/// 安装时顺带补 `WS_THICKFRAME`（见 [`ensure_sizebox_style`]）——没有这一位，
/// 命中测试再准也拖不动窗口。
#[cfg(windows)]
pub fn install_native_resize(win: &slint::Window) {
    // 安装即复位拖拽标志：窗口（重）建时不可能正处于原生拖拽循环中，
    // 若上一轮拖拽的 WM_EXITSIZEMOVE 已丢失（标志卡在 true），这里是
    // 确定性的清零点，与 size_move_active 里的 30s 超时兜底双保险。
    SIZE_MOVE_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
    SIZE_MOVE_STARTED_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    if let Some(hwnd) = extract_hwnd_from(win) {
        // SAFETY：hwnd 为当前线程上合法的 Slint 窗口；回调只读窗口矩形 /
        // 写 MINMAXINFO，不持有任何跨线程状态
        unsafe {
            let _ = SetWindowSubclass(
                hwnd,
                Some(native_resize_proc),
                NATIVE_RESIZE_SUBCLASS_ID,
                0,
            );
        }
        ensure_sizebox_style(hwnd);
    } else {
        log::warn!("原生边缘缩放子类化失败：无法提取 HWND（500ms 后重试）");
    }
}

#[cfg(not(windows))]
pub fn install_native_resize(_win: &slint::Window) {}

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
        let new = toggle_tool_window_bits(style);
        assert_ne!(new & TOOLWINDOW, 0);
        assert_eq!(new & APPWINDOW, 0);
        assert_ne!(new & 0x1000, 0); // 不破坏其他位
    }

    /// 已设置且无 APPWINDOW 时幂等（ensure 里 `new_style != style` 才改）
    #[test]
    fn tool_window_idempotent_when_already_clean() {
        let style = TOOLWINDOW | 0x1000; // 无 APPWINDOW
        assert_eq!(toggle_tool_window_bits(style), style);
    }

    /// 典型 winit 初始样式（0x40118）应判定为需要修改
    #[test]
    fn tool_window_detects_winit_default_needs_fix() {
        let style = 0x40118; // 实测 winit 默认：APPWINDOW|WINDOWEDGE|TOPMOST|ACCEPTFILES
        let new = toggle_tool_window_bits(style);
        assert_ne!(new, style); // APPWINDOW 被清除 → 需要修改
        assert_eq!(new & APPWINDOW, 0);
        assert_ne!(new & TOOLWINDOW, 0);
        assert_eq!(new & 0x100, 0x100); // WINDOWEDGE 保留
    }

    // ===== WS_EX_APPWINDOW（进程详情页 / 卡顿分析页要留在任务栏）=====

    /// 主窗口位逻辑：置 APPWINDOW + 清 TOOLWINDOW + 保留其他
    #[test]
    fn app_window_sets_bit_and_clears_toolwindow() {
        let style = TOOLWINDOW | 0x1000;
        let new = toggle_app_window_bits(style);
        assert_ne!(new & APPWINDOW, 0);
        assert_eq!(new & TOOLWINDOW, 0);
        assert_ne!(new & 0x1000, 0);
    }

    /// winit 默认样式（已含 APPWINDOW）→ app 版本应是恒等（零重绘开销）
    #[test]
    fn app_window_idempotent_on_winit_default() {
        let style = 0x40118;
        assert_eq!(toggle_app_window_bits(style), style);
    }

    /// 两个方向互为逆运算：工具位集合与普通位集合不重叠，来回切换不残留
    #[test]
    fn tool_and_app_window_bits_are_inverse() {
        let base = 0x10100; // WINDOWEDGE + ACCEPTFILES
        let tool = toggle_tool_window_bits(base);
        let app = toggle_app_window_bits(tool);
        assert_eq!(app & TOOLWINDOW, 0);
        assert_ne!(app & APPWINDOW, 0);
        // 再切回工具窗口应回到最初的工具位集合
        assert_eq!(toggle_tool_window_bits(app), tool);
    }

    // ===== WS_THICKFRAME（原生尺寸拖拽循环的前置样式位）=====

    /// 实测的进程详情窗口样式（CAPTION|SYSMENU|MINIMIZEBOX|CLIPSIBLINGS|
    /// CLIPCHILDREN|VISIBLE = 0x16ca0000，**没有** WS_THICKFRAME）：补位后必须
    /// 恰好多出 0x40000，且既有位一个不少——否则补样式 B 顺手弄坏 A。
    #[test]
    fn sizebox_added_to_real_detail_window_style() {
        let style = 0x16ca0000isize;
        let fixed = with_sizebox_style(style);
        assert_eq!(fixed, 0x16ce0000);
        assert_eq!(fixed & style, style, "既有样式位必须全部保留");
        assert_ne!(fixed & WS_SIZEBOX_BIT, 0);
    }

    /// 幂等：已带该位时结果不变（ensure 里 `new_style != style` 才写回，
    /// 保证周期性断言是零代价的）。
    #[test]
    fn sizebox_idempotent() {
        let once = with_sizebox_style(0x16ca0000);
        assert_eq!(with_sizebox_style(once), once);
        // 空样式只补这一位
        assert_eq!(with_sizebox_style(0), WS_SIZEBOX_BIT);
    }

    // ===== NativeMenuCmd（右键原生菜单命令）=====

    /// 验证：命令码与 TrackPopupMenu 返回值映射一致（1/2/3/4/5/6）
    #[test]
    fn native_menu_cmd_ids_stable() {
        assert_eq!(NativeMenuCmd::TogglePause as u32, 1);
        assert_eq!(NativeMenuCmd::ToggleClickThrough as u32, 2);
        assert_eq!(NativeMenuCmd::Quit as u32, 3);
        assert_eq!(NativeMenuCmd::ProcessList as u32, 4);
        assert_eq!(NativeMenuCmd::Analysis as u32, 5);
        assert_eq!(NativeMenuCmd::HideOverlay as u32, 6);
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
            5 => Some(NativeMenuCmd::Analysis),
            6 => Some(NativeMenuCmd::HideOverlay),
            _ => None,
        };
        assert_eq!(map(1), Some(NativeMenuCmd::TogglePause));
        assert_eq!(map(2), Some(NativeMenuCmd::ToggleClickThrough));
        assert_eq!(map(3), Some(NativeMenuCmd::Quit));
        assert_eq!(map(4), Some(NativeMenuCmd::ProcessList));
        assert_eq!(map(5), Some(NativeMenuCmd::Analysis));
        assert_eq!(map(6), Some(NativeMenuCmd::HideOverlay));
        assert_eq!(map(0), None); // 用户取消
        assert_eq!(map(99), None);
    }

    // ===== 贴边吸附（snap_axis 纯函数）=====

    /// 工作区横向 [0, 1920]，阈值 16：近端 / 远端 / 中间各自行为
    #[test]
    fn snap_axis_near_far_and_middle() {
        // 近端：距左缘 10px → 贴 0
        assert_eq!(snap_axis(10, 300, 0, 1920, 16), 0);
        // 远端：右缘 1900 距 1920 差 20px（>16）→ 不动
        assert_eq!(snap_axis(1600, 300, 0, 1920, 16), 1600);
        // 远端：右缘 1910 距差 10px（<=16）→ 贴 1620
        assert_eq!(snap_axis(1610, 300, 0, 1920, 16), 1620);
        // 中间：两侧都远 → 不动
        assert_eq!(snap_axis(500, 300, 0, 1920, 16), 500);
    }

    /// 窗口比工作区还宽时不启用远端吸附（否则算出越界坐标）
    #[test]
    fn snap_axis_skips_far_when_wider_than_work_area() {
        assert_eq!(snap_axis(2, 2000, 0, 1920, 16), 0); // 近端仍吸
        assert_eq!(snap_axis(1000, 2000, 0, 1920, 16), 1000); // 远端不吸
    }

    /// 阈值非正（吸附近似关闭）→ 原位置返回
    #[test]
    fn snap_axis_zero_threshold_is_noop() {
        assert_eq!(snap_axis(3, 300, 0, 1920, 0), 3);
    }

    // ===== 工作区夹取（clamp_rect_to_work / place_popup_rect）=====

    /// 1920×1080 去任务栏后的典型工作区
    const WORK: (i32, i32, i32, i32) = (0, 0, 1920, 1040);

    /// 完全在工作区内 → 原样返回（调用方据此不做多余的 SetWindowPos）
    #[test]
    fn clamp_rect_noop_when_inside() {
        assert_eq!(clamp_rect_to_work(100, 100, 900, 506, WORK), (100, 100, 900, 506));
    }

    /// 右侧越界（x=1500 + 900 = 2400 > 1920）→ 左移到 1020
    #[test]
    fn clamp_rect_pulls_back_from_right_edge() {
        assert_eq!(clamp_rect_to_work(1500, 100, 900, 506, WORK), (1020, 100, 900, 506));
    }

    /// 下方越界（y=800 + 506 = 1306 > 1040）→ 上移到 534
    #[test]
    fn clamp_rect_pulls_back_from_bottom_edge() {
        assert_eq!(clamp_rect_to_work(100, 800, 900, 506, WORK), (100, 534, 900, 506));
    }

    /// 左上越界（跨屏拖动后残留的负坐标）→ 贴回工作区左上
    #[test]
    fn clamp_rect_pulls_back_from_negative() {
        assert_eq!(clamp_rect_to_work(-200, -50, 900, 506, WORK), (0, 0, 900, 506));
    }

    /// 窗口比工作区还大 → 缩到工作区大小，位置贴左上（不能留下越界坐标）
    #[test]
    fn clamp_rect_shrinks_when_larger_than_work() {
        assert_eq!(clamp_rect_to_work(0, 0, 3000, 2000, WORK), (0, 0, 1920, 1040));
        assert_eq!(clamp_rect_to_work(500, 500, 3000, 2000, WORK), (0, 0, 1920, 1040));
    }

    /// 多屏负坐标工作区（主屏左侧的副屏 rcWork = (-1920,0,0,1080)）
    #[test]
    fn clamp_rect_handles_negative_work_origin() {
        let work = (-1920, 0, 0, 1080);
        assert_eq!(clamp_rect_to_work(-1000, 100, 900, 506, work), (-1000, 100, 900, 506));
        assert_eq!(clamp_rect_to_work(-50, 100, 900, 506, work), (-900, 100, 900, 506));
    }

    /// 锚点在屏幕中部：弹窗放锚点下方 gap 处
    #[test]
    fn popup_prefers_below_anchor() {
        let anchor = (100, 100, 400, 62);
        assert_eq!(
            place_popup_rect(anchor, 900, 506, WORK, 8),
            (100, 170, 900, 506)
        );
    }

    /// 锚点贴底：下方放不下 → 翻到上方
    #[test]
    fn popup_flips_above_when_no_room_below() {
        let anchor = (100, 900, 400, 62); // 下方剩余 1040-962=78 < 506
        let (x, y, w, h) = place_popup_rect(anchor, 900, 506, WORK, 8);
        assert_eq!((x, w, h), (100, 900, 506));
        assert_eq!(y, 900 - 8 - 506); // 386：贴锚点上方，仍完整可见
        assert!(y + h <= WORK.3);
    }

    /// 锚点靠右：横向被夹回，右边缘不越界
    #[test]
    fn popup_clamped_horizontally() {
        let anchor = (1700, 100, 200, 62);
        let (x, _y, w, _h) = place_popup_rect(anchor, 900, 506, WORK, 8);
        assert_eq!(x + w, 1920);
        assert_eq!(x, 1020);
    }

    /// 上下都放不下（弹窗比工作区还高）：退回下方，位置交给 clamp
    #[test]
    fn popup_falls_back_to_clamp_when_too_tall() {
        let work = (0, 0, 1920, 400);
        let anchor = (100, 200, 400, 62);
        let (x, y, w, h) = place_popup_rect(anchor, 900, 506, work, 8);
        assert_eq!((x, y, w, h), (100, 0, 900, 400));
    }

    /// 两边都放不下时挑空间更大的一侧：锚点贴底、弹窗与锚点同高
    /// （上下都塞不进），选上方空间更大 → 弹窗贴顶，锚点仍在底部露出来，
    /// 而不是整个被弹窗盖住（旧逻辑会盖住，覆盖同一矩形）。
    #[test]
    fn popup_picks_larger_side_instead_of_covering_anchor() {
        // 工作区高 1040；锚点贴底 978..1040；上方剩余 970，下方剩 -8
        let anchor = (100, 978, 900, 62);
        let (x, y, w, h) = place_popup_rect(anchor, 900, 1012, WORK, 8);
        assert_eq!((x, y, w, h), (100, 0, 900, 1012));
        // 弹窗比工作区还高，注定要压住锚点一部分；关键是别「完全相同」地
        // 整个盖住（旧逻辑会返回与锚点同一矩形，用户什么都看不到）
        assert!(y + h <= WORK.3);
        assert!((x, y, w, h) != anchor, "弹窗不能与锚点完全重合");
    }

    // ===== 几何对账（Slint 布局 vs 原生窗口矩形）=====

    /// 四边一致 → 不脱节
    #[test]
    fn geometry_diverged_false_when_identical() {
        assert!(!geometry_diverged((100, 200, 800, 600), (100, 200, 800, 600)));
    }

    /// 右/下尺寸漂移 1px（丢一次 WM_WINDOWPOSCHANGED 同步的典型形态）→ 脱节
    #[test]
    fn geometry_diverged_true_on_size_drift() {
        assert!(geometry_diverged((100, 200, 800, 600), (100, 200, 801, 600)));
        assert!(geometry_diverged((100, 200, 800, 600), (100, 200, 800, 601)));
    }

    /// 位置漂移（跨屏拖动后位置回传丢失）→ 脱节
    #[test]
    fn geometry_diverged_true_on_position_drift() {
        assert!(geometry_diverged((100, 200, 800, 600), (99, 200, 800, 600)));
        assert!(geometry_diverged((100, 200, 800, 600), (100, -200, 800, 600)));
    }

    // ===== 原生边缘缩放命中测试（hit_test_zone）=====

    /// HT 应答码 Win32 数值（windows-rs 常量），测试断言用
    const HT_CODES: [(&str, u32); 9] = [
        ("client", 1),
        ("left", 10),
        ("right", 11),
        ("top", 12),
        ("top-left", 13),
        ("top-right", 14),
        ("bottom", 15),
        ("bottom-left", 16),
        ("bottom-right", 17),
    ];

    fn ht_code(name: &str) -> u32 {
        HT_CODES.iter().find(|(n, _)| *n == name).unwrap().1
    }

    /// 200% 缩放：edge=16、corner=48 物理像素，窗口 (100,200)-(900,800)
    const R: (i32, i32, i32, i32) = (100, 200, 900, 800);
    const EDGE: i32 = 16;
    const CORNER: i32 = 48;

    #[test]
    fn hit_test_zone_edges() {
        assert_eq!(hit_test_zone(105, 500, R, EDGE, CORNER), ht_code("left"));
        assert_eq!(hit_test_zone(895, 500, R, EDGE, CORNER), ht_code("right"));
        assert_eq!(hit_test_zone(500, 205, R, EDGE, CORNER), ht_code("top"));
        assert_eq!(hit_test_zone(500, 795, R, EDGE, CORNER), ht_code("bottom"));
    }

    #[test]
    fn hit_test_zone_corners() {
        assert_eq!(hit_test_zone(110, 240, R, EDGE, CORNER), ht_code("top-left"));
        assert_eq!(hit_test_zone(880, 220, R, EDGE, CORNER), ht_code("top-right"));
        assert_eq!(hit_test_zone(120, 780, R, EDGE, CORNER), ht_code("bottom-left"));
        assert_eq!(hit_test_zone(890, 790, R, EDGE, CORNER), ht_code("bottom-right"));
    }

    /// 边的端头被角区吸收（corner > edge）：左边缘的上端应判为左上角
    #[test]
    fn hit_test_zone_corner_absorbs_strip_end() {
        assert_eq!(hit_test_zone(105, 230, R, EDGE, CORNER), ht_code("top-left"));
        // 角区之外的边中段仍是边
        assert_eq!(hit_test_zone(105, 500, R, EDGE, CORNER), ht_code("left"));
    }

    #[test]
    fn hit_test_zone_interior_is_client() {
        assert_eq!(hit_test_zone(500, 500, R, EDGE, CORNER), ht_code("client"));
        // 100% 缩放（edge=8, corner=24）下窗口中心同样为 client
        assert_eq!(hit_test_zone(500, 500, R, 8, 24), ht_code("client"));
    }

    /// 边热区宽度生效：100% 缩放下 x=l+8 恰在热区外缘 → client
    #[test]
    fn hit_test_zone_respects_edge_width() {
        assert_eq!(hit_test_zone(107, 500, R, 8, 24), ht_code("left"));
        assert_eq!(hit_test_zone(108, 500, R, 8, 24), ht_code("client"));
    }

    // ===== RowMenuCmd（进程详情行右键菜单命令）=====

    /// 验证：命令码与 TrackPopupMenu 返回值映射一致（1/2/3/4）
    #[test]
    fn row_menu_cmd_ids_stable() {
        assert_eq!(RowMenuCmd::OpenLocation as u32, 1);
        assert_eq!(RowMenuCmd::Kill as u32, 2);
        assert_eq!(RowMenuCmd::KillTree as u32, 3);
        assert_eq!(RowMenuCmd::Detail as u32, 4);
    }

    /// 验证：命令码到枚举的映射（生产分支与单测共用 `RowMenuCmd::from_id`）
    #[test]
    fn row_menu_cmd_from_trackpopup_value() {
        assert_eq!(RowMenuCmd::from_id(1), Some(RowMenuCmd::OpenLocation));
        assert_eq!(RowMenuCmd::from_id(2), Some(RowMenuCmd::Kill));
        assert_eq!(RowMenuCmd::from_id(3), Some(RowMenuCmd::KillTree));
        assert_eq!(RowMenuCmd::from_id(4), Some(RowMenuCmd::Detail));
        assert_eq!(RowMenuCmd::from_id(0), None); // 用户取消 / 点标题行
        assert_eq!(RowMenuCmd::from_id(99), None);
    }

    /// 命令码与枚举取值必须处处一致：`from_id(cmd as u32) == Some(cmd)`。
    /// 新增菜单项时这条会先红，避免「加了变体忘了进 from_id」。
    #[test]
    fn row_menu_cmd_round_trips_through_from_id() {
        for cmd in [
            RowMenuCmd::OpenLocation,
            RowMenuCmd::Kill,
            RowMenuCmd::KillTree,
            RowMenuCmd::Detail,
        ] {
            assert_eq!(
                RowMenuCmd::from_id(cmd as u32),
                Some(cmd),
                "命令码 {} 在 from_id 里缺失或映射错位",
                cmd as u32
            );
        }
    }
}
