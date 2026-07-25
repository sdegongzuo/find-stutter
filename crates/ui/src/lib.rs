//! find-stutter UI（Slint 实现）
//!
//! 模块：
//! - [`overlay`]    纯数据：格式化函数、`OverlayState` 状态、UI 属性推送
//! - [`skin`]       皮肤配置（TOML 反序列化，无 UI 框架依赖）
//! - [`window`]     Windows 原生窗口控制：置顶、点击穿透、HWND 缓存
//!
//! 入口：[`run_overlay`] 创建 Slint 窗口、启动后台采集线程、进入事件循环。

pub mod overlay;
pub mod skin;

#[cfg(windows)]
pub mod window;

use std::sync::Arc;
use std::time::Duration;

use find_stutter_core::{Collector, Config, Detector, Logger, Severity, StorageConfig};
use parking_lot::Mutex;
use slint::{ComponentHandle, PhysicalPosition, Timer, TimerMode, Weak};

use crate::overlay::{apply_metrics, OverlayState};
use crate::skin::SkinConfig;

slint::include_modules!();

/// 启动悬浮窗入口
///
/// 流程：
/// 1. 创建 Slint 窗口（`Overlay`），绑定回调 + 应用皮肤
/// 2. 启动后台采集线程（共享 Arc<Mutex<Sample/OverlayState/paused>>）
/// 3. 启动 1Hz UI 刷新定时器（从共享状态读 → 推 Slint 属性）
/// 4. `run()` 进入 Slint 事件循环
pub fn run_overlay() -> anyhow::Result<()> {
    let window = Overlay::new()?;
    let skin = SkinConfig::load("default");

    // 共享状态：采集线程写入，UI 定时器读取
    let sample: Arc<Mutex<find_stutter_core::Sample>> =
        Arc::new(Mutex::new(find_stutter_core::Sample::default()));
    let state: Arc<Mutex<OverlayState>> = Arc::new(Mutex::new(OverlayState::default()));
    let paused: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));

    // 应用初始皮肤（颜色 / 字体 / 圆角）
    overlay::apply_skin(&window, &skin);

    // ===== 回调绑定 =====
    bind_callbacks(&window, &paused, &state);

    // ===== 启动后台采集线程 =====
    spawn_collector(sample.clone(), state.clone(), paused.clone());

    // ===== 启动 1Hz UI 刷新定时器 =====
    start_refresh_timer(window.as_weak(), sample.clone(), state.clone(), skin.clone());

    // 显示窗口，进入事件循环
    window.show()?;
    window.run()?;
    Ok(())
}

