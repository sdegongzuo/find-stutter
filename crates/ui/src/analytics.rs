//! 卡顿分析页只读查询层（PRD §6.1 / §7 / M2）。
//!
//! 全程只读 `stutter.db`：不写库、不改 service、不新增任何采集逻辑。
//! 所有聚合查询都带 `WHERE timestamp BETWEEN ? AND ?` 时间范围，并依赖
//! `idx_samples_ts` / `idx_events_ts` 时间戳索引（首次打开时幂等创建）。
//!
//! ## 时区口径（PRD §3.3）
//!
//! - `timestamp` 落库是 UTC RFC3339。
//! - KPI「今日卡顿 N 次」**必须与悬浮窗 `event_count_today` 完全一致**：
//!   两者共用核心单一来源 `local_today_bounds_utc()`（本地零点→现在，BETWEEN UTC 边界，
//!   见 `crates/core/src/logger.rs`），分析页 `load_kpi_today` / `TimeRange::Today` 都走它，
//!   任何一处「今日卡顿 N 次」口径都一致，用户不会困惑。
//! - 趋势分桶按**本地时区**：`strftime('%Y-%m-%d %H:00', datetime(timestamp,'localtime'))`，
//!   否则 UTC+8 用户会整体偏移 8 小时。
//! - KPI「高峰时段 HH:00」取自今日本地时区分桶后的最高桶。

use std::path::Path;

use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};
use find_stutter_core::ProcessBrief;
use rusqlite::{params, Connection, OpenFlags};

/// 时间范围选择器（PRD F7）。
///
/// - `Today`：今日（本地时区零点，与 `local_today_bounds_utc` 对齐）
/// - `Last7` / `Last30`：近 7 / 30 天（按 UTC 当前时刻往前推）
/// - `Custom(from, to)`：自定义 RFC3339 区间
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeRange {
    Today,
    Last7,
    Last30,
    Custom(String, String),
}

impl TimeRange {
    /// 返回 SQL `BETWEEN` 的 (start, end) RFC3339 边界。
    /// `end` 总是「现在」（含当前进行中的卡顿）；`start` 按范围推导。
    pub fn bounds(&self) -> (String, String) {
        let now = Utc::now();
        let start = match self {
            TimeRange::Today => {
                // 今日「本地」零点对应的 UTC 时刻：与悬浮窗 event_count_today 共用
                // 单一来源 logger::local_today_bounds_utc()，保证「今日」范围与今日计数口径一致。
                find_stutter_core::logger::local_today_bounds_utc().0
            }
            TimeRange::Last7 => now - ChronoDuration::days(7),
            TimeRange::Last30 => now - ChronoDuration::days(30),
            TimeRange::Custom(from, _) => {
                // 解析失败回退到今日起点，避免查询异常
                DateTime::parse_from_rfc3339(from)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc())
            }
        };
        let end = match self {
            TimeRange::Custom(_, to) => DateTime::parse_from_rfc3339(to)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or(now),
            _ => now,
        };
        (start.to_rfc3339(), end.to_rfc3339())
    }

    /// 把下拉索引（0今日/1近7天/2近30天/3自定义）转成范围。
    pub fn from_index(idx: i32) -> Self {
        match idx {
            1 => TimeRange::Last7,
            2 => TimeRange::Last30,
            3 => TimeRange::Custom(String::new(), String::new()), // 自定义：M3 接管具体区间
            _ => TimeRange::Today,
        }
    }

    /// 范围的中文标签（用于基础模式结论文案，如「今日最大的卡顿元凶」）。
    pub fn label(&self) -> &'static str {
        match self {
            TimeRange::Today => "今日",
            TimeRange::Last7 => "近7天",
            TimeRange::Last30 => "近30天",
            TimeRange::Custom(..) => "所选时段",
        }
    }
}

/// 一个时间桶的聚合结果（PRD F1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrendPoint {
    /// 本地时区分桶键：`YYYY-MM-DD HH:00`
    pub bucket: String,
    /// 该桶卡顿次数
    pub count: u32,
    /// 该桶卡顿累计时长（ms）
    pub total_ms: u64,
    /// 该桶 critical 次数
    pub critical: u32,
    /// 该桶 major 次数
    pub major: u32,
    /// 该桶 minor 次数
    pub minor: u32,
}

/// KPI 卡片汇总（基础模式核心，全部按「今日」口径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KpiSummary {
    /// 今日卡顿次数（对齐 event_count_today）
    pub today_count: u32,
    /// 今日最严重一次持续时长（ms）；无数据为 0
    pub worst_duration_ms: u64,
    /// 今日高峰时段 `HH:00`（本地时区）；无数据为 "—"
    pub peak_hour: String,
    /// 今日头号元凶进程名；无 culprits 时为 "—"
    pub top_culprit: String,
}

/// 打开一个只读 SQLite 连接（WAL 读视图，可并发于 service 写库）。
/// 与 `DbReader` 各自持有独立只读连接——WAL 模式下允许多读者，互不阻塞。
pub fn open_readonly(db_path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // 强制 WAL 读视图，确保读到未 checkpoint 的 WAL 数据
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL;");
    Ok(conn)
}

/// 幂等创建时间戳索引（PRD §3.3 / M2）。
///
/// 旧库 `stutter_events`/`samples` 无 timestamp 索引，按时间范围聚合会全表扫描；
/// 建一次即服务全期。
///
/// 注意：索引是 schema 级写操作。分析页严格「只读 stutter.db、不得写库」，连接以
/// `SQLITE_OPEN_READ_ONLY` 打开，因此 `CREATE INDEX` 必然失败（"attempt to write a
/// readonly database"）。这里对「只读」错误**优雅降级**为 `Ok`——不报错、不写库，
/// 查询自然退化为全表扫描（分析页数据量小，可接受）；索引只在连接可写时真正落地。
pub fn ensure_indexes(conn: &Connection) -> anyhow::Result<()> {
    match conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_samples_ts ON samples(timestamp); \
         CREATE INDEX IF NOT EXISTS idx_events_ts ON stutter_events(timestamp);",
    ) {
        Ok(()) => Ok(()),
        Err(e) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::ReadOnly) => {
            // 只读连接（分析页严格只读），无法落索引 → 降级全表扫描，不报错
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// F1：趋势图分桶粒度（高级模式可选，PRD §4 F1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendBucket {
    /// 按小时聚合（默认）
    Hour,
    /// 按 15 分钟聚合
    QuarterHour,
    /// 按天聚合
    Day,
}

impl From<&str> for TrendBucket {
    /// 由 slint 下拉文本映射；未知值一律回退 `Hour`，保证不崩。
    fn from(s: &str) -> Self {
        match s {
            "15 分钟" => TrendBucket::QuarterHour,
            "天" => TrendBucket::Day,
            _ => TrendBucket::Hour, // "小时" 或未知 → 默认小时
        }
    }
}

