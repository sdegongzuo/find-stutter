//! 历史流量统计窗口（悬浮窗「今日 ↑x ↓y」点击打开）。
//!
//! 复用 `AnalysisWindow` 的窗口模式（`Arc<Mutex<Option<Arc<...>>>>` 首次创建 +
//! 之后只 `refresh()`，见 `lib.rs`），风格也对齐 `Analysis`：no-frame 窗口 +
//! 透明背景 + 白底背板 + 自绘标题栏（拖动经 `drag-moved` 回调）。
//!
//! ## 线程模型（硬约束：UI 线程绝不碰 sqlite）
//!
//! - `refresh()`：克隆 db 路径 + `Weak` 句柄 → 后台线程查
//!   [`daily_network_traffic`] → `slint::invoke_from_event_loop` 把结果推回 UI 线程。
//!   用 [`AtomicU64`] generation 防抖：每次 `refresh()` 自增，回到 UI 线程时比对，
//!   过期结果直接丢弃（连点两次不会出现旧数据覆盖新数据）。
//! - 视图内一切切换（日/周/月、对数/线性、翻月）**只读内存缓存重算**，
//!   不重新查库：聚合与日历生成都是几十行的纯函数，UI 线程做无感知。
//!
//! ## 口径
//!
//! 数据层 [`daily_network_traffic`] 已经处理了「>3 天的分钟级降采样行 ×60 补偿」，
//! 本模块**不做任何补偿/换算**，只做按日 → 按周/月的纯加法聚合。

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate, Utc};
use find_stutter_core::logger::{daily_network_traffic, DailyTraffic};
use slint::{Brush, Color, ComponentHandle, ModelRc, SharedString, VecModel};

/// 窗口初始尺寸（逻辑像素，与 `overlay.slint` 的 `TrafficStats` 声明同源）。
///
/// 只在「显示之前推算落点」时用：那一刻窗口还没创建，拿不到它自己的尺寸。
/// 改 .slint 里的 width/height 必须同步改这里，否则居中会算偏。
const TRAFFIC_LOGICAL_W: f32 = 880.0;
const TRAFFIC_LOGICAL_H: f32 = 620.0;

/// 查库回溯天数：日历可翻到前两个月，多取一点冗余无害（服务只保留有限天数）。
const LOOKBACK_DAYS: i64 = 60;

const GIB: u64 = 1024 * 1024 * 1024;
const TIB: u64 = GIB * 1024;

/// 历史流量统计窗口句柄。
pub struct TrafficStatsWindow {
    ui: crate::TrafficStats,
    /// 数据库路径（来自 config.toml）
    db_path: PathBuf,
    /// 日聚合缓存（后台线程写入，UI 线程只读重算；切换视图/翻月不重查库）
    daily: Arc<Mutex<Vec<DailyTraffic>>>,
    /// 刷新代号：后台结果回到 UI 线程时比对，过期即丢弃
    generation: Arc<AtomicU64>,
    /// 统计方式（0=日 / 1=周 / 2=月）
    stats_mode: Arc<Mutex<i32>>,
    /// 图表比例（0=对数 / 1=线性）
    scale_mode: Arc<Mutex<i32>>,
    /// 日历当前显示的年月（本地时区当月为初值）
    month: Arc<Mutex<(i32, u32)>>,
}

