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
//! - `Running` → 右列首行绿色 "● 运行中 · HH:MM:SS"
//! - `Stale`   → 黄色 "● 数据延迟"
//! - `Stopped` / `NoDatabase` → 红色 "● 服务已停止"
//! - 暂停按钮在非 Running 时禁用

pub mod analysis;
pub mod analytics;
pub mod auto_start;
pub mod chart;
pub mod elevate;
pub mod hotreload;
pub mod logging;
pub mod notify;
pub mod overlay;
pub mod process_list;
pub mod reader;
pub mod skin;
pub mod taskbar;
pub mod traffic;
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

/// 选择渲染后端：**默认软件光栅化**（`renderer-software`）。
///
/// ## 为什么不用 femtovg / OpenGL 作默认
///
/// 2026-11-03 针对「进程详情页连续拖放卡顿和延迟」实测：本机主屏（3120×2080）
/// 由虚拟显示适配器驱动（`Win32_VideoController` 里同时挂着
/// `Honor Virtual Display Device` / `GameViewer Virtual Display Adapter` /
/// `IddDesk Device`），femtovg 在这种显示路径上提交帧会被反复阻塞——
///
/// | 渲染器 | 每步窗口线程阻塞（120 步 × 3px / 8ms） |
/// |--------|----------------------------------------|
/// | femtovg (GL) | 中位 3ms，**最大 2828ms**，全程带 300–2000ms 长尾 |
/// | software     | 中位 5.3ms，**最大 12.9ms**，两轮完全一致 |
///
/// 也就是说 GL 的帧提交延迟直接变成了「拖一下卡一下」。本程序是纯 2D 小界面
/// （悬浮窗 + 进程列表 + 折线图），软件光栅化的代价完全可接受：拖拽期间单核
/// ~70%（只在重绘时消耗），静止时只在属性变化时重绘。
///
/// 保留覆盖入口：显式设置了 `SLINT_BACKEND`（如 `SLINT_BACKEND=winit-femtovg`）
/// 时不干预，便于在有正经 GPU 的机器上换回 GL。
fn select_renderer() {
    if std::env::var_os("SLINT_BACKEND").is_some() {
        log::info!("检测到 SLINT_BACKEND，跳过渲染器默认选择");
        return;
    }
    match slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
    {
        Ok(()) => log::info!("渲染后端: winit + software（软件光栅化）"),
        // 选不上不致命：`select` 失败时 Slint 会走它自己的默认后端，
        // 功能照常，只是可能重新引入 GL 帧提交抖动。
        Err(e) => log::warn!("软件渲染器选择失败，回退 Slint 默认后端: {}", e),
    }
}

