//! 卡顿分析窗口（PRD M1 / M2）。
//!
//! 复用 `ProcessListWindow` 的模式：独立置顶 slint 窗口 + 首次创建复用
//!（`Arc<Mutex<Option<Arc<AnalysisWindow>>>>`，见 `lib.rs`）。
//!
//! 与进程详情页的区别：本窗口是**只读分析**（打开/刷新时查询一次 stutter.db），
//! 不常驻采样线程、不 1Hz 轮询。窗口关闭（`ui.hide()`）不影响悬浮窗常驻监控。
//!
//! ## 渲染职责拆分
//!
//! - **KPI 卡片**：`refresh()` 同步查 `analytics::load_kpi_today` → 写文本属性。
//! - **趋势图**：`refresh()` 在后台线程用 plotters 渲染位图（避免 UI 冻结，
//!   见 PRD §6.3），完成后 `invoke_from_event_loop` 把 `slint::Image` 推回 UI 线程。

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::{Brush, Color, ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};

use crate::analytics::{
    self, parse_event_sort_column, EventRow, EventSnapshot, EventSort, ResourceView, TimeRange,
    TrendBucket,
};

/// 卡顿分析窗口句柄。
pub struct AnalysisWindow {
    ui: crate::Analysis,
    /// 数据库路径（来自 config.toml，缺失时回退默认 "stutter.db"）
    db_path: PathBuf,
    /// 当前时间范围索引（下拉选择；用于同步 ComboBox 显示）
    range_index: Arc<Mutex<i32>>,
    /// 当前模式（基础=false / 高级=true）
    advanced: Arc<Mutex<bool>>,
    /// 自定义范围起止（本地时区文本）；选中「自定义」并由用户「应用范围」后写入
    custom_range: Arc<Mutex<Option<(String, String)>>>,
    /// 进程钻取筛选名（空=不过滤）；点击元凶榜某进程后写入。
    /// 仅由 show() 的回调闭包持有（捕获其克隆），结构体字段保留引用以维持存活，
    /// 自身不被直接读取，故允许 dead_code。
    #[allow(dead_code)]
    drill_filter: Arc<Mutex<Option<String>>>,
    /// 当前生效的时间范围（供钻取/清除筛选时增量刷新事件表）。
    /// 同上，仅由回调闭包持有其克隆，结构体字段保留引用。
    #[allow(dead_code)]
    current_range: Arc<Mutex<TimeRange>>,
    /// F1：当前趋势分桶粒度（高级模式可选，默认小时）
    bucket: Arc<Mutex<TrendBucket>>,
    /// F7：自动刷新定时器（高级模式；间隔变更时重启/停止）。用 `Arc<Mutex<>>` 包装以便
    /// 在多个 'static 回调闭包间共享同一实例；单次创建、长期持有，回调捕获 Weak 句柄
    /// 避免泄漏；切到「关闭」或窗口关闭时 stop()。
    /// 结构体字段保留其一处强引用以维持存活（闭包亦各自持克隆），自身不被直接读取，
    /// 故允许 dead_code。
    #[allow(dead_code)]
    auto_refresh_timer: Arc<Mutex<Timer>>,
    /// F7：当前自动刷新间隔（秒）；None = 关闭，默认 30。基础模式切回时停刷、进入高级时
    /// 按此值启动。结构体字段保留强引用供闭包克隆，自身不被直接读取，故允许 dead_code。
    #[allow(dead_code)]
    auto_interval: Arc<Mutex<Option<u64>>>,
    /// F3（高级 hover）：与资源图卡顿竖线对齐的卡顿事件列表（按 timestamp 升序，与
    /// `hover_buckets` 同序 zip）。由 refresh 的资源线程填充，供 on_resource_hover 定位。
    #[allow(dead_code)]
    hover_events: Arc<Mutex<Vec<EventRow>>>,
    /// F3（高级 hover）：每个 `hover_events` 对应事件的桶序号（clamp 到 [0,bucket_count-1]）。
    #[allow(dead_code)]
    hover_buckets: Arc<Mutex<Vec<i64>>>,
    /// F3（高级 hover）：与 `hover_events` 同序的每个事件快照（事件瞬间资源全字段）。
    /// 由资源后台线程按 `hover_events` 同序计算并写入，供 `on_resource_hover` 末尾追加快照行。
    #[allow(dead_code)]
    hover_snaps: Arc<Mutex<Vec<Option<EventSnapshot>>>>,
    /// F3（高级 snapshot）：当前事件表显示的事件（表序、含钻取筛选与排序），供 on_row_clicked
    /// 按序号取该次卡顿的 ts_secs 加载 snapshot。
    #[allow(dead_code)]
    table_events: Arc<Mutex<Vec<EventRow>>>,
    /// F3（高级可选指标 + 对数轴）：由 5 个 CheckBox 与「对数轴」CheckBox 驱动。
    #[allow(dead_code)]
    resource_view: Arc<Mutex<ResourceView>>,
}

impl AnalysisWindow {
    /// 创建并显示卡顿分析窗口（首次调用；之后复用 `refresh()`）。
    pub fn show() -> anyhow::Result<Self> {
        let config = find_stutter_core::Config::load("config.toml").unwrap_or_else(|e| {
            log::warn!("analysis: config load failed ({}), using defaults", e);
            find_stutter_core::Config::default()
        });
        let db_path = PathBuf::from(config.storage.db_path);

        let ui = crate::Analysis::new()?;
        let range_index: Arc<Mutex<i32>> = Arc::new(Mutex::new(0));
        let advanced: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let custom_range: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
        let drill_filter: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let current_range: Arc<Mutex<TimeRange>> = Arc::new(Mutex::new(TimeRange::Today));
        let bucket: Arc<Mutex<TrendBucket>> = Arc::new(Mutex::new(TrendBucket::Hour));
        let auto_refresh_timer: Arc<Mutex<Timer>> = Arc::new(Mutex::new(Timer::default()));
        // F7：自动刷新间隔（None=关闭，默认 30 秒）
        let auto_interval: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(Some(30)));
        // F3：资源图 hover / 事件行 snapshot 所需的共享状态（高级模式）
        let hover_events: Arc<Mutex<Vec<EventRow>>> = Arc::new(Mutex::new(Vec::new()));
        let hover_buckets: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
        let hover_snaps: Arc<Mutex<Vec<Option<EventSnapshot>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let table_events: Arc<Mutex<Vec<EventRow>>> = Arc::new(Mutex::new(Vec::new()));
        let resource_view: Arc<Mutex<ResourceView>> = Arc::new(Mutex::new(ResourceView::default()));