impl TrafficStatsWindow {
    /// 创建并显示窗口（首次调用；之后复用 `refresh()`）。
    pub fn show(db_path: PathBuf) -> anyhow::Result<Self> {
        let ui = crate::TrafficStats::new()?;
        let daily: Arc<Mutex<Vec<DailyTraffic>>> = Arc::new(Mutex::new(Vec::new()));
        let generation = Arc::new(AtomicU64::new(0));
        let stats_mode: Arc<Mutex<i32>> = Arc::new(Mutex::new(0));
        let scale_mode: Arc<Mutex<i32>> = Arc::new(Mutex::new(0));
        let month: Arc<Mutex<(i32, u32)>> = Arc::new(Mutex::new(current_month()));

        // 皮肤注入：让窗口外框跟随 skin.toml（与 Analysis / ProcessList 一致）
        apply_skin(&ui);

        // 首帧先铺一份「本月 + 空数据」的日历占位（42 格），避免刷新回来前空白
        recompute(&ui, &[], 0, 0, *month.lock().unwrap());

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

        // 关闭：只隐藏，保留实例复用（下次点击入口直接 refresh）
        let weak_close = ui.as_weak();
        ui.on_close_requested(move || {
            if let Some(ui) = weak_close.upgrade() {
                let _ = ui.hide();
            }
        });

        // 视图切换：纯展示切换（两视图数据同时在线），只记日志
        ui.on_view_changed(move |idx| {
            log::info!("历史流量统计：切换到{}", if idx == 1 { "日历视图" } else { "列表视图" });
        });

        // 统计方式（日/周/月）：只读缓存重算
        let daily_for_mode = daily.clone();
        let mode_for_stats = stats_mode.clone();
        let scale_for_mode = scale_mode.clone();
        let month_for_mode = month.clone();
        let weak_mode = ui.as_weak();
        ui.on_stats_mode_changed(move |idx| {
            *mode_for_stats.lock().unwrap() = idx;
            log::info!(
                "历史流量统计：统计方式 = {}",
                match idx {
                    1 => "周",
                    2 => "月",
                    _ => "日",
                }
            );
            if let Some(ui) = weak_mode.upgrade() {
                let data = daily_for_mode.lock().unwrap().clone();
                recompute(
                    &ui,
                    &data,
                    idx,
                    *scale_for_mode.lock().unwrap(),
                    *month_for_mode.lock().unwrap(),
                );
            }
        });

        // 图表比例（对数/线性）：只读缓存重算
        let daily_for_scale = daily.clone();
        let mode_for_scale = stats_mode.clone();
        let scale_for_scale = scale_mode.clone();
        let month_for_scale = month.clone();
        let weak_scale = ui.as_weak();
        ui.on_scale_mode_changed(move |idx| {
            *scale_for_scale.lock().unwrap() = idx;
            if let Some(ui) = weak_scale.upgrade() {
                // 回写属性：ComboBox 的 current-index 绑在 root.scale-mode 上，
                // slint 端选中后只改了它自己的索引，这里把根属性同步过来。
                ui.set_scale_mode(idx);
                let data = daily_for_scale.lock().unwrap().clone();
                recompute(
                    &ui,
                    &data,
                    *mode_for_scale.lock().unwrap(),
                    idx,
                    *month_for_scale.lock().unwrap(),
                );
            }
        });

        // 翻月（-1 / +1）：只重算日历，不查库
        let daily_for_step = daily.clone();
        let mode_for_step = stats_mode.clone();
        let scale_for_step = scale_mode.clone();
        let month_for_step = month.clone();
        let weak_step = ui.as_weak();
        ui.on_month_step(move |delta| {
            {
                let mut m = month_for_step.lock().unwrap();
                *m = shift_month(*m, delta);
            }
            if let Some(ui) = weak_step.upgrade() {
                let data = daily_for_step.lock().unwrap().clone();
                recompute(
                    &ui,
                    &data,
                    *mode_for_step.lock().unwrap(),
                    *scale_for_step.lock().unwrap(),
                    *month_for_step.lock().unwrap(),
                );
            }
        });

        // 回到本月
        let daily_for_reset = daily.clone();
        let mode_for_reset = stats_mode.clone();
        let scale_for_reset = scale_mode.clone();
        let month_for_reset = month.clone();
        let weak_reset = ui.as_weak();
        ui.on_month_reset(move || {
            *month_for_reset.lock().unwrap() = current_month();
            if let Some(ui) = weak_reset.upgrade() {
                let data = daily_for_reset.lock().unwrap().clone();
                recompute(
                    &ui,
                    &data,
                    *mode_for_reset.lock().unwrap(),
                    *scale_for_reset.lock().unwrap(),
                    *month_for_reset.lock().unwrap(),
                );
            }
        });

        // 首帧即终态：显示之前把落点算好（光标所在显示器工作区居中），
        // 避免先按 winit 默认位置画一帧再跳到居中的闪动（同 Analysis）。
        let planned = crate::window::plan_centered_rect(TRAFFIC_LOGICAL_W, TRAFFIC_LOGICAL_H);
        if let Some(rect) = planned {
            ui.window()
                .set_position(slint::PhysicalPosition::new(rect.0, rect.1));
        }
        ui.show()?;
        // 推算失败（拿不到工作区）才退回「显示后夹取」
        if planned.is_none() {
            crate::window::fit_window_to_work(ui.window());
        }
        // 普通应用窗口（留在任务栏 / Alt-Tab），不置顶
        crate::window::ensure_app_window_for(ui.window());
        crate::window::bring_to_front(ui.window());
        // winit 在 show 后重算样式 → 延迟补一次（与 Analysis 一致）
        let weak_toolwin = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
            if let Some(ui) = weak_toolwin.upgrade() {
                crate::window::ensure_app_window_for(ui.window());
            }
        });

        let win = Self {
            ui,
            db_path,
            daily,
            generation,
            stats_mode,
            scale_mode,
            month,
        };
        // 首次打开查一次库（异步，不阻塞首帧）
        win.refresh();
        Ok(win)
    }

    /// 重新查库并刷新视图（窗口已关闭则先 show）。
    ///
    /// 查库在后台线程：UI 线程只做属性写入 + 几十行纯函数重算。
    pub fn refresh(&self) {
        if !self.ui.window().is_visible() {
            let _ = self.ui.show();
            crate::window::ensure_app_window_for(self.ui.window());
        }
        // 复用已有窗口时前置（窗口不置顶，可能已被别的 APP 压到后面）
        crate::window::bring_to_front(self.ui.window());

        // 代号自增：本线程的结果若比当前代号旧，回到 UI 线程时丢弃
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let gen_arc = self.generation.clone();
        let weak = self.ui.as_weak();
        let db_path = self.db_path.clone();
        let daily = self.daily.clone();
        let stats_mode = self.stats_mode.clone();
        let scale_mode = self.scale_mode.clone();
        let month = self.month.clone();

        std::thread::Builder::new()
            .name("traffic-daily".into())
            .spawn(move || {
                // 多取 60 天：日历可以往前翻两个月
                let since = (Utc::now() - ChronoDuration::days(LOOKBACK_DAYS))
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string();
                let mut rows = match crate::analytics::open_readonly(&db_path) {
                    Ok(conn) => match daily_network_traffic(&conn, &since) {
                        Ok(v) => v,
                        Err(e) => {
                            log::warn!("历史流量统计：每日流量查询失败 ({})", e);
                            Vec::new()
                        }
                    },
                    Err(e) => {
                        log::warn!("历史流量统计：打开数据库失败 ({})", e);
                        Vec::new()
                    }
                };
                // 过滤未来日期（时钟回拨 / 服务写入了异常时间戳时不该出现在图表里）
                let today = Local::now().date_naive();
                rows.retain(|d| d.date <= today);

                let _ = slint::invoke_from_event_loop(move || {
                    // 过期结果直接丢弃（防抖）
                    if gen_arc.load(Ordering::SeqCst) != generation {
                        return;
                    }
                    let Some(ui) = weak.upgrade() else {
                        return;
                    };
                    let data = {
                        let mut cache = daily.lock().unwrap();
                        *cache = rows;
                        cache.clone()
                    };
                    recompute(
                        &ui,
                        &data,
                        *stats_mode.lock().unwrap(),
                        *scale_mode.lock().unwrap(),
                        *month.lock().unwrap(),
                    );
                });
            })
            .ok();
    }

    /// 底层 Slint 窗口（供 lib.rs 1Hz tick 守护窗口样式）。
    pub fn window(&self) -> &slint::Window {
        self.ui.window()
    }
}

