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
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW,
    GetWindowLongPtrW, GetWindowRect, GWL_EXSTYLE, SetWindowLongPtrW, SetWindowPos, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT,
    HTCLIENT, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, WM_GETMINMAXINFO, WM_NCHITTEST,
    WM_NCLBUTTONDOWN, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, MINMAXINFO,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;

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
        let _ = AppendMenuW(
            hmenu,
            MF_STRING,
            NativeMenuCmd::Analysis as usize,
            wide("卡顿分析"),
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
            5 => Some(NativeMenuCmd::Analysis),
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

/// 弹一次行右键菜单（顶部标题行 + 「打开文件所在的位置」+「停止进程」，有子进程时
/// 追加「结束进程树」），**不循环**。返回 [`RowMenuOutcome`]：
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
        // 服务条目（pid<=0）没有可定位的可执行文件 → 置灰「打开文件所在的位置」
        let locate_flags = if pid <= 0 { MF_GRAYED } else { MF_STRING };
        let _ = AppendMenuW(
            hmenu,
            locate_flags,
            RowMenuCmd::OpenLocation as usize,
            wide("打开文件所在的位置"),
        );
        let _ = AppendMenuW(hmenu, MF_STRING, RowMenuCmd::Kill as usize, wide("停止进程"));
        // 结束进程树：连子进程一起终止（任务管理器同款入口）。
        // 仅当该进程确实有后代时才显示——无可杀子树时该入口无意义。
        if kill_tree_enabled {
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

        match cmd.0 as u32 {
            1 => RowMenuOutcome::Command(RowMenuCmd::OpenLocation),
            2 => RowMenuOutcome::Command(RowMenuCmd::Kill),
            3 => RowMenuOutcome::Command(RowMenuCmd::KillTree),
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

// ===== 原生边缘缩放（WM_NCHITTEST 子类化，进程详情窗口专用）=====

/// 详情面板是否打开。面板为全窗遮罩，打开期间边缘热区应让位（不接管命中测试），
/// 由 process_list.rs 在 set_detail_visible 的两处调用点同步翻转。
pub static DETAIL_PANEL_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 子类化 ID（同一 HWND 上多个子类时区分用；本窗口只有一个）
const NATIVE_RESIZE_SUBCLASS_ID: usize = 0x46_53; // "FS"

/// 边缘热区宽度（逻辑像素，与 slint 端 resize-margin 一致）
const NATIVE_RESIZE_EDGE_LOGICAL: i32 = 8;
/// 角落热区宽度（逻辑像素，与 slint 端右下角扁平热区一致）
const NATIVE_RESIZE_CORNER_LOGICAL: i32 = 24;

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
    // SAFETY：本回调由系统在窗口线程上调用；全部裸调用仅操作本窗口的
    // 矩形/尺寸信息，不跨线程共享可变状态
    unsafe {
        match msg {
        WM_NCHITTEST => {
            // 详情面板为模态遮罩，打开期间不接管（热区让位给面板交互）
            if DETAIL_PANEL_OPEN.load(std::sync::atomic::Ordering::SeqCst) {
                return DefSubclassProc(hwnd, msg, wparam, lparam);
            }
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
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        WM_GETMINMAXINFO => {
            // 原生尺寸循环的 min/max 夹取（逻辑常量 × DPI → 物理像素）
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
#[cfg(windows)]
pub fn install_native_resize(win: &slint::Window) {
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
        assert_eq!(NativeMenuCmd::Analysis as u32, 5);
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
            _ => None,
        };
        assert_eq!(map(1), Some(NativeMenuCmd::TogglePause));
        assert_eq!(map(2), Some(NativeMenuCmd::ToggleClickThrough));
        assert_eq!(map(3), Some(NativeMenuCmd::Quit));
        assert_eq!(map(4), Some(NativeMenuCmd::ProcessList));
        assert_eq!(map(5), Some(NativeMenuCmd::Analysis));
        assert_eq!(map(0), None); // 用户取消
        assert_eq!(map(99), None);
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

    /// 验证：命令码与 TrackPopupMenu 返回值映射一致（1/2/3）
    #[test]
    fn row_menu_cmd_ids_stable() {
        assert_eq!(RowMenuCmd::OpenLocation as u32, 1);
        assert_eq!(RowMenuCmd::Kill as u32, 2);
        assert_eq!(RowMenuCmd::KillTree as u32, 3);
    }

    /// 验证：命令码到枚举的映射（show_row_menu 的返回逻辑）
    #[test]
    fn row_menu_cmd_from_trackpopup_value() {
        let map = |v: u32| match v {
            1 => Some(RowMenuCmd::OpenLocation),
            2 => Some(RowMenuCmd::Kill),
            3 => Some(RowMenuCmd::KillTree),
            _ => None,
        };
        assert_eq!(map(1), Some(RowMenuCmd::OpenLocation));
        assert_eq!(map(2), Some(RowMenuCmd::Kill));
        assert_eq!(map(3), Some(RowMenuCmd::KillTree));
        assert_eq!(map(0), None); // 用户取消
        assert_eq!(map(99), None);
    }
}