fn bind_callbacks(window: &Overlay, paused: &Arc<Mutex<bool>>, state: &Arc<Mutex<OverlayState>>) {
    // 拖动：手动跟踪窗口起点 + 鼠标位置，drag-move 时调用 set_position。
    // 关键：仅在用户实际拖动期间更新窗口位置（不是每帧），
    // 避免 egui 即时模式下「每帧 OuterPosition」导致透明窗重影/闪烁。
    {
        let weak = window.as_weak();
        // 共享拖动状态：start_window_pos + start_local_pos
        let drag_state: Arc<Mutex<Option<DragState>>> = Arc::new(Mutex::new(None));

        // down：开始拖动（local_x/y 由 .slint 端从 touch.mouse_x/y 传入）
        let weak_for_start = weak.clone();
        let drag_state_for_start = drag_state.clone();
        window.on_drag_start(move |local_x, local_y| {
            let Some(w) = weak_for_start.upgrade() else { return };
            if w.get_click_through() {
                return; // 穿透模式禁止拖动
            }
            let start_pos = w.window().position();
            *drag_state_for_start.lock() = Some(DragState {
                start_window_pos: start_pos,
                start_local: (local_x, local_y),
            });
        });

        // move：更新窗口位置
        let weak_for_move = weak.clone();
        let drag_state_for_move = drag_state.clone();
        window.on_drag_move(move |local_x, local_y| {
            let Some(w) = weak_for_move.upgrade() else { return };
            if w.get_click_through() {
                return;
            }
            let Some(state) = drag_state_for_move.lock().clone() else { return };
            let scale = w.window().scale_factor();
            let dx = (local_x - state.start_local.0) * scale;
            let dy = (local_y - state.start_local.1) * scale;
            let new_x = state.start_window_pos.x + dx as i32;
            let new_y = state.start_window_pos.y + dy as i32;
            w.window().set_position(PhysicalPosition::new(new_x, new_y));
        });

        // up：结束拖动
        let drag_state_for_end = drag_state.clone();
        window.on_drag_end(move || {
            *drag_state_for_end.lock() = None;
        });
    }

    // 左键单击：展开/收起详情
    {
        let weak = window.as_weak();
        window.on_expand_toggle(move || {
            if let Some(w) = weak.upgrade() {
                let cur = w.get_expanded();
                w.set_expanded(!cur);
                w.set_menu_y(if !cur { 200.0 } else { 90.0 });
            }
        });
    }

    // 右键：弹出/关闭菜单
    {
        let weak = window.as_weak();
        window.on_context_menu(move || {
            if let Some(w) = weak.upgrade() {
                let cur = w.get_menu_open();
                w.set_menu_open(!cur);
            }
        });
    }

    // 关闭菜单
    {
        let weak = window.as_weak();
        window.on_menu_close(move || {
            if let Some(w) = weak.upgrade() {
                w.set_menu_open(false);
            }
        });
    }

    // 暂停/恢复
    {
        let paused = paused.clone();
        let state = state.clone();
        window.on_menu_pause(move || {
            let mut g = paused.lock();
            *g = !*g;
            if *g {
                let mut s = state.lock();
                s.last_stutter_severity = None;
                s.flash_until = std::time::Instant::now();
            }
        });
    }

    // 切换点击穿透（菜单项）
    {
        let weak = window.as_weak();
        let state = state.clone();
        window.on_menu_click_through(move || {
            if let Some(w) = weak.upgrade() {
                let new = !w.get_click_through();
                w.set_click_through(new);
                #[cfg(windows)]
                {
                    if let Err(e) = window::set_click_through_for(&w, new) {
                        log::warn!("set_click_through({}) failed: {}", new, e);
                    }
                }
                if !new {
                    let mut s = state.lock();
                    s.flash_until = std::time::Instant::now();
                }
            }
        });
    }

    // T 键
    {
        let weak = window.as_weak();
        window.on_key_toggle_click_through(move || {
            if let Some(w) = weak.upgrade() {
                let new = !w.get_click_through();
                w.set_click_through(new);
                #[cfg(windows)]
                {
                    if let Err(e) = window::set_click_through_for(&w, new) {
                        log::warn!("set_click_through({}) failed: {}", new, e);
                    }
                }
            }
        });
    }

    // 退出
    {
        let weak = window.as_weak();
        window.on_menu_quit(move || {
            if let Some(w) = weak.upgrade() {
                w.window().hide().ok();
                slint::quit_event_loop().ok();
            }
        });
    }
}

/// 拖动起点状态
#[derive(Clone, Copy)]
struct DragState {
    /// 拖动开始时窗口在屏幕上的位置（物理像素）
    start_window_pos: PhysicalPosition,
    /// 拖动开始时鼠标相对于 TouchArea 的位置（逻辑像素）
    start_local: (f32, f32),
}

fn spawn_collector(
    sample: Arc<Mutex<find_stutter_core::Sample>>,
    state: Arc<Mutex<OverlayState>>,
    paused: Arc<Mutex<bool>>,
) {
    std::thread::spawn(move || {
        let config = Config::load("config.toml").unwrap_or_default();
        let mut collector = Collector::new();
        let mut detector = Detector::new(&config.detection);
        let mut logger = Logger::new(&config.storage).unwrap_or_else(|e| {
            log::error!("Logger init failed ({}), using default path", e);
            Logger::new(&StorageConfig::default()).expect("fallback logger init")
        });
        let mut tick: u32 = 0;

        loop {
            if *paused.lock() {
                std::thread::sleep(Duration::from_millis(400));
                continue;
            }

            let s = collector.collect();
            tick += 1;

            if let Err(e) = logger.write_sample(&s) {
                log::warn!("write_sample failed: {}", e);
            }

            if let Some(event) = detector.analyze(&s) {
                if let Err(e) = logger.write_event(&event) {
                    log::warn!("write_event failed: {}", e);
                }
                let mut st = state.lock();
                st.stutter_count += 1;
                if matches!(event.severity, Severity::Major | Severity::Critical) {
                    st.last_stutter_severity = Some(event.severity);
                    st.flash_until = std::time::Instant::now() + Duration::from_millis(900);
                }
            }

            if tick % 10 == 0 {
                let _ = logger.flush();
            }
            if tick % 3600 == 0 {
                let _ = logger.cleanup();
            }

            {
                let mut g = sample.lock();
                *g = s.clone();
            }
            {
                let mut st = state.lock();
                st.sent_total = s.net_sent_total;
                st.recv_total = s.net_recv_total;
            }

            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

fn start_refresh_timer(
    weak: Weak<Overlay>,
    sample: Arc<Mutex<find_stutter_core::Sample>>,
    state: Arc<Mutex<OverlayState>>,
    skin: SkinConfig,
) {
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
        if let Some(w) = weak.upgrade() {
            let s = sample.lock().clone();
            let st = state.lock().clone();
            apply_metrics(&w, &s, &st, &skin);
        }
    });
}