/// 本地时区的当月（年, 月）。
fn current_month() -> (i32, u32) {
    let today = Local::now().date_naive();
    (today.year(), today.month())
}

/// 按月平移（`delta` = -1 / +1），跨年自动进退位。
fn shift_month((year, month): (i32, u32), delta: i32) -> (i32, u32) {
    let mut y = year;
    let mut m = month as i32 + delta;
    while m > 12 {
        m -= 12;
        y += 1;
    }
    while m < 1 {
        m += 12;
        y -= 1;
    }
    (y, m as u32)
}

/// 聚合后的一行（列表视图数据源）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct AggRow {
    /// 该桶的代表日期（日=当天 / 周=周一 / 月=当月 1 号），仅用于排序
    sort_date: NaiveDate,
    /// 显示标签
    label: String,
    up: u64,
    down: u64,
}

impl AggRow {
    fn total(&self) -> u64 {
        self.up.saturating_add(self.down)
    }
}

/// 按 `mode`（0=日 / 1=周 / 2=月）聚合，结果按日期**降序**（最新在上）。
///
/// 周用 ISO 周（周一起算）：跨月同属一个 ISO 周的两天会合并到同一行。
fn aggregate(daily: &[DailyTraffic], mode: i32) -> Vec<AggRow> {
    use std::collections::BTreeMap;

    // 键用代表日期（升序），最后整体 reverse 得到降序；同周/同月天然合并。
    let mut by_key: BTreeMap<NaiveDate, AggRow> = BTreeMap::new();
    for d in daily {
        let (key, label) = match mode {
            1 => {
                let iso = d.date.iso_week();
                // 该日所在 ISO 周的周一：date - num_days_from_monday
                let monday = d.date - ChronoDuration::days(d.date.weekday().num_days_from_monday() as i64);
                (monday, format!("{}/第 {} 周", iso.year(), iso.week()))
            }
            2 => {
                let first = NaiveDate::from_ymd_opt(d.date.year(), d.date.month(), 1).unwrap_or(d.date);
                (first, format!("{}/{}", d.date.year(), d.date.month()))
            }
            _ => (d.date, d.date.format("%Y-%m-%d").to_string()),
        };
        let slot = by_key.entry(key).or_insert_with(|| AggRow {
            sort_date: key,
            label,
            up: 0,
            down: 0,
        });
        slot.up = slot.up.saturating_add(d.sent);
        slot.down = slot.down.saturating_add(d.recv);
    }

    let mut rows: Vec<AggRow> = by_key.into_values().collect();
    rows.reverse();
    rows
}