/// 时间趋势聚合（PRD F1 + §7 F1 草稿，改 localtime 分桶）。
///
/// - `bucket`：分桶粒度（详见 [`TrendBucket`]），高级模式由 slint 下拉选择传入。
/// - 返回按本地时区分桶的趋势点序列（已 ORDER BY bucket）。空范围返回空 Vec。
pub fn load_trend(
    conn: &Connection,
    range: &TimeRange,
    bucket: TrendBucket,
) -> anyhow::Result<Vec<TrendPoint>> {
    let (start, end) = range.bounds();
    // 分桶表达式按粒度变化（均为本地时区）；%M/15*15 把分钟归到 0/15/30/45 档。
    let bucket_expr = match bucket {
        TrendBucket::Hour => "strftime('%Y-%m-%d %H:00', datetime(timestamp, 'localtime'))",
        TrendBucket::QuarterHour => "strftime('%Y-%m-%d %H:', datetime(timestamp, 'localtime')) \
            || printf('%02d', (CAST(strftime('%M', datetime(timestamp, 'localtime')) AS INTEGER) / 15) * 15)",
        TrendBucket::Day => "strftime('%Y-%m-%d', datetime(timestamp, 'localtime'))",
    };
    let sql = format!(
        "SELECT {bucket_expr} AS bucket,
                COUNT(*)                                            AS cnt,
                COALESCE(SUM(duration_ms), 0)                       AS total_ms,
                SUM(CASE severity WHEN 'critical' THEN 1 ELSE 0 END) AS c_crit,
                SUM(CASE severity WHEN 'major'    THEN 1 ELSE 0 END) AS c_major,
                SUM(CASE severity WHEN 'minor'    THEN 1 ELSE 0 END) AS c_minor
         FROM stutter_events
         WHERE timestamp BETWEEN ?1 AND ?2
         GROUP BY bucket ORDER BY bucket"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![start, end], |row| {
        Ok(TrendPoint {
            bucket: row.get(0)?,
            count: row.get::<_, i64>(1)? as u32,
            total_ms: row.get::<_, i64>(2)? as u64,
            critical: row.get::<_, i64>(3)? as u32,
            major: row.get::<_, i64>(4)? as u32,
            minor: row.get::<_, i64>(5)? as u32,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// F2：单进程归因聚合（PRD §6.4 / M3）。
///
/// 把时间范围内所有事件的 `culprits` JSON 在内存中按进程 `name` 聚合
/// （聚合同名不同 PID），得到每个进程的：作为元凶出现次数、关联卡顿累计时长、
/// 最高单次 CPU 占用、最高单次内存占用。
#[derive(Debug, Clone, PartialEq)]
pub struct CulpritAgg {
    /// 进程名（同名不同 PID 已合并）
    pub name: String,
    /// 作为元凶出现在 N 次卡顿中
    pub count: u32,
    /// 关联卡顿累计时长（ms）
    pub total_duration_ms: u64,
    /// 最高单次 CPU 占用（%）
    pub max_cpu: f32,
    /// 最高单次内存占用（MB）
    pub max_mem_mb: u64,
}

/// F4：卡顿类型归类计数（PRD §6.4 路线 2 关键词归类）。
#[derive(Debug, Clone, PartialEq)]
pub struct CauseTypeCount {
    /// 类型中文名（如「网络突增」）
    pub cause_type: String,
    /// 该类型出现次数（一个事件若含多个 cause，按每个 cause 各计一次）
    pub count: u32,
    /// 占比（0..100），相对所有类型总计数
    pub percent: f32,
}

/// 探测某表是否存在某列（旧库兼容：P5 前 `stutter_events` 无 `culprits` 列）。
///
/// 失败时（表不存在/查询异常）一律回退 `false`，由调用方决定如何降级。
fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
    conn.prepare(&format!("PRAGMA table_info({})", table))
        .and_then(|mut stmt| {
            let names: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(names.iter().any(|n| n == column))
        })
        .unwrap_or(false)
}

/// F2：进程归因 Top N（PRD §6.4 / M3 + §7 F2 草稿）。
///
/// 读取时间范围内事件的 `culprits` JSON，按进程 `name` 聚合，取出现次数最高的
/// `limit` 个（同名不同 PID 合并）。
///
/// 容错：
/// - 旧库无 `culprits` 列 → 整体回退空 Vec（不崩、不写库）。
/// - 单行 JSON 为空/解析失败 → 跳过该行，继续其余。
pub fn load_culprits(
    conn: &Connection,
    range: &TimeRange,
    limit: usize,
) -> anyhow::Result<Vec<CulpritAgg>> {
    if !has_column(conn, "stutter_events", "culprits") {
        return Ok(Vec::new());
    }
    let (start, end) = range.bounds();
    let mut stmt = conn.prepare(
        "SELECT culprits, duration_ms FROM stutter_events \
         WHERE timestamp BETWEEN ?1 AND ?2 AND culprits IS NOT NULL",
    )?;
    let rows = stmt.query_map(params![start, end], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
    })?;

    use std::collections::HashMap;
    // name -> 聚合累加器
    let mut agg: HashMap<String, CulpritAgg> = HashMap::new();
    for r in rows {
        let (json, dur) = r?;
        let culprits: Vec<ProcessBrief> = serde_json::from_str(&json).unwrap_or_default();
        for c in culprits {
            let entry = agg.entry(c.name.clone()).or_insert_with(|| CulpritAgg {
                name: c.name.clone(),
                count: 0,
                total_duration_ms: 0,
                max_cpu: 0.0,
                max_mem_mb: 0,
            });
            entry.count += 1;
            entry.total_duration_ms += dur;
            entry.max_cpu = entry.max_cpu.max(c.cpu_usage);
            entry.max_mem_mb = entry.max_mem_mb.max(c.mem_used_mb);
        }
    }

    let mut out: Vec<CulpritAgg> = agg.into_values().collect();
    // 排序：出现次数降序；并列时累计时长降序
    out.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then(b.total_duration_ms.cmp(&a.total_duration_ms))
    });
    out.truncate(limit);
    Ok(out)
}

/// F4：把单条 cause 文案归类成类型名（PRD §6.4 路线 2）。
///
/// 关键词表严格依据 `detector.rs` 当前实际产出的文案（见 `check_hard_thresholds`
/// 与 `check_spike` / `cause_key`）：
/// - `"CPU usage ..."`            → CPU 过高（硬阈值）
/// - `"CPU spike: ..."`           → CPU 突增（spike）
/// - `"Disk write spike: ..."`    → 磁盘突增
/// - `"Network spike: ..."`       → 网络突增
/// - `"Memory available spike: ..."` → 内存骤降
/// - `"Memory usage ..."`          → 内存过高（硬阈值，使用率百分比口径；与
///   `detector.rs` 的 `mem_threshold_percent` 分支对应，覆盖大内存机器漏报）
/// - `"Available memory ..."`     → 内存不足（硬阈值，绝对可用 MB 口径）
/// - `"Commit charge ..."`        → 提交电荷（commit charge 压力，比可用内存更早预警）
/// - `"Memory paging ..."`        → 内存分页（Page Reads/sec 换页抖动，真正的 swap 卡顿信号）
/// - 其它                         → 其他
///
/// ⚠️ 粗糙归类：依赖检测器文案，文案一改可能漂移（PRD §6.4 风险）。
pub fn classify_cause(cause: &str) -> &'static str {
    if cause.starts_with("CPU usage") {
        "CPU 过高"
    } else if cause.starts_with("CPU spike") {
        "CPU 突增"
    } else if cause.starts_with("Disk write") {
        "磁盘突增"
    } else if cause.starts_with("Network") {
        "网络突增"
    } else if cause.starts_with("Memory available") {
        "内存骤降"
    } else if cause.starts_with("Memory usage") {
        "内存过高"
    } else if cause.starts_with("Available memory") {
        "内存不足"
    } else if cause.starts_with("Commit charge") {
        "提交电荷"
    } else if cause.starts_with("Memory paging") {
        "内存分页"
    } else {
        "其他"
    }
}

/// F4：卡顿类型占比（PRD §6.4 / M3 + §7 F4 草稿）。
///
/// 读取时间范围内事件的 `causes` 数组，逐条按关键词归类计数，返回各类型计数与占比。
///
/// 容错：
/// - `causes` 列缺失（极端旧库）→ 回退空 Vec。
/// - 单行 JSON 为空/解析失败 → 跳过该行。
pub fn load_cause_types(
    conn: &Connection,
    range: &TimeRange,
) -> anyhow::Result<Vec<CauseTypeCount>> {
    if !has_column(conn, "stutter_events", "causes") {
        return Ok(Vec::new());
    }
    let (start, end) = range.bounds();
    let mut stmt = conn.prepare(
        "SELECT causes FROM stutter_events \
         WHERE timestamp BETWEEN ?1 AND ?2 AND causes IS NOT NULL",
    )?;
    let rows = stmt.query_map(params![start, end], |row| row.get::<_, String>(0))?;

    use std::collections::HashMap;
    let mut tally: HashMap<&'static str, u32> = HashMap::new();
    for r in rows {
        let json = r?;
        let causes: Vec<String> = serde_json::from_str(&json).unwrap_or_default();
        for c in causes {
            *tally.entry(classify_cause(&c)).or_insert(0) += 1;
        }
    }

    let total: u32 = tally.values().sum();
    let mut out: Vec<CauseTypeCount> = if total == 0 {
        Vec::new()
    } else {
        tally
            .into_iter()
            .map(|(cause_type, count)| CauseTypeCount {
                cause_type: cause_type.to_string(),
                count,
                percent: count as f32 / total as f32 * 100.0,
            })
            .collect()
    };
    // 按计数降序
    out.sort_by(|a, b| b.count.cmp(&a.count));
    Ok(out)
}