        // 皮肤注入：让 Analysis 主框架跟随 skin.toml（与 Overlay/ProcessList 一致）
        apply_skin(&ui);

        // 拖动：标题栏 TouchArea 传 dx/dy → window.set_position()
        let weak_drag = ui.as_weak();
        ui.on_drag_moved(move |dx, dy| {
            use slint::PhysicalPosition;
            if let Some(ui) = weak_drag.upgrade() {
                let window = ui.window();
                let scale = window.scale_factor();
                let pos = window.position();
                window.set_position(PhysicalPosition::new(
                    pos.x + (dx as f32 * scale) as i32,
                    pos.y + (dy as f32 * scale) as i32,
                ));
            }
        });

        // 关闭按钮 → 隐藏窗口（保留实例，下次打开复用）
        let weak_close = ui.as_weak();
        // F7：窗口关闭时停掉自动刷新定时器，避免隐藏期间仍后台轮询
        let timer_for_close = auto_refresh_timer.clone();
        ui.on_close_requested(move || {
            timer_for_close.lock().unwrap().stop();
            if let Some(ui) = weak_close.upgrade() {
                let _ = ui.hide();
            }
        });

        // 基础/高级 切换：记录模式（slint 端已翻转 advanced-mode 属性）
        let advanced_for_cb = advanced.clone();
        let drill_for_toggle = drill_filter.clone();
        let custom_for_toggle = custom_range.clone();
        let range_for_toggle = current_range.clone();
        let bucket_for_toggle = bucket.clone();
        let db_for_toggle = db_path.clone();
        let timer_for_toggle = auto_refresh_timer.clone();
        let auto_interval_for_toggle = auto_interval.clone();
        let he_for_toggle = hover_events.clone();
        let hb_for_toggle = hover_buckets.clone();
        let hs_for_toggle = hover_snaps.clone();
        let te_for_toggle = table_events.clone();
        let rv_for_toggle = resource_view.clone();
        let weak_toggle = ui.as_weak();
        ui.on_toggle_mode_changed(move |on| {
            *advanced_for_cb.lock().unwrap() = on;
            log::info!("卡顿分析：{} 模式", if on { "高级" } else { "基础" });
            // 切回基础模式时清除钻取筛选，并停掉自动刷新（PRD §4 F7：自动刷新仅高级模式）
            if !on {
                *drill_for_toggle.lock().unwrap() = None;
                timer_for_toggle.lock().unwrap().stop();
                log::info!("卡顿分析：基础模式，自动刷新已停");
            } else if let Some(secs) = *auto_interval_for_toggle.lock().unwrap() {
                // 进入高级模式：按当前自动刷新间隔启动定时器（None 已在上面停刷）
                start_auto_refresh(
                    &weak_toggle,
                    &db_for_toggle,
                    &range_for_toggle,
                    &bucket_for_toggle,
                    &advanced_for_cb,
                    &timer_for_toggle,
                    &he_for_toggle,
                    &hb_for_toggle,
                    &hs_for_toggle,
                    &te_for_toggle,
                    &rv_for_toggle,
                    secs,
                );
            }
            if let Some(ui) = weak_toggle.upgrade() {
                let range = range_for_toggle.lock().unwrap().clone();
                // 自定义范围在有值时使用，否则回退默认
                let range = resolve_range(range, &custom_for_toggle.lock().unwrap());
                let b = *bucket_for_toggle.lock().unwrap();
                refresh_window(
                    &ui,
                    &db_for_toggle,
                    range,
                    on,
                    b,
                    &he_for_toggle,
                    &hb_for_toggle,
                    &hs_for_toggle,
                    &te_for_toggle,
                    &rv_for_toggle,
                );
            }
        });

        // 时间范围下拉：字符串标签 → 索引，记录后重查
        // （slint ComboBox `selected` 回调给的是选中文本而非索引）
        let range_for_cb = range_index.clone();
        let custom_for_cb = custom_range.clone();
        let current_for_cb = current_range.clone();
        let bucket_for_range_cb = bucket.clone();
        let weak_range = ui.as_weak();
        let db_for_range = db_path.clone();
        let advanced_for_range = advanced.clone();
        let he_for_range = hover_events.clone();
        let hb_for_range = hover_buckets.clone();
        let hs_for_range = hover_snaps.clone();
        let te_for_range = table_events.clone();
        let rv_for_range = resource_view.clone();
        ui.on_range_changed(move |value: SharedString| {
            let idx = match value.as_str() {
                "近7天" => 1,
                "近30天" => 2,
                "自定义" => 3,
                _ => 0, // 今日
            };
            *range_for_cb.lock().unwrap() = idx;
            log::info!("卡顿分析：时间范围 = {} ({})", idx, value);
            // 选中非「自定义」时清空自定义区间，使其回退默认范围
            if idx != 3 {
                *custom_for_cb.lock().unwrap() = None;
            }
            if let Some(ui) = weak_range.upgrade() {
                let base = TimeRange::from_index(idx);
                let range = resolve_range(base, &custom_for_cb.lock().unwrap());
                *current_for_cb.lock().unwrap() = range.clone();
                let adv = advanced_for_range.lock().unwrap().clone();
                let b = *bucket_for_range_cb.lock().unwrap();
                refresh_window(
                    &ui,
                    &db_for_range,
                    range,
                    adv,
                    b,
                    &he_for_range,
                    &hb_for_range,
                    &hs_for_range,
                    &te_for_range,
                    &rv_for_range,
                );
            }
        });

        // F1：分桶粒度变更（高级模式）→ 记录粒度并重渲趋势图
        let bucket_for_cb = bucket.clone();
        let current_for_bucket = current_range.clone();
        let custom_for_bucket = custom_range.clone();
        let db_for_bucket = db_path.clone();
        let advanced_for_bucket = advanced.clone();
        let he_for_bucket = hover_events.clone();
        let hb_for_bucket = hover_buckets.clone();
        let hs_for_bucket = hover_snaps.clone();
        let te_for_bucket = table_events.clone();
        let rv_for_bucket = resource_view.clone();
        let weak_bucket = ui.as_weak();
        ui.on_bucket_changed(move |value: SharedString| {
            let b = TrendBucket::from(value.as_str());
            *bucket_for_cb.lock().unwrap() = b;
            log::info!("卡顿分析：分桶粒度 = {:?}", b);
            if let Some(ui) = weak_bucket.upgrade() {
                let range = resolve_range(
                    current_for_bucket.lock().unwrap().clone(),
                    &custom_for_bucket.lock().unwrap(),
                );
                refresh_window(
                    &ui,
                    &db_for_bucket,
                    range,
                    *advanced_for_bucket.lock().unwrap(),
                    b,
                    &he_for_bucket,
                    &hb_for_bucket,
                    &hs_for_bucket,
                    &te_for_bucket,
                    &rv_for_bucket,
                );
            }
        });