/// 图表条宽度比例。
///
/// - 线性：`v / max`
/// - 对数：以**数据自身的最小非零值**为下界——`(ln v − ln min_nz) / (ln max − ln min_nz)`，
///   让最小非零条为 0、最大条为 1、中间按对数分布。若用 `ln(1+v)/ln(1+max)` 这类
///   以 0 为起点的口径，数据动态范围只有一两个数量级时（本机日流量 7~750 GB），
///   所有条都会被压到 0.75 以上、几乎等长，对数比例就失去意义了（实测踩坑）。
/// - `v == 0` 或 `max == 0` → 0.0；非零值全相等（max == min_nz）→ 1.0；结果 clamp 到 [0,1]
fn bar_ratio(value: u64, min_nz: u64, max: u64, log_scale: bool) -> f32 {
    if value == 0 || max == 0 {
        return 0.0;
    }
    let v = value as f64;
    let m = max as f64;
    let r = if log_scale {
        if max == min_nz {
            1.0
        } else {
            ((v.ln() - (min_nz as f64).ln()) / (m.ln() - (min_nz as f64).ln())).max(0.0)
        }
    } else {
        v / m
    };
    if r.is_finite() {
        (r as f32).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// 字节数转人类可读文本（"14.58 MB" / "590 KB" / "3.2 GB" / "1.05 TB"）。
///
/// 命名与 `process_list::format_bytes` 区分开：那边是「速率/百分比」场景的既有实现，
/// 本窗口按累计流量的量级重新定精度（>100 保留 1 位、否则 2 位，再去掉多余的 0）。
fn format_traffic_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{} B", bytes);
    }
    let mut v = bytes as f64;
    let mut unit = 0usize;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    let text = if v >= 100.0 {
        format!("{:.1}", v)
    } else {
        format!("{:.2}", v)
    };
    format!("{} {}", trim_trailing_zeros(&text), UNITS[unit])
}

/// 去掉小数末尾多余的 0（"590.0" → "590"，"3.20" → "3.2"，"1.05" 不变）。
fn trim_trailing_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    let t = s.trim_end_matches('0').trim_end_matches('.');
    if t.is_empty() {
        "0".to_string()
    } else {
        t.to_string()
    }
}

/// 日历格右上角的紧凑流量文本（"1.0T" / "12G" / "3.2G" / "590M" / "12K"）。
///
/// 格子宽约 90px，只放得下 4~5 个字符；无数据（0 字节）返回空串，由 slint 侧不画。
/// 精度取舍：TB 恒 1 位小数；GB 小于 10 保留 1 位（"3.2G"），否则取整（"12G"）；
/// MB / KB / B 取整——避免同一列里数字长短参差。
fn format_cal_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return String::new();
    }
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    let v = bytes as f64;
    if bytes >= TIB {
        format!("{:.1}T", v / TIB as f64)
    } else if bytes >= GIB {
        let g = v / GIB as f64;
        if g < 10.0 {
            format!("{:.1}G", g)
        } else {
            format!("{:.0}G", g)
        }
    } else if bytes >= MB {
        format!("{:.0}M", v / MB as f64)
    } else if bytes >= KB {
        format!("{:.0}K", v / KB as f64)
    } else {
        format!("{bytes}B")
    }
}