/// 今日 KPI 汇总（基础模式 4 卡片）。
///
/// 全部按「今日」口径：today_count 与悬浮窗 `event_count_today` 共用
/// `local_today_bounds_utc()`（本地零点 → 现在），保证两处「今日卡顿 N 次」一致。
pub fn load_kpi_today(conn: &Connection) -> anyhow::Result<KpiSummary> {
    // 今日范围（本地零点 → 现在）；今日计数与范围聚合共用同一边界，保证口径一致。
    let (start, end) = TimeRange::Today.bounds();

    // 1) 今日次数：与 reader.event_count_today / logger.event_count_today 一致
    //    （本地零点 → 现在，BETWEEN UTC 边界）
    let today_count: u32 = conn
        .query_row(
            "SELECT COUNT(*) FROM stutter_events WHERE timestamp BETWEEN ?1 AND ?2",
            params![start, end],
            |row| row.get::<_, i64>(0).map(|n| n as u32),
        )
        .unwrap_or(0);

    // 2) 今日最严重一次持续时长
    let worst_duration_ms: u64 = conn
        .query_row(
            "SELECT COALESCE(MAX(duration_ms), 0) FROM stutter_events \
             WHERE timestamp BETWEEN ?1 AND ?2",
            params![start, end],
            |row| row.get::<_, i64>(0).map(|n| n as u64),
        )
        .unwrap_or(0);

    // 3) 高峰时段：今日本地时区分桶取次数最多桶的 HH:00
    let peak_hour = {
        let trend = load_trend(conn, &TimeRange::Today, TrendBucket::Hour).unwrap_or_default();
        trend
            .iter()
            .max_by_key(|p| p.count)
            .and_then(|p| {
                // bucket 形如 "YYYY-MM-DD HH:00"，取空格后 "HH:00"
                p.bucket.split(' ').nth(1).map(|h| h.to_string())
            })
            .unwrap_or_else(|| "—".to_string())
    };

    // 4) 头号元凶：今日事件 culprits 按进程名计数取 Top1（复用 F2 聚合）。
    //    旧库无 culprits 列 → load_culprits 回退空 → 取 "—"。
    let top_culprit = load_culprits(conn, &TimeRange::Today, 1)
        .ok()
        .and_then(|mut v| v.pop())
        .map(|c| c.name)
        .unwrap_or_else(|| "—".to_string());

    Ok(KpiSummary {
        today_count,
        worst_duration_ms,
        peak_hour,
        top_culprit,
    })
}

/// 把时长（ms）格式化为中文可读文本（如 "3.5s" / "1.2min"）。
/// 供 KPI「最严重一次」卡片显示。
pub fn format_duration(ms: u64) -> String {
    if ms == 0 {
        return "—".to_string();
    }
    let secs = ms as f64 / 1000.0;
    if secs < 60.0 {
        format!("{:.1}s", secs)
    } else {
        format!("{:.1}min", secs / 60.0)
    }
}

// ===================== M4 / F3：系统资源关联 =====================

/// F3：降采样后的单个时间桶资源聚合点（PRD §6.3 降采样必做）。
///
/// 降采样策略选型：**SQL 层按时间桶聚合（PRD §6.3 选项 (a)）**。
/// 理由：1Hz 采样下「今日」86400 行、「近30天」259 万行，若把原始点直接喂
/// plotters 会卡 UI 线程且浪费内存。故在 SQL 里按 `bucket_secs` 分桶
/// （`(unixepoch(timestamp)-基准)/bucket_secs` 整数除法），每桶取
/// `AVG`/`MIN`/`MAX`；桶数 = 显示宽度同量级（200~2000），与 PRD「几百~几千点」一致。
/// 这样后台线程只把「桶数」个点读回内存，内存与渲染开销恒定，不随范围增长。
///
/// 量纲处理（PRD F3 多轴/归一化）：CPU%/内存% 落在左轴 0-100；磁盘读/写 B/s
/// 量纲与 % 差几个数量级，单独落在右轴（由 chart 渲染时按 max 自适应），
/// 不再归一到左轴以免淹没 % 曲线。GPU%（可选，可为 NULL）落在左轴（同 %）。
#[derive(Debug, Clone, PartialEq)]
pub struct ResourcePoint {
    /// 桶序号（0..桶数），作为 X 轴等距坐标
    pub x: i64,
    /// 桶代表时刻（unix epoch 秒 = base + x*bucket_secs）
    pub ts_secs: i64,
    pub cpu_avg: f32,
    pub cpu_min: f32,
    pub cpu_max: f32,
    pub mem_avg: f32,
    pub mem_min: f32,
    pub mem_max: f32,
    pub disk_read_avg: f64,
    pub disk_read_min: f64,
    pub disk_read_max: f64,
    pub disk_write_avg: f64,
    pub disk_write_min: f64,
    pub disk_write_max: f64,
    pub gpu_avg: Option<f32>,
}

/// F3：资源曲线渲染所需的全部数据（采样降采样结果 + 卡顿事件竖线位置）。
#[derive(Debug, Clone)]
pub struct ResourceData {
    pub points: Vec<ResourcePoint>,
    /// 卡顿事件对应的桶序号（已 clamp 到 [0, 桶数-1]），用于画竖线标记
    pub event_x: Vec<i64>,
    /// 范围起点 epoch 秒（桶基准）
    pub base_secs: i64,
    /// 每桶秒数
    pub bucket_secs: i64,
    /// 范围总秒数
    pub span_secs: i64,
}

impl ResourceData {
    /// 磁盘读/写 B/s 的最大值（用于右轴量程；空数据返回 1 避免除零）
    pub fn max_disk(&self) -> f64 {
        self.points
            .iter()
            .map(|p| p.disk_read_avg.max(p.disk_write_avg))
            .fold(0.0f64, f64::max)
            .max(1.0)
    }
    /// X 轴桶序号 → 本地时区时间标签（短格式，跨天范围显示月-日）
    pub fn x_label(&self, x: i64) -> String {
        use chrono::TimeZone;
        let secs = self.base_secs + x * self.bucket_secs;
        let dt = Utc.timestamp_opt(secs, 0).single().unwrap_or_else(Utc::now);
        let local = dt.with_timezone(&Local);
        if self.span_secs <= 2 * 86400 {
            local.format("%H:%M").to_string()
        } else {
            local.format("%m-%d %H:%M").to_string()
        }
    }
}

/// F3（高级）：单次卡顿事件的资源快照（事件瞬间前后采样窗口的均值，PRD §4 F3）。
#[derive(Debug, Clone, PartialEq)]
pub struct EventSnapshot {
    /// CPU 平均使用率 %
    pub cpu: f32,
    /// 内存平均使用率 %
    pub mem: f32,
    /// 磁盘读平均速率 B/s
    pub disk_read: f64,
    /// 磁盘写平均速率 B/s
    pub disk_write: f64,
    /// GPU 平均利用率 %（可能为空）
    pub gpu: Option<f32>,
}

/// F3（高级）：读取某次卡顿时刻前后 [ts-3s, ts+3s] 的采样均值，作为该次卡顿的资源快照。
///
/// 全程只读 `stutter.db`。优先取窗口内 `AVG`；窗口内无采样（如事件落点附近无 1Hz 数据）
/// 时回退取「最接近 ts 的一条」采样（PRD §4 F3 高级模式 hover 看事件点资源状态）。
pub fn load_event_snapshot(conn: &Connection, ts_secs: i64) -> Option<EventSnapshot> {
    let lo = ts_secs - 3;
    let hi = ts_secs + 3;
    // 1) 窗口内 AVG
    let avg = conn
        .query_row(
            "SELECT AVG(cpu_usage), AVG(mem_usage_percent), AVG(disk_read_bps),
                    AVG(disk_write_bps), AVG(gpu_usage)
             FROM samples
             WHERE CAST(strftime('%s', timestamp) AS INTEGER) BETWEEN ?1 AND ?2",
            params![lo, hi],
            |row| {
                Ok((
                    row.get::<_, Option<f64>>(0)?,
                    row.get::<_, Option<f64>>(1)?,
                    row.get::<_, Option<f64>>(2)?,
                    row.get::<_, Option<f64>>(3)?,
                    row.get::<_, Option<f64>>(4)?,
                ))
            },
        )
        .ok();
    if let Some((Some(c), Some(m), Some(dr), Some(dw), gpu)) = avg {
        return Some(EventSnapshot {
            cpu: c as f32,
            mem: m as f32,
            disk_read: dr,
            disk_write: dw,
            gpu: gpu.map(|v| v as f32),
        });
    }
    // 2) 回退：取最接近 ts 的一条采样
    let nearest = conn
        .query_row(
            "SELECT cpu_usage, mem_usage_percent, disk_read_bps, disk_write_bps, gpu_usage
             FROM samples
             ORDER BY ABS(CAST(strftime('%s', timestamp) AS INTEGER) - ?1) ASC
             LIMIT 1",
            params![ts_secs],
            |row| {
                Ok((
                    row.get::<_, f64>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, Option<f64>>(4)?,
                ))
            },
        )
        .ok();
    nearest.map(|(c, m, dr, dw, gpu)| EventSnapshot {
        cpu: c as f32,
        mem: m as f32,
        disk_read: dr,
        disk_write: dw,
        gpu: gpu.map(|v| v as f32),
    })
}