        // F7：自动刷新间隔变更（高级模式）→ 更新 auto_interval 并重启/停止定时器
        let timer_for_auto = auto_refresh_timer.clone();
        let auto_interval_for_auto = auto_interval.clone();
        let weak_auto = ui.as_weak();
        let db_for_auto = db_path.clone();
        let range_for_auto = current_range.clone();
        let bucket_for_auto = bucket.clone();
        let advanced_for_auto = advanced.clone();
        let he_for_auto = hover_events.clone();
        let hb_for_auto = hover_buckets.clone();
        let hs_for_auto = hover_snaps.clone();
        let te_for_auto = table_events.clone();
        let rv_for_auto = resource_view.clone();
        ui.on_auto_refresh_changed(move |value: SharedString| {
            let interval: Option<u64> = match value.as_str() {
                "60 秒" => Some(60),
                "5 分钟" => Some(300),
                "30 秒" => Some(30),
                _ => None, // 关闭
            };
            *auto_interval_for_auto.lock().unwrap() = interval;
            match interval {
                None => {
                    timer_for_auto.lock().unwrap().stop();
                    log::info!("卡顿分析：自动刷新已关闭");
                }
                Some(secs) => {
                    // 复用 start_auto_refresh 构造并启动 Repeating 定时器（与 on_toggle_mode_changed
                    // 共用同一逻辑，消除重复的 Timer 启动闭包，B1）。
                    start_auto_refresh(
                        &weak_auto,
                        &db_for_auto,
                        &range_for_auto,
                        &bucket_for_auto,
                        &advanced_for_auto,
                        &timer_for_auto,
                        &he_for_auto,
                        &hb_for_auto,
                        &hs_for_auto,
                        &te_for_auto,
                        &rv_for_auto,
                        secs,
                    );
                    log::info!("卡顿分析：自动刷新 = {} 秒", secs);
                }
            }
        });

        // F3（高级）：资源图 hover 看最近卡顿事件（按 X 比例定位 event_buckets 中最近事件）
        let weak_hover = ui.as_weak();
        let he_for_hover = hover_events.clone();
        let hb_for_hover = hover_buckets.clone();
        let hs_for_hover = hover_snaps.clone();
        ui.on_resource_hover(move |ratio: f32| {
            let events = he_for_hover.lock().unwrap().clone();
            let buckets = hb_for_hover.lock().unwrap().clone();
            if events.is_empty() {
                if let Some(ui) = weak_hover.upgrade() {
                    ui.set_resource_hover_info(SharedString::from("无卡顿事件"));
                }
                return;
            }
            // 桶数 = 最大桶序号 +1（桶在 [0, bucket_count-1]）
            let bucket_count = buckets.iter().cloned().max().unwrap_or(0) + 1;
            let target = (ratio * (bucket_count as f32 - 1.0))
                .round()
                .clamp(0.0, (bucket_count - 1) as f32) as i64;
            // 找 event_buckets 中与 target 最近的事件（同序 zip）
            let mut best = 0usize;
            let mut best_dist = i64::MAX;
            for (i, &b) in buckets.iter().enumerate() {
                let d = (b - target).abs();
                if d < best_dist {
                    best_dist = d;
                    best = i;
                }
            }
            let e = &events[best];
            let mut info = format!(
                "{} | {} | 原因：{} | 元凶：{}",
                e.time_local, e.severity_cn, e.causes_text, e.culprits_text
            );
            // 追加该次 snapshot 资源全字段（与 hover_events 同序，PRD §4 F3）
            let snaps = hs_for_hover.lock().unwrap().clone();
            if let Some(Some(snap)) = snaps.get(best) {
                info.push_str(&format!(
                    " | 采样 CPU {:.1}% 内存 {:.1}% 磁盘读 {} 写 {}",
                    snap.cpu,
                    snap.mem,
                    fmt_bytes(snap.disk_read),
                    fmt_bytes(snap.disk_write)
                ));
                if let Some(g) = snap.gpu {
                    info.push_str(&format!(" GPU {:.1}%", g));
                }
            }
            if let Some(ui) = weak_hover.upgrade() {
                ui.set_resource_hover_info(SharedString::from(info));
            }
        });

        // F3（高级）：点击原始事件表某行 → 加载该次卡顿的 snapshot 资源详情
        let weak_row = ui.as_weak();
        let te_for_row = table_events.clone();
        let db_for_row = db_path.clone();
        ui.on_row_clicked(move |idx: i32| {
            let events = te_for_row.lock().unwrap().clone();
            if idx < 0 || idx as usize >= events.len() {
                return;
            }
            let e = &events[idx as usize];
            // 组装基础明细（时间/等级/时长/原因/元凶），有 snapshot 时追加采样行
            let mut detail = format!(
                "时间 {}\n严重 {} | 持续 {}ms\n原因 {}\n元凶 {}",
                e.time_local, e.severity_cn, e.duration_ms, e.causes_text, e.culprits_text
            );
            if let Ok(conn) = analytics::open_readonly(&db_for_row) {
                if let Some(snap) = analytics::load_event_snapshot(&conn, e.ts_secs) {
                    detail.push_str(&format!(
                        "\n采样 CPU {:.1}% / 内存 {:.1}% / 磁盘读 {} / 写 {}",
                        snap.cpu,
                        snap.mem,
                        fmt_bytes(snap.disk_read),
                        fmt_bytes(snap.disk_write)
                    ));
                    if let Some(g) = snap.gpu {
                        detail.push_str(&format!(" / GPU {:.1}%", g));
                    }
                }
            }
            if let Some(ui) = weak_row.upgrade() {
                ui.set_event_detail(SharedString::from(detail));
            }
        });