/// 流量档位（0..4；`total == 0` 视为无数据 → -1）。
fn traffic_level(total: u64) -> i32 {
    if total == 0 {
        return -1;
    }
    if total < GIB {
        0
    } else if total < 10 * GIB {
        1
    } else if total < 100 * GIB {
        2
    } else if total < TIB {
        3
    } else {
        4
    }
}

/// 生成 42 个日历格子（周日为每行第一列；首个格子是当月 1 号所在周的周日）。
///
/// 同一天若有多条（正常不会发生）取和；非本月格子不着色（`level = -1`）。
fn build_calendar(year: i32, month: u32, daily: &[DailyTraffic]) -> Vec<crate::CalCell> {
    use std::collections::HashMap;

    let mut by_day: HashMap<NaiveDate, u64> = HashMap::new();
    for d in daily {
        if d.date.year() == year && d.date.month() == month {
            let slot = by_day.entry(d.date).or_insert(0);
            *slot = slot.saturating_add(d.sent.saturating_add(d.recv));
        }
    }

    let Some(first) = NaiveDate::from_ymd_opt(year, month, 1) else {
        return Vec::new();
    };
    // 周日为一周第一列：1 号距离所在周的周日还有几天
    let offset = first.weekday().num_days_from_sunday() as i64;
    let start = first - ChronoDuration::days(offset);

    let mut cells = Vec::with_capacity(42);
    let today = Local::now().date_naive();
    for i in 0..42i64 {
        let date = start + ChronoDuration::days(i);
        let in_month = date.year() == year && date.month() == month;
        // 本月内取当日总量（无样本 = 0），档位与右上角紧凑流量都由它派生
        let total = if in_month {
            by_day.get(&date).copied().unwrap_or(0)
        } else {
            0
        };
        let level = if in_month { traffic_level(total) } else { -1 };
        cells.push(crate::CalCell {
            day: date.day() as i32,
            level,
            in_month,
            // 今天只在当前月份里出现，跨月格子不必描边
            is_today: in_month && date == today,
            total_text: if in_month {
                SharedString::from(format_cal_bytes(total))
            } else {
                SharedString::new()
            },
        });
    }
    cells
}

/// 某月（本地日期）的累计流量：(上传, 下载, 是否有数据)。
fn month_totals(year: i32, month: u32, daily: &[DailyTraffic]) -> (u64, u64, bool) {
    let mut up = 0u64;
    let mut down = 0u64;
    let mut has = false;
    for d in daily {
        if d.date.year() == year && d.date.month() == month {
            has = true;
            up = up.saturating_add(d.sent);
            down = down.saturating_add(d.recv);
        }
    }
    (up, down, has)
}

/// 重算并写入两个视图的全部属性（纯内存，UI 线程可放心调用）。
fn recompute(
    ui: &crate::TrafficStats,
    daily: &[DailyTraffic],
    stats_mode: i32,
    scale_mode: i32,
    month: (i32, u32),
) {
    // === 列表视图 ===
    let agg = aggregate(daily, stats_mode);
    let max = agg.iter().map(|r| r.total()).max().unwrap_or(0);
    // 对数比例的下界：数据中最小的非零总量（0 值条恒为 0，不参与参照）
    let min_nz = agg
        .iter()
        .map(|r| r.total())
        .filter(|v| *v > 0)
        .min()
        .unwrap_or(0);
    let log_scale = scale_mode == 0;
    let total_up: u64 = agg.iter().fold(0u64, |a, r| a.saturating_add(r.up));
    let total_down: u64 = agg.iter().fold(0u64, |a, r| a.saturating_add(r.down));
    let total = total_up.saturating_add(total_down);

    let rows: Vec<crate::TrafficRow> = agg
        .iter()
        .map(|r| crate::TrafficRow {
            label: SharedString::from(r.label.clone()),
            up: SharedString::from(format_traffic_bytes(r.up)),
            down: SharedString::from(format_traffic_bytes(r.down)),
            total: SharedString::from(format_traffic_bytes(r.total())),
            ratio: bar_ratio(r.total(), min_nz, max, log_scale),
        })
        .collect();

    ui.set_has_data(!rows.is_empty());
    ui.set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
    // 汇总拆两段：主行（条数 + 累计）加粗，明细（上传 / 下载）弱化
    ui.set_summary_main(SharedString::from(format!(
        "共 {} 条 · 累计 {}",
        agg.len(),
        format_traffic_bytes(total)
    )));
    ui.set_summary_sub(SharedString::from(format!(
        "上传 {} · 下载 {}",
        format_traffic_bytes(total_up),
        format_traffic_bytes(total_down)
    )));

    // === 日历视图 ===
    ui.set_cells(ModelRc::from(Rc::new(VecModel::from(build_calendar(
        month.0, month.1, daily,
    )))));
    ui.set_month_text(SharedString::from(format!("{} 年 {} 月", month.0, month.1)));
    let (m_up, m_down, has_month) = month_totals(month.0, month.1, daily);
    if has_month {
        ui.set_cal_summary_main(SharedString::from(format!(
            "本月总流量 {}",
            format_traffic_bytes(m_up.saturating_add(m_down))
        )));
        ui.set_cal_summary_sub(SharedString::from(format!(
            "上传 {} · 下载 {}",
            format_traffic_bytes(m_up),
            format_traffic_bytes(m_down)
        )));
    } else {
        ui.set_cal_summary_main(SharedString::from("本月暂无数据"));
        ui.set_cal_summary_sub(SharedString::new());
    }
}