/// F3（高级）：资源图可选显示指标 + 对数轴视图（PRD §4 F3 / §5）。
///
/// 由 slint 端 5 个 CheckBox（cpu/mem/disk_read/disk_write/gpu）与「对数轴（磁盘）」
/// CheckBox 驱动；`render_resource_chart` 据此决定绘制哪些系列、磁盘轴是否取对数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceView {
    pub cpu: bool,
    pub mem: bool,
    pub disk_read: bool,
    pub disk_write: bool,
    pub gpu: bool,
    /// 磁盘读/写 B/s 改用对数归一（数量级悬殊的尖峰可见）
    pub log_disk: bool,
}

impl Default for ResourceView {
    /// 默认全部指标勾选、对数轴关闭（与 slint 端 CheckBox 默认态一致）。
    fn default() -> Self {
        ResourceView {
            cpu: true,
            mem: true,
            disk_read: true,
            disk_write: true,
            gpu: true,
            log_disk: false,
        }
    }
}

/// F3：读取时间范围内的 `samples` 并按显示宽度降采样（PRD §6.3 / §7 F3 草稿）。
///
/// - `width_px`：资源图显示像素宽度，决定桶数（PRD「按显示像素宽度降采样」）。
///   桶数 clamp 到 200~2000，再据范围总长算出 `bucket_secs`。
/// - 同时取同范围 `stutter_events.timestamp`（事件数少）折算为桶序号，作为竖线标记。
/// - 全程只读；空范围/无 samples → 返回 `points` 为空的 `ResourceData`（图表端占位）。
pub fn load_resource_samples(
    conn: &Connection,
    range: &TimeRange,
    width_px: u32,
) -> anyhow::Result<ResourceData> {
    let (start, end) = range.bounds();
    let base_dt = DateTime::parse_from_rfc3339(&start)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    let end_dt = DateTime::parse_from_rfc3339(&end)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    let base_secs = base_dt.timestamp();
    let span_secs = (end_dt.timestamp() - base_secs).max(1);
    // 桶数：与显示宽度同量级（PRD「几百~几千点」），clamp 200~2000
    let num_buckets = (width_px as i64).clamp(200, 2000);
    let bucket_secs = ((span_secs + num_buckets - 1) / num_buckets).max(1);
    let n = ((span_secs + bucket_secs - 1) / bucket_secs) + 1; // 覆盖整段的桶数

    // 降采样：SQL 层按整型桶序号 GROUP BY，每桶 AVG/MIN/MAX
    let mut stmt = conn.prepare(
        "SELECT ((CAST(strftime('%s', timestamp) AS INTEGER) - ?3) / ?4) AS bucket,
                AVG(cpu_usage)         AS cpu_avg,
                MIN(cpu_usage)         AS cpu_min,
                MAX(cpu_usage)         AS cpu_max,
                AVG(mem_usage_percent) AS mem_avg,
                MIN(mem_usage_percent) AS mem_min,
                MAX(mem_usage_percent) AS mem_max,
                AVG(disk_read_bps)     AS dr_avg,
                MIN(disk_read_bps)     AS dr_min,
                MAX(disk_read_bps)     AS dr_max,
                AVG(disk_write_bps)    AS dw_avg,
                MIN(disk_write_bps)    AS dw_min,
                MAX(disk_write_bps)    AS dw_max,
                AVG(gpu_usage)         AS gpu_avg
         FROM samples
         WHERE timestamp BETWEEN ?1 AND ?2
         GROUP BY bucket ORDER BY bucket",
    )?;
    let rows = stmt.query_map(params![start, end, base_secs, bucket_secs], |row| {
        Ok((
            row.get::<_, i64>(0)?, // bucket
            row.get::<_, Option<f64>>(1)?,
            row.get::<_, Option<f64>>(2)?,
            row.get::<_, Option<f64>>(3)?,
            row.get::<_, Option<f64>>(4)?,
            row.get::<_, Option<f64>>(5)?,
            row.get::<_, Option<f64>>(6)?,
            row.get::<_, Option<f64>>(7)?,
            row.get::<_, Option<f64>>(8)?,
            row.get::<_, Option<f64>>(9)?,
            row.get::<_, Option<f64>>(10)?,
            row.get::<_, Option<f64>>(11)?,
            row.get::<_, Option<f64>>(12)?,
            row.get::<_, Option<f64>>(13)?,
        ))
    })?;
    let mut points: Vec<ResourcePoint> = Vec::new();
    for r in rows {
        let (b, ca, cmi, cma, ma, mmi, mma, dr, dr_min, dr_max, dw, dw_min, dw_max, ga) = r?;
        let x = b.clamp(0, n - 1);
        points.push(ResourcePoint {
            x,
            ts_secs: base_secs + x * bucket_secs,
            cpu_avg: ca.unwrap_or(0.0) as f32,
            cpu_min: cmi.unwrap_or(0.0) as f32,
            cpu_max: cma.unwrap_or(0.0) as f32,
            mem_avg: ma.unwrap_or(0.0) as f32,
            mem_min: mmi.unwrap_or(0.0) as f32,
            mem_max: mma.unwrap_or(0.0) as f32,
            disk_read_avg: dr.unwrap_or(0.0),
            disk_read_min: dr_min.unwrap_or(0.0),
            disk_read_max: dr_max.unwrap_or(0.0),
            disk_write_avg: dw.unwrap_or(0.0),
            disk_write_min: dw_min.unwrap_or(0.0),
            disk_write_max: dw_max.unwrap_or(0.0),
            gpu_avg: ga.map(|v| v as f32),
        });
    }

    // 卡顿事件竖线：取同范围事件时间戳，折算桶序号（clamp 到有效区间）
    let mut ev_stmt = conn.prepare(
        "SELECT CAST(strftime('%s', timestamp) AS INTEGER) AS t
         FROM stutter_events WHERE timestamp BETWEEN ?1 AND ?2",
    )?;
    let ev_rows = ev_stmt.query_map(params![start, end], |row| row.get::<_, i64>(0))?;
    let mut event_x: Vec<i64> = Vec::new();
    for r in ev_rows {
        let t = r?;
        let bx = ((t - base_secs) / bucket_secs).clamp(0, n - 1);
        event_x.push(bx);
    }

    Ok(ResourceData {
        points,
        event_x,
        base_secs,
        bucket_secs,
        span_secs,
    })
}

// ===================== M5 / F5+F8：原始事件表 + CSV 导出 =====================

/// F5/F8：单条卡顿事件明细（供高级模式原始事件表与 CSV 导出）。
#[derive(Debug, Clone, PartialEq)]
pub struct EventRow {
    /// 本地时区格式化时间 `YYYY-MM-DD HH:MM:SS`（PRD §3.3 时区口径）。
    pub time_local: String,
    /// 卡顿发生时刻（UTC epoch 秒）；用于按时刻对齐资源采样与加载 snapshot。
    /// 解析失败 / 旧库异常填 0。
    pub ts_secs: i64,
    /// 持续时长（ms）
    pub duration_ms: u64,
    /// 严重程度中文标签（轻微 / 严重 / 危急）
    pub severity_cn: String,
    /// 触发原因：causes JSON 数组按「；」拼接（可读、避免 CSV 逗号冲突）
    pub causes_text: String,
    /// 元凶进程：culprits 的 name 按「,」拼接
    pub culprits_text: String,
    /// 元凶进程名原始列表（仅用于内存钻取过滤，不进 slint 模型）
    pub culprit_names: Vec<String>,
}

/// 把 severity 原始值映射为中文标签（CSV / 表格统一口径）。
fn severity_cn(sev: &str) -> String {
    let s = match sev {
        "critical" => "危急",
        "major" => "严重",
        "minor" => "轻微",
        _ => sev, // 未知值原样保留，便于排查
    };
    s.to_string()
}