        // F3（高级）：切换某资源指标显隐 → 只重跑资源刷新（不重查其它）
        let weak_metric = ui.as_weak();
        let rv_for_metric = resource_view.clone();
        let db_for_metric = db_path.clone();
        let range_for_metric = current_range.clone();
        ui.on_metric_toggled(move |name: SharedString, on: bool| {
            {
                let mut v = rv_for_metric.lock().unwrap();
                match name.as_str() {
                    "cpu" => v.cpu = on,
                    "mem" => v.mem = on,
                    "disk_read" => v.disk_read = on,
                    "disk_write" => v.disk_write = on,
                    "gpu" => v.gpu = on,
                    _ => {}
                }
            }
            if let Some(ui) = weak_metric.upgrade() {
                let range = range_for_metric.lock().unwrap().clone();
                let v = *rv_for_metric.lock().unwrap();
                refresh_resource(&ui, &db_for_metric, range, v);
            }
        });

        // F3（高级）：切换磁盘对数轴 → 只重跑资源刷新
        let weak_log = ui.as_weak();
        let rv_for_log = resource_view.clone();
        let db_for_log = db_path.clone();
        let range_for_log = current_range.clone();
        ui.on_log_axis_changed(move |on: bool| {
            rv_for_log.lock().unwrap().log_disk = on;
            if let Some(ui) = weak_log.upgrade() {
                let range = range_for_log.lock().unwrap().clone();
                let v = *rv_for_log.lock().unwrap();
                refresh_resource(&ui, &db_for_log, range, v);
            }
        });

        // F8：导出 CSV（高级模式按钮）→ 写用户可写目录（桌面/CWD），不碰 stutter.db
        let db_for_export = db_path.clone();
        let current_for_export = current_range.clone();
        let custom_for_export = custom_range.clone();
        let range_index_for_export = range_index.clone();
        let weak_export = ui.as_weak();
        ui.on_export_csv_requested(move || {
            let base = TimeRange::from_index(*range_index_for_export.lock().unwrap());
            let range = resolve_range(base, &custom_for_export.lock().unwrap());
            *current_for_export.lock().unwrap() = range.clone();
            match export_current_range(&db_for_export, &range) {
                Ok(path) => {
                    log::info!("卡顿分析：已导出事件 CSV → {}", path);
                    if let Some(ui) = weak_export.upgrade() {
                        ui.set_export_status(SharedString::from(format!("已导出：{}", path)));
                    }
                }
                Err(e) => {
                    log::warn!("卡顿分析：导出 CSV 失败 ({})", e);
                    if let Some(ui) = weak_export.upgrade() {
                        ui.set_export_status(SharedString::from(format!("导出失败：{}", e)));
                    }
                }
            }
        });

        // F5：进程钻取（点击元凶榜某进程）→ 高级模式按 name 过滤事件表
        let drill_for_click = drill_filter.clone();
        let current_for_click = current_range.clone();
        let custom_for_click = custom_range.clone();
        let db_for_click = db_path.clone();
        let advanced_for_click = advanced.clone();
        let weak_click = ui.as_weak();
        let range_index_for_click = range_index.clone();
        let te_for_click = table_events.clone();
        ui.on_culprit_clicked(move |name: SharedString| {
            // 仅高级模式支持钻取；基础模式点击忽略
            if !*advanced_for_click.lock().unwrap() {
                return;
            }
            let name = name.to_string();
            log::info!("卡顿分析：钻取进程 = {}", name);
            *drill_for_click.lock().unwrap() = Some(name.clone());
            let base = TimeRange::from_index(*range_index_for_click.lock().unwrap());
            let range = resolve_range(base, &custom_for_click.lock().unwrap());
            *current_for_click.lock().unwrap() = range.clone();
            if let Some(ui) = weak_click.upgrade() {
                ui.set_drill_name(SharedString::from(name.clone()));
                let sort = read_event_sort(&ui);
                refill_event_table(&ui, &db_for_click, &range, &Some(name), &sort, &te_for_click);
            }
        });

        // F5：自定义范围「应用」→ 以自定义区间重查
        let custom_for_apply = custom_range.clone();
        let current_for_apply = current_range.clone();
        let db_for_apply = db_path.clone();
        let advanced_for_apply = advanced.clone();
        let bucket_for_apply = bucket.clone();
        let he_for_apply = hover_events.clone();
        let hb_for_apply = hover_buckets.clone();
        let hs_for_apply = hover_snaps.clone();
        let te_for_apply = table_events.clone();
        let rv_for_apply = resource_view.clone();
        let weak_apply = ui.as_weak();
        ui.on_custom_range_applied(move |from: SharedString, to: SharedString| {
            let range = TimeRange::Custom(from.to_string(), to.to_string());
            *custom_for_apply.lock().unwrap() = Some((from.to_string(), to.to_string()));
            *current_for_apply.lock().unwrap() = range.clone();
            log::info!("卡顿分析：应用自定义范围 {} ~ {}", from, to);
            if let Some(ui) = weak_apply.upgrade() {
                let adv = *advanced_for_apply.lock().unwrap();
                let b = *bucket_for_apply.lock().unwrap();
                refresh_window(
                    &ui,
                    &db_for_apply,
                    range,
                    adv,
                    b,
                    &he_for_apply,
                    &hb_for_apply,
                    &hs_for_apply,
                    &te_for_apply,
                    &rv_for_apply,
                );
            }
        });

        // F5：清除钻取筛选
        let drill_for_clear = drill_filter.clone();
        let current_for_clear = current_range.clone();
        let custom_for_clear = custom_range.clone();
        let db_for_clear = db_path.clone();
        let weak_clear = ui.as_weak();
        let range_index_for_clear = range_index.clone();
        let te_for_clear = table_events.clone();
        ui.on_clear_drill_requested(move || {
            *drill_for_clear.lock().unwrap() = None;
            if let Some(ui) = weak_clear.upgrade() {
                ui.set_drill_name(SharedString::from(""));
                let base = TimeRange::from_index(*range_index_for_clear.lock().unwrap());
                let range = resolve_range(base, &custom_for_clear.lock().unwrap());
                *current_for_clear.lock().unwrap() = range.clone();
                let sort = read_event_sort(&ui);
                refill_event_table(&ui, &db_for_clear, &range, &None, &sort, &te_for_clear);
            }
        });

