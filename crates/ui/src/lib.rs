//! find-stutter UI overlay (P3 read-only mode).
//!
//! ## P3 架构变化
//!
//! 不再在 GUI 内启动 Collector 线程。GUI 只做一件事：
//! 1Hz 轮询 `stutter.db`（由 find-stutter-service 后台持续写入）。
//!
//! - 删除 `spawn_collector()` / `Collector` 实例
//! - 新增 [`reader::DbReader`]：SQLite 只读连接 + 服务健康检测
//! - 1Hz 定时器 → 调 `DbReader::poll()` → 拿 `PollResult` 喂 Slint
//!
//! 服务健康检测：
//! - `Running`：心跳在 5s 内
//! - `Stale`：心跳存在但 > 5s
//! - `Stopped`：心跳表为空
//! - `NoDatabase`：stutter.db 不存在
//!
//! UI 反应：
//! - `Running` → 顶部状态条绿色 "● 服务运行中"
//! - `Stale`   → 黄色 "● 服务卡顿"
//! - `Stopped` / `NoDatabase` → 红色 "● 服务已停止"
//! - 暂停按钮在非 Running 时禁用

pub mod analysis;
pub mod analytics;
pub mod auto_start;
pub mod chart;
pub mod elevate;
pub mod hotreload;
pub mod notify;
pub mod overlay;
pub mod process_list;
pub mod reader;
pub mod skin;
pub mod taskbar;
pub mod tray;
pub mod window;

/// 趋势图渲染入口（chart.rs；M2 用 plotters 实现，M1 为桩）。
pub(crate) use chart::render_trend_chart;
/// F4 卡顿类型占比饼图渲染入口（chart.rs；M3）。
pub(crate) use chart::render_cause_pie;
/// F3 系统资源关联图渲染入口（chart.rs；M4，双轴：CPU%/内存% + 磁盘 B/s）。
pub(crate) use chart::render_resource_chart;

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use slint::{ComponentHandle, Timer};

use crate::overlay::OverlayState;
use crate::reader::{DbReader, PollResult};

slint::include_modules!();