/// 把单条 causes JSON 数组展开为「；」分隔的可读文本。
///
/// 读不出时用空串；数组内多个原因以中文分号「；」拼接（不用逗号，避免与
/// CSV 字段分隔符冲突，也更符合中文阅读习惯）。
fn join_causes(json: &str) -> String {
    let causes: Vec<String> = serde_json::from_str(json).unwrap_or_default();
    causes
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .collect::<Vec<_>>()
        .join("；")
}

/// 把 culprits JSON 数组展开为进程名列表 + 「,」分隔文本。
fn parse_culprits(json: &str) -> (Vec<String>, String) {
    let culprits: Vec<ProcessBrief> = serde_json::from_str(json).unwrap_or_default();
    let names: Vec<String> = culprits.iter().map(|c| c.name.clone()).collect();
    let text = names.join(",");
    (names, text)
}

/// F5：读取时间范围内的事件明细（PRD §5 高级模式原始事件表）。
///
/// 返回按 `timestamp` 升序排列的事件，`time_local` 已转本地时区。
/// 容错：旧库缺 `causes` / `culprits` 列 → 对应字段回退空（不崩、不写库）。
///
/// 排序由 [`EventSort`] 控制（PRD §5「可排序/筛选」）：默认按时间升序，与旧行为一致。
pub fn load_events(conn: &Connection, range: &TimeRange) -> anyhow::Result<Vec<EventRow>> {
    load_events_sorted(conn, range, &EventSort::default())
}

/// 事件表排序字段（PRD §5 列头排序）。
///
/// - `Time`：按 `timestamp`（本地时区文本，定宽零填充，字典序即时间序）；
/// - `Duration`：按 `duration_ms` 数值；
/// - `Severity`：按枚举序（危急 > 严重 > 轻微）排序；
/// - `Causes` / `Culprits`：按展开后文本字典序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventSortColumn {
    Time,
    Duration,
    Severity,
    Causes,
    Culprits,
}

/// 事件表排序规则（字段 + 方向）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventSort {
    pub column: EventSortColumn,
    /// true=升序，false=降序
    pub asc: bool,
}

impl Default for EventSort {
    /// 默认按时间升序（与 M5 原始行为一致）。
    fn default() -> Self {
        EventSort {
            column: EventSortColumn::Time,
            asc: true,
        }
    }
}

/// 把 slint 端传来的列名映射到 [`EventSortColumn`]。
///
/// 未知列名（含 ProcessList 的 pid/name 等）一律回退 `Time`，保证不崩。
pub fn parse_event_sort_column(name: &str) -> EventSortColumn {
    match name {
        "duration" => EventSortColumn::Duration,
        "severity" => EventSortColumn::Severity,
        "causes" => EventSortColumn::Causes,
        "culprits" => EventSortColumn::Culprits,
        _ => EventSortColumn::Time,
    }
}

/// F5：读取时间范围内的事件明细并按 `sort` 排序（PRD §5 高级模式原始事件表）。
///
/// 数据读取后统一在内存中按 `sort` 排序，因 `causes_text` / `culprits_text` 是
/// 解析后的派生字段、无法在 SQL 层排序；这样旧库（缺列）也能一致排序。
pub fn load_events_sorted(
    conn: &Connection,
    range: &TimeRange,
    sort: &EventSort,
) -> anyhow::Result<Vec<EventRow>> {
    let (start, end) = range.bounds();
    let has_causes = has_column(conn, "stutter_events", "causes");
    let has_culprits = has_column(conn, "stutter_events", "culprits");

    let causes_sql = if has_causes {
        "COALESCE(causes, '[]')"
    } else {
        "'[]'"
    };
    let culprits_sql = if has_culprits {
        "COALESCE(culprits, '[]')"
    } else {
        "'[]'"
    };
    let sql = format!(
        "SELECT timestamp, duration_ms, severity, {causes_sql}, {culprits_sql} \
         FROM stutter_events WHERE timestamp BETWEEN ?1 AND ?2 ORDER BY timestamp"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![start, end], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)? as u64,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;

    let mut out = Vec::new();
    for r in rows {
        let (ts, dur, sev, causes_json, culprits_json) = r?;
        // timestamp 落库 UTC RFC3339 → 解析为本地时区展示（与 KPI/趋势一致口径）
        let time_local = DateTime::parse_from_rfc3339(&ts)
            .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|_| ts.clone());
        // 解析 UTC epoch 秒（供 hover/snapshot 按时刻对齐资源采样）；失败填 0。
        let ts_secs = DateTime::parse_from_rfc3339(&ts)
            .map(|d| d.timestamp())
            .unwrap_or(0);
        let causes_text = join_causes(&causes_json);
        let (culprit_names, culprits_text) = parse_culprits(&culprits_json);
        out.push(EventRow {
            time_local,
            ts_secs,
            duration_ms: dur,
            severity_cn: severity_cn(&sev),
            causes_text,
            culprits_text,
            culprit_names,
        });
    }

    // 内存排序（severity 用枚举序，其余按字段语义）
    let sev_rank = |e: &EventRow| -> u8 {
        match e.severity_cn.as_str() {
            "危急" => 3,
            "严重" => 2,
            "轻微" => 1,
            _ => 0,
        }
    };
    out.sort_by(|a, b| {
        let ord = match sort.column {
            EventSortColumn::Time => a.time_local.cmp(&b.time_local),
            EventSortColumn::Duration => a.duration_ms.cmp(&b.duration_ms),
            EventSortColumn::Severity => sev_rank(a).cmp(&sev_rank(b)),
            EventSortColumn::Causes => a.causes_text.cmp(&b.causes_text),
            EventSortColumn::Culprits => a.culprits_text.cmp(&b.culprits_text),
        };
        if sort.asc {
            ord
        } else {
            ord.reverse()
        }
    });
    Ok(out)
}