        // F5：事件表列头排序。更新 UI 排序状态（箭头显示）后按排序列重查并回填事件表，
        // 保留当前时间范围与钻取筛选（PRD §5「可排序/筛选」）。
        let custom_for_sort = custom_range.clone();
        let current_for_sort = current_range.clone();
        let db_for_sort = db_path.clone();
        let range_index_for_sort = range_index.clone();
        let weak_sort = ui.as_weak();
        let te_for_sort = table_events.clone();
        ui.on_sort_requested(move |col: SharedString| {
            if let Some(ui) = weak_sort.upgrade() {
                let col = col.to_string();
                let same_col = ui.get_sort_column().to_string() == col;
                // 同列再次点击切换方向；新列默认：时长/等级降序（最大/最严重在前），其余升序
                let asc = if same_col {
                    !ui.get_sort_ascending()
                } else {
                    !(col == "duration" || col == "severity")
                };
                ui.set_sort_column(SharedString::from(col.clone()));
                ui.set_sort_ascending(asc);
                let base = TimeRange::from_index(*range_index_for_sort.lock().unwrap());
                let range = resolve_range(base, &custom_for_sort.lock().unwrap());
                *current_for_sort.lock().unwrap() = range.clone();
                let drill = ui.get_drill_name().to_string();
                let drill_opt = if drill.is_empty() { None } else { Some(drill) };
                let sort = EventSort {
                    column: parse_event_sort_column(&col),
                    asc,
                };
                refill_event_table(&ui, &db_for_sort, &range, &drill_opt, &sort, &te_for_sort);
            }
        });

        // 刷新按钮：重新查询
        let weak_refresh = ui.as_weak();
        let range_refresh = range_index.clone();
        let custom_refresh = custom_range.clone();
        let advanced_refresh = advanced.clone();
        let bucket_refresh = bucket.clone();
        let he_for_refresh = hover_events.clone();
        let hb_for_refresh = hover_buckets.clone();
        let hs_for_refresh = hover_snaps.clone();
        let te_for_refresh = table_events.clone();
        let rv_for_refresh = resource_view.clone();
        // 闭包捕获 db_path 副本（move 后原 db_path 仍留给下方 refresh_window 与结构体）
        let db_path_for_refresh = db_path.clone();
        ui.on_refresh_requested(move || {
            if let Some(ui) = weak_refresh.upgrade() {
                let base = TimeRange::from_index(*range_refresh.lock().unwrap());
                let range = resolve_range(base, &custom_refresh.lock().unwrap());
                let adv = *advanced_refresh.lock().unwrap();
                let b = *bucket_refresh.lock().unwrap();
                refresh_window(
                    &ui,
                    &db_path_for_refresh,
                    range,
                    adv,
                    b,
                    &he_for_refresh,
                    &hb_for_refresh,
                    &hs_for_refresh,
                    &te_for_refresh,
                    &rv_for_refresh,
                );
            }
        });

        ui.show()?;
        // 不在 Windows 系统任务栏显示（工具窗口样式）
        crate::window::ensure_tool_window_for(ui.window());
        // winit 在 show 后重算样式 → 延迟补一次（与 ProcessList 一致）
        let weak_toolwin = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(500), move || {
            if let Some(ui) = weak_toolwin.upgrade() {
                crate::window::ensure_tool_window_for(ui.window());
            }
        });

        // 首次打开即查询一次（基础模式默认，不启动自动刷新；PRD §4 F7 自动刷新仅高级模式）
        refresh_window(
            &ui,
            &db_path.clone(),
            TimeRange::Today,
            false,
            TrendBucket::Hour,
            &hover_events,
            &hover_buckets,
            &hover_snaps,
            &table_events,
            &resource_view,
        );

        Ok(Self {
            ui,
            db_path,
            range_index,
            advanced,
            custom_range,
            drill_filter,
            current_range,
            bucket,
            auto_refresh_timer,
            auto_interval,
            hover_events,
            hover_buckets,
            hover_snaps,
            table_events,
            resource_view,
        })
    }

    /// 立即刷新：重查 KPI + 趋势图（窗口已关闭则先 show）。
    /// 非阻塞：趋势图位图渲染放后台线程，不阻塞 UI 线程。
    pub fn refresh(&self) {
        if !self.ui.window().is_visible() {
            let _ = self.ui.show();
            crate::window::ensure_tool_window_for(self.ui.window());
        }
        let base = TimeRange::from_index(*self.range_index.lock().unwrap());
        let range = resolve_range(base, &self.custom_range.lock().unwrap());
        let adv = *self.advanced.lock().unwrap();
        let b = *self.bucket.lock().unwrap();
        refresh_window(
            &self.ui,
            &self.db_path.clone(),
            range,
            adv,
            b,
            &self.hover_events,
            &self.hover_buckets,
            &self.hover_snaps,
            &self.table_events,
            &self.resource_view,
        );
    }

    /// 底层 Slint 窗口（供 lib.rs 1Hz tick 守护 tool-window 样式）。
    pub fn window(&self) -> &slint::Window {
        self.ui.window()
    }
}