/// 1Hz 轮询 tick 周期
const POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// 启动 UI overlay（只读模式）。
///
/// 加载 config.toml → 构造 `DbReader` → 起 1Hz 定时器 → 启动 Slint 事件循环。
///
/// 日志：用 `try_init` 容忍重复（bin crate 也可能 init 一次），
///       重复时静默忽略，不影响主流程。
pub fn run() -> anyhow::Result<()> {
    let _ = env_logger::try_init();

    let mut config = find_stutter_core::Config::load("config.toml").unwrap_or_else(|e| {
        log::warn!("config load failed ({}), using defaults", e);
        find_stutter_core::Config::default()
    });
    log::info!(
        "find-stutter overlay (P3 read-only) starting, db={}",
        config.storage.db_path
    );

    // 0) P3+：自动检测 + 启动后台服务（不影响 GUI 启动，失败只记日志）。
    //    自动测试环境可用 FIND_STUTTER_SKIP_SERVICE=1 或
    //    config.toml [ui] auto_start_service = false 完全跳过（避免弹 UAC）。
    let auto = if config.ui.auto_start_service
        && !auto_start::auto_start_disabled()
    {
        auto_start::ensure_service_running(std::path::Path::new(&config.storage.db_path))
    } else {
        log::info!(
            "服务自动启动已关闭 (auto_start_service={}, FIND_STUTTER_SKIP_SERVICE={:?})",
            config.ui.auto_start_service,
            std::env::var_os("FIND_STUTTER_SKIP_SERVICE")
        );
        auto_start::AutoStartResult::Skipped
    };
    if auto.is_ok() {
        log::info!("后台服务: {}", auto.message());
    } else {
        log::warn!("后台服务: {}", auto.message());
    }

    // 0b) P2：配置 / 皮肤热加载 watcher（notify 失败则降级为不监听）
    let watcher = hotreload::ConfigWatcher::new("config.toml", "skins").unwrap_or_else(|e| {
        log::warn!("配置热加载 watcher 启动失败 ({}), 热更新禁用", e);
        hotreload::ConfigWatcher::disabled()
    });

    // 1) 加载皮肤
    let skin_cfg = skin::SkinConfig::load(&config.ui.skin);
    let state = Arc::new(Mutex::new(OverlayState::new(skin_cfg)));

    // 2) 构造只读 reader（db 暂时不存在不会立即失败，下一次 tick 会重试）
    let reader = Arc::new(DbReader::new(config.storage.db_path.clone()));

    // 3) 启动 Slint 窗口
    let ui = Overlay::new()?;
    // 3a) 接管拖动：no-frame 模式下去掉了原生 HTCAPTION，
    //     必须把 TouchArea 的 dx/dy 转成 window.set_position()。
    //     on_drag_moved 要求 callback 是 'static，所以 clone Weak 拿进来。
    let weak_ui_for_drag = ui.as_weak();
    ui.on_drag_moved(move |dx, dy| {
        use slint::PhysicalPosition;
        if let Some(ui) = weak_ui_for_drag.upgrade() {
            let window = ui.window();
            let scale = window.scale_factor();
            let pos = window.position();
            // dx/dy 是 logical px，转 physical px 后叠加
            let new_x = pos.x + (dx as f32 * scale) as i32;
            let new_y = pos.y + (dy as f32 * scale) as i32;
            window.set_position(PhysicalPosition::new(new_x, new_y));
        }
    });
    ui.show()?;
    // 3a) 悬浮窗不显示在 Windows 系统任务栏（WS_EX_TOOLWINDOW 工具窗口样式）；
    //     注意：winit 在 show 后会重算扩展样式覆盖手动设置，所以这里先设一次，
    //     再由下方 1Hz tick 每 tick 守护（ensure_tool_window_for 幂等）。
    crate::window::ensure_tool_window_for(ui.window());

    // 3a2) 右键菜单（Win32 原生 TrackPopupMenu，可超出悬浮窗区域）
    //      menu-requested(x, y) = 窗口内逻辑坐标 → 原生菜单 → 执行动作
    //      「进程详情」→ 按需创建进程列表窗口 + sysinfo 采集一次快照
    let weak_ui_for_menu = ui.as_weak();
    let state_for_menu = state.clone();
    // 进程详情页配置（高亮阈值 + 刷新间隔；由菜单闭包捕获）
    let proc_highlight_pct = config.ui.process_highlight_pct;
    let proc_refresh_ms = config.ui.process_refresh_ms;
    // 进程列表窗口：首次打开时创建，之后复用（每次打开都重新采集刷新）
    let process_win: std::sync::Arc<
        std::sync::Mutex<Option<std::sync::Arc<crate::process_list::ProcessListWindow>>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    let process_win_for_menu = process_win.clone();
    // 卡顿分析窗口：复用进程详情页的「首次创建 + 复用 + refresh」模式（PRD M1 F6）
    let analysis_win: std::sync::Arc<
        std::sync::Mutex<Option<std::sync::Arc<crate::analysis::AnalysisWindow>>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    let analysis_win_for_menu = analysis_win.clone();
    ui.on_menu_requested(move |x: f32, y: f32| {
        let paused = state_for_menu.lock().paused;
        if let Some(ui) = weak_ui_for_menu.upgrade() {
            let cmd = crate::window::show_context_menu(&ui, x, y, paused);
            match cmd {
                Some(crate::window::NativeMenuCmd::TogglePause) => {
                    let mut s = state_for_menu.lock();
                    s.paused = !s.paused;
                    let p = s.paused;
                    drop(s);
                    log::info!("右键菜单暂停监控: {}", if p { "暂停" } else { "恢复" });
                    // 同步悬浮窗右下角按钮文字（slint 端按钮仅本地翻转自己的状态）
                    if let Some(ui2) = weak_ui_for_menu.upgrade() {
                        ui2.set_paused(p);
                    }
                }
                Some(crate::window::NativeMenuCmd::ToggleClickThrough) => {
                    let mut s = state_for_menu.lock();
                    s.click_through = !s.click_through;
                    let enable = s.click_through;
                    drop(s);
                    match crate::window::set_click_through_for(&ui, enable) {
                        Ok(()) => log::info!("点击穿透 {}", if enable { "开" } else { "关" }),
                        Err(e) => log::warn!("点击穿透切换失败: {}", e),
                    }
                }
                Some(crate::window::NativeMenuCmd::ProcessList) => {
                    let mut guard = process_win_for_menu.lock().unwrap();
                    if guard.is_none() {
                        match crate::process_list::ProcessListWindow::show(
                            proc_highlight_pct,
                            proc_refresh_ms,
                        ) {
                            Ok(w) => {
                                log::info!("进程详情窗口已打开");
                                *guard = Some(std::sync::Arc::new(w));
                            }
                            Err(e) => {
                                log::warn!("进程详情窗口打开失败: {}", e);
                                return;
                            }
                        }
                    }
                    if let Some(w) = guard.as_ref() {
                        w.refresh(); // 立即重采样 + 重绘
                    }
                }
                Some(crate::window::NativeMenuCmd::Quit) => {
                    log::info!("右键菜单退出");
                    let _ = slint::quit_event_loop();
                }
                Some(crate::window::NativeMenuCmd::Analysis) => {
                    let mut guard = analysis_win_for_menu.lock().unwrap();
                    if guard.is_none() {
                        match crate::analysis::AnalysisWindow::show() {
                            Ok(w) => {
                                log::info!("卡顿分析窗口已打开");
                                *guard = Some(std::sync::Arc::new(w));
                            }
                            Err(e) => {
                                log::warn!("卡顿分析窗口打开失败: {}", e);
                                return;
                            }
                        }
                    }
                    if let Some(w) = guard.as_ref() {
                        w.refresh(); // 立即查询 + 渲染
                    }
                }
                None => {} // 用户取消（点空白 / Esc）
            }
        }
    });

    // 3a3) 悬浮窗右下角暂停按钮 → 真正切换暂停状态（slint 端已翻转按钮文字，
    //      这里翻转共享状态；1Hz tick 在 paused 时冻结指标显示与卡顿通知）
    let state_for_pause = state.clone();
    ui.on_toggle_pause(move || {
        let mut s = state_for_pause.lock();
        s.paused = !s.paused;
        log::info!("暂停按钮: {}", if s.paused { "暂停" } else { "恢复" });
    });

    // 3b) P1：系统托盘图标（后台线程 + win32 消息循环；失败不阻塞 GUI）
    let tray: Option<std::sync::Arc<crate::tray::Tray>> =
        match crate::tray::Tray::spawn() {
            Ok(t) => {
                log::info!("托盘图标已创建");
                Some(std::sync::Arc::new(t))
            }
            Err(e) => {
                log::warn!("托盘图标启动失败 ({}), 继续运行", e);
                None
            }
        };

    // 3c) P2：任务栏嵌入（伪任务栏窗口，config.ui.taskbar = true 时启用）
    let taskbar: Option<std::sync::Arc<crate::taskbar::TaskbarWindow>> =
        if config.ui.taskbar {
            match crate::taskbar::TaskbarWindow::show() {
                Ok(t) => {
                    log::info!("任务栏嵌入窗口已显示");
                    Some(std::sync::Arc::new(t))
                }
                Err(e) => {
                    log::warn!("任务栏嵌入窗口启动失败 ({}), 继续运行", e);
                    None
                }
            }
        } else {
            None
        };

    // 4) 1Hz 轮询：reader.poll() → 推送到 Slint
    let timer = Timer::default();
    let weak_ui = ui.as_weak();
    let reader_for_tick = reader.clone();
    let state_for_tick = state.clone();
    let tray_for_tick = tray.clone();
    let taskbar_for_tick = taskbar.clone();
    let process_win_for_tick = process_win.clone();
    let analysis_win_for_tick = analysis_win.clone();
    timer.start(
        slint::TimerMode::Repeated,
        POLL_INTERVAL,
        move || {
            // -2) 守护：三个窗口都不显示在 Windows 系统任务栏。
            //     winit 在 show / 状态变化时会重算扩展样式（覆盖 WS_EX_TOOLWINDOW、
            //     加回 WS_EX_APPWINDOW），因此每 tick 复查一次；
            //     ensure_tool_window_for 幂等，样式无变化时零开销（微秒级）。
            if let Some(ui) = weak_ui.upgrade() {
                crate::window::ensure_tool_window_for(ui.window());
                if let Some(tb) = &taskbar_for_tick {
                    crate::window::ensure_tool_window_for(tb.window());
                }
                if let Some(pw) = process_win_for_tick.lock().unwrap().as_ref() {
                    crate::window::ensure_tool_window_for(pw.window());
                }
                if let Some(aw) = analysis_win_for_tick.lock().unwrap().as_ref() {
                    crate::window::ensure_tool_window_for(aw.window());
                }
            }
            // -1) 消费托盘命令（后台线程投递）
            if let Some(tray) = &tray_for_tick {
                while let Some(cmd) = tray.try_recv() {
                    if let Some(ui) = weak_ui.upgrade() {
                        let mut s = state_for_tick.lock();
                        crate::tray::apply_command(&ui, &mut s, cmd, &analysis_win_for_tick);
                    }
                }
            }

            // 0) 消费热加载事件（配置 / 皮肤变更）
            if let Some(ev) = watcher.try_recv() {
                match &ev {
                    hotreload::HotReloadEvent::ConfigChanged(_) => {
                        if let Ok(new_config) =
                            find_stutter_core::Config::load("config.toml")
                        {
                            log::info!("config.toml 热更新: db={}", new_config.storage.db_path);
                            // 皮肤名变了 → 重新加载皮肤
                            if new_config.ui.skin != config.ui.skin {
                                let new_skin = skin::SkinConfig::load(&new_config.ui.skin);
                                state_for_tick.lock().skin = new_skin;
                            }
                            config = new_config;
                        }
                    }
                    hotreload::HotReloadEvent::SkinChanged { skin_name, .. } => {
                        // 只热更新当前启用的皮肤
                        if skin_name == &config.ui.skin {
                            log::info!("skin.toml 热更新: {}", skin_name);
                            let new_skin = skin::SkinConfig::load(skin_name);
                            state_for_tick.lock().skin = new_skin;
                        }
                    }
                }
                watcher.clear_debounce();
            }

            let paused = state_for_tick.lock().paused;
            // overlay 只显示上次卡顿时间，走轻量查询（只读 timestamp 列，
            // 省掉 snapshot/culprits 两个大 JSON 的每 tick 反序列化）。
            let poll: PollResult = reader_for_tick.poll_light();
            // 1) 更新共享状态（暂停时也保持数据新鲜，恢复后立即显示最新值）
            state_for_tick.lock().update_from_poll(&poll);
            // 1b) P2：检测到新的 Major/Critical 事件 → 弹系统通知（暂停时不弹）
            if !paused {
                let mut s = state_for_tick.lock();
                if let Some(ev) = &poll.event {
                    if crate::notify::should_notify(s.last_notified_at, ev, &config.notifications) {
                        s.last_notified_at = Some(ev.timestamp);
                        crate::notify::show_stutter_notification(ev);
                    }
                }
            }
            // 2) 推到 Slint（窗口已关闭时不操作；暂停时冻结指标显示）
            if !paused {
                if let Some(ui) = weak_ui.upgrade() {
                    let s = state_for_tick.lock();
                    overlay::apply_metrics(&ui, &s);
                    // P2：任务栏窗口同步（若启用）
                    if let Some(tb) = &taskbar_for_tick {
                        tb.apply(&s);
                    }
                }
            } else if let Some(ui) = weak_ui.upgrade() {
                // 暂停中：指标冻结，但服务状态行明确显示「已暂停」，
                // 恢复后由 apply_metrics 恢复为真实服务状态
                ui.set_service_status(slint::SharedString::from("⏸ 已暂停"));
                ui.set_service_status_color(slint::Brush::SolidColor(
                    slint::Color::from_rgb_u8(0x8a, 0x8a, 0x92),
                ));
            }
        },
    );

    // 5) 启动 Slint 事件循环（阻塞）
    slint::run_event_loop_until_quit()?;
    Ok(())
}