/// F8：把时间范围内的卡顿事件导出为 CSV（中文表头，PRD §8 / AGENTS.md）。
///
/// - 全程只读 `stutter.db`；本函数只**写用户文件**（由调用方决定可写路径，
///   不写 stutter.db 所在目录）。
/// - 表头：`时间,持续毫秒,严重程度,触发原因,元凶进程`（与现有 export CLI 的中文
///   表头风格一致）。
/// - 原因/cullprits 是 JSON 数组：原因按「；」展开为多段可读文本，元凶按「,」拼接
///   进程名（多列展开易错位，逗号/分号拼接最稳健；csv crate 自动处理引号转义）。
pub fn export_events_csv(
    conn: &Connection,
    range: &TimeRange,
    path: &Path,
) -> anyhow::Result<()> {
    let rows = load_events(conn, range)?;
    let mut wtr = csv::Writer::from_path(path)?;
    wtr.write_record([
        "时间",
        "持续毫秒",
        "严重程度",
        "触发原因",
        "元凶进程",
    ])?;
    for r in &rows {
        wtr.write_record([
            &r.time_local,
            &r.duration_ms.to_string(),
            &r.severity_cn,
            &r.causes_text,
            &r.culprits_text,
        ])?;
    }
    wtr.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use find_stutter_core::{Logger, ProcessBrief, Sample, Severity, StorageConfig};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_db(name: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("fs_analytics_{}_{}.db", name, nanos))
            .to_str()
            .unwrap()
            .to_string()
    }

    /// 测试用：本地零点对应的 UTC 时刻。直接复用核心单一来源
    /// `find_stutter_core::logger::local_today_bounds_utc()`，保证与真实查询口径一致。
    fn local_midnight_utc() -> DateTime<Utc> {
        find_stutter_core::logger::local_today_bounds_utc().0
    }

    /// 写入若干今日事件（不同 local 小时桶），返回 db 路径。
    fn seed_today(db: &str, hours: &[u32], severities: &[Severity]) {
        let cfg = StorageConfig {
            db_path: db.to_string(),
            retention_days: 30,
        };
        let mut logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();        let base = local_midnight_utc();
        for (i, (h, sev)) in hours.iter().zip(severities.iter()).enumerate() {
            let ts = base + ChronoDuration::hours(*h as i64) + ChronoDuration::minutes(i as i64);
            let mut s = Sample::default();
            s.cpu_usage = 95.0;
            let ev = find_stutter_core::StutterEvent {
                timestamp: ts,
                duration_ms: 1000 * (i + 1) as u64,
                severity: *sev,
                causes: vec!["CPU usage 95.0% > 90.0%".into()],
                snapshot: s,
                culprits: vec![ProcessBrief {
                    pid: 100 + i as u32,
                    name: format!("app{}.exe", i % 2),
                    cpu_usage: 80.0,
                    mem_used_mb: 200,
                }],
            };
            logger.write_event(&ev).unwrap();
        }
        logger.flush().unwrap();
    }

    /// 写入若干「近期」事件（相对 `Local::now()` 的过去几分钟），保证落在今日范围内、
    /// 且不依赖当前是几点（避免本地时间贴近零点时 `+N` 小时种子落入未来导致用例失败）。
    /// 与 `seed_today` 仅差种子时刻的取法，事件字段语义完全一致。
    fn seed_recent_today(db: &str, severities: &[Severity]) {
        let cfg = StorageConfig {
            db_path: db.to_string(),
            retention_days: 30,
        };
        let mut logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();
        let base = Local::now();
        for (i, sev) in severities.iter().enumerate() {
            // 10/9/8 分钟前（均在今日、过去），分布在同一本地小时桶内
            let ts = (base - ChronoDuration::minutes(10 - i as i64)).with_timezone(&Utc);
            let mut s = Sample::default();
            s.cpu_usage = 95.0;
            let ev = find_stutter_core::StutterEvent {
                timestamp: ts,
                duration_ms: 1000 * (i + 1) as u64,
                severity: *sev,
                causes: vec!["CPU usage 95.0% > 90.0%".into()],
                snapshot: s,
                culprits: vec![ProcessBrief {
                    pid: 100 + i as u32,
                    name: format!("app{}.exe", i % 2),
                    cpu_usage: 80.0,
                    mem_used_mb: 200,
                }],
            };
            logger.write_event(&ev).unwrap();
        }
        logger.flush().unwrap();
    }

    #[test]
    fn ensure_indexes_idempotent() {
        let db = unique_db("idx");
        // 用可写连接验证 CREATE INDEX 语句正确且幂等（分析页只读连接下会优雅降级，
        // 不落索引但也不报错——索引仅在连接可写时真正落地）。
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE stutter_events (id INTEGER PRIMARY KEY, timestamp TEXT); \
             CREATE TABLE samples (id INTEGER PRIMARY KEY, timestamp TEXT);",
        )
        .unwrap();
        ensure_indexes(&conn).unwrap();
        ensure_indexes(&conn).unwrap(); // 第二次应不再报错（IF NOT EXISTS）
        let has: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='idx_events_ts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has, 1);
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn load_kpi_today_aligns_with_event_count() {
        let db = unique_db("kpi");
        // 用「近期」种子（相对 now 的过去几分钟）而非「零点 +N 小时」，避免本地时间
        // 贴近零点时种子落入未来、漏算今日次数（用例与时辰无关、稳定可重复）。
        seed_recent_today(&db, &[Severity::Minor, Severity::Major, Severity::Critical]);
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        let kpi = load_kpi_today(&conn).unwrap();
        // 今日 3 次，应与 reader.event_count_today 一致
        assert_eq!(kpi.today_count, 3);
        // 最严重一次 = 3000ms（最后一个，duration = 1000*(i+1)）
        assert_eq!(kpi.worst_duration_ms, 3000);
        // 高峰时段存在（HH:00 格式）
        assert!(kpi.peak_hour.ends_with(":00"));
        assert_ne!(kpi.peak_hour, "—");
        // 头号元凶非空（app0.exe / app1.exe 各出现，取其一）
        assert_ne!(kpi.top_culprit, "—");
        assert!(kpi.top_culprit.starts_with("app"));
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn load_trend_buckets_by_local_hour() {
        let db = unique_db("trend");
        seed_today(&db, &[9, 9, 10], &[Severity::Major, Severity::Minor, Severity::Critical]);
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        // 用覆盖种子时段的「固定自定义范围」而非 Today：种子事件按本地零点 +9/+10 小时，
        // 若恰好在本地零点附近运行用例，Today 的上界（now）会早于这些未来时刻导致漏查。
        // 固定 [本地零点, 本地零点+12h] 使用例与时辰无关、稳定可重复（仍验证本地时区分桶）。
        let base = local_midnight_utc();
        let range = TimeRange::Custom(base.to_rfc3339(), (base + ChronoDuration::hours(12)).to_rfc3339());
        let trend = load_trend(&conn, &range, TrendBucket::Hour).unwrap();
        // 按本地时区分桶：9,9,10 → 两个本地桶（一个 count=2、一个 count=1）。
        // 断言不依赖绝对小时字符串（时区偏移会平移桶标签），只看桶数与计数。
        assert_eq!(trend.len(), 2);
        let total: u32 = trend.iter().map(|p| p.count).sum();
        assert_eq!(total, 3);
        let big = trend.iter().find(|p| p.count == 2).expect("应有一个 count=2 的桶");
        assert_eq!(big.major + big.minor + big.critical, 2);
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn time_range_bounds_today_starts_at_local_midnight() {
        use chrono::Timelike;
        let (start, end) = TimeRange::Today.bounds();
        // 今日起点应为「本地零点」换算成的 UTC 时刻（偏移 +00:00），
        // 其本地时区下的时:分:秒应为 00:00:00。
        let start_dt = DateTime::parse_from_rfc3339(&start).unwrap();
        let start_local = start_dt.with_timezone(&Local);
        assert_eq!(
            (start_local.hour(), start_local.minute(), start_local.second()),
            (0, 0, 0),
            "今日起点应为本地零点: {}",
            start
        );
        let end_dt = DateTime::parse_from_rfc3339(&end).unwrap();
        assert!(end_dt >= start_dt, "结束应不早于起点: {} ~ {}", start, end);
    }

    #[test]
    fn legacy_schema_without_culprits_does_not_crash() {
        // 手工构造无 culprits 列的旧库
        let db = unique_db("legacy");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE stutter_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                severity TEXT NOT NULL,
                causes TEXT NOT NULL,
                snapshot TEXT NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO stutter_events (timestamp, duration_ms, severity, causes, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                Utc::now().to_rfc3339(),
                2000i64,
                "major",
                r#"["x"]"#,
                r#"{"cpu_usage":1.0}"#,
            ],
        )
        .unwrap();
        drop(conn);
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        let kpi = load_kpi_today(&conn).unwrap();
        assert_eq!(kpi.today_count, 1);
        // 无 culprits 列 → 头号元凶回退 "—"
        assert_eq!(kpi.top_culprit, "—");
        std::fs::remove_file(&db).ok();
    }

    /// 写入若干今日事件，每个事件的 culprits 由 `culprits_per_event` 提供（同名不同 PID 可验证聚合）。
    fn seed_events(db: &str, culprits_per_event: &[Vec<ProcessBrief>], causes_per_event: &[Vec<String>]) {
        let cfg = StorageConfig {
            db_path: db.to_string(),
            retention_days: 30,
        };
        let mut logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();
        let base = local_midnight_utc();
        for (i, (cs, caz)) in culprits_per_event.iter().zip(causes_per_event.iter()).enumerate() {
            let ts = base + ChronoDuration::minutes(i as i64);
            let mut s = Sample::default();
            s.cpu_usage = 95.0;
            let ev = find_stutter_core::StutterEvent {
                timestamp: ts,
                duration_ms: 1000 * (i + 1) as u64,
                severity: Severity::Minor,
                causes: caz.clone(),
                snapshot: s,
                culprits: cs.clone(),
            };
            logger.write_event(&ev).unwrap();
        }
        logger.flush().unwrap();
    }

    #[test]
    fn load_culprits_aggregates_by_name() {
        // app.exe 出现 2 次（不同 PID），bg.exe 出现 1 次
        let pb = |pid: u32, name: &str, cpu: f32, mem: u64| ProcessBrief {
            pid,
            name: name.to_string(),
            cpu_usage: cpu,
            mem_used_mb: mem,
        };
        let culprits = vec![
            vec![pb(1, "app.exe", 90.0, 500), pb(2, "bg.exe", 10.0, 100)],
            vec![pb(3, "app.exe", 80.0, 600)],
            vec![pb(4, "bg.exe", 20.0, 200)],
        ];
        let causes = vec![
            vec!["CPU usage 95.0% > 90.0%".to_string()],
            vec!["CPU usage 95.0% > 90.0%".to_string()],
            vec!["Available memory 100MB < 500MB".to_string()],
        ];
        let db = unique_db("culp_agg");
        seed_events(&db, &culprits, &causes);
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        let top = load_culprits(&conn, &TimeRange::Today, 10).unwrap();
        // 两个进程各出现 2 次；并列时按累计时长降序，bg.exe(4000) > app.exe(3000)
        assert_eq!(top.len(), 2);
        let app = top.iter().find(|c| c.name == "app.exe").unwrap();
        assert_eq!(app.count, 2);
        // 累计时长 = 1000 + 2000 = 3000ms
        assert_eq!(app.total_duration_ms, 3000);
        // 最高单次 CPU = max(90,80) = 90
        assert_eq!(app.max_cpu, 90.0);
        // 最高单次内存 = max(500,600) = 600
        assert_eq!(app.max_mem_mb, 600);
        let bg = top.iter().find(|c| c.name == "bg.exe").unwrap();
        assert_eq!(bg.count, 2);
        assert_eq!(bg.total_duration_ms, 4000);
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn load_culprits_limit_and_missing_column() {
        let pb = |pid: u32, name: &str| ProcessBrief {
            pid,
            name: name.to_string(),
            cpu_usage: 50.0,
            mem_used_mb: 100,
        };
        let culprits = vec![
            vec![pb(1, "a.exe")],
            vec![pb(2, "b.exe")],
            vec![pb(3, "c.exe")],
        ];
        let causes = vec![
            vec!["x".to_string()],
            vec!["x".to_string()],
            vec!["x".to_string()],
        ];
        let db = unique_db("culp_limit");
        seed_events(&db, &culprits, &causes);
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        // limit=2 → 截断到 2 条
        let top = load_culprits(&conn, &TimeRange::Today, 2).unwrap();
        assert_eq!(top.len(), 2);

        // 旧库无 culprits 列 → 回退空 Vec（不崩）
        let legacy = unique_db("culp_legacy");
        let lc = Connection::open(&legacy).unwrap();
        lc.execute_batch(
            "CREATE TABLE stutter_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                severity TEXT NOT NULL,
                causes TEXT NOT NULL,
                snapshot TEXT NOT NULL
            );",
        )
        .unwrap();
        lc.execute(
            "INSERT INTO stutter_events (timestamp, duration_ms, severity, causes, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                Utc::now().to_rfc3339(),
                2000i64,
                "major",
                r#"["x"]"#,
                r#"{"cpu_usage":1.0}"#,
            ],
        )
        .unwrap();
        drop(lc);
        let lc_conn = open_readonly(std::path::Path::new(&legacy)).unwrap();
        let empty = load_culprits(&lc_conn, &TimeRange::Today, 10).unwrap();
        assert!(empty.is_empty());
        std::fs::remove_file(&db).ok();
        std::fs::remove_file(&legacy).ok();
    }

    #[test]
    fn classify_cause_covers_detector_text() {
        // 严格对应 detector.rs 实际产出的文案（见 cause_key 前缀表）
        assert_eq!(classify_cause("CPU usage 95.0% > 90.0%"), "CPU 过高");
        assert_eq!(
            classify_cause("CPU usage 85.0%（滞回保持，阈值 90%）"),
            "CPU 过高"
        );
        assert_eq!(classify_cause("CPU spike: 1.0% → 3.0%"), "CPU 突增");
        assert_eq!(classify_cause("Disk write spike: 1B/s → 3B/s"), "磁盘突增");
        assert_eq!(classify_cause("Network spike: 1B/s → 3B/s"), "网络突增");
        assert_eq!(
            classify_cause("Memory available spike: 1000MB → 200MB"),
            "内存骤降"
        );
        assert_eq!(classify_cause("Memory usage 95.0% > 90%"), "内存过高");
        assert_eq!(classify_cause("Available memory 100MB < 500MB"), "内存不足");
        assert_eq!(classify_cause("Commit charge 95.0% > 90%"), "提交电荷");
        assert_eq!(classify_cause("Memory paging 200.0/s > 50/s"), "内存分页");
        assert_eq!(classify_cause("weird unknown text"), "其他");
    }

    #[test]
    fn load_cause_types_counts_and_percent() {
        // 3 次卡顿：2 次含 CPU usage，1 次含 Network spike + Available memory
        let causes = vec![
            vec!["CPU usage 95.0% > 90.0%".to_string()],
            vec!["CPU usage 95.0% > 90.0%".to_string()],
            vec![
                "Network spike: 1B/s → 3B/s".to_string(),
                "Available memory 100MB < 500MB".to_string(),
            ],
        ];
        let culprits = vec![vec![], vec![], vec![]];
        let db = unique_db("cause_types");
        seed_events(&db, &culprits, &causes);
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        let res = load_cause_types(&conn, &TimeRange::Today).unwrap();
        // 总计数 = 2 + 2 = 4（Network 1 + Available memory 1）
        assert_eq!(res.len(), 3);
        let total: u32 = res.iter().map(|c| c.count).sum();
        assert_eq!(total, 4);
        // CPU 过高 计数 2，占比 50%
        let cpu = res.iter().find(|c| c.cause_type == "CPU 过高").unwrap();
        assert_eq!(cpu.count, 2);
        assert!((cpu.percent - 50.0).abs() < 0.01);
        // 降序：CPU 过高(2) 在前
        assert_eq!(res[0].cause_type, "CPU 过高");
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn load_cause_types_empty_when_no_events() {
        let db = unique_db("cause_empty");
        let cfg = StorageConfig {
            db_path: db.clone(),
            retention_days: 30,
        };
        let mut logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();
        logger.flush().unwrap();
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        let res = load_cause_types(&conn, &TimeRange::Today).unwrap();
        assert!(res.is_empty());
        std::fs::remove_file(&db).ok();
    }

    /// 写入若干时序 sample（供 F3 降采样单测）。
    fn seed_samples(db: &str, samples: &[Sample]) {
        let cfg = StorageConfig {
            db_path: db.to_string(),
            retention_days: 30,
        };
        let mut logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();
        for s in samples {
            logger.write_sample(s).unwrap();
        }
        logger.flush().unwrap();
    }

    #[test]
    fn load_resource_downsamples_to_bucket_count() {
        // 3600 个采样跨 1 小时（今日），验证降采样把点数压到桶数量级
        let db = unique_db("res_down");
        let base = local_midnight_utc();
        let mut samples = Vec::new();
        for i in 0..3600 {
            let mut s = Sample::default();
            s.timestamp = base + ChronoDuration::seconds(i as i64);
            s.cpu_usage = 50.0 + 30.0 * (i as f32 / 3600.0).sin();
            s.mem_usage_percent = 40.0;
            s.disk_read_bps = 1_000_000 + (i as u64 % 10) * 100_000;
            s.gpu_usage = Some(20.0);
            samples.push(s);
        }
        seed_samples(&db, &samples);
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        // width=200 → 桶数约 200，实际桶数应 <= 201 且 > 0
        let data = load_resource_samples(&conn, &TimeRange::Today, 200).unwrap();
        assert!(!data.points.is_empty(), "应有降采样点");
        assert!(
            data.points.len() <= 201,
            "桶数应被限制，实际 {}",
            data.points.len()
        );
        // avg 在 0-100 合理区间，且 max>=avg>=min
        for p in &data.points {
            assert!((0.0..=100.0).contains(&p.cpu_avg));
            assert!(p.cpu_max >= p.cpu_avg && p.cpu_avg >= p.cpu_min);
        }
        // GPU 有值应被保留
        assert!(data.points.iter().all(|p| p.gpu_avg.is_some()));
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn load_resource_empty_when_no_samples() {
        let db = unique_db("res_empty");
        let cfg = StorageConfig {
            db_path: db.clone(),
            retention_days: 30,
        };
        let mut logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();
        logger.flush().unwrap();
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        let data = load_resource_samples(&conn, &TimeRange::Today, 200).unwrap();
        assert!(data.points.is_empty());
        assert!(data.event_x.is_empty());
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn load_resource_maps_event_timestamps_to_buckets() {
        // 60 个采样 + 1 个卡顿事件，验证 event_x 折算到 [0, 桶数-1]
        let db = unique_db("res_ev");
        let base = local_midnight_utc();
        let mut samples = Vec::new();
        for i in 0..60 {
            let mut s = Sample::default();
            s.timestamp = base + ChronoDuration::seconds(i as i64);
            s.cpu_usage = 10.0;
            s.mem_usage_percent = 20.0;
            samples.push(s);
        }
        seed_samples(&db, &samples);

        let snap = Sample::default();
        let ev = find_stutter_core::StutterEvent {
            timestamp: base + ChronoDuration::seconds(30),
            duration_ms: 1000,
            severity: Severity::Minor,
            causes: vec!["x".into()],
            snapshot: snap,
            culprits: vec![],
        };
        {
            let cfg = StorageConfig {
                db_path: db.clone(),
                retention_days: 30,
            };
            let logger = Logger::new(&cfg).unwrap();
            logger.write_event(&ev).unwrap();
        }
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        let data = load_resource_samples(&conn, &TimeRange::Today, 200).unwrap();
        assert_eq!(data.event_x.len(), 1, "应有 1 个卡顿竖线");
        // 今日起点 = base；事件在 base+30s；width=200 时 bucket_secs=432 → bx=0
        assert!(data.event_x[0] >= 0);
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn load_events_returns_sorted_with_local_time_and_cn_severity() {
        // 两个事件：先写「较早」再写「较晚」，验证按 timestamp 升序返回
        let pb = |pid: u32, name: &str| ProcessBrief {
            pid,
            name: name.to_string(),
            cpu_usage: 50.0,
            mem_used_mb: 100,
        };
        let culprits = vec![vec![pb(1, "app.exe")], vec![pb(2, "svc.exe")]];
        let causes = vec![
            vec!["CPU usage 95.0% > 90.0%".to_string()],
            vec!["Network spike: 1B/s → 3B/s".to_string()],
        ];
        let db = unique_db("events");
        seed_events(&db, &culprits, &causes);
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        let rows = load_events(&conn, &TimeRange::Today).unwrap();
        assert_eq!(rows.len(), 2);
        // 升序：第一项应早于第二项（按 duration 推断 1000 < 2000，且时间戳也递增）
        assert!(rows[0].duration_ms < rows[1].duration_ms);
        // 本地时区格式 `YYYY-MM-DD HH:MM:SS`
        assert!(rows[0].time_local.contains(' '));
        assert_eq!(rows[0].severity_cn, "轻微");
        // 原因按「；」拼接；元凶按「,」拼接
        assert!(rows[0].causes_text.contains("CPU"));
        assert_eq!(rows[0].culprits_text, "app.exe");
        assert_eq!(rows[0].culprit_names, vec!["app.exe".to_string()]);
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn export_events_csv_writes_chinese_header_and_rows() {
        let pb = |pid: u32, name: &str| ProcessBrief {
            pid,
            name: name.to_string(),
            cpu_usage: 50.0,
            mem_used_mb: 100,
        };
        let culprits = vec![vec![pb(1, "app.exe"), pb(2, "bg.exe")]];
        let causes = vec![vec![
            "CPU usage 95.0% > 90.0%".to_string(),
            "Available memory 100MB < 500MB".to_string(),
        ]];
        let db = unique_db("export_csv");
        seed_events(&db, &culprits, &causes);
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();

        // 临时输出文件（用户可写目录，绝不碰 stutter.db 所在目录）
        let out = std::env::temp_dir()
            .join(format!("fs_export_{}.csv", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        export_events_csv(&conn, &TimeRange::Today, &out).unwrap();

        // 读回校验表头与行数（csv crate 自动处理引号转义）
        let mut rdr = csv::Reader::from_path(&out).unwrap();
        let headers: Vec<String> = rdr.headers().unwrap().iter().map(|s| s.to_string()).collect();
        assert_eq!(
            headers,
            vec!["时间", "持续毫秒", "严重程度", "触发原因", "元凶进程"]
        );
        let records: Vec<csv::StringRecord> = rdr.records().map(|r| r.unwrap()).collect();
        assert_eq!(records.len(), 1);
        // 元凶多进程应以逗号拼接进同一字段
        assert_eq!(records[0].get(4).unwrap(), "app.exe,bg.exe");
        // 多原因以「；」拼接进同一字段
        assert!(records[0].get(3).unwrap().contains("；"));
        std::fs::remove_file(&db).ok();
        std::fs::remove_file(&out).ok();
    }

    #[test]
    fn load_events_handles_legacy_schema_without_culprits_causes() {
        // 手工构造缺 culprits/causes 列的旧库，验证不崩且对应字段回退空
        let db = unique_db("events_legacy");
        let lc = Connection::open(&db).unwrap();
        lc.execute_batch(
            "CREATE TABLE stutter_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                severity TEXT NOT NULL,
                snapshot TEXT NOT NULL
            );",
        )
        .unwrap();
        lc.execute(
            "INSERT INTO stutter_events (timestamp, duration_ms, severity, snapshot)
             VALUES (?1, ?2, ?3, ?4)",
            params![Utc::now().to_rfc3339(), 1500i64, "major", r#"{"cpu_usage":1.0}"#],
        )
        .unwrap();
        drop(lc);
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        let rows = load_events(&conn, &TimeRange::Today).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].severity_cn, "严重");
        assert_eq!(rows[0].causes_text, "");
        assert_eq!(rows[0].culprits_text, "");
        assert!(rows[0].culprit_names.is_empty());
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn load_events_sorted_by_duration_and_severity() {
        // 3 个事件：时长 1000/3000/2000，严重度 分别为 轻微/严重/危急。
        // 先按时间升序 seed（seed_events 按分钟递增写入）。
        let pb = |pid: u32, name: &str| ProcessBrief {
            pid,
            name: name.to_string(),
            cpu_usage: 50.0,
            mem_used_mb: 100,
        };
        let culprits = vec![vec![pb(1, "app.exe")], vec![pb(2, "svc.exe")], vec![pb(3, "bg.exe")]];
        let causes = vec![
            vec!["CPU usage 95.0% > 90.0%".to_string()],
            vec!["Network spike: 1B/s -> 3B/s".to_string()],
            vec!["Available memory 100MB < 500MB".to_string()],
        ];
        let db = unique_db("events_sort");
        {
            let cfg = StorageConfig { db_path: db.clone(), retention_days: 30 };
            let mut logger = Logger::new(&cfg).unwrap();
            logger.touch_heartbeat().unwrap();
            let base = local_midnight_utc();
            let sevs = [Severity::Minor, Severity::Major, Severity::Critical];
            let durs = [1000u64, 3000, 2000];
            for (i, (sev, dur)) in sevs.iter().zip(durs.iter()).enumerate() {
                let mut s = Sample::default();
                s.cpu_usage = 50.0;
                let ev = find_stutter_core::StutterEvent {
                    timestamp: base + ChronoDuration::minutes(i as i64),
                    duration_ms: *dur,
                    severity: *sev,
                    causes: causes[i].clone(),
                    snapshot: s,
                    culprits: culprits[i].clone(),
                };
                logger.write_event(&ev).unwrap();
            }
            logger.flush().unwrap();
        }
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();

        // 默认：时间升序（1000 < 2000 < 3000 对应写入顺序）
        let rows = load_events(&conn, &TimeRange::Today).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].duration_ms, 1000);
        assert_eq!(rows[2].duration_ms, 2000);

        // 按 duration 升序：1000, 2000, 3000
        let asc = load_events_sorted(
            &conn,
            &TimeRange::Today,
            &EventSort { column: EventSortColumn::Duration, asc: true },
        )
        .unwrap();
        assert_eq!(asc[0].duration_ms, 1000);
        assert_eq!(asc[1].duration_ms, 2000);
        assert_eq!(asc[2].duration_ms, 3000);

        // 按 duration 降序：3000, 2000, 1000
        let desc = load_events_sorted(
            &conn,
            &TimeRange::Today,
            &EventSort { column: EventSortColumn::Duration, asc: false },
        )
        .unwrap();
        assert_eq!(desc[0].duration_ms, 3000);
        assert_eq!(desc[2].duration_ms, 1000);

        // 按 severity 降序：危急(3) > 严重(2) > 轻微(1)
        let by_sev = load_events_sorted(
            &conn,
            &TimeRange::Today,
            &EventSort { column: EventSortColumn::Severity, asc: false },
        )
        .unwrap();
        assert_eq!(by_sev[0].severity_cn, "危急");
        assert_eq!(by_sev[1].severity_cn, "严重");
        assert_eq!(by_sev[2].severity_cn, "轻微");

        // 解析列名：未知列名回退时间升序（不崩）
        assert_eq!(
            parse_event_sort_column("pid"),
            EventSortColumn::Time
        );
        assert_eq!(
            parse_event_sort_column("culprits"),
            EventSortColumn::Culprits
        );
        std::fs::remove_file(&db).ok();
    }
}