/// 启动 UI overlay（只读模式）。
///
/// 加载 config.toml → 构造 `DbReader` → 起 1Hz 定时器 → 启动 Slint 事件循环。
///
/// 日志：用 `try_init` 容忍重复（bin crate 也可能 init 一次），
///       重复时静默忽略，不影响主流程。
pub fn run() -> anyhow::Result<()> {
    // 日志 stderr + gui.log 双写，并装 panic 钩子：release 下 GUI 会 FreeConsole，
    // 没有落盘日志的话「进程突然消失」将无从取证（panic 也不会进 WER）。
    logging::init();
    logging::install_panic_hook();

    select_renderer();

    let mut config = find_stutter_core::Config::load("config.toml").unwrap_or_else(|e| {
        log::warn!("config load failed ({}), using defaults", e);
        find_stutter_core::Config::default()
    });
    log::info!(
        "find-stutter overlay (P3 read-only) starting, db={}",
        config.storage.db_path
    );

    // 统一工作目录到 config 所在目录（= db 所在目录）。
    // 原因：双击 / explorer / 计划任务启动时 CWD 是 `target\release` 甚至
    // `C:\Windows`，而下面 skins/、热加载监听、CSV 导出等都按相对路径找 ——
    // 若不收口，皮肤会退回默认配色、热加载直接失效（日志里那条
    // `skins dir "skins" does not exist` 就是这么来的）。
    // Config::load 已把 db_path 解析成绝对路径，用它的父目录即可。
    if let Some(dir) = std::path::Path::new(&config.storage.db_path).parent() {
        let same = std::env::current_dir()
            .map(|c| c == dir)
            .unwrap_or(false);
        if !same {
            match std::env::set_current_dir(dir) {
                Ok(()) => log::info!("工作目录已切到 config 所在目录: {}", dir.display()),
                Err(e) => log::warn!("切换工作目录失败 ({}): {}", dir.display(), e),
            }
        }
    }

    // 低内存模式：软件渲染器经 SLINT_BACKEND 环境变量选择，必须发生在任何 Slint
    // 窗口/平台层创建之前。（edition 2024 起 set_var 为 unsafe；此处处于 main 线程
    // 启动早期、尚无并发读者，单次写入安全。）
    if config.ui.software_render {
        // SAFETY：见上——启动早期、无并发环境变量访问。
        unsafe {
            std::env::set_var("SLINT_BACKEND", "winit-software");
        }
        log::info!("软件渲染模式已启用 (SLINT_BACKEND=winit-software)");
    }

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
    // 拖动标记：区分「拖动过再松手」与「原地单击」——只有前者触发贴边吸附。
    // 位移超过 ~3 逻辑像素才算真正拖动，抵消单击时的抖动。
    let dragged = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dragged_for_move = dragged.clone();
    let weak_ui_for_drag = ui.as_weak();
    ui.on_drag_moved(move |dx, dy| {
        use slint::PhysicalPosition;
        if dx.abs() + dy.abs() > 3.0 {
            dragged_for_move.store(true, std::sync::atomic::Ordering::Relaxed);
        }
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

    // 松手贴边：拖动结束（左键松开）且确实拖动过时，把窗口吸附到所在
    // 显示器工作区最近的边缘（阈值内才吸）。
    let snap_enabled = config.ui.snap_enabled;
    let snap_threshold = config.ui.snap_threshold;
    let weak_ui_for_drag_end = ui.as_weak();
    ui.on_drag_ended(move || {
        if !dragged.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return; // 原地单击，不吸附
        }
        if !snap_enabled {
            return;
        }
        if let Some(ui) = weak_ui_for_drag_end.upgrade() {
            if crate::window::snap_to_work_area(&ui.window(), snap_threshold) {
                log::info!("贴边吸附：已吸附到工作区边缘");
            }
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
    // 托盘可用标记：托盘线程在下方 3b 才启动，「隐藏悬浮窗」菜单项是条件显示
    //（托盘是唯一恢复入口，托盘失败时不给该入口，避免窗口藏起来找不回）。
    // 菜单回调与托盘启动共享这个原子标记，启动后置位即可。
    let tray_ok = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tray_ok_for_menu = tray_ok.clone();
    // 进程详情页配置（高亮阈值 + 刷新间隔；由菜单闭包捕获）
    let proc_highlight_pct = config.ui.process_highlight_pct;
    let proc_refresh_ms = config.ui.process_refresh_ms;
    // 进程详情页的「网络」列数据来自 service 采集落库（ETW），需要 db 路径；
    // 三个打开入口各持一份 clone（资源区点击 / 右键菜单 / 测试钩子）
    let proc_db_path_for_res = config.storage.db_path.clone();
    let proc_db_path_for_menu = config.storage.db_path.clone();
    let proc_db_path = config.storage.db_path.clone();
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
    let analysis_win_for_event = analysis_win.clone();
    // 历史流量统计窗口：同款「首次创建 + 复用 + refresh」模式（悬浮窗「今日 ↑x ↓y」入口）
    let traffic_win: std::sync::Arc<
        std::sync::Mutex<Option<std::sync::Arc<crate::traffic::TrafficStatsWindow>>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    let traffic_win_for_event = traffic_win.clone();
    let traffic_db_path = config.storage.db_path.clone();

    // 3a2b) 资源区左键点击：点悬浮窗 CPU / 内存 / 网络文字区 → 新开一个精简列
    //        进程弹窗，按点击维度排序（cpu/mem/net）。每次点击都是独立窗口，
    //        故用 Vec 跟踪：每 tick 守护 toolwindow 样式；关闭时经 on_closed
    //        回调按 id 移除并 drop（停下该窗口的采样线程）。
    let resource_wins: std::sync::Arc<
        std::sync::Mutex<Vec<std::sync::Arc<crate::process_list::ProcessListWindow>>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let resource_closed: std::sync::Arc<std::sync::Mutex<Vec<usize>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let weak_ui_for_res = ui.as_weak();
    let resource_wins_cb = resource_wins.clone();
    let resource_closed_cb = resource_closed.clone();
    ui.on_resource_clicked(move |kind: slint::SharedString| {
        // 维度映射（含「点内存 → 专用工作集 + 全列」的取舍）在
        // `process_list::resource_sort` 里，纯函数、有单测；这里只做窗口编排。
        // 悬浮窗上那个「内存 xx%」是全机物理内存使用率（总-可用），不是各进程
        // 专用工作集求和——两者只在"看谁占得多"这个取向上保持一致。
        let Some(target) = crate::process_list::resource_sort(kind.as_str()) else {
            return; // 未命中资源区（返回空串）
        };
        let sort = target.key.to_string();
        let compact = !target.full_columns;
        // 日志给中文列名 + 内部 key：排障时看 "pmem/专用工作集" 不用二次翻译，
        // 但 key 才是列头箭头比对、sort_groups 分支的真正输入，两者都留。
        let sort_label = match sort.as_str() {
            "cpu" => "CPU",
            "pmem" => "专用工作集",
            "net" => "网络",
            _ => sort.as_str(),
        };
        log::info!(
            "资源区点击: 打开进程弹窗（按 {} 排序[{}]，{}）",
            sort_label,
            sort,
            if compact { "精简列" } else { "全列" }
        );
        // 落点在 ProcessListWindow::show 内部、**显示之前**算好：贴悬浮窗下方
        // （放下不翻上方）并夹进工作区。放到 show 之后再做，会先画出一帧
        // 默认位置，用户看到「先露一截、再跳到完整位置」的闪动。
        let anchor = weak_ui_for_res.upgrade();
        let w = match crate::process_list::ProcessListWindow::show(
            proc_highlight_pct,
            proc_refresh_ms,
            compact,
            true, // 资源区临时弹窗：不占任务栏
            &sort,
            Some(std::path::PathBuf::from(proc_db_path_for_res.clone())),
            anchor.as_ref().map(|u| u.window()),
        ) {
            Ok(w) => w,
            Err(e) => {
                log::warn!("资源区进程弹窗打开失败: {}", e);
                return;
            }
        };
        let w = std::sync::Arc::new(w);
        let wid = w.id();
        let closed = resource_closed_cb.clone();
        w.set_on_closed(move || {
            closed.lock().unwrap().push(wid);
        });
        resource_wins_cb.lock().unwrap().push(w);
    });

    // 3a2c) 点悬浮窗「今日卡顿 N」→ 直接打开卡顿分析窗口（默认今日范围），
    // 与右键菜单复用同一个窗口句柄：已打开时只刷新并前置，避免重复建窗。
    ui.on_event_count_clicked(move || {
        log::info!("今日卡顿点击: 打开卡顿分析");
        let mut guard = analysis_win_for_event.lock().unwrap();
        if guard.is_none() {
            match crate::analysis::AnalysisWindow::show() {
                Ok(w) => *guard = Some(std::sync::Arc::new(w)),
                Err(e) => {
                    log::warn!("卡顿分析窗口打开失败: {}", e);
                    return;
                }
            }
        }
        if let Some(w) = guard.as_ref() {
            w.refresh();
        }
    });

    // 3a2d) 点悬浮窗「今日 ↑x ↓y」→ 打开历史流量统计窗口（与「今日卡顿」同构：
    // 首次创窗，之后只 refresh + 前置，不重复建窗）。查库全在窗口内部的后台线程，
    // 这里只做窗口编排。
    ui.on_traffic_clicked(move || {
        log::info!("今日流量点击: 打开历史流量统计");
        let mut guard = traffic_win_for_event.lock().unwrap();
        if guard.is_none() {
            match crate::traffic::TrafficStatsWindow::show(std::path::PathBuf::from(
                traffic_db_path.clone(),
            )) {
                Ok(w) => *guard = Some(std::sync::Arc::new(w)),
                Err(e) => {
                    log::warn!("历史流量统计窗口打开失败: {}", e);
                    return;
                }
            }
        }
        if let Some(w) = guard.as_ref() {
            w.refresh();
        }
    });

    ui.on_menu_requested(move |x: f32, y: f32| {
        let paused = state_for_menu.lock().paused;
        if let Some(ui) = weak_ui_for_menu.upgrade() {
            let allow_hide = tray_ok_for_menu.load(std::sync::atomic::Ordering::Relaxed);
            let cmd = crate::window::show_context_menu(&ui, x, y, paused, allow_hide);
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
                            false,
                            false, // 主进程详情页：普通应用窗口
                            "cpu",
                            Some(std::path::PathBuf::from(proc_db_path_for_menu.clone())),
                            Some(&ui.window()), // 贴悬浮窗打开（首个窗口才定位）
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
                Some(crate::window::NativeMenuCmd::HideOverlay) => {
                    // 隐藏悬浮窗：恢复入口是托盘（左键单击 / 托盘菜单）。
                    // 该菜单项仅在托盘可用时出现（见 show_context_menu 的 allow_hide）。
                    log::info!("右键菜单隐藏悬浮窗（托盘可恢复）");
                    if let Err(e) = ui.hide() {
                        log::warn!("隐藏悬浮窗失败: {}", e);
                    }
                }
                None => {} // 用户取消（点空白 / Esc）
            }
        }
    });

    // 3a3) 暂停入口已收敛到右键菜单 / 托盘；悬浮窗本体不再有可点击暂停区，
    //      避免右列整行热区误触。

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
    // 托盘就绪 → 允许右键菜单出现「隐藏悬浮窗」（否则窗口藏起来无法恢复）
    if tray.is_some() {
        tray_ok.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // 3b2) 悬浮窗关闭请求（Alt+F4 / 外部 WM_CLOSE）：
    //      托盘可用时改成「隐藏」（与右键菜单「隐藏悬浮窗」一致，常驻监控不该被
    //      误按 Alt+F4 杀掉）；托盘不可用时按退出处理。
    //      **必须记日志**：这条路径此前无声退出，13:44 那次「窗口突然消失」就查不出原因。
    let tray_ok_for_close = tray_ok.clone();
    // 注：Overlay 的 .slint 没有声明 close-requested 回调，这里用窗口级 API。
    ui.window().on_close_requested(move || {
        if tray_ok_for_close.load(std::sync::atomic::Ordering::Relaxed) {
            log::info!("悬浮窗收到关闭请求 → 隐藏（托盘可恢复），进程继续运行");
            slint::CloseRequestResponse::HideWindow
        } else {
            log::info!("悬浮窗收到关闭请求 → 托盘不可用，按退出处理");
            let _ = slint::quit_event_loop();
            slint::CloseRequestResponse::KeepWindowShown
        }
    });

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

    // 4) 1Hz 轮询：**DB 读取放后台线程**，UI 线程只负责取最新结果并推给 Slint。
    //
    // 为什么必须分线程（2026-11-03 实测）：`poll_light()` 每 10 次 tick 会跑一次
    // 「今日 + 本周累计网络流量」`SUM(net_*_bps) FROM samples WHERE timestamp
    // BETWEEN …`，在本机 28 万行的 samples 上是 180–230ms 的索引范围扫描。这条
    // 查询跑在 UI 线程上时，用户拖窗口边缘只要撞上它，整个原生尺寸循环就停摆
    // 200ms —— 表现为「连续拖放卡顿和延迟」（探针 `FIND_STUTTER_PROF=1` 的
    // `[PROF] 1Hz tick 2xx ms（poll 2xx ms / …）` 就是它）。
    //
    // 查询成本还会随 samples 保留量增长（30 天 retention 可达数百万行），
    // 所以这里不是「把间隔调大」而是彻底移出 UI 线程：无论查询多慢，UI 线程
    // 只是 `try_recv` 一次（纳秒级），慢了最多晚一拍刷新，绝不阻塞窗口。
    let (poll_tx, poll_rx) = std::sync::mpsc::channel::<PollResult>();
    {
        let reader_for_thread = reader.clone();
        let prof_thread = std::env::var("FIND_STUTTER_PROF").as_deref() == Ok("1");
        let spawned = std::thread::Builder::new()
            .name("db-poller".into())
            .spawn(move || loop {
                let t0 = std::time::Instant::now();
                let poll = reader_for_thread.poll_light();
                if prof_thread {
                    let ms = t0.elapsed().as_millis();
                    if ms >= 20 {
                        log::info!("[PROF] 后台 poll_light {}ms", ms);
                    }
                }
                // 发送失败 = UI 已退出（接收端 drop）→ 结束线程
                if poll_tx.send(poll).is_err() {
                    break;
                }
                let spent = t0.elapsed();
                if spent < POLL_INTERVAL {
                    std::thread::sleep(POLL_INTERVAL - spent);
                }
            });
        // 线程创建失败绝不能静默：UI 只 try_recv，收不到数据时界面会一直
        // 显示空数据且日志里毫无痕迹，排障无从下手。
        if let Err(e) = spawned {
            log::error!("db-poller 线程创建失败: {}（界面将收不到任何 DB 数据）", e);
        }
    }
    let timer = Timer::default();
    let weak_ui = ui.as_weak();
    let state_for_tick = state.clone();
    let tray_for_tick = tray.clone();
    let taskbar_for_tick = taskbar.clone();
    let process_win_for_tick = process_win.clone();
    let analysis_win_for_tick = analysis_win.clone();
    let traffic_win_for_tick = traffic_win.clone();
    let resource_wins_for_tick = resource_wins.clone();
    let resource_closed_for_tick = resource_closed.clone();

    // 测试钩子：FIND_STUTTER_TEST_OPEN_PROCESS=1 时启动后打开进程详情窗口，
    // 供自动化回归直连（绕开右键菜单的模拟点击不可靠问题）。
    //
    // **延迟 1s 再开**：Slint 的 show() 是异步建窗，紧随其后的几百毫秒里悬浮窗
    // 的 HWND 还取不到（`window_handle()` 返回 Err），落点算不出来；真实使用中
    // 这段空窗由用户操作耗时覆盖，测试必须模拟同样的时序，否则量到的是异常路径。
    if std::env::var("FIND_STUTTER_TEST_OPEN_PROCESS").as_deref() == Ok("1") {
        log::info!("[TEST] 环境变量命中：1s 后打开进程详情窗口");
        let weak_for_test = ui.as_weak();
        let process_win_test = process_win.clone();
        slint::Timer::single_shot(Duration::from_millis(1000), move || {
            let Some(ui) = weak_for_test.upgrade() else {
                return;
            };
            let mut guard = process_win_test.lock().unwrap();
            if guard.is_none() {
                match crate::process_list::ProcessListWindow::show(
                    proc_highlight_pct,
                    proc_refresh_ms,
                    false,
                    false, // 主进程详情页：普通应用窗口
                    "cpu",
                    Some(std::path::PathBuf::from(proc_db_path.clone())),
                    Some(&ui.window()),
                ) {
                    Ok(w) => {
                        log::info!("[TEST] 进程详情窗口已打开");
                        *guard = Some(std::sync::Arc::new(w));
                    }
                    Err(e) => log::warn!("[TEST] 进程详情窗口打开失败: {}", e),
                }
            }
        });
    }
    // 「连续拖放卡顿」排查探针：`FIND_STUTTER_PROF=1` 时每个 1Hz tick 打印
    // 各段耗时（DB 轮询 / 状态更新 / 推 Slint）。默认关闭且一次也不写日志。
    //
    // 为什么需要：边缘拖拽走 Windows 原生模态尺寸循环，UI 线程在这期间仍要
    // 派发这个 tick——tick 里任何一段阻塞都会直接变成「拖拽卡一拍」，而
    // 单看总耗时无法区分是 sqlite 读、状态合并还是 Slint 属性写入。
    let prof_tick = std::env::var("FIND_STUTTER_PROF").as_deref() == Ok("1");
    timer.start(
        slint::TimerMode::Repeated,
        POLL_INTERVAL,
        move || {
            // 用户正拖着某个窗口的边框 / 标题栏：这一拍整体让路。
            //
            // 拖拽期间窗口尺寸由 Windows 原生模态循环驱动，UI 线程上任何额外
            // 工作（写 Slint 属性 → 触发另一次重绘、守护窗口样式、消费托盘命令）
            // 都是在和用户的鼠标抢同一帧。数据晚一拍推送完全不可感知（拖拽本身
            // 也就一两秒），拖拽结束后第一个 tick 立刻恢复正常。
            if crate::window::size_move_active() {
                return;
            }
            let prof_t0 = std::time::Instant::now();
            let prof_poll;
            let prof_state;
            let prof_push;
            // -2) 守护窗口样式（winit 在 show / 状态变化时会重算扩展样式）：
            //     悬浮窗 + 伪任务栏 + 资源区临时弹窗 = 工具窗口（不进系统任务栏、不进 Alt-Tab）；
            //     进程详情页 + 卡顿分析页 = 普通应用窗口（要留在任务栏，作为
            //     被其它 APP 压下去后的切回入口）。两者都幂等，样式无变化时
            //     零开销（微秒级）。
            if let Some(ui) = weak_ui.upgrade() {
                crate::window::ensure_tool_window_for(ui.window());
                if let Some(tb) = &taskbar_for_tick {
                    crate::window::ensure_tool_window_for(tb.window());
                }
                if let Some(pw) = process_win_for_tick.lock().unwrap().as_ref() {
                    crate::window::ensure_app_window_for(pw.window());
                }
                if let Some(aw) = analysis_win_for_tick.lock().unwrap().as_ref() {
                    crate::window::ensure_app_window_for(aw.window());
                }
                // 历史流量统计窗口同为普通应用窗口（留在任务栏 / Alt-Tab）
                if let Some(tw) = traffic_win_for_tick.lock().unwrap().as_ref() {
                    crate::window::ensure_app_window_for(tw.window());
                }
                // 资源区弹窗：先移除已关闭的（drop 停采样线程），再守护剩余
                {
                    let mut wins = resource_wins_for_tick.lock().unwrap();
                    let closed: Vec<usize> = resource_closed_for_tick
                        .lock()
                        .unwrap()
                        .drain(..)
                        .collect();
                    if !closed.is_empty() {
                        wins.retain(|w| !closed.contains(&w.id()));
                    }
                    // 资源区弹窗是临时窗口（工具窗口样式）：关闭后移除跟踪即可
                    for w in wins.iter() {
                        crate::window::ensure_tool_window_for(w.window());
                    }
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
            // 后台线程（db-poller）把结果投进 channel：这里只取最新一条——UI 线程
            // 被长时间占用后 channel 里可能积压多条，逐条 apply 毫无意义（只有
            // 最后一帧会显示），直接跳到最新。
            let prof_a = std::time::Instant::now();
            let mut latest: Option<PollResult> = None;
            while let Ok(p) = poll_rx.try_recv() {
                latest = Some(p);
            }
            prof_poll = prof_a.elapsed().as_millis();
            // 1) 更新共享状态（暂停时也保持数据新鲜，恢复后立即显示最新值）
            let prof_b = std::time::Instant::now();
            if let Some(poll) = &latest {
                state_for_tick.lock().update_from_poll(poll);
                // 1b) P2：检测到新的 Major/Critical 事件 → 弹系统通知（暂停时不弹）
                if !paused {
                    let mut s = state_for_tick.lock();
                    if let Some(ev) = &poll.event {
                        if crate::notify::should_notify(
                            s.last_notified_at,
                            ev,
                            &config.notifications,
                        ) {
                            s.last_notified_at = Some(ev.timestamp);
                            crate::notify::show_stutter_notification(ev);
                        }
                    }
                }
            }
            prof_state = prof_b.elapsed().as_millis();
            // 2) 推到 Slint（窗口已关闭时不操作；暂停时冻结指标显示）
            let prof_c = std::time::Instant::now();
            if !paused {
                if let Some(ui) = weak_ui.upgrade() {
                    let mut s = state_for_tick.lock();
                    overlay::apply_metrics(&ui, &mut s);
                    // P2：任务栏窗口同步（若启用）
                    if let Some(tb) = &taskbar_for_tick {
                        tb.apply(&s);
                    }
                }
            } else { match weak_ui.upgrade() { Some(ui) => {
                // 暂停中：指标冻结，但服务状态行明确显示「已暂停」，
                // 恢复后由 apply_metrics 恢复为真实服务状态。
                //
                // `set_paused(true)` 必须在这里补：`apply_metrics` 只在 `!paused`
                // 分支调用，而按钮文案、第二行提示都由 slint 的 `paused` 驱动。
                // 漏掉的后果（P1-1）：从托盘暂停后首行写「已暂停」，按钮仍写
                // 「暂停」、第二行仍显示心跳——同一窗口三个互相矛盾的信号，
                // 且此时点「暂停」实际是恢复，用户被反向操作。
                ui.set_paused(true);
                ui.set_service_status(slint::SharedString::from("已暂停"));
                ui.set_service_status_color(slint::Brush::SolidColor(
                    slint::Color::from_rgb_u8(0x8a, 0x8a, 0x92),
                ));
            } _ => {}}}
            prof_push = prof_c.elapsed().as_millis();
            if prof_tick {
                let total = prof_t0.elapsed().as_millis();
                if total >= 5 {
                    log::info!(
                        "[PROF] 1Hz tick {}ms（poll {}ms / state {}ms / push {}ms）",
                        total,
                        prof_poll,
                        prof_state,
                        prof_push
                    );
                }
            }
        },
    );

    // 5) 启动 Slint 事件循环（阻塞）
    slint::run_event_loop_until_quit()?;
    // 走到这里 = 事件循环正常结束（quit_event_loop 或最后一个窗口关闭）。
    // 这一行是「正常退出」与「进程被杀 / 崩溃」的分界线：没有它就没有收尾机会。
    log::info!("事件循环已退出，GUI 进程正常结束");
    Ok(())
}