/// 皮肤注入：让窗口外框跟随 skin.toml（与 Analysis / ProcessList 一致）。
fn apply_skin(ui: &crate::TrafficStats) {
    let config = find_stutter_core::Config::load("config.toml")
        .unwrap_or_else(|_| find_stutter_core::Config::default());
    let skin = crate::skin::SkinConfig::load(&config.ui.skin);
    ui.set_skin_bg(Brush::SolidColor(
        parse_skin_color(&skin.background_color).unwrap_or(Color::from_rgb_u8(0xff, 0xff, 0xff)),
    ));
    ui.set_skin_border_color(
        parse_skin_color(&skin.border_color).unwrap_or(Color::from_rgb_u8(0xc0, 0xc0, 0xc0)),
    );
    ui.set_skin_border_radius(skin.border_radius as f32);
    // 文字色与内部面板固定为浅色中性色（与 Analysis 内部区域一致）
    ui.set_skin_text_color(Color::from_rgb_u8(0x1e, 0x1e, 0x2e));
    ui.set_skin_subtext_color(Color::from_rgb_u8(0x60, 0x60, 0x66));
    ui.set_skin_panel_bg(Color::from_rgb_u8(0xfa, 0xfb, 0xfc));
    ui.set_skin_panel_border(Color::from_rgb_u8(0xe2, 0xe4, 0xea));
}

