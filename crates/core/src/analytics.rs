//! 卡顿分析只读查询层 + 根因分析纯函数（PRD §6.1 / §7 / M2 / F-RC5~F-RC16）。
//!
//! ADR-0001：本模块原住 `ui` crate，现下沉到 core——UI（分析页图表）与
//! CLI（agent JSON 查询）共用同一份分析口径，避免两边各自漂移。
//!
//! 全程只读 `stutter.db`：不写库、不改 service、不新增任何采集逻辑
//! （唯一例外：`open_report_writer` 系列，GUI 保存根因结论的「窄写权」，
//! 只碰 `root_cause_reports` 表）。
//! 所有聚合查询都带 `WHERE timestamp BETWEEN ? AND ?` 时间范围，并依赖
//! `idx_samples_ts` / `idx_events_ts` 时间戳索引（首次打开时幂等创建）。
//!
//! ## 时区口径（PRD §3.3）
//!
//! - `timestamp` 落库是 UTC RFC3339。
//! - KPI「今日卡顿 N 次」**必须与悬浮窗 `event_count_today` 完全一致**：
//!   两者共用核心单一来源 `local_today_bounds_utc()`（本地零点→现在，BETWEEN UTC 边界，
//!   见 `logger.rs`），分析页 `load_kpi_today` / `TimeRange::Today` 都走它，
//!   任何一处「今日卡顿 N 次」口径都一致，用户不会困惑。
//! - 趋势分桶按**本地时区**：`strftime('%Y-%m-%d %H:00', datetime(timestamp,'localtime'))`，
//!   否则 UTC+8 用户会整体偏移 8 小时。
//! - KPI「高峰时段 HH:00」取自今日本地时区分桶后的最高桶。

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};
use crate::{
    CauseKind, DetectionConfig, ProcessBrief, ProcessModule, RootCauseReport, Sample, Severity,
    StackSample, StutterEvent, WindowsEventRecord,
};
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
                crate::logger::local_today_bounds_utc().0
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
// =====================================================================
// F-RC15：分析结论落库（root_cause_reports，GUI 窄写权 + UPSERT 幂等）
// =====================================================================
/// 当前分析算法版本（写 root_cause_reports.algorithm_version，用于回溯审计 / 新旧结论对比）。
/// 算法升级后递增；GUI 读回旧结论时发现版本不一致，可提示用户重新生成。
pub const ANALYSIS_ALGO_VERSION: &str = "rc5-rc14.v1";

/// F-RC15：打开一个「窄写权」连接，仅供 **GUI** 写入 root_cause_reports 分析结论。
///
/// **边界（CONTEXT「GUI 对 stutter.db 只读，唯一例外」）**：这是 GUI 保存根因
/// 结论的窄写权入口，**CLI 与其他调用方禁止使用**——CLI 是纯查询界面
/// （ADR-0001 决策 4），只应经 `open_readonly` 读库。service 是唯一全量写库者。
///
/// 设计要点：
/// - 整个分析页其余读路径严格保持只读（`open_readonly`），不碰 samples / stutter_events；
/// - 本连接仅在用户点击「保存结论」时创建，只执行 root_cause_reports 的 UPSERT，
///   写完即弃，持有时间极短；
/// - WAL 模式下读写并发安全，service 写库不受影响。
pub fn open_report_writer(db_path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;");
    Ok(conn)
}

/// F-RC15：幂等确保 root_cause_reports 表存在（兼容旧库 / 旧版本 GUI 首次写结论）。
/// 与 logger 侧共享同一 DDL 定义（crate::logger），避免 SQL 重复。
pub fn ensure_report_table(conn: &Connection) -> anyhow::Result<()> {
    crate::logger::ensure_root_cause_report_table(conn)
}

/// F-RC15：UPSERT 一条分析结论。按 event_id 幂等（一事件一条），algorithm_version 记录
/// 算法版本——重算后覆盖旧结论，读回可对比新旧（PRD §5.1/R12）。
/// 与 logger 侧共享同一 UPSERT 语句（crate::logger），避免 SQL 重复。
pub fn save_root_cause_report(conn: &Connection, report: &RootCauseReport) -> anyhow::Result<()> {
    crate::logger::upsert_root_cause_report(conn, report)
}

// =====================================================================
// F-RC16：软件根因回溯数据（root_cause_reports + 三张子表）
// =====================================================================
/// 某事件的软件根因回溯数据：已保存结论 + 进程模块 / 事件日志 / 调用栈热点。
#[derive(Debug, Clone, Default)]
pub struct SoftwareRootCauseData {
    /// 已保存的分析结论（无则为 None）
    pub report: Option<RootCauseReport>,
    /// 进程已加载模块快照（F-RC14-b）
    pub modules: Vec<ProcessModule>,
    /// 白名单命中的事件日志（F-RC14-c）
    pub win_events: Vec<WindowsEventRecord>,
    /// 调用栈热点（F-RC14-d）
    pub stack_samples: Vec<StackSample>,
}

