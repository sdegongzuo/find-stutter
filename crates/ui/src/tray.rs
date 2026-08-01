//! 系统托盘图标（P1）。
//!
//! 用 `tray-icon` + `muda` 创建托盘图标 + 右键菜单：
//! - 「显示/隐藏悬浮窗」
//! - 「暂停/恢复监控」
//! - 「退出」
//!
//! ## 事件循环集成
//!
//! `tray-icon` 在 Windows 上要求**创建托盘的线程**必须有 win32 消息循环
//! （内部隐藏窗口 `tray_icon_app` 靠 `GetMessage`/`DispatchMessage` 泵消息）。
//! Slint 的 winit 事件循环在 UI 主线程，但我们不能把托盘塞进 Slint 的
//! 事件循环（它不对外暴露消息泵）。
//!
//! 方案：**后台线程**创建托盘图标并跑一个最小 win32 消息循环
//! （`GetMessageW` + `TranslateMessage` + `DispatchMessageW`），
//! 菜单事件 / 托盘点击事件通过 `muda::MenuEvent::receiver()` /
//! `tray_icon::TrayIconEvent::receiver()` 全局 channel 轮询，
//! 再用 [`slint::invoke_from_event_loop`] 投递到 UI 线程执行。
//!
//! 这样托盘事件处理与 Slint 事件循环解耦，不需要碰 Slint 内部。

use std::sync::mpsc::{Receiver, Sender};

use slint::ComponentHandle;

use crate::Overlay;

/// 托盘菜单项 ID（与菜单事件 id 匹配）
pub mod menu_id {
    pub const SHOW_HIDE: &str = "show_hide";
    pub const TOGGLE_PAUSE: &str = "toggle_pause";
    pub const QUIT: &str = "quit";
}

/// 托盘后台线程与 UI 线程之间的事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    /// 显示 / 隐藏悬浮窗
    ShowHide,
    /// 暂停 / 恢复监控（UI 只读模式：切换 paused 标志，暂停刷新显示）
    TogglePause,
    /// 退出程序（quit_event_loop）
    Quit,
}

/// 托盘句柄：持有后台线程 + receiver。
/// 只要它活着，托盘图标就在系统托盘上。
pub struct Tray {
    /// 后台线程的 join handle（持有防止线程退出）
    _thread: std::thread::JoinHandle<()>,
    /// 接收托盘命令
    rx: Receiver<TrayCommand>,
    /// 发送端（保留以维持类型；实际由线程持有）
    #[allow(dead_code)]
    tx: Sender<TrayCommand>,
}

impl Tray {
    /// 启动托盘后台线程。
    ///
    /// 失败（创建图标 / 菜单失败）返回 `Err`，调用方 log warn 后继续
    /// （托盘是增强功能，失败不应阻塞 GUI）。
    pub fn spawn() -> anyhow::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();

        #[cfg(windows)]
        let thread = {
            let tx = tx.clone();
            std::thread::Builder::new()
                .name("find-stutter-tray".into())
                .spawn(move || {
                    if let Err(e) = run_tray_loop(tx) {
                        log::warn!("托盘图标启动失败: {}", e);
                    }
                })?
        };

        #[cfg(not(windows))]
        let thread = {
            let _ = tx;
            std::thread::Builder::new()
                .name("find-stutter-tray".into())
                .spawn(|| {})?
        };

        Ok(Self { _thread: thread, rx, tx })
    }

    /// 非阻塞接收一个托盘命令。
    pub fn try_recv(&self) -> Option<TrayCommand> {
        self.rx.try_recv().ok()
    }
}