/// 从 Analysis 组件读取 db 路径（回调闭包内复用实例时用）。
///
/// 查询并刷新一个 Analysis 窗口的内容（KPI + 趋势图 + … + 事件表 + 资源图）。
///
/// 抽成自由函数便于 `show` / `refresh` / 各回调复用，避免重复逻辑。
/// `advanced` 决定：基础模式只回填结论所需（Top5 + 结论文案、隐藏事件表）；
/// 高级模式回填全部（Top10 + 原始事件表）。
/// `hover_events` / `hover_buckets` / `table_events` / `view` 为 F3 高级交互所需的
/// 共享状态（分别供 hover 定位、snapshot 取行、资源图可选指标）。各回调已克隆 Arcs
/// 传入，避免持有窗口强引用。
/// `hover_events` / `hover_buckets` 为 F3 hover 定位的卡顿事件列表与桶序号；
/// `hover_snaps` 与 `hover_events` 同序，存每次卡顿的资源快照全字段（hover 信息末尾追加快照）。
fn refresh_window(
    ui: &crate::Analysis,
    db_path: &PathBuf,
    range: TimeRange,
    advanced: bool,
    bucket: TrendBucket,
    hover_events: &Arc<Mutex<Vec<EventRow>>>,
    hover_buckets: &Arc<Mutex<Vec<i64>>>,
    hover_snaps: &Arc<Mutex<Vec<Option<EventSnapshot>>>>,
    table_events: &Arc<Mutex<Vec<EventRow>>>,
    view: &Arc<Mutex<ResourceView>>,
) {
    // 打开只读连接（WAL 下并发于 service 写库）；失败则显示「无数据」
    let conn = match analytics::open_readonly(db_path) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("卡顿分析：打开数据库失败 ({})", e);
            ui.set_kpi_today(SharedString::from("无数据"));
            ui.set_kpi_worst(SharedString::from("—"));
            ui.set_kpi_peak(SharedString::from("—"));
            ui.set_kpi_top(SharedString::from("—"));
            return;
        }
    };
    // 幂等创建时间戳索引（仅首次；后续调用是 no-op）
    if let Err(e) = analytics::ensure_indexes(&conn) {
        log::warn!("卡顿分析：创建索引失败 ({}), 大范围查询可能较慢", e);
    }

    // KPI（今日口径）
    match analytics::load_kpi_today(&conn) {
        Ok(kpi) => {
            ui.set_kpi_today(SharedString::from(format!("{}", kpi.today_count)));
            ui.set_kpi_worst(SharedString::from(analytics::format_duration(
                kpi.worst_duration_ms,
            )));
            ui.set_kpi_peak(SharedString::from(kpi.peak_hour));
            ui.set_kpi_top(SharedString::from(kpi.top_culprit));
        }
        Err(e) => {
            log::warn!("卡顿分析：KPI 查询失败 ({})", e);
            ui.set_kpi_today(SharedString::from("—"));
            ui.set_kpi_worst(SharedString::from("—"));
            ui.set_kpi_peak(SharedString::from("—"));
            ui.set_kpi_top(SharedString::from("—"));
        }
    }

    // 基础模式 Top5 / 高级模式 Top10（PRD F2）
    let top_n = if advanced { 10 } else { 5 };

    // F2：元凶进程 Top N（同步查询，数据量小不阻塞 UI，PRD §6.3）
    match analytics::load_culprits(&conn, &range, top_n) {
        Ok(culprits) => {
            let has = !culprits.is_empty();
            let max_count = culprits.iter().map(|c| c.count).max().unwrap_or(0).max(1);
            let rows: Vec<crate::CulpritRow> = culprits
                .iter()
                .map(|c| crate::CulpritRow {
                    name: SharedString::from(c.name.clone()),
                    count: SharedString::from(format!("{} 次", c.count)),
                    duration: SharedString::from(analytics::format_duration(c.total_duration_ms)),
                    max_cpu: SharedString::from(format!("{:.1}%", c.max_cpu)),
                    max_mem: SharedString::from(format!("{} MB", c.max_mem_mb)),
                    bar_fraction: c.count as f32 / max_count as f32,
                })
                .collect();
            let model = Rc::new(VecModel::from(rows));
            ui.set_culprit_model(ModelRc::from(model));
            ui.set_has_culprits(has);
            // 结论（基础模式展示）：取出现次数最多的进程 + 范围标签
            let conclusion = if let Some(top) = culprits.first() {
                format!("{} 是{}最大的卡顿元凶", top.name, range.label())
            } else {
                format!("{}暂无卡顿元凶数据", range.label())
            };
            ui.set_culprit_conclusion(SharedString::from(conclusion));
        }
        Err(e) => {
            log::warn!("卡顿分析：元凶聚合失败 ({})", e);
            ui.set_has_culprits(false);
            ui.set_culprit_conclusion(SharedString::from("—"));
        }
    }

    // F4：卡顿类型占比饼图（后台线程渲染，PRD §6.4 / M3）
    let weak_cause = ui.as_weak();
    let db_path_cause = db_path.clone();
    let range_cause = range.clone();
    std::thread::Builder::new()
        .name("analysis-cause-chart".into())
        .spawn(move || {
            // 后台线程独立开连接查询 + 渲染（UI 线程不阻塞）
            if let Ok(c) = analytics::open_readonly(&db_path_cause) {
                if let Ok(types) = analytics::load_cause_types(&c, &range_cause) {
                    super::render_cause_pie(&types, 420, 280, move |image| {
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak_cause.upgrade() {
                                if let Some(buf) = image {
                                    ui.set_cause_image(slint::Image::from_rgba8(buf));
                                    ui.set_has_cause(true);
                                } else {
                                    ui.set_has_cause(false);
                                }
                            }
                        })
                        .ok();
                    });
                } else {
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak_cause.upgrade() {
                            ui.set_has_cause(false);
                        }
                    })
                    .ok();
                }
            }
        })
        .ok();

    // 趋势图：后台线程用 plotters 渲染 → 推回 UI 线程（M2；见 render_trend_chart）
    let weak = ui.as_weak();
    // 后台线程需要独立 range 副本（move 走），避免与下方 refill_event_table 的借用冲突
    let range_trend = range.clone();
    // F3 资源图线程需要的 range 克隆
    let range_res = range.clone();
    let db_path_render = db_path.clone();
    std::thread::Builder::new()
        .name("analysis-chart".into())
        .spawn(move || {
            // 后台线程独立开连接渲染（UI 线程不阻塞，PRD §6.3）
            if let Ok(c) = analytics::open_readonly(&db_path_render) {
                if let Ok(trend) = analytics::load_trend(&c, &range_trend, bucket) {
                    super::render_trend_chart(&trend, 860, 300, move |image| {
                        // 推回 UI 线程设置位图
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                if let Some(buf) = image {
                                    // 后台线程渲染所得的像素缓冲 → slint::Image
                                    ui.set_trend_image(slint::Image::from_rgba8(buf));
                                    ui.set_has_chart(true);
                                } else {
                                    ui.set_has_chart(false);
                                }
                            }
                        })
                        .ok();
                    });
                }
            }
        })
        .ok();

    // F3：系统资源关联图（后台线程加载 samples → SQL 降采样 → 渲染，独立回调/属性，
    // 与 M3 饼图线程互不干扰，PRD §6.3 / M4）。
    let weak_res = ui.as_weak();
    let db_path_res = db_path.clone();
    let view_res = view.clone();
    let he_res = hover_events.clone();
    let hb_res = hover_buckets.clone();
    let hs_res = hover_snaps.clone();
    std::thread::Builder::new()
        .name("analysis-resource-chart".into())
        .spawn(move || {
            if let Ok(c) = analytics::open_readonly(&db_path_res) {
                if let Ok(data) = analytics::load_resource_samples(&c, &range_res, 860) {
                    // F3 C1：加载与资源图卡顿竖线同序（按 timestamp 升序）的事件，
                    // 按同一公式折算桶序号，存入 hover_events/hover_buckets 供
                    // on_resource_hover 定位最近事件（与 ResourceData.event_x 同序 zip）。
                    let n = ((data.span_secs + data.bucket_secs - 1) / data.bucket_secs) + 1;
                    let hover_evs = analytics::load_events(&c, &range_res).unwrap_or_default();
                    let hover_bks: Vec<i64> = hover_evs
                        .iter()
                        .map(|e| {
                            ((e.ts_secs - data.base_secs) / data.bucket_secs).clamp(0, n - 1)
                        })
                        .collect();
                    // 与 hover_evs 同序计算每个事件的资源快照（事件瞬间全字段），
                    // 供 on_resource_hover 末尾追加快照行（PRD §4 F3）。
                    let hover_snps: Vec<Option<EventSnapshot>> = hover_evs
                        .iter()
                        .map(|e| analytics::load_event_snapshot(&c, e.ts_secs))
                        .collect();
                    *he_res.lock().unwrap() = hover_evs;
                    *hb_res.lock().unwrap() = hover_bks;
                    *hs_res.lock().unwrap() = hover_snps;

                    // 按当前 ResourceView（可选指标 + 对数轴）渲染
                    let v = *view_res.lock().unwrap();
                    super::render_resource_chart(&data, 860, 220, &v, move |image| {
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak_res.upgrade() {
                                if let Some(buf) = image {
                                    ui.set_resource_image(slint::Image::from_rgba8(buf));
                                    ui.set_has_resource(true);
                                } else {
                                    ui.set_has_resource(false);
                                }
                            }
                        })
                        .ok();
                    });
                } else {
                    *he_res.lock().unwrap() = Vec::new();
                    *hb_res.lock().unwrap() = Vec::new();
                    *hs_res.lock().unwrap() = Vec::new();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak_res.upgrade() {
                            ui.set_has_resource(false);
                        }
                    })
                    .ok();
                }
            }
        })
        .ok();

    // F5：高级模式回填原始事件表；基础模式不加载（由 slint 端隐藏该区，
    // 仅回填结论所需，PRD F5）。钻取筛选名从 UI 端读取作为增量刷新依据。
    if advanced {
        let drill = ui.get_drill_name().to_string();
        let drill_opt = if drill.is_empty() { None } else { Some(drill) };
        let sort = read_event_sort(ui);
        refill_event_table(ui, db_path, &range, &drill_opt, &sort, table_events);
    } else {
        // 基础模式清空事件表，避免陈旧数据残留
        ui.set_event_model(ModelRc::from(Rc::new(VecModel::from(
            Vec::<crate::EventRow>::new(),
        ))));
        ui.set_event_count_text(SharedString::from(""));
        ui.set_drill_name(SharedString::from(""));
    }
}