/// F-RC16：回读某事件的软件根因回溯数据（结论 + 三张子表），全程只读。
/// 表不存在（旧库）时优雅返回默认空结构，不报错。
pub fn load_software_root_cause(
    conn: &Connection,
    event_id: i64,
) -> anyhow::Result<SoftwareRootCauseData> {
    let mut out = SoftwareRootCauseData::default();
    let report_sql = "SELECT event_id, algorithm_version, primary_cause, confidence, cause_chain, software_root_cause, baseline_delta, computed_at FROM root_cause_reports WHERE event_id = ?1";
    if let Ok(mut stmt) = conn.prepare_cached(report_sql) {
        let mut rows = stmt.query(params![event_id])?;
        if let Some(row) = rows.next()? {
            out.report = Some(RootCauseReport {
                event_id: row.get(0)?,
                algorithm_version: row.get(1)?,
                primary_cause: row.get(2)?,
                confidence: row.get::<_, f64>(3)? as f32,
                cause_chain: serde_json::from_str(&row.get::<_, String>(4)?).unwrap_or_default(),
                software_root_cause: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or(serde_json::Value::Null),
                baseline_delta: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or(serde_json::Value::Null),
                computed_at: row.get(7)?,
            });
        }
    }
    let modules_sql = "SELECT pid, process_name, module_path, module_size FROM process_modules WHERE event_id = ?1 ORDER BY module_size DESC";
    if let Ok(mut stmt) = conn.prepare_cached(modules_sql) {
        let mut rows = stmt.query(params![event_id])?;
        while let Some(row) = rows.next()? {
            out.modules.push(ProcessModule {
                pid: row.get(0)?,
                process_name: row.get(1)?,
                module_path: row.get(2)?,
                module_size: row.get::<_, i64>(3)? as u64,
            });
        }
    }
    let win_sql = "SELECT channel, provider, win_event_id, level, message, ts FROM windows_events WHERE event_id = ?1 ORDER BY ts DESC";
    if let Ok(mut stmt) = conn.prepare_cached(win_sql) {
        let mut rows = stmt.query(params![event_id])?;
        while let Some(row) = rows.next()? {
            out.win_events.push(WindowsEventRecord {
                channel: row.get(0)?,
                provider: row.get(1)?,
                win_event_id: row.get(2)?,
                level: row.get(3)?,
                message: row.get(4)?,
                ts: row.get(5)?,
            });
        }
    }
    let stack_sql = "SELECT pid, process_name, module, rva, sample_count FROM stack_samples WHERE event_id = ?1 ORDER BY sample_count DESC";
    if let Ok(mut stmt) = conn.prepare_cached(stack_sql) {
        let mut rows = stmt.query(params![event_id])?;
        while let Some(row) = rows.next()? {
            out.stack_samples.push(StackSample {
                pid: row.get(0)?,
                process_name: row.get(1)?,
                module: row.get(2)?,
                rva: row.get::<_, i64>(3)? as u64,
                sample_count: row.get::<_, i64>(4)? as u64,
            });
        }
    }
    Ok(out)
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

/// 任意区间的 KPI 汇总（`load_kpi_today` 的泛化版本，ADR-0001：CLI analysis 复用）。
///
/// 四步口径与「今日」版完全一致，只是把范围参数化：
/// 1) 区间事件数（`today_count` 字段承载；范围 = Today 时即「今日卡顿 N 次」，
///    与悬浮窗 `event_count_today` 共用 `local_today_bounds_utc()` 单一来源）；
/// 2) 区间最严重一次持续时长；
/// 3) 高峰时段（区间本地时区分桶取次数最多桶的 HH:00）；
/// 4) 头号元凶（区间事件 culprits 按进程名计数取 Top1，复用 F2 聚合）。
pub fn load_kpi(conn: &Connection, range: &TimeRange) -> anyhow::Result<KpiSummary> {
    let (start, end) = range.bounds();

    // 1) 区间次数（Today 范围时与 reader.event_count_today / logger 一致）
    let today_count: u32 = conn
        .query_row(
            "SELECT COUNT(*) FROM stutter_events WHERE timestamp BETWEEN ?1 AND ?2",
            params![start, end],
            |row| row.get::<_, i64>(0).map(|n| n as u32),
        )
        .unwrap_or(0);

    // 2) 最严重一次持续时长
    let worst_duration_ms: u64 = conn
        .query_row(
            "SELECT COALESCE(MAX(duration_ms), 0) FROM stutter_events \
             WHERE timestamp BETWEEN ?1 AND ?2",
            params![start, end],
            |row| row.get::<_, i64>(0).map(|n| n as u64),
        )
        .unwrap_or(0);

    // 3) 高峰时段：区间本地时区分桶取次数最多桶的 HH:00
    let peak_hour = {
        let trend = load_trend(conn, range, TrendBucket::Hour).unwrap_or_default();
        trend
            .iter()
            .max_by_key(|p| p.count)
            .and_then(|p| {
                // bucket 形如 "YYYY-MM-DD HH:00"，取空格后 "HH:00"
                p.bucket.split(' ').nth(1).map(|h| h.to_string())
            })
            .unwrap_or_else(|| "—".to_string())
    };

    // 4) 头号元凶：区间事件 culprits 按进程名计数取 Top1（复用 F2 聚合）。
    //    旧库无 culprits 列 → load_culprits 回退空 → 取 "—"。
    let top_culprit = load_culprits(conn, range, 1)
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

/// 今日 KPI 汇总（基础模式 4 卡片）——`load_kpi` 的「今日」薄包装。
///
/// 全部按「今日」口径：today_count 与悬浮窗 `event_count_today` 共用
/// `local_today_bounds_utc()`（本地零点 → 现在），保证两处「今日卡顿 N 次」一致。
pub fn load_kpi_today(conn: &Connection) -> anyhow::Result<KpiSummary> {
    load_kpi(conn, &TimeRange::Today)
}

/// samples 表单行的读视图（区间查询返回结构，ADR-0001：CLI `samples` 子命令复用）。
///
/// 与 `samples` 表列一一对应（不含 `top_processes`——该列不落库）；
/// 数值列保留 `Option` 忠实呈现 NULL（旧库迁移列可能无值），由调用方决定回退。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SampleRow {
    /// 采样时刻（库内原文：UTC RFC3339）
    pub timestamp: String,
    pub cpu_usage: Option<f64>,
    pub cpu_freq_mhz: Option<f64>,
    pub mem_usage_percent: Option<f64>,
    pub mem_used_mb: Option<i64>,
    pub mem_total_mb: Option<i64>,
    pub mem_available_mb: Option<i64>,
    pub swap_usage_percent: Option<f64>,
    pub disk_read_bps: Option<i64>,
    pub disk_write_bps: Option<i64>,
    pub disk_busy_percent: Option<f64>,
    pub disk_avg_io_ms: Option<f64>,
    pub net_sent_bps: Option<i64>,
    pub net_recv_bps: Option<i64>,
    pub net_sent_total: Option<i64>,
    pub net_recv_total: Option<i64>,
    pub gpu_usage: Option<f64>,
    pub cpu_temp: Option<f64>,
    pub gpu_temp: Option<f64>,
    pub process_count: Option<i64>,
    pub thread_count: Option<i64>,
    pub commit_bytes: Option<i64>,
    pub commit_limit: Option<i64>,
    pub page_reads_per_sec: Option<f64>,
    pub dpc_percent: Option<f64>,
    pub interrupt_percent: Option<f64>,
    pub context_switches_per_sec: Option<i64>,
}

/// 读取时间范围内**最新 `limit` 条**采样，返回时转为**时间升序**（ADR-0001：CLI 复用）。
///
/// - 样本量大（1Hz、保留 30 天），SQL 层 `ORDER BY timestamp DESC LIMIT` 截取
///   最新 N 条后反转为升序，内存占用不随范围增长；
/// - 与 events 的「最新 N 条 + 升序输出」语义保持一致（见 CLI `samples` help）；
/// - 全程只读，依赖 `idx_samples_ts` 索引（无索引时退化为全表扫描，可接受）。
pub fn load_samples_range(
    conn: &Connection,
    start: &str,
    end: &str,
    limit: usize,
) -> anyhow::Result<Vec<SampleRow>> {
    let mut stmt = conn.prepare(
        "SELECT timestamp, cpu_usage, cpu_freq_mhz, mem_usage_percent, mem_used_mb,
                mem_total_mb, mem_available_mb, swap_usage_percent,
                disk_read_bps, disk_write_bps, disk_busy_percent, disk_avg_io_ms,
                net_sent_bps, net_recv_bps, net_sent_total, net_recv_total,
                gpu_usage, cpu_temp, gpu_temp, process_count, thread_count,
                commit_bytes, commit_limit, page_reads_per_sec,
                dpc_percent, interrupt_percent, context_switches_per_sec
         FROM samples
         WHERE timestamp BETWEEN ?1 AND ?2
         ORDER BY timestamp DESC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![start, end, limit as i64], |row| {
        Ok(SampleRow {
            timestamp: row.get(0)?,
            cpu_usage: row.get(1)?,
            cpu_freq_mhz: row.get(2)?,
            mem_usage_percent: row.get(3)?,
            mem_used_mb: row.get(4)?,
            mem_total_mb: row.get(5)?,
            mem_available_mb: row.get(6)?,
            swap_usage_percent: row.get(7)?,
            disk_read_bps: row.get(8)?,
            disk_write_bps: row.get(9)?,
            disk_busy_percent: row.get(10)?,
            disk_avg_io_ms: row.get(11)?,
            net_sent_bps: row.get(12)?,
            net_recv_bps: row.get(13)?,
            net_sent_total: row.get(14)?,
            net_recv_total: row.get(15)?,
            gpu_usage: row.get(16)?,
            cpu_temp: row.get(17)?,
            gpu_temp: row.get(18)?,
            process_count: row.get(19)?,
            thread_count: row.get(20)?,
            commit_bytes: row.get(21)?,
            commit_limit: row.get(22)?,
            page_reads_per_sec: row.get(23)?,
            dpc_percent: row.get(24)?,
            interrupt_percent: row.get(25)?,
            context_switches_per_sec: row.get(26)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    // DESC 取最新 N 条 → 反转为时间升序（与 events 输出语义一致）
    out.reverse();
    Ok(out)
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
/// F3（高级）：把 [start, end] 区间内 1Hz 采样降采样为 `ResourceData`（桶聚合 + 卡顿竖线）。
///
/// 抽成私有 helper，供 `load_resource_samples`（按 `TimeRange`）与
/// `load_resource_samples_window`（按事件中心 ±N 秒窄窗口，F-RC10 前导曲线）共用，消除重复。
fn downsample_samples(
    conn: &Connection,
    start: &str,
    end: &str,
    width_px: u32,
) -> anyhow::Result<ResourceData> {
    let base_dt = DateTime::parse_from_rfc3339(start)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    let end_dt = DateTime::parse_from_rfc3339(end)
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

pub fn load_resource_samples(
    conn: &Connection,
    range: &TimeRange,
    width_px: u32,
) -> anyhow::Result<ResourceData> {
    let (start, end) = range.bounds();
    downsample_samples(conn, &start, &end, width_px)
}

/// F-RC10：取某次卡顿事件前后 ±`half_secs` 秒的窄窗口资源采样，供钻取卡「前导曲线」渲染。
///
/// 与 `load_resource_samples` 共用 `downsample_samples` 降采样逻辑；窗口以事件时刻
/// （epoch 秒）为中心，对窄窗口用更细的桶（仍为 1Hz 友好量级）。
pub fn load_resource_samples_window(
    conn: &Connection,
    center_ts_secs: i64,
    half_secs: i64,
    width_px: u32,
) -> anyhow::Result<ResourceData> {
    use chrono::TimeZone;
    let start = Utc
        .timestamp_opt(center_ts_secs.saturating_sub(half_secs), 0)
        .single()
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());
    let end = Utc
        .timestamp_opt(center_ts_secs.saturating_add(half_secs), 0)
        .single()
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());
    downsample_samples(conn, &start, &end, width_px)
}

// ===================== M5 / F5+F8：原始事件表 + CSV 导出 =====================

// ===================== 卡顿根因分析（PRD §6.5 / F-RC5~F-RC13）纯函数层 =====================
//
// 以下函数均为**纯函数**：输入 Slice/引用 → 输出，无 I/O、无 DB、不修改入参。
// 供高级模式根因分析面板在内存中对历史事件做聚合/归因/画像，不写库、不改 service。

/// 温度→降频根因判定的「负载下限」（%，与 detector.rs 的 `THERMAL_LOAD_MIN_USAGE` 一致）：
/// 降频仅在负载下才有归因意义。what-if 单帧无历史频率峰值，故以「存在频率读数 + 负载」表征掉档。
const WHATIF_THERMAL_LOAD_MIN_USAGE: f32 = 50.0;

/// F-RC11：主因相对次因「明显领先」的判定阈值。
const LEADING_GAP_MS: i64 = 1000; // 首触时间差 >= 1s 视为明显领先
const LEADING_RATIO: f64 = 1.5; // 主因强度/次因强度 >= 1.5 视为明显领先
/// F-RC7：进程作为元凶出现次数 >= 该值才算稳定基线，否则降级为噪声/绝对阈值。
const MIN_BASELINE_APPEARANCES: u32 = 3;
/// F-RC7：偏离倍数阈值，当前占用 > 基线 × 该值才标显著偏离。
const DEVIATION_FACTOR: f32 = 1.5;

/// F-RC5：主因加权归因结果（单进程聚合）。
#[derive(Debug, Clone, PartialEq)]
pub struct WeightedCulprit {
    /// 进程名
    pub name: String,
    /// 累加权重 = Σ(duration_norm × 主因信号强度)
    pub weight: f64,
    /// 作为元凶出现次数
    pub count: u32,
    /// 关联卡顿累计时长（ms）
    pub total_duration_ms: u64,
}

/// 取某 cause 的归一化越阈幅度（0..~2），供 F-RC5/F-RC11 复用。
///
/// 按 cause 从 snapshot 取对应指标的归一化强度（不乘 severity，否则重复计数 cause 数）。
fn cause_strength(event: &StutterEvent, kind: CauseKind) -> f64 {
    let s = &event.snapshot;
    match kind {
        CauseKind::CpuHigh | CauseKind::CpuSpike => (s.cpu_usage / 100.0) as f64,
        CauseKind::MemLow => (s.mem_usage_percent / 100.0) as f64,
        CauseKind::DiskBusy => (s.disk_busy_percent / 100.0) as f64,
        CauseKind::DpcInterrupt | CauseKind::InterruptStorm => {
            (s.dpc_percent.max(s.interrupt_percent) / 100.0) as f64
        }
        CauseKind::ThermalThrottle => s.cpu_temp.map_or(0.5, |t| (t / 100.0) as f64),
        // UiFrozen/NetSpike/DiskSpike/GpuHigh/None 等无可量化瞬时强度 → 取居中 0.5
        _ => 0.5,
    }
}

/// 取事件主因的归一化强度（无主因时取居中 0.5）。
fn primary_strength(event: &StutterEvent) -> f64 {
    match event.primary_cause {
        Some(k) => cause_strength(event, k),
        None => 0.5,
    }
}

/// F-RC5：按 `duration × 主因信号强度` 累加每进程权重，按 name 聚合、按 weight 降序取前 limit。
///
/// - `set_max_duration` = 事件中最大 duration（最小 1 防除零）；`duration_norm(e) = e.duration_ms / set_max_duration`。
/// - `primary_strength(e)` 取主因相对阈值的归一化越阈幅度（见 [`cause_strength`]，0..~2）。
/// - `weight(e, culprit) = duration_norm(e) × primary_strength(e)`；不乘 severity，避免与 cause 数重复计数。
pub fn weighted_culprits(events: &[StutterEvent], limit: usize) -> Vec<WeightedCulprit> {
    if events.is_empty() {
        return Vec::new();
    }
    let set_max_duration = events
        .iter()
        .map(|e| e.duration_ms)
        .max()
        .unwrap_or(0)
        .max(1);
    let mut agg: HashMap<String, WeightedCulprit> = HashMap::new();
    for e in events {
        let duration_norm = e.duration_ms as f64 / set_max_duration as f64;
        let w = duration_norm * primary_strength(e);
        for c in &e.culprits {
            let entry = agg.entry(c.name.clone()).or_insert_with(|| WeightedCulprit {
                name: c.name.clone(),
                weight: 0.0,
                count: 0,
                total_duration_ms: 0,
            });
            entry.weight += w;
            entry.count += 1;
            entry.total_duration_ms += e.duration_ms;
        }
    }
    let mut out: Vec<WeightedCulprit> = agg.into_values().collect();
    // 按权重降序（f64 用 partial_cmp 兜底，避免 NaN 导致 panic）
    out.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(limit);
    out
}

/// 按首触时刻升序排列事件的 cause_kinds（缺首触记 +∞ 排最末，枚举名兜底保证确定性）。
/// F-RC6 / F-RC9 / F-RC11 共用，避免重复排序闭包。
fn sorted_causes_by_first_touch(event: &StutterEvent) -> Vec<(CauseKind, i64)> {
    let mut kinds: Vec<(CauseKind, i64)> = event
        .cause_kinds
        .iter()
        .map(|k| (*k, event.cause_first_touch.get(k).copied().unwrap_or(i64::MAX)))
        .collect();
    kinds.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| format!("{:?}", a.0).cmp(&format!("{:?}", b.0)))
    });
    kinds
}

/// F-RC6：因果方向——用各 cause 首触时刻定位触发者（最小偏移）与放大器。
///
/// - `trigger` = `cause_kinds` 中 `cause_first_touch` 偏移最小且存在的那一个。
/// - `amplifiers` = 其余按偏移升序。某 `cause_kinds` 项若无 `first_touch` 记 `+∞`（视为最晚）。
/// - 纯函数层仅依赖事件自身字段；bulk 窗口回退由调用方在真实流程中处理（不在本函数签名内）。
pub fn causal_direction(event: &StutterEvent) -> (Option<CauseKind>, Vec<CauseKind>) {
    let offsets = sorted_causes_by_first_touch(event);
    let trigger = offsets.first().map(|(k, _)| *k);
    let amplifiers: Vec<CauseKind> = offsets.iter().skip(1).map(|(k, _)| *k).collect();
    (trigger, amplifiers)
}

/// F-RC7：进程基线（某进程作为元凶出现时的典型占用）。
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessBaseline {
    /// 进程名
    pub name: String,
    /// 典型 CPU 占用（该进程作为元凶时 cpu_usage 的均值，代表常态而非峰值）
    pub typical_cpu: f32,
    /// 典型内存占用 MB（均值）
    pub typical_mem_mb: f64,
    /// 出现次数（用于区分稳定基线 vs 偶发噪声）
    pub appearances: u32,
}

/// F-RC7：从历史事件 culprits 聚合每进程作为元凶时的典型占用（typical = 均值）。
///
/// 用均值而非最大值：基线代表「常态」，当前值才可能超过基线 × factor 被标为显著偏离；
/// 若取历史最大值作基线，则当前占用几乎永远 <= 基线，偏离判定近乎永不触发（逻辑失效）。
pub fn process_baseline(events: &[StutterEvent]) -> HashMap<String, ProcessBaseline> {
    // 累加 sum + count，最后算均值（避免每步重算）
    let mut agg: HashMap<String, (f32, f64, u32)> = HashMap::new(); // (sum_cpu, sum_mem_mb, count)
    for e in events {
        for c in &e.culprits {
            let entry = agg.entry(c.name.clone()).or_insert((0.0, 0.0, 0));
            entry.0 += c.cpu_usage;
            entry.1 += c.mem_used_mb as f64;
            entry.2 += 1;
        }
    }
    agg.into_iter()
        .map(|(name, (sum_cpu, sum_mem, count))| {
            (
                name.clone(),
                ProcessBaseline {
                    name,
                    typical_cpu: if count > 0 { sum_cpu / count as f32 } else { 0.0 },
                    typical_mem_mb: if count > 0 { sum_mem / count as f64 } else { 0.0 },
                    appearances: count,
                },
            )
        })
        .collect()
}

/// F-RC7：对给定事件每个 culprit 计算偏离倍率与显著性。
///
/// - `multiple = culprit.cpu_usage / baseline.typical_cpu`（baseline 存在且 typical>0）。
/// - `significant = appearances>=3 且 multiple>1.5`；baseline 缺失或 `appearances<3`
///   → `significant=false`（降级绝对阈值，标注噪声，避免单次偶发误判）。
pub fn deviation_flags(
    event: &StutterEvent,
    baseline: &HashMap<String, ProcessBaseline>,
) -> Vec<(String, f32, bool)> {
    let mut out = Vec::new();
    for c in &event.culprits {
        let (multiple, significant) = match baseline.get(&c.name) {
            Some(b) => {
                let multiple = if b.typical_cpu > 0.0 {
                    c.cpu_usage / b.typical_cpu
                } else {
                    c.cpu_usage
                };
                // 出现不足 MIN_BASELINE_APPEARANCES 次视为噪声，不标显著（即使倍率高）
                let significant = b.appearances >= MIN_BASELINE_APPEARANCES && multiple > DEVIATION_FACTOR;
                (multiple, significant)
            }
            // baseline 缺失 → 标注噪声，significant=false
            None => (0.0, false),
        };
        out.push((c.name.clone(), multiple, significant));
    }
    out
}

/// F-RC8：进程共现对（cfg 无序，a<b 排序避免重复对）。
#[derive(Debug, Clone, PartialEq)]
pub struct CoOccurrence {
    /// 进程名对（已按字典序排序，保证无序且去重）
    pub pair: (String, String),
    /// 共同出现频次
    pub count: u32,
}

/// F-RC8：统计每对进程名在事件中共同出现的频次，按 count 降序、去重返回。
///
/// 同事件内同名进程只计一次对；无序对按 `(a,b)`（a<=b）归并，避免 (A,B)/(B,A) 重复。
pub fn cooccurrence_pairs(events: &[StutterEvent]) -> Vec<CoOccurrence> {
    let mut tally: HashMap<(String, String), u32> = HashMap::new();
    for e in events {
        // 同事件内按名字去重（同名不同 PID 视为同一进程）
        let mut names: Vec<String> = e.culprits.iter().map(|c| c.name.clone()).collect();
        names.sort();
        names.dedup();
        for i in 0..names.len() {
            for j in (i + 1)..names.len() {
                let (a, b) = (names[i].clone(), names[j].clone());
                let pair = if a <= b { (a, b) } else { (b, a) };
                *tally.entry(pair).or_insert(0) += 1;
            }
        }
    }
    let mut out: Vec<CoOccurrence> = tally
        .into_iter()
        .map(|(pair, count)| CoOccurrence { pair, count })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count));
    out
}