/// 解析 `#RRGGBB` 皮肤色为 slint `Color`（与 overlay / analysis 同款逻辑）。
fn parse_skin_color(s: &str) -> Option<Color> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::from_rgb_u8(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32, sent: u64, recv: u64) -> DailyTraffic {
        DailyTraffic {
            date: NaiveDate::from_ymd_opt(y, m, d).unwrap(),
            sent,
            recv,
        }
    }

    // === 字节格式化 ===

    #[test]
    fn format_bytes_below_1k_is_integer_b() {
        assert_eq!(format_traffic_bytes(0), "0 B");
        assert_eq!(format_traffic_bytes(1), "1 B");
        assert_eq!(format_traffic_bytes(1023), "1023 B");
    }

    #[test]
    fn format_bytes_units_and_precision() {
        assert_eq!(format_traffic_bytes(1024), "1 KB");
        assert_eq!(format_traffic_bytes(590 * 1024), "590 KB");
        assert_eq!(format_traffic_bytes(15288238), "14.58 MB");
        assert_eq!(format_traffic_bytes(3435973837), "3.2 GB");
        assert_eq!(format_traffic_bytes(1154487209165), "1.05 TB");
        // 超过 TB 仍以 TB 为单位，不越界
        assert_eq!(format_traffic_bytes(u64::MAX), "16777216 TB");
    }

    /// 日历格右上角的紧凑文本（4~5 字符以内，无数据为空串）。
    #[test]
    fn format_cal_bytes_is_compact() {
        let gb = 1024 * 1024 * 1024u64;
        assert_eq!(format_cal_bytes(0), "");
        assert_eq!(format_cal_bytes(512), "512B");
        assert_eq!(format_cal_bytes(12 * 1024), "12K");
        assert_eq!(format_cal_bytes(590 * 1024 * 1024), "590M");
        assert_eq!(format_cal_bytes(3 * gb + gb / 5), "3.2G");
        assert_eq!(format_cal_bytes(12 * gb), "12G");
        assert_eq!(format_cal_bytes(1024 * gb), "1.0T");
    }

    // === 聚合 ===

    /// 周聚合：跨月同属一个 ISO 周（2026-11-01 是周日，与 2026-10-31 同属第 44 周）
    /// 必须合并，且周一（2026-11-02）另起一周。
    #[test]
    fn weekly_aggregates_cross_month_same_iso_week() {
        let data = vec![
            day(2026, 10, 31, 100, 200),  // 周六，ISO 第 44 周
            day(2026, 11, 1, 1, 2),       // 周日，仍是第 44 周
            day(2026, 11, 2, 10, 20),     // 周一，第 45 周（周一起算）
        ];
        let rows = aggregate(&data, 1);
        assert_eq!(rows.len(), 2, "跨月的同一 ISO 周必须合并成一个桶");
        // 降序：最新一周在上
        assert_eq!(rows[0].label, "2026/第 45 周");
        assert_eq!(rows[0].sort_date, NaiveDate::from_ymd_opt(2026, 11, 2).unwrap());
        assert_eq!(rows[0].up, 10);
        assert_eq!(rows[0].down, 20);
        assert_eq!(rows[1].label, "2026/第 44 周");
        assert_eq!(rows[1].sort_date, NaiveDate::from_ymd_opt(2026, 10, 26).unwrap());
        assert_eq!(rows[1].up, 101, "10-31 与 11-01 应相加");
        assert_eq!(rows[1].down, 202);
    }

    /// 月聚合：按年月分桶，标签 "YYYY/M"，降序。
    #[test]
    fn monthly_aggregates_by_year_month() {
        let data = vec![
            day(2026, 10, 31, 5, 6),
            day(2026, 10, 1, 1, 1),
            day(2026, 11, 1, 7, 8),
        ];
        let rows = aggregate(&data, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "2026/11");
        assert_eq!(rows[0].up, 7);
        assert_eq!(rows[0].down, 8);
        assert_eq!(rows[0].total(), 15);
        assert_eq!(rows[1].label, "2026/10");
        assert_eq!(rows[1].up, 6);
        assert_eq!(rows[1].down, 7);
    }

    /// 日聚合：一条一天不变形，降序排列。
    #[test]
    fn daily_aggregates_per_date_desc() {
        let data = vec![day(2026, 11, 1, 1, 1), day(2026, 11, 3, 3, 4), day(2026, 11, 2, 2, 2)];
        let rows = aggregate(&data, 0);
        assert_eq!(rows.len(), 3);
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["2026-11-03", "2026-11-02", "2026-11-01"]);
        assert_eq!(rows[0].total(), 7);
    }

    // === 比例 ===

    #[test]
    fn ratio_linear_and_zero_safe() {
        // 线性分支不使用 min_nz（传 0 表示无参照）
        assert_eq!(bar_ratio(0, 0, 100, false), 0.0);
        assert_eq!(bar_ratio(50, 0, 100, false), 0.5);
        assert_eq!(bar_ratio(100, 0, 100, false), 1.0);
        // max = 0 不 panic，任何值都给 0
        assert_eq!(bar_ratio(0, 0, 0, false), 0.0);
        assert_eq!(bar_ratio(123, 0, 0, false), 0.0);
        assert_eq!(bar_ratio(123, 0, 0, true), 0.0);
    }

    #[test]
    fn ratio_log_uses_min_nonzero_as_floor() {
        // 0 值恒为 0
        assert_eq!(bar_ratio(0, 10, 100, true), 0.0);
        // 最小非零值是对数下界（ratio 0），最大值是 1
        assert_eq!(bar_ratio(10, 10, 100, true), 0.0);
        assert_eq!(bar_ratio(100, 10, 100, true), 1.0);
        // 几何中点：min=10, max=100, v≈sqrt(10*100)≈32 → 对数正中 0.5
        let mid = bar_ratio(32, 10, 100, true);
        assert!((mid - 0.5).abs() < 0.02, "对数中点应约 0.5，实测 {mid}");
        // 单调
        let a = bar_ratio(10, 10, 100, true);
        let b = bar_ratio(31, 10, 100, true);
        let c = bar_ratio(100, 10, 100, true);
        assert!(a == 0.0 && a < b && b < c, "对数比例必须单调递增");
        // 非零值全相等 → 1.0（都是同一量级，条形等长合理）
        assert_eq!(bar_ratio(50, 50, 50, true), 1.0);
        // 超出 max 的值 clamp 到 1
        assert_eq!(bar_ratio(200, 10, 100, true), 1.0);
        // 真实场景回归：7~753 GB 的日流量在对数下必须有明显梯度
        // （旧实现 ln(1+v)/ln(1+max) 会把全部条压到 0.74~1.0，视觉等长）
        let gb = 1024.0 * 1024.0 * 1024.0;
        let small = (7.21 * gb) as u64;
        let big = (753.2 * gb) as u64;
        let mid_v = (58.34 * gb) as u64;
        let r_small = bar_ratio(small, small, big, true);
        let r_mid = bar_ratio(mid_v, small, big, true);
        assert_eq!(r_small, 0.0, "最小值应贴 0");
        assert_eq!(bar_ratio(big, small, big, true), 1.0);
        assert!(
            r_mid > 0.25 && r_mid < 0.75,
            "中间量级应有明显梯度，实测 {r_mid}"
        );
    }

    // === 日历 ===

    /// 固定用例：2026-11-01 是周日 → 格子 0 必须是 1 号本身。
    #[test]
    fn calendar_nov_2026_starts_on_first() {
        let cells = build_calendar(2026, 11, &[]);
        assert_eq!(cells.len(), 42);
        assert_eq!(cells[0].day, 1);
        assert!(cells[0].in_month, "1 号所在格必须属于本月");
        // 2026-11 有 30 天：0..29 为本月，30..41 为下月
        assert!(cells[29].in_month);
        assert_eq!(cells[29].day, 30);
        assert!(!cells[30].in_month);
        assert_eq!(cells[30].day, 1, "第 31 格是 12 月 1 日");
        assert_eq!(cells.len(), 42);
        let in_month = cells.iter().filter(|c| c.in_month).count();
        assert_eq!(in_month, 30);
        // 每行 7 格：第 1 列都是周日
        assert_eq!(cells[0].day, 1);
        assert_eq!(cells[7].day, 8);
        assert_eq!(cells[14].day, 15);
    }

    /// 偏移正确：2026-10-01 是周四 → 前 4 格是 9 月的周日..周三，格子 4 才是 1 号。
    #[test]
    fn calendar_offset_for_month_starting_thursday() {
        let cells = build_calendar(2026, 10, &[]);
        assert_eq!(cells.len(), 42);
        assert!(!cells[0].in_month);
        assert_eq!(cells[0].day, 27, "首格是 9 月 27 日（周日）");
        assert_eq!(cells[4].day, 1);
        assert!(cells[4].in_month);
        // 末尾跨到下月
        assert!(!cells[41].in_month);
    }

    /// 档位：无样本 -1，1.5GB → 1，总和按上传+下载计。
    #[test]
    fn calendar_levels_by_total_traffic() {
        let gb = 1024 * 1024 * 1024;
        let data = vec![
            day(2026, 11, 5, gb / 2, gb),        // 1.5GB → level 1
            day(2026, 11, 6, 0, 0),               // 有样本但 0 字节 → 视为无数据
            day(2026, 11, 7, 150 * gb, 0),        // 150GB → level 3
            day(2026, 11, 8, 2048 * gb, 0),       // 2TB → level 4
        ];
        let cells = build_calendar(2026, 11, &data);
        let find = |d: i32| {
            cells
                .iter()
                .find(|c| c.in_month && c.day == d)
                .cloned()
                .unwrap()
        };
        assert_eq!(find(5).level, 1);
        assert_eq!(find(7).level, 3);
        assert_eq!(find(8).level, 4);
        assert_eq!(find(6).level, -1, "0 字节按无数据处理");
        assert_eq!(find(9).level, -1, "无样本按无数据处理");
        // 非本月一律 -1（slint 侧据此不着色）
        assert!(cells.iter().filter(|c| !c.in_month).all(|c| c.level == -1));
    }

    #[test]
    fn month_summary_totals() {
        let data = vec![day(2026, 11, 1, 10, 20), day(2026, 11, 2, 1, 2), day(2026, 10, 5, 9, 9)];
        let (up, down, has) = month_totals(2026, 11, &data);
        assert!(has);
        assert_eq!(up, 11);
        assert_eq!(down, 22);
        // 没有数据的月份
        let (_, _, has_empty) = month_totals(2026, 9, &data);
        assert!(!has_empty);
    }

    #[test]
    fn shift_month_crosses_year_boundaries() {
        assert_eq!(shift_month((2026, 11), -1), (2026, 10));
        assert_eq!(shift_month((2026, 12), 1), (2027, 1));
        assert_eq!(shift_month((2026, 1), -1), (2025, 12));
        assert_eq!(shift_month((2026, 11), 0), (2026, 11));
    }
}