/// 仅刷新原始事件表（供钻取/清除筛选/排序增量更新，避免重渲图表）。
///
/// 独立开只读连接读取 `load_events_sorted`，按 `drill` 进程名（若有）过滤，
/// 写入 `event-model` 与条数提示 `event-count-text`。同时把当前显示的事件列表（表序、
/// 含钻取筛选与排序）存入 `table_events`，供 `on_row_clicked` 按序号取该次卡顿加载 snapshot。
fn refill_event_table(
    ui: &crate::Analysis,
    db_path: &PathBuf,
    range: &TimeRange,
    drill: &Option<String>,
    sort: &EventSort,
    table_events: &Arc<Mutex<Vec<EventRow>>>,
) {
    match analytics::open_readonly(db_path) {
        Ok(conn) => {
            match analytics::load_events_sorted(&conn, range, sort) {
                Ok(mut events) => {
                    // 进程钻取：仅保留元凶含该进程名的事件（精确匹配 name）
                    if let Some(name) = drill {
                        events.retain(|e| e.culprit_names.iter().any(|n| n == name));
                    }
                    let total = events.len();
                    let rows: Vec<crate::EventRow> = events
                        .iter()
                        .map(|e| crate::EventRow {
                            time: SharedString::from(e.time_local.clone()),
                            duration: SharedString::from(format!("{}", e.duration_ms)),
                            duration_ms: e.duration_ms as i32,
                            severity: SharedString::from(e.severity_cn.clone()),
                            causes: SharedString::from(e.causes_text.clone()),
                            culprits: SharedString::from(e.culprits_text.clone()),
                        })
                        .collect();
                    let model = Rc::new(VecModel::from(rows));
                    ui.set_event_model(ModelRc::from(model));
                    // 条数文案：区分「全部」与「钻取筛选」
                    let text = match drill {
                        Some(name) => format!("筛选「{}」：共 {} 条", name, total),
                        None => format!("共 {} 条", total),
                    };
                    ui.set_event_count_text(SharedString::from(text));
                    // 留存表序事件列表供 snapshot 取行（F3 C2）
                    *table_events.lock().unwrap() = events;
                }
                Err(e) => {
                    log::warn!("卡顿分析：事件表查询失败 ({})", e);
                    ui.set_event_count_text(SharedString::from("事件查询失败"));
                    *table_events.lock().unwrap() = Vec::new();
                }
            }
        }
        Err(e) => {
            log::warn!("卡顿分析：事件表打开数据库失败 ({})", e);
            ui.set_event_count_text(SharedString::from("无数据"));
            *table_events.lock().unwrap() = Vec::new();
        }
    }
}