/// F-RC9：因果链——多 cause 按首触时刻升序排成有向链（根因→传导→表象）。
///
/// 即 `cause_kinds` 按 `cause_first_touch` 偏移升序；缺首触记 `+∞`（排最末）。
pub fn cause_chain(event: &StutterEvent) -> Vec<CauseKind> {
    sorted_causes_by_first_touch(event)
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

/// F-RC11：根因置信度（0..1, 中文标签）。
///
/// 不按 cause 数量压低（多因并发是 major/critical 的定义本身，数量多≠置信低）。
/// - 单 cause → `(0.9, "高")`。
/// - 多 cause：算 primary 首触与次因首触差 Δt、primary 强度比；
///   primary 明显领先（Δt>=1000ms 或强度比>=1.5）→ `(0.7, "较高")`；
///   否则 → `(0.35, "主因不显著，疑多因并发")`。
pub fn root_cause_confidence(event: &StutterEvent) -> (f32, &'static str) {
    if event.cause_kinds.len() <= 1 {
        return (0.9, "高");
    }
    // 按首触偏移升序（共用 helper，保证与 F-RC6/F-RC9 排序一致）
    let sorted = sorted_causes_by_first_touch(event);
    let primary = event.primary_cause.unwrap_or(sorted.first().map(|(k, _)| *k).unwrap_or(CauseKind::UiFrozen));
    let primary_touch = event
        .cause_first_touch
        .get(&primary)
        .copied()
        .or_else(|| sorted.first().map(|(_, t)| *t))
        .unwrap_or(0);
    // 次因 = 排序后第一个非 primary 的首触
    let second_touch = sorted.iter().find(|(k, _)| *k != primary).map(|(_, t)| *t);
    // Δt：primary 相对次因的领先量（primary 先行者时为正）
    let dt = match second_touch {
        Some(t) if t != i64::MAX => t - primary_touch,
        _ => 0,
    };
    // primary 强度比 = primary_strength / 次因强度
    let ps = primary_strength(event);
    let ss = sorted
        .iter()
        .find(|(k, _)| *k != primary)
        .map(|(k, _)| cause_strength(event, *k))
        .unwrap_or(ps);
    let ratio = if ss > 0.0 { ps / ss } else { ps };

    if dt >= LEADING_GAP_MS || ratio >= LEADING_RATIO {
        (0.7, "较高")
    } else {
        (0.35, "主因不显著，疑多因并发")
    }
}

/// F-RC12：what-if 纯客户端重算——用单帧 Sample + 可调阈值判定「若阈值 X 是否会触发该 cause」。
///
/// 覆盖瞬时硬阈值类 cause：CPU / Mem(usage+available) / Commit / Paging / DiskBusy / DPC /
/// Interrupt / CtxSwitch / Thermal。不含 spike 类（需历史基线，what-if 不做）。
/// 返回文案前缀与 detector `check_hard_thresholds` 对齐（如 "CPU usage"/"Memory usage"/
/// "Available memory"/"Commit charge"/"Memory paging"/"Disk busy"/"DPC time"/"Interrupt time"/
/// "Context switches"/"Thermal throttle"），便于上层比对。
///
/// 注意语义差异（what-if 仅用单帧，无 detector 的运行时状态）：
/// - CPU/内存/磁盘/DPC/中断/上下文：用瞬时 `>` 阈值判定，忽略 detector 的滞回状态机。
/// - Thermal：单帧无历史频率峰值，以「温度超阈 + 有频率读数 + 负载」作为降频代理判据，
///   与 detector 的「频率 < 峰值×比例」语义不同（what-if 无法获知历史峰值）。
///
/// 注：本函数已随 ADR-0001 下沉到 core（原住 UI crate），达成 PRD §5.1「分析层共用
/// 纯函数以消除阈值漂移（R6/R8）」的期望位置；UI 的 what-if 面板与后续 CLI 均直接调用。
pub fn detect_core(sample: &Sample, cfg: &DetectionConfig) -> Vec<String> {
    let mut causes = Vec::new();

    // CPU：瞬时 > 阈值（忽略滞回）
    if sample.cpu_usage > cfg.cpu_threshold {
        causes.push(format!(
            "CPU usage {:.1}% > {}%",
            sample.cpu_usage, cfg.cpu_threshold
        ));
    }
    // 可用内存不足（绝对下限）
    if sample.mem_available_mb < cfg.mem_threshold_mb {
        causes.push(format!(
            "Available memory {}MB < {}MB",
            sample.mem_available_mb, cfg.mem_threshold_mb
        ));
    }
    // 内存使用率过高（百分比口径，与可用下限为「或」关系）
    if sample.mem_usage_percent > cfg.mem_threshold_percent {
        causes.push(format!(
            "Memory usage {:.1}% > {}%",
            sample.mem_usage_percent, cfg.mem_threshold_percent
        ));
    }
    // 提交电荷（commit charge）：阶段 E 起降级为「压力证据」，不再作为独立
    // cause（与实时判定 detector.rs 保持一致）——commit 高只是记账上限逼近，
    // 本身对性能零影响；真出问题时由可用内存低/分页信号触发。
    let commit_ratio = if sample.commit_limit > 0 {
        sample.commit_bytes as f64 / sample.commit_limit as f64 * 100.0
    } else {
        0.0
    };
    // 分页活动速率（真正的 swap 卡顿信号）；与实时判定共用同一证据方法，
    // 需存在内存/磁盘压力证据（阶段 B 起为放大器口径，两处不漂移）。
    if sample.page_reads_per_sec > cfg.page_reads_threshold
        && cfg.paging_has_pressure_evidence(commit_ratio, sample)
    {
        causes.push(format!(
            "Memory paging {:.1}/s > {}/s",
            sample.page_reads_per_sec, cfg.page_reads_threshold
        ));
    }
    // 磁盘真繁忙度（% Disk Time 或 单次 IO 延迟，任一超阈值）
    if sample.disk_busy_percent > cfg.disk_busy_threshold_percent
        || sample.disk_avg_io_ms > cfg.disk_io_threshold_ms
    {
        causes.push(format!(
            "Disk busy {:.1}% (IO {:.1}ms)",
            sample.disk_busy_percent, sample.disk_avg_io_ms
        ));
    }
    // % DPC Time
    if sample.dpc_percent > cfg.dpc_threshold_percent {
        causes.push(format!(
            "DPC time {:.1}% > {}%",
            sample.dpc_percent, cfg.dpc_threshold_percent
        ));
    }
    // % Interrupt Time
    if sample.interrupt_percent > cfg.interrupt_threshold_percent {
        causes.push(format!(
            "Interrupt time {:.1}% > {}%",
            sample.interrupt_percent, cfg.interrupt_threshold_percent
        ));
    }
    // Context Switches/sec（机器相对口径，与实时判定 detector.rs 一致：按逻辑核
    // 归一 + CPU 侧压力证据；归一分母共用 types::logical_core_count，不漂移）。
    // 单帧限制：无滞回状态，证据门只能逐帧生效——实时判定中「带内滞回保持」
    // 的帧在此可能结论不同，属 what-if 单帧重算的固有限制（见标准 §3 第 10 行）。
    let ctx_cores = crate::types::logical_core_count(sample) as f32;
    let ctx_per_core = sample.context_switches_per_sec / ctx_cores;
    if ctx_per_core > cfg.context_switch_threshold_per_core
        && cfg.ctx_switch_has_pressure_evidence(sample)
    {
        causes.push(format!(
            "Context switches {:.0}/s = {:.0}/core > {:.0}/core",
            sample.context_switches_per_sec,
            ctx_per_core,
            cfg.context_switch_threshold_per_core
        ));
    }
    // Thermal：温度高 + 负载 + 有频率读数（单帧无历史峰值，以存在频率读数表征掉档）
    if let Some(t) = sample.cpu_temp {
        if t > cfg.thermal_threshold_celsius
            && sample.cpu_freq_mhz.is_some()
            && sample.cpu_usage > WHATIF_THERMAL_LOAD_MIN_USAGE
        {
            causes.push(format!(
                "Thermal throttle: CPU {:.0}°C, freq {:.0}MHz",
                t,
                sample.cpu_freq_mhz.unwrap_or(0.0)
            ));
        }
    }
    causes
}

/// F-RC13：同类事件画像（按 signature 聚类）。
#[derive(Debug, Clone, PartialEq)]
pub struct EventProfile {
    /// 画像签名：排序后 cause_kinds 名 + "|" + 排序后 culprit 名 + "|" + duration 桶（短/中/长）
    pub signature: String,
    /// 该类事件出现次数
    pub count: u32,
    /// 平均持续时长（ms）
    pub avg_duration_ms: f64,
}

/// 计算单事件 signature（供聚类与匹配复用）。
fn signature_of(e: &StutterEvent) -> String {
    let mut kinds: Vec<String> = e.cause_kinds.iter().map(|k| format!("{:?}", k)).collect();
    kinds.sort();
    let mut culprits: Vec<String> = e.culprits.iter().map(|c| c.name.clone()).collect();
    culprits.sort();
    let bucket = if e.duration_ms < 1000 {
        "短"
    } else if e.duration_ms <= 5000 {
        "中"
    } else {
        "长"
    };
    format!("{}|{}|{}", kinds.join("|"), culprits.join("|"), bucket)
}

/// F-RC13：按 signature 聚类事件，返回按 count 降序的画像列表。
pub fn cluster_profiles(events: &[StutterEvent]) -> Vec<EventProfile> {
    let mut tally: HashMap<String, (u32, u64)> = HashMap::new(); // sig -> (count, total_duration)
    for e in events {
        let sig = signature_of(e);
        let entry = tally.entry(sig).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += e.duration_ms;
    }
    let mut out: Vec<EventProfile> = tally
        .into_iter()
        .map(|(signature, (count, total))| EventProfile {
            signature,
            count,
            avg_duration_ms: if count > 0 {
                total as f64 / count as f64
            } else {
                0.0
            },
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count));
    out
}

/// F-RC13：对给定事件匹配已知画像（count>=2 的 profile 才算「已知」），返回人话结论。无匹配返回 None。
pub fn match_profile(event: &StutterEvent, profiles: &[EventProfile]) -> Option<String> {
    let sig = signature_of(event);
    let known = profiles.iter().find(|p| p.count >= 2 && p.signature == sig)?;
    let avg_s = known.avg_duration_ms / 1000.0;
    let proc_names: Vec<String> = {
        let mut v: Vec<String> = event.culprits.iter().map(|c| c.name.clone()).collect();
        v.sort();
        v.dedup();
        v
    };
    let proc = if proc_names.is_empty() {
        "未知进程".to_string()
    } else {
        proc_names.join("、")
    };
    Some(format!(
        "匹配已知画像：进程{}典型卡顿（{} 次，平均 {:.1}s）",
        proc, known.count, avg_s
    ))
}

/// F5/F8：单条卡顿事件明细（供高级模式原始事件表与 CSV 导出）。
#[derive(Debug, Clone, PartialEq)]
pub struct EventRow {
    /// 事件主键（stutter_events.id）；用于钻取卡精准关联被点事件（F-RC10）。
    pub id: i64,
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

/// F-RC10/11/13：回读时间范围内**完整** `StutterEvent`（含结构化 cause_kinds /
/// primary_cause / cause_first_touch / snapshot / culprits / onset_ts），供根因钻取卡与
/// 同类画像对比在内存中计算。全程只读；service 已落库这些列（F-RC1 迁移），本函数不写库。
///
/// 各结构化列容错：旧库缺列时回退默认空值（不崩）。`primary_cause` 列存储为 JSON
/// （`null` 或枚举字符串），解析失败时按 `None` 处理。
pub fn load_full_events(
    conn: &Connection,
    range: &TimeRange,
) -> anyhow::Result<Vec<StutterEvent>> {
    let (start, end) = range.bounds();
    let has_causes = has_column(conn, "stutter_events", "causes");
    let has_culprits = has_column(conn, "stutter_events", "culprits");
    let has_kinds = has_column(conn, "stutter_events", "cause_kinds");
    let has_primary = has_column(conn, "stutter_events", "primary_cause");
    let has_touch = has_column(conn, "stutter_events", "cause_first_touch");
    let has_snapshot = has_column(conn, "stutter_events", "snapshot");
    let has_onset = has_column(conn, "stutter_events", "onset_ts");

    let causes_sql = if has_causes { "COALESCE(causes, '[]')" } else { "'[]'" };
    let culprits_sql = if has_culprits {
        "COALESCE(culprits, '[]')"
    } else {
        "'[]'"
    };
    let kinds_sql = if has_kinds {
        "COALESCE(cause_kinds, '[]')"
    } else {
        "'[]'"
    };
    let primary_sql = if has_primary { "primary_cause" } else { "NULL" };
    let touch_sql = if has_touch {
        "COALESCE(cause_first_touch, '{}')"
    } else {
        "'{}'"
    };
    let snapshot_sql = if has_snapshot {
        "COALESCE(snapshot, '{}')"
    } else {
        "'{}'"
    };
    let onset_sql = if has_onset { "onset_ts" } else { "0" };

    let sql = format!(
        "SELECT id, timestamp, duration_ms, severity, {causes_sql}, {culprits_sql}, \
         {kinds_sql}, {primary_sql}, {touch_sql}, {snapshot_sql}, {onset_sql} \
         FROM stutter_events WHERE timestamp BETWEEN ?1 AND ?2 ORDER BY timestamp"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![start, end], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)? as u64,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, i64>(10)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (id, ts, dur, sev, causes_json, culprits_json, kinds_json, primary_json, touch_json, snapshot_json, onset) =
            r?;
        let timestamp = DateTime::parse_from_rfc3339(&ts)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let causes: Vec<String> = serde_json::from_str(&causes_json).unwrap_or_default();
        let cause_kinds: Vec<CauseKind> = serde_json::from_str(&kinds_json).unwrap_or_default();
        let primary_cause: Option<CauseKind> = match primary_json {
            Some(s) if s != "null" => serde_json::from_str(&s).unwrap_or(None),
            _ => None,
        };
        let cause_first_touch: HashMap<CauseKind, i64> =
            serde_json::from_str(&touch_json).unwrap_or_default();
        let snapshot: Sample = serde_json::from_str(&snapshot_json).unwrap_or_default();
        let culprits: Vec<ProcessBrief> = serde_json::from_str(&culprits_json).unwrap_or_default();
        out.push(StutterEvent {
            id,
            timestamp,
            duration_ms: dur,
            severity: parse_severity(&sev),
            causes,
            cause_kinds,
            primary_cause,
            cause_first_touch,
            onset_ts: if onset <= 0 { None } else { Some(onset) },
            snapshot,
            culprits,
        });
    }
    Ok(out)
}

/// 把日志存储的 severity 字符串（"minor"/"major"/"critical"）解析回枚举。
fn parse_severity(s: &str) -> Severity {
    match s {
        "critical" => Severity::Critical,
        "major" => Severity::Major,
        _ => Severity::Minor,
    }
}

/// F-RC10/11/13：把 `CauseKind` 枚举映射为中文展示标签（与 detector 文案前缀一致）。
pub fn cause_kind_label(kind: CauseKind) -> &'static str {
    match kind {
        CauseKind::CpuHigh => "CPU 占用高",
        CauseKind::CpuSpike => "CPU 突增",
        CauseKind::MemLow => "内存不足",
        CauseKind::DiskBusy => "磁盘繁忙",
        CauseKind::DiskSpike => "磁盘突增",
        CauseKind::GpuHigh => "GPU 占用高",
        CauseKind::ThermalThrottle => "温度降频",
        CauseKind::DpcInterrupt => "DPC 风暴",
        CauseKind::InterruptStorm => "中断风暴",
        CauseKind::ContextSwitchStorm => "上下文切换风暴",
        CauseKind::NetSpike => "网络突增",
        CauseKind::UiFrozen => "界面冻结",
        CauseKind::ProcessHandleLeak => "句柄泄漏",
        CauseKind::HandleHigh => "句柄数偏高",
        CauseKind::GdiObjectLeak => "GDI 对象泄漏",
        CauseKind::DriverTimeout => "驱动超时",
        CauseKind::ServiceCrash => "服务崩溃",
        CauseKind::DiskIoError => "磁盘 I/O 错误",
        CauseKind::HardwareError => "硬件错误",
    }
}