#[cfg(windows)]
fn run_tray_loop(tx: Sender<TrayCommand>) -> anyhow::Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage, MSG,
    };

    // 1) 构建右键菜单
    let menu = build_menu()?;

    // 2) 构建托盘图标（纯色 32x32 图标，无需外部资源文件）
    let icon = build_icon()?;
    let tray = tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("find-stutter 系统卡顿监控")
        .with_icon(icon)
        .build()?;
    let _tray = tray;

    // 3) 事件泵：轮询菜单事件 + 托盘点击事件，转发到 channel
    let tx_menu = tx.clone();
    let tx_tray = tx.clone();
    std::thread::Builder::new()
        .name("find-stutter-tray-events".into())
        .spawn(move || {
            loop {
                // 菜单事件
                if let Ok(ev) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                    let cmd = match ev.id().0.as_str() {
                        menu_id::SHOW_HIDE => Some(TrayCommand::ShowHide),
                        menu_id::TOGGLE_PAUSE => Some(TrayCommand::TogglePause),
                        menu_id::QUIT => Some(TrayCommand::Quit),
                        _ => None,
                    };
                    if let Some(cmd) = cmd {
                        let _ = tx_menu.send(cmd);
                    }
                }
                // 托盘图标点击（左键单击 = 显示/隐藏悬浮窗）
                if let Ok(ev) = tray_icon::TrayIconEvent::receiver().try_recv() {
                    use tray_icon::MouseButtonState;
                    if let tray_icon::TrayIconEvent::Click {
                        button: tray_icon::MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = ev
                    {
                        let _ = tx_tray.send(TrayCommand::ShowHide);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        })?;

    // 4) 跑 win32 消息循环（tray-icon 隐藏窗口靠它派发消息）。
    //    这个循环阻塞本线程直到消息队列关闭；托盘存在期间一直跑。
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }
    Ok(())
}

#[cfg(windows)]
fn build_menu() -> anyhow::Result<tray_icon::menu::Menu> {
    use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};

    let menu = Menu::new();
    let show_hide = MenuItem::with_id(menu_id::SHOW_HIDE, "显示/隐藏悬浮窗", true, None);
    let toggle_pause = MenuItem::with_id(menu_id::TOGGLE_PAUSE, "暂停/恢复监控", true, None);
    let sep = PredefinedMenuItem::separator();
    let quit = MenuItem::with_id(menu_id::QUIT, "退出", true, None);
    menu.append_items(&[&show_hide, &toggle_pause, &sep, &quit])?;
    Ok(menu)
}

#[cfg(windows)]
fn build_icon() -> anyhow::Result<tray_icon::Icon> {
    // 生成一个 32x32 的纯色图标（深蓝底 + 黄色圆点，无外部资源依赖）
    const SIZE: u32 = 32;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let cx = SIZE as f32 / 2.0;
    let cy = SIZE as f32 / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let idx = ((y * SIZE + x) * 4) as usize;
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= 10.0 {
                // 中心黄色圆
                rgba[idx] = 0xF9;
                rgba[idx + 1] = 0xE2;
                rgba[idx + 2] = 0xAF;
                rgba[idx + 3] = 0xFF;
            } else if dist <= 16.0 {
                // 外圈深蓝
                rgba[idx] = 0x1E;
                rgba[idx + 1] = 0x1E;
                rgba[idx + 2] = 0x2E;
                rgba[idx + 3] = 0xFF;
            } else {
                // 透明
                rgba[idx + 3] = 0x00;
            }
        }
    }
    Ok(tray_icon::Icon::from_rgba(rgba, SIZE, SIZE)?)
}

/// 把托盘命令应用到 UI。
///
/// 在 UI 线程调用（通过 `slint::invoke_from_event_loop` 或直接主线程）。
pub fn apply_command(ui: &Overlay, state: &mut crate::overlay::OverlayState, cmd: TrayCommand) {
    match cmd {
        TrayCommand::ShowHide => {
            if ui.window().is_visible() {
                ui.hide().ok();
            } else {
                ui.show().ok();
            }
        }
        TrayCommand::TogglePause => {
            state.paused = !state.paused;
        }
        TrayCommand::Quit => {
            let _ = slint::quit_event_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证：菜单 ID 常量稳定（后台线程按此匹配事件）
    #[test]
    fn menu_ids_are_stable() {
        assert_eq!(menu_id::SHOW_HIDE, "show_hide");
        assert_eq!(menu_id::TOGGLE_PAUSE, "toggle_pause");
        assert_eq!(menu_id::QUIT, "quit");
    }

    /// 验证：TrayCommand PartialEq 可用于事件分发
    #[test]
    fn tray_command_eq() {
        assert_eq!(TrayCommand::ShowHide, TrayCommand::ShowHide);
        assert_ne!(TrayCommand::ShowHide, TrayCommand::Quit);
    }

    /// 验证：spawn 不阻塞、返回句柄，且空队列 try_recv 返回 None
    #[test]
    fn spawn_and_try_recv_empty() {
        let tray = Tray::spawn().expect("托盘启动失败");
        assert!(tray.try_recv().is_none());
    }
}