/// F7：构造并启动自动刷新 Repeating 定时器（消除两处重复的 Timer 启动闭包，B1）。
///
/// 闭包内用 `weak.upgrade()` 防窗口泄漏（窗口关闭后自动跳过）；触发时按当前 range/bucket/
/// advanced 重跑 `refresh_window`（含 hover/表/snapshot 共享状态）。`secs` 为刷新间隔；
/// 调用方（on_auto_refresh_changed / on_toggle_mode_changed）据 `auto_interval` 传入。
fn start_auto_refresh(
    weak: &Weak<crate::Analysis>,
    db: &PathBuf,
    range: &Arc<Mutex<TimeRange>>,
    bucket: &Arc<Mutex<TrendBucket>>,
    advanced: &Arc<Mutex<bool>>,
    timer: &Arc<Mutex<Timer>>,
    hover_events: &Arc<Mutex<Vec<EventRow>>>,
    hover_buckets: &Arc<Mutex<Vec<i64>>>,
    hover_snaps: &Arc<Mutex<Vec<Option<EventSnapshot>>>>,
    table_events: &Arc<Mutex<Vec<EventRow>>>,
    view: &Arc<Mutex<ResourceView>>,
    secs: u64,
) {
    let weak = weak.clone();
    let db = db.clone();
    let rng = range.clone();
    let bk = bucket.clone();
    let adv = advanced.clone();
    let tmr = timer.clone();
    let he = hover_events.clone();
    let hb = hover_buckets.clone();
    let hs = hover_snaps.clone();
    let te = table_events.clone();
    let rv = view.clone();
    // 单次创建、长期持有的定时器：start 覆盖上一次设置（切模式/改间隔时复用同一实例）。
    tmr.lock().unwrap().start(TimerMode::Repeated, Duration::from_secs(secs), move || {
        if let Some(ui) = weak.upgrade() {
            let range = rng.lock().unwrap().clone();
            let b = *bk.lock().unwrap();
            let a = *adv.lock().unwrap();
            refresh_window(&ui, &db, range, a, b, &he, &hb, &hs, &te, &rv);
        }
    });
}

/// F3（高级）：只重跑资源图刷新（不重查 KPI/趋势/饼图/事件表）。
///
/// 复用现有资源加载 + `render_resource_chart` 逻辑，按 `view`（可选指标 + 对数轴）重渲。
/// 用于 CheckBox 切换指标 / 对数轴时增量更新，避免整窗重查（PRD §4 F3 / §5）。
fn refresh_resource(ui: &crate::Analysis, db_path: &PathBuf, range: TimeRange, view: ResourceView) {
    let weak = ui.as_weak();
    let db = db_path.clone();
    std::thread::Builder::new()
        .name("analysis-resource-chart".into())
        .spawn(move || {
            if let Ok(c) = analytics::open_readonly(&db) {
                if let Ok(data) = analytics::load_resource_samples(&c, &range, 860) {
                    super::render_resource_chart(&data, 860, 220, &view, move |image| {
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                if let Some(buf) = image {
                                    ui.set_resource_image(slint::Image::from_rgba8(buf));
                                    ui.set_has_resource(true);
                                } else {
                                    ui.set_has_resource(false);
                                }
                            }
                        })
                        .ok();
                    });
                } else {
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_has_resource(false);
                        }
                    })
                    .ok();
                }
            }
        })
        .ok();
}

/// 字节数（B/s）转人类可读短标签（供事件 snapshot 详情的磁盘读/写展示）。
fn fmt_bytes(b: f64) -> String {
    if b >= 1e9 {
        format!("{:.1}G", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1}M", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.1}K", b / 1e3)
    } else {
        format!("{:.0}", b)
    }
}

/// 把下拉索引推导出的 `TimeRange` 与已保存的自定义区间合并：
/// 仅当索引指向「自定义」且自定义区间已设置时，使用自定义区间；否则原样返回。
fn resolve_range(base: TimeRange, custom: &Option<(String, String)>) -> TimeRange {
    match (&base, custom) {
        (TimeRange::Custom(..), Some((from, to))) => {
            TimeRange::Custom(from.clone(), to.clone())
        }
        _ => base,
    }
}

/// 从 Analysis 组件读取当前事件表排序状态（slint 端列头箭头维护的同款来源）。
fn read_event_sort(ui: &crate::Analysis) -> EventSort {
    let col = ui.get_sort_column().to_string();
    EventSort {
        column: parse_event_sort_column(&col),
        asc: ui.get_sort_ascending(),
    }
}

/// F8：把当前时间范围事件导出为 CSV 到用户可写目录（桌面，回退 CWD）。
///
/// 不写 stutter.db 所在目录（PRD §8 / 硬约束）。文件名含范围标签，如
/// `卡顿事件_今日.csv`。返回最终路径（供 UI 提示）。
fn export_current_range(db_path: &PathBuf, range: &TimeRange) -> anyhow::Result<String> {
    // 用户可写目录：优先桌面（Windows USERPROFILE/Desktop），回退当前工作目录
    let dir = std::env::var("USERPROFILE")
        .map(|p| std::path::Path::new(&p).join("Desktop"))
        .ok()
        .filter(|d| d.exists())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let label = match range {
        TimeRange::Today => "今日",
        TimeRange::Last7 => "近7天",
        TimeRange::Last30 => "近30天",
        TimeRange::Custom(..) => "所选时段",
    };
    let filename = format!("卡顿事件_{}.csv", label);
    let path = dir.join(filename);

    let conn = analytics::open_readonly(db_path)?;
    analytics::ensure_indexes(&conn).ok();
    analytics::export_events_csv(&conn, range, &path)?;
    Ok(path.to_string_lossy().to_string())
}

/// 皮肤注入：让 Analysis 主框架跟随 skin.toml（与 Overlay/ProcessList 一致）。
///
/// 仅注入外框背景/边框/圆角与文字色；内部面板沿用固定浅色中性色（与进程详情页
/// 内部区域一致）。深色/浅色切换通过 config.ui.skin 指定的皮肤文件生效。
fn apply_skin(ui: &crate::Analysis) {
    let config = find_stutter_core::Config::load("config.toml").unwrap_or_else(|_| {
        find_stutter_core::Config::default()
    });
    let skin = crate::skin::SkinConfig::load(&config.ui.skin);
    ui.set_skin_bg(Brush::SolidColor(
        parse_color(&skin.background_color).unwrap_or(Color::from_rgb_u8(0xff, 0xff, 0xff)),
    ));
    ui.set_skin_border_color(
        parse_color(&skin.border_color).unwrap_or(Color::from_rgb_u8(0xc0, 0xc0, 0xc0)),
    );
    ui.set_skin_border_radius(skin.border_radius as f32);
    // 文字色：默认深色 #1e1e2e（与 ProcessList 文本色一致）；浅色皮肤下保持可读
    ui.set_skin_text_color(Color::from_rgb_u8(0x1e, 0x1e, 0x2e));
    ui.set_skin_subtext_color(Color::from_rgb_u8(0x60, 0x60, 0x66));
    ui.set_skin_panel_bg(Color::from_rgb_u8(0xfa, 0xfb, 0xfc));
    ui.set_skin_panel_border(Color::from_rgb_u8(0xe2, 0xe4, 0xea));
}

/// 解析 `#RRGGBB` 皮肤色为 slint `Color`（与 overlay.rs 同款逻辑）。
fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::from_rgb_u8(r, g, b))
}