/// F-RC15：把落库的稳定 CauseKind key（`{:?}` 变体名，如 "CpuHigh"）解析回枚举。
/// 与 [`cause_kind_label`] 成对：落库存 key、展示转 label，回读不丢类型。
pub fn cause_kind_from_key(key: &str) -> Option<CauseKind> {
    match key {
        "CpuHigh" => Some(CauseKind::CpuHigh),
        "CpuSpike" => Some(CauseKind::CpuSpike),
        "MemLow" => Some(CauseKind::MemLow),
        "DiskBusy" => Some(CauseKind::DiskBusy),
        "DiskSpike" => Some(CauseKind::DiskSpike),
        "GpuHigh" => Some(CauseKind::GpuHigh),
        "ThermalThrottle" => Some(CauseKind::ThermalThrottle),
        "DpcInterrupt" => Some(CauseKind::DpcInterrupt),
        "InterruptStorm" => Some(CauseKind::InterruptStorm),
        "ContextSwitchStorm" => Some(CauseKind::ContextSwitchStorm),
        "NetSpike" => Some(CauseKind::NetSpike),
        "UiFrozen" => Some(CauseKind::UiFrozen),
        "ProcessHandleLeak" => Some(CauseKind::ProcessHandleLeak),
        "HandleHigh" => Some(CauseKind::HandleHigh),
        "GdiObjectLeak" => Some(CauseKind::GdiObjectLeak),
        "DriverTimeout" => Some(CauseKind::DriverTimeout),
        "ServiceCrash" => Some(CauseKind::ServiceCrash),
        "DiskIoError" => Some(CauseKind::DiskIoError),
        "HardwareError" => Some(CauseKind::HardwareError),
        _ => None,
    }
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
        "SELECT id, timestamp, duration_ms, severity, {causes_sql}, {culprits_sql} \
         FROM stutter_events WHERE timestamp BETWEEN ?1 AND ?2 ORDER BY timestamp"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![start, end], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)? as u64,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;

    let mut out = Vec::new();
    for r in rows {
        let (id, ts, dur, sev, causes_json, culprits_json) = r?;
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
            id,
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
    use crate::{CauseKind, DetectionConfig, Logger, ProcessBrief, Sample, Severity, StorageConfig};
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
    /// `crate::logger::local_today_bounds_utc()`，保证与真实查询口径一致。
    fn local_midnight_utc() -> DateTime<Utc> {
        crate::logger::local_today_bounds_utc().0
    }

    /// 写入若干今日事件（不同 local 小时桶），返回 db 路径。
    fn seed_today(db: &str, hours: &[u32], severities: &[Severity]) {
        let cfg = StorageConfig {
            db_path: db.to_string(),
            retention_days: 30,
            event_retention_days: 30,
        };
        let mut logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();        let base = local_midnight_utc();
        for (i, (h, sev)) in hours.iter().zip(severities.iter()).enumerate() {
            let ts = base + ChronoDuration::hours(*h as i64) + ChronoDuration::minutes(i as i64);
            let mut s = Sample::default();
            s.cpu_usage = 95.0;
            let ev = crate::StutterEvent {
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
                    ..Default::default()
                }],
                ..Default::default()
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
            event_retention_days: 30,
        };
        let mut logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();
        let base = Local::now();
        for (i, sev) in severities.iter().enumerate() {
            // 10/9/8 分钟前（均在今日、过去），分布在同一本地小时桶内
            let ts = (base - ChronoDuration::minutes(10 - i as i64)).with_timezone(&Utc);
            let mut s = Sample::default();
            s.cpu_usage = 95.0;
            let ev = crate::StutterEvent {
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
                    ..Default::default()
                }],
                ..Default::default()
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
            event_retention_days: 30,
        };
        let mut logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();
        let base = local_midnight_utc();
        for (i, (cs, caz)) in culprits_per_event.iter().zip(causes_per_event.iter()).enumerate() {
            let ts = base + ChronoDuration::minutes(i as i64);
            let mut s = Sample::default();
            s.cpu_usage = 95.0;
            let ev = crate::StutterEvent {
                timestamp: ts,
                duration_ms: 1000 * (i + 1) as u64,
                severity: Severity::Minor,
                causes: caz.clone(),
                snapshot: s,
                culprits: cs.clone(),
                ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            event_retention_days: 30,
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
            event_retention_days: 30,
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
            event_retention_days: 30,
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
        let ev = crate::StutterEvent {
            timestamp: base + ChronoDuration::seconds(30),
            duration_ms: 1000,
            severity: Severity::Minor,
            causes: vec!["x".into()],
            snapshot: snap,
            culprits: vec![],
            ..Default::default()
        };
        {
            let cfg = StorageConfig {
                db_path: db.clone(),
                retention_days: 30,
                event_retention_days: 30,
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
        };
        let culprits = vec![vec![pb(1, "app.exe")], vec![pb(2, "svc.exe")], vec![pb(3, "bg.exe")]];
        let causes = vec![
            vec!["CPU usage 95.0% > 90.0%".to_string()],
            vec!["Network spike: 1B/s -> 3B/s".to_string()],
            vec!["Available memory 100MB < 500MB".to_string()],
        ];
        let db = unique_db("events_sort");
        {
            let cfg = StorageConfig { db_path: db.clone(), retention_days: 30, event_retention_days: 30 };
            let mut logger = Logger::new(&cfg).unwrap();
            logger.touch_heartbeat().unwrap();
            let base = local_midnight_utc();
            let sevs = [Severity::Minor, Severity::Major, Severity::Critical];
            let durs = [1000u64, 3000, 2000];
            for (i, (sev, dur)) in sevs.iter().zip(durs.iter()).enumerate() {
                let mut s = Sample::default();
                s.cpu_usage = 50.0;
                let ev = crate::StutterEvent {
                    timestamp: base + ChronoDuration::minutes(i as i64),
                    duration_ms: *dur,
                    severity: *sev,
                    causes: causes[i].clone(),
                    snapshot: s,
                culprits: culprits[i].clone(),
                ..Default::default()
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

    // ===================== F-RC5~F-RC13 纯函数单测 =====================

    /// 纯函数测试用：内存直接构造事件（不落库），复用 StutterEvent/Sample/ProcessBrief。
    fn mk_event(
        duration_ms: u64,
        cause_kinds: Vec<CauseKind>,
        primary_cause: Option<CauseKind>,
        first_touch: &[(CauseKind, i64)],
        snapshot: Sample,
        culprits: Vec<ProcessBrief>,
    ) -> crate::StutterEvent {
        let mut ft = HashMap::new();
        for (k, v) in first_touch {
            ft.insert(*k, *v);
        }
        crate::StutterEvent {
            duration_ms,
            cause_kinds,
            primary_cause,
            cause_first_touch: ft,
            snapshot,
            culprits,
            ..Default::default()
        }
    }

    #[test]
    fn rc5_weighted_culprits_orders_by_weight_and_aggregates() {
        // 3 个事件，primary_cause 不同、duration 不同：
        // e1(dur=5000, CpuHigh, cpu=100)、e2(dur=2000, MemLow, mem%=100)、e3(dur=1000, DiskBusy, disk%=100)
        // set_max=5000；appA 出现在 e1+e3 → 权重 1.0+0.2=1.2，count=2，total=6000
        //                          appB 出现在 e2   → 权重 0.4，count=1，total=2000
        let pb = |pid: u32, name: &str, cpu: f32, mem: u64| ProcessBrief {
            pid,
            name: name.to_string(),
            cpu_usage: cpu,
            mem_used_mb: mem,
            ..Default::default()
        };
        let mut s1 = Sample::default();
        s1.cpu_usage = 100.0;
        let mut s2 = Sample::default();
        s2.mem_usage_percent = 100.0;
        let mut s3 = Sample::default();
        s3.disk_busy_percent = 100.0;
        let e1 = mk_event(
            5000,
            vec![CauseKind::CpuHigh],
            Some(CauseKind::CpuHigh),
            &[],
            s1,
            vec![pb(1, "appA.exe", 90.0, 100)],
        );
        let e2 = mk_event(
            2000,
            vec![CauseKind::MemLow],
            Some(CauseKind::MemLow),
            &[],
            s2,
            vec![pb(2, "appB.exe", 50.0, 100)],
        );
        let e3 = mk_event(
            1000,
            vec![CauseKind::DiskBusy],
            Some(CauseKind::DiskBusy),
            &[],
            s3,
            vec![pb(3, "appA.exe", 10.0, 100)],
        );
        let events = [e1, e2, e3];
        let top = weighted_culprits(&events, 10);
        assert_eq!(top.len(), 2);
        // 按权重降序：appA(1.2) 在前
        assert_eq!(top[0].name, "appA.exe");
        assert!((top[0].weight - 1.2).abs() < 1e-9);
        assert_eq!(top[0].count, 2);
        assert_eq!(top[0].total_duration_ms, 6000);
        assert_eq!(top[1].name, "appB.exe");
        assert!((top[1].weight - 0.4).abs() < 1e-9);
        assert_eq!(top[1].count, 1);
        assert_eq!(top[1].total_duration_ms, 2000);
    }

    #[test]
    fn rc6_causal_direction_marks_trigger_and_amplifiers() {
        // cause_kinds=[MemLow, DiskBusy]，首触 {MemLow:0, DiskBusy:1200}
        let e = mk_event(
            2000,
            vec![CauseKind::MemLow, CauseKind::DiskBusy],
            Some(CauseKind::MemLow),
            &[(CauseKind::MemLow, 0), (CauseKind::DiskBusy, 1200)],
            Sample::default(),
            vec![],
        );
        let (trigger, amplifiers) = causal_direction(&e);
        assert_eq!(trigger, Some(CauseKind::MemLow));
        assert_eq!(amplifiers, vec![CauseKind::DiskBusy]);
    }

    #[test]
    fn rc7_deviation_flags_significant_only_when_baseline_stable_and_spiking() {
        let pb = |pid: u32, name: &str, cpu: f32, mem: u64| ProcessBrief {
            pid,
            name: name.to_string(),
            cpu_usage: cpu,
            mem_used_mb: mem,
            ..Default::default()
        };
        // appA.exe 出现 3 次（cpu 均为 50）→ baseline: typical_cpu=50, appearances=3
        // appB.exe 仅出现 1 次（appearances<3）
        let mut events: Vec<crate::StutterEvent> = (0..3)
            .map(|i| {
                let mut s = Sample::default();
                s.cpu_usage = 50.0;
                mk_event(
                    1000,
                    vec![],
                    None,
                    &[],
                    s,
                    vec![pb(100 + i, "appA.exe", 50.0, 100)],
                )
            })
            .collect();
        {
            let mut s = Sample::default();
            s.cpu_usage = 30.0;
            events.push(mk_event(
                1000,
                vec![],
                None,
                &[],
                s,
                vec![pb(200, "appB.exe", 30.0, 80)],
            ));
        }
        let baseline = process_baseline(&events);
        let a_base = baseline.get("appA.exe").unwrap();
        assert_eq!(a_base.appearances, 3);
        assert_eq!(a_base.typical_cpu, 50.0);

        // 测试事件：appA.exe cpu=100（= typical 的 2×，>1.5）→ significant=true
        //           appB.exe 在 baseline 中仅 1 次（appearances<3）→ significant=false（噪声）
        let ev_culprits = vec![pb(1, "appA.exe", 100.0, 200), pb(2, "appB.exe", 30.0, 80)];
        let ev = mk_event(1500, vec![], None, &[], Sample::default(), ev_culprits);
        let flags = deviation_flags(&ev, &baseline);
        let a = flags.iter().find(|(n, _, _)| n == "appA.exe").unwrap();
        assert_eq!(a.2, true);
        assert!((a.1 - 2.0).abs() < 1e-6, "multiple 应为 100/50=2.0，实际 {}", a.1);
        let bb = flags.iter().find(|(n, _, _)| n == "appB.exe").unwrap();
        assert_eq!(bb.2, false, "appearances<3 应标为噪声不显著");
    }

    #[test]
    fn rc8_cooccurrence_pairs_count_and_order() {
        let pb = |pid: u32, name: &str| ProcessBrief {
            pid,
            name: name.to_string(),
            cpu_usage: 10.0,
            mem_used_mb: 50,
            ..Default::default()
        };
        // 事件1: A+B+C；事件2: A+B；事件3: A+B → A+B 共现 3 次，A+C / B+C 各 1 次
        let ev1 = mk_event(
            1000,
            vec![],
            None,
            &[],
            Sample::default(),
            vec![pb(1, "appA.exe"), pb(2, "appB.exe"), pb(3, "appC.exe")],
        );
        let ev2 = mk_event(
            1000,
            vec![],
            None,
            &[],
            Sample::default(),
            vec![pb(4, "appA.exe"), pb(5, "appB.exe")],
        );
        let ev3 = mk_event(
            1000,
            vec![],
            None,
            &[],
            Sample::default(),
            vec![pb(6, "appA.exe"), pb(7, "appB.exe")],
        );
        let pairs = cooccurrence_pairs(&[ev1, ev2, ev3]);
        let ab = pairs
            .iter()
            .find(|p| {
                (p.pair.0 == "appA.exe" && p.pair.1 == "appB.exe")
                    || (p.pair.0 == "appB.exe" && p.pair.1 == "appA.exe")
            })
            .unwrap();
        assert_eq!(ab.count, 3);
        // 按 count 降序，最大频次排首，且无序对已排序（a<=b）
        assert_eq!(pairs[0].count, 3);
        assert!(pairs[0].pair.0 <= pairs[0].pair.1);
    }

    #[test]
    fn rc9_cause_chain_orders_by_first_touch() {
        let e = mk_event(
            2000,
            vec![CauseKind::MemLow, CauseKind::DiskBusy],
            Some(CauseKind::MemLow),
            &[(CauseKind::MemLow, 0), (CauseKind::DiskBusy, 1200)],
            Sample::default(),
            vec![],
        );
        assert_eq!(cause_chain(&e), vec![CauseKind::MemLow, CauseKind::DiskBusy]);
    }

    #[test]
    fn rc11_confidence_single_cause_is_high() {
        let e = mk_event(
            2000,
            vec![CauseKind::CpuHigh],
            Some(CauseKind::CpuHigh),
            &[],
            Sample::default(),
            vec![],
        );
        let (conf, label) = root_cause_confidence(&e);
        assert_eq!(conf, 0.9);
        assert_eq!(label, "高");
    }

    #[test]
    fn rc11_confidence_multi_leading_primary_is_highish() {
        // primary(CpuHigh) 首触 0，次因(DiskBusy) 首触 1500 → Δt=1500>=1000 → 0.7 较高
        let mut s = Sample::default();
        s.cpu_usage = 100.0;
        s.disk_busy_percent = 50.0;
        let e = mk_event(
            2000,
            vec![CauseKind::CpuHigh, CauseKind::DiskBusy],
            Some(CauseKind::CpuHigh),
            &[(CauseKind::CpuHigh, 0), (CauseKind::DiskBusy, 1500)],
            s,
            vec![],
        );
        let (conf, label) = root_cause_confidence(&e);
        assert_eq!(conf, 0.7);
        assert_eq!(label, "较高");
    }

    #[test]
    fn rc11_confidence_multi_overlapping_is_low() {
        // primary 与次因首触重叠（均为 0），强度比 0.5/0.5=1.0 < 1.5 → 0.35，标签含"多因并发"
        let mut s = Sample::default();
        s.cpu_usage = 50.0;
        s.disk_busy_percent = 50.0;
        let e = mk_event(
            2000,
            vec![CauseKind::CpuHigh, CauseKind::DiskBusy],
            Some(CauseKind::CpuHigh),
            &[(CauseKind::CpuHigh, 0), (CauseKind::DiskBusy, 0)],
            s,
            vec![],
        );
        let (conf, label) = root_cause_confidence(&e);
        assert_eq!(conf, 0.35);
        assert!(label.contains("多因并发"));
    }

    #[test]
    fn rc12_detect_core_high_cpu_and_thermal_and_clean() {
        let cfg = DetectionConfig::default();
        // 高 CPU（其余指标正常：可用内存充足、使用率/提交均安全）→ 仅含 "CPU usage"
        let mut s = Sample::default();
        s.cpu_usage = 95.0;
        s.mem_available_mb = 8000;
        s.mem_usage_percent = 30.0;
        s.commit_limit = 1_000_000;
        s.commit_bytes = 100_000;
        let out = detect_core(&s, &cfg);
        assert!(
            out.iter().any(|c| c.contains("CPU usage")),
            "高 CPU 应包含 CPU usage: {:?}",
            out
        );

        // 全正常 sample（所有指标在阈值内）→ 空
        let mut normal = Sample::default();
        normal.cpu_usage = 30.0;
        normal.mem_available_mb = 8000;
        normal.mem_usage_percent = 30.0;
        normal.commit_limit = 1_000_000;
        normal.commit_bytes = 100_000;
        normal.page_reads_per_sec = 0.0;
        assert!(
            detect_core(&normal, &cfg).is_empty(),
            "全正常应为空: {:?}",
            detect_core(&normal, &cfg)
        );

        // Thermal：温度高 + 频率读数 + 负载高 → 含 "Thermal throttle"
        let mut hot = Sample::default();
        hot.cpu_temp = Some(95.0);
        hot.cpu_freq_mhz = Some(2000.0);
        hot.cpu_usage = 80.0;
        hot.mem_available_mb = 8000;
        hot.mem_usage_percent = 30.0;
        let out2 = detect_core(&hot, &cfg);
        assert!(
            out2.iter().any(|c| c.contains("Thermal throttle")),
            "温度降频应包含 Thermal throttle: {:?}",
            out2
        );

        // 阶段 E2a 回归：提交电荷单独偏高（95% > 90%）→ 不再产出独立 cause
        // （commit 高只是记账上限逼近，降级为「压力证据」；与实时判定一致）
        let mut commit_only = Sample::default();
        commit_only.cpu_usage = 30.0;
        commit_only.mem_available_mb = 8000;
        commit_only.mem_usage_percent = 30.0;
        commit_only.commit_limit = 1_000_000;
        commit_only.commit_bytes = 950_000;
        let out3 = detect_core(&commit_only, &cfg);
        assert!(
            out3.is_empty(),
            "提交电荷单独偏高不应触发 what-if cause: {:?}",
            out3
        );

        // 阶段 E2a 证据角色：commit 高 + 分页速率高 → 经 "Memory paging" 触发
        let mut thrash = commit_only.clone();
        thrash.page_reads_per_sec = 400.0;
        let out4 = detect_core(&thrash, &cfg);
        assert!(
            out4.iter().any(|c| c.contains("Memory paging")),
            "commit 作为证据应放行 paging: {:?}",
            out4
        );
    }

    /// 2026-08-17 误报治理回归：what-if 单帧重算的上下文切换口径与实时判定
    /// 一致——按核归一越线 **且** CPU 侧压力证据成立才产出 cause；
    /// 无证据的高切换、多核日常基线均不产出。
    #[test]
    fn rc_detect_core_ctx_switch_per_core_with_evidence() {
        let cfg = DetectionConfig::default();
        // 14 核、150k/s ≈ 10.7k/core（越进入线 10k/core）+ CPU 85%（证据成立）
        let mut s = Sample::default();
        s.cpu_usage = 85.0;
        s.mem_available_mb = 8000;
        s.mem_usage_percent = 30.0;
        s.cpu_per_core = vec![85.0; 14];
        s.context_switches_per_sec = 150_000.0;
        let out = detect_core(&s, &cfg);
        assert!(
            out.iter().any(|c| c.contains("Context switches")),
            "按核越线 + CPU 证据应产出 ctx cause: {:?}",
            out
        );

        // 同速率但 CPU 50% < 80% 证据线（DPC/中断正常）→ 不产出
        let mut low = s.clone();
        low.cpu_usage = 50.0;
        low.cpu_per_core = vec![50.0; 14];
        assert!(
            !detect_core(&low, &cfg)
                .iter()
                .any(|c| c.contains("Context switches")),
            "无 CPU 侧证据的高切换不应产出: {:?}",
            detect_core(&low, &cfg)
        );

        // 日常基线 70k/s ≈ 5k/core（远离进入线）→ 不产出
        let mut base = low.clone();
        base.context_switches_per_sec = 70_000.0;
        assert!(
            !detect_core(&base, &cfg)
                .iter()
                .any(|c| c.contains("Context switches")),
            "多核日常基线不应产出: {:?}",
            detect_core(&base, &cfg)
        );
    }

    #[test]
    fn rc13_cluster_profiles_and_match() {
        let pb = |pid: u32, name: &str| ProcessBrief {
            pid,
            name: name.to_string(),
            cpu_usage: 10.0,
            mem_used_mb: 50,
            ..Default::default()
        };
        // 2 个同 signature 事件：CpuHigh + appA.exe + 中桶(2000/3000ms)
        let mk = |dur: u64| {
            let mut s = Sample::default();
            s.cpu_usage = 95.0;
            mk_event(
                dur,
                vec![CauseKind::CpuHigh],
                Some(CauseKind::CpuHigh),
                &[],
                s,
                vec![pb(1, "appA.exe")],
            )
        };
        let e1 = mk(2000);
        let e2 = mk(3000);
        // 1 个不同：MemLow + appB.exe + 长桶(8000ms)
        let mut s3 = Sample::default();
        s3.mem_usage_percent = 95.0;
        let e3 = mk_event(
            8000,
            vec![CauseKind::MemLow],
            Some(CauseKind::MemLow),
            &[],
            s3,
            vec![pb(2, "appB.exe")],
        );
        let profiles = cluster_profiles(&[e1.clone(), e2.clone(), e3.clone()]);
        // 2 个不同 signature；CpuHigh+appA+中 出现 2 次
        assert_eq!(profiles.len(), 2);
        let sig = signature_of(&e1);
        let prof = profiles.iter().find(|p| p.signature == sig).unwrap();
        assert!(prof.count >= 2);
        assert_eq!(prof.count, 2);
        assert!((prof.avg_duration_ms - 2500.0).abs() < 1e-9);

        // 对 e1（已知画像）→ Some 且含"匹配已知画像"
        let m = match_profile(&e1, &profiles);
        assert!(m.is_some());
        assert!(m.unwrap().contains("匹配已知画像"));

        // 对 e3（仅 1 次，非已知）→ None
        assert!(match_profile(&e3, &profiles).is_none());
    }

    #[test]
    fn load_full_events_roundtrip_structured_fields() {
        // 验证 load_full_events 能把 service 落库的结构化字段（cause_kinds /
        // primary_cause / cause_first_touch / snapshot / culprits / onset_ts）完整重建，
        // 供 F-RC10/11/13 钻取卡与画像对比使用。
        let pb = |pid: u32, name: &str, cpu: f32, mem: u64| ProcessBrief {
            pid,
            name: name.to_string(),
            cpu_usage: cpu,
            mem_used_mb: mem,
            ..Default::default()
        };
        let db = unique_db("full_events");
        let ev = crate::StutterEvent {
            timestamp: local_midnight_utc() + ChronoDuration::minutes(5),
            duration_ms: 1500,
            severity: Severity::Major,
            causes: vec!["CPU usage 95.0% > 90.0%".to_string()],
            cause_kinds: vec![CauseKind::CpuHigh, CauseKind::MemLow],
            primary_cause: Some(CauseKind::CpuHigh),
            cause_first_touch: {
                let mut m = std::collections::HashMap::new();
                m.insert(CauseKind::CpuHigh, 0i64);
                m.insert(CauseKind::MemLow, 800i64);
                m
            },
            onset_ts: Some(
                (local_midnight_utc() + ChronoDuration::minutes(5)).timestamp() - 1500,
            ),
            snapshot: {
                let mut s = Sample::default();
                s.cpu_usage = 96.0;
                s.mem_usage_percent = 92.0;
                s
            },
            culprits: vec![pb(1, "app.exe", 96.0, 512)],
            ..Default::default()
        };
        {
            let cfg = StorageConfig { db_path: db.clone(), retention_days: 30, event_retention_days: 30 };
            let mut logger = Logger::new(&cfg).unwrap();
            logger.touch_heartbeat().unwrap();
            logger.write_event(&ev).unwrap();
            logger.flush().unwrap();
            // 补一条采样，确保前导曲线窄窗口查询有数据；
            // 让其 timestamp 落在事件 ±60s 窗口内，否则 load_resource_samples_window 返回空。
            let mut s = Sample::default();
            s.timestamp = ev.timestamp;
            s.cpu_usage = 90.0;
            logger.write_sample(&s).unwrap();
            logger.flush().unwrap();
        }
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        let all = load_full_events(&conn, &TimeRange::Today).unwrap();
        assert_eq!(all.len(), 1);
        let got = &all[0];
        assert_eq!(got.cause_kinds, vec![CauseKind::CpuHigh, CauseKind::MemLow]);
        assert_eq!(got.primary_cause, Some(CauseKind::CpuHigh));
        assert_eq!(got.cause_first_touch.get(&CauseKind::MemLow), Some(&800i64));
        assert_eq!(got.snapshot.cpu_usage, 96.0);
        assert_eq!(got.culprits.len(), 1);
        assert_eq!(got.culprits[0].name, "app.exe");
        assert!(got.onset_ts.is_some());

        // F-RC10：cause_kind_label 中文映射
        assert_eq!(cause_kind_label(CauseKind::CpuHigh), "CPU 占用高");
        assert_eq!(cause_kind_label(CauseKind::ThermalThrottle), "温度降频");
        assert_eq!(cause_kind_label(CauseKind::UiFrozen), "界面冻结");

        // F-RC10：前导曲线窄窗口采样（事件 ±60s 应有降采样点）
        let ts = got.timestamp.timestamp();
        let data = load_resource_samples_window(&conn, ts, 60, 780).unwrap();
        assert!(!data.points.is_empty(), "窄窗口应返回采样点");

        std::fs::remove_file(&db).ok();
    }

    // ===================== ADR-0001：CLI 复用的区间读接口测试 =====================

    #[test]
    fn load_samples_range_limit_takes_latest_and_ascending() {
        // 10 条跨 10 秒的采样，limit=4 → 最新 4 条（第 6..10 秒），且输出升序
        let db = unique_db("samples_range_limit");
        let base = local_midnight_utc();
        let samples: Vec<Sample> = (0..10)
            .map(|i| {
                let mut s = Sample::default();
                s.timestamp = base + ChronoDuration::seconds(i);
                s.cpu_usage = 10.0 + i as f32;
                s.mem_usage_percent = 40.0;
                s
            })
            .collect();
        seed_samples(&db, &samples);
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        let (start, end) = TimeRange::Today.bounds();
        let rows = load_samples_range(&conn, &start, &end, 4).unwrap();
        assert_eq!(rows.len(), 4, "limit=4 应只返回最新 4 条");
        // 最新 4 条 = 第 7..10 秒（cpu_usage 16..19），升序排列
        let cpus: Vec<f64> = rows.iter().map(|r| r.cpu_usage.unwrap()).collect();
        assert_eq!(cpus, vec![16.0, 17.0, 18.0, 19.0]);
        assert!(rows[0].timestamp < rows[3].timestamp, "应按时间升序输出");
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn load_samples_range_window_filter() {
        let db = unique_db("samples_range_window");
        let base = local_midnight_utc();
        let samples: Vec<Sample> = (0..10)
            .map(|i| {
                let mut s = Sample::default();
                s.timestamp = base + ChronoDuration::seconds(i);
                s.cpu_usage = 30.0;
                s
            })
            .collect();
        seed_samples(&db, &samples);
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        // [零点, 零点+3.999s] → 第 0/1/2/3 秒共 4 条
        let end = (base + ChronoDuration::seconds(3) + ChronoDuration::milliseconds(999)).to_rfc3339();
        let rows = load_samples_range(&conn, &base.to_rfc3339(), &end, 100).unwrap();
        assert_eq!(rows.len(), 4);
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn load_samples_range_empty_when_no_data() {
        let db = unique_db("samples_range_empty");
        let cfg = StorageConfig {
            db_path: db.clone(),
            retention_days: 30,
            event_retention_days: 30,
        };
        let mut logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();
        logger.flush().unwrap();
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        let (start, end) = TimeRange::Today.bounds();
        let rows = load_samples_range(&conn, &start, &end, 10).unwrap();
        assert!(rows.is_empty());
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn load_samples_range_tolerates_legacy_null_columns() {
        // 旧库迁移列（disk_busy_percent 等 F-RC2 列）可能为 NULL：
        // SampleRow 忠实返回 None，由调用方（CLI JSON 组装）决定回退。
        let db = unique_db("samples_range_null");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE samples (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                cpu_usage REAL,
                cpu_freq_mhz REAL,
                mem_usage_percent REAL,
                mem_used_mb INTEGER,
                mem_total_mb INTEGER,
                mem_available_mb INTEGER,
                swap_usage_percent REAL,
                disk_read_bps INTEGER,
                disk_write_bps INTEGER,
                disk_busy_percent REAL,
                disk_avg_io_ms REAL,
                net_sent_bps INTEGER,
                net_recv_bps INTEGER,
                net_sent_total INTEGER,
                net_recv_total INTEGER,
                gpu_usage REAL,
                cpu_temp REAL,
                gpu_temp REAL,
                process_count INTEGER,
                thread_count INTEGER,
                commit_bytes INTEGER,
                commit_limit INTEGER,
                page_reads_per_sec REAL,
                dpc_percent REAL,
                interrupt_percent REAL,
                context_switches_per_sec INTEGER
            );
            INSERT INTO samples (timestamp, cpu_usage) VALUES ('2026-08-16T00:00:00+00:00', 42.0);",
        )
        .unwrap();
        drop(conn);
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        let rows =
            load_samples_range(&conn, "2026-08-16T00:00:00+00:00", "2026-08-17T00:00:00+00:00", 10)
                .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cpu_usage, Some(42.0));
        assert_eq!(rows[0].disk_busy_percent, None, "未落值列应为 None 而非 0");
        assert_eq!(rows[0].mem_used_mb, None);
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn load_kpi_custom_range_matches_window() {
        // 3 个事件在今日零点 +0/1/2 分钟（duration 1000/2000/3000）；
        // Custom [零点, +1.5min] → count=2、worst=2000
        let db = unique_db("kpi_range");
        {
            let cfg = StorageConfig {
                db_path: db.clone(),
                retention_days: 30,
                event_retention_days: 30,
            };
            let mut logger = Logger::new(&cfg).unwrap();
            logger.touch_heartbeat().unwrap();
            let base = local_midnight_utc();
            for i in 0..3 {
                let mut s = Sample::default();
                s.cpu_usage = 95.0;
                let ev = crate::StutterEvent {
                    timestamp: base + ChronoDuration::minutes(i),
                    duration_ms: 1000 * (i + 1) as u64,
                    severity: Severity::Minor,
                    causes: vec!["CPU usage 95.0% > 90.0%".into()],
                    snapshot: s,
                    culprits: vec![],
                    ..Default::default()
                };
                logger.write_event(&ev).unwrap();
            }
            logger.flush().unwrap();
        }
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        let base = local_midnight_utc();
        let range = TimeRange::Custom(
            base.to_rfc3339(),
            (base + ChronoDuration::seconds(90)).to_rfc3339(),
        );
        let kpi = load_kpi(&conn, &range).unwrap();
        assert_eq!(kpi.today_count, 2, "[0, 90s] 应只含第 0/1 分钟两个事件");
        assert_eq!(kpi.worst_duration_ms, 2000);
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn load_kpi_today_delegates_to_load_kpi() {
        // 「今日」薄包装与泛化版在 Today 范围下结果一致
        let db = unique_db("kpi_delegate");
        seed_recent_today(&db, &[Severity::Minor, Severity::Major]);
        let conn = open_readonly(std::path::Path::new(&db)).unwrap();
        let a = load_kpi_today(&conn).unwrap();
        let b = load_kpi(&conn, &TimeRange::Today).unwrap();
        assert_eq!(a.today_count, b.today_count);
        assert_eq!(a.worst_duration_ms, b.worst_duration_ms);
        assert_eq!(a.peak_hour, b.peak_hour);
        assert_eq!(a.top_culprit, b.top_culprit);
        std::fs::remove_file(&db).ok();
    }
}
