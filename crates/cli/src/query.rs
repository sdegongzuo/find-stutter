//! 只读查询层：把 core 的分析聚合转成 agent 友好的 JSON（ADR-0001 决策 1/2）。
//!
//! ## JSON 契约（ADR-0001 决策 1 + CONTEXT「CLI（界面轴·agent）」）
//!
//! - 键英文、值保留原文（severity 用 minor/major/critical 原词、cause 文案原样）；
//! - 时间一律 ISO8601（RFC3339，UTC）；
//! - `--from/--to/--limit` 过滤；events / samples 顶层输出 JSON 数组，便于 jq 管道；
//! - 固定口径查询全部走 `find_stutter_core::analytics` 的既有函数（口径与 GUI
//!   分析页一致，不另写 SQL 聚合，避免漂移）；[`sql_json`]（`query` 子命令）是
//!   刻意保留的例外——诊断期的灵活聚合逃生口，不承载任何固定口径。

use std::path::Path;

use chrono::Utc;
use find_stutter_core::analytics::{
    self, TimeRange, TrendBucket,
};
use find_stutter_core::{Config, StutterEvent};
use rusqlite::Connection;
use serde_json::{json, Map, Value};

use crate::timeparse::parse_time_arg;

/// 计算查询窗口（UTC RFC3339 边界）。
///
/// - `from`/`to` 任一缺失时：from 回退「本地今日零点」、to 回退「现在」
///   （与 GUI 分析页 TimeRange::Today 的口径一致，见 analytics::TimeRange）；
/// - 提供时按参数解析（本地时区语义见 [`crate::timeparse`]）。
pub fn resolve_range(from: Option<&str>, to: Option<&str>) -> anyhow::Result<(String, String)> {
    let start = match from {
        Some(s) => parse_time_arg(s)
            .ok_or_else(|| anyhow::anyhow!("无法解析 --from \"{}\"（支持 RFC3339、YYYY-MM-DD HH:MM:SS、YYYY-MM-DD）", s))?
            .to_rfc3339(),
        None => analytics::TimeRange::Today.bounds().0,
    };
    let end = match to {
        Some(s) => parse_time_arg(s)
            .ok_or_else(|| anyhow::anyhow!("无法解析 --to \"{}\"（支持 RFC3339、YYYY-MM-DD HH:MM:SS、YYYY-MM-DD）", s))?
            .to_rfc3339(),
        None => Utc::now().to_rfc3339(),
    };
    Ok((start, end))
}

/// 打开只读连接（复用 analytics 的 WAL 读视图口径）。
pub fn open_db(db_path: &Path) -> anyhow::Result<Connection> {
    analytics::open_readonly(db_path)
}

/// 从 config.toml 拿生效 db 路径（加载失败回退默认配置，与 GUI 启动口径一致）。
pub fn db_path_from_config() -> String {
    match Config::load("config.toml") {
        Ok(c) => c.storage.db_path,
        Err(_) => Config::default().storage.db_path,
    }
}

/// `query` 子命令：只读 SQL 直查，行数组 JSON（列名 → 值）。
///
/// 定位：`events` / `samples` / `analysis` 是**固定口径**查询（走 analytics，
/// 与 GUI 一致不漂移）；`query` 是诊断期的**灵活逃生口**——按天分组计数、
/// 分布统计、最长连续段这类一次性聚合不适合各建子命令，交给 SQL 表达。
/// 安全边界（双层）：
/// 1. 连接只读（[`open_db`] → `analytics::open_readonly`，SQLite 层拒绝任何写）；
/// 2. 语句级 `Statement::readonly()` 校验，仅放行 SELECT / WITH / PRAGMA 等读
///    语句（写语句在执行前即被拒，错误信息更友好）；`prepare` 拒绝多语句拼接，
///    防止「第一条读语句掩护后续写语句」。
/// 值映射：TEXT→字符串、INTEGER→整数、REAL→浮点、NULL→null、BLOB→hex 字符串。
/// 同名列（如自联结 SELECT 同名列）后者覆盖前者，与 SQLite 行为一致由调用方规避。
pub fn sql_json(conn: &Connection, sql: &str) -> anyhow::Result<Value> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| anyhow::anyhow!("SQL 解析失败: {}", e))?;
    if !stmt.readonly() {
        anyhow::bail!("只允许只读语句（SELECT / WITH / PRAGMA），写操作被拒绝");
    }
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query([])?;
    let mut arr = Vec::new();
    while let Some(row) = rows.next()? {
        let mut m = Map::new();
        for (i, name) in names.iter().enumerate() {
            let v = match row.get_ref(i)? {
                rusqlite::types::ValueRef::Null => Value::Null,
                rusqlite::types::ValueRef::Integer(n) => json!(n),
                rusqlite::types::ValueRef::Real(f) => json!(f),
                rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                rusqlite::types::ValueRef::Blob(b) => json!(
                    b.iter().map(|x| format!("{:02x}", x)).collect::<String>()
                ),
            };
            m.insert(name.clone(), v);
        }
        arr.push(Value::Object(m));
    }
    Ok(Value::Array(arr))
}

/// `events` 子命令：时间范围内最近的 `limit` 条卡顿事件（时间升序输出）。
///
/// 复用 `analytics::load_full_events`（与 GUI 根因钻取同一重建逻辑，含旧库列
/// 回退），limit 语义为「取最新 N 条」：先全量读出（事件保留期 7 天、量级小），
/// 再截取末尾 N 条。
pub fn events_json(
    conn: &Connection,
    from: Option<&str>,
    to: Option<&str>,
    limit: usize,
) -> anyhow::Result<Value> {
    let (start, end) = resolve_range(from, to)?;
    let range = TimeRange::Custom(start.clone(), end.clone());
    let mut events = analytics::load_full_events(conn, &range)?;
    // 取「最新 limit 条」但保持时间升序输出（jq 侧时序处理更自然）
    if events.len() > limit {
        let split = events.len() - limit;
        events.drain(..split);
    }
    let arr: Vec<Value> = events.iter().map(event_to_json).collect();
    Ok(Value::Array(arr))
}

/// 单条事件 → JSON 对象（键英文，值保留原文；时间 ISO8601）。
pub fn event_to_json(e: &StutterEvent) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), json!(e.id));
    m.insert("timestamp".into(), json!(e.timestamp.to_rfc3339()));
    // onset_ts：库内为 Unix 毫秒（≈ timestamp - duration_ms，见 types.rs），
    // CLI 契约「时间一律 ISO8601」→ 统一输出 RFC3339 字符串；
    // 缺失（旧库 / 极端值溢出）时输出 null。
    m.insert(
        "onset_ts".into(),
        match e.onset_ts.and_then(chrono::DateTime::from_timestamp_millis) {
            Some(dt) => json!(dt.to_rfc3339()),
            None => Value::Null,
        },
    );
    m.insert("duration_ms".into(), json!(e.duration_ms));
    // severity 保留落库原词（minor/major/critical）
    m.insert("severity".into(), json!(e.severity.to_string()));
    m.insert(
        "primary_cause".into(),
        match e.primary_cause {
            Some(k) => json!(format!("{:?}", k)),
            None => Value::Null,
        },
    );
    m.insert(
        "cause_kinds".into(),
        json!(e
            .cause_kinds
            .iter()
            .map(|k| format!("{:?}", k))
            .collect::<Vec<_>>()),
    );
    m.insert("causes".into(), json!(e.causes));
    m.insert("culprits".into(), culprits_json(&e.culprits));
    // 各 cause 首触时刻：**相对 onset 的偏移毫秒**（0=与卡顿同时起点，正数=晚于起点；
    // 见 detector.rs「首触时刻：相对 onset（卡顿起点）的偏移毫秒」与 types.rs 注释）。
    // 是相对时长而非绝对时间点，故保留数值，字段名带 _offset_ms 以示语义。
    m.insert(
        "cause_first_touch_offset_ms".into(),
        json!(e
            .cause_first_touch
            .iter()
            .map(|(k, v)| (format!("{:?}", k), *v))
            .collect::<std::collections::BTreeMap<String, i64>>()),
    );
    Value::Object(m)
}

/// culprits → 精简 JSON 数组（保留原文进程名；补全维度由调用方决定）。
fn culprits_json(culprits: &[find_stutter_core::ProcessBrief]) -> Value {
    Value::Array(
        culprits
            .iter()
            .map(|c| {
                json!({
                    "pid": c.pid,
                    "name": c.name,
                    "cpu_usage": c.cpu_usage,
                    "mem_used_mb": c.mem_used_mb,
                })
            })
            .collect(),
    )
}

/// `samples` 子命令：时间范围内最近的 `limit` 条采样（时间升序输出）。
///
/// 样本量大（1Hz、保留 30 天），默认 limit 由 clap 层给 1000 并在 help 说明。
/// SQL 与行映射下沉在 core（`analytics::load_samples_range`：SQL 层
/// `ORDER BY timestamp DESC LIMIT` 截取最新 N 条后反转回升序，内存不随范围增长），
/// 与 GUI 共用同一读口径；本函数只做 JSON 组装。
pub fn samples_json(
    conn: &Connection,
    from: Option<&str>,
    to: Option<&str>,
    limit: usize,
) -> anyhow::Result<Value> {
    let (start, end) = resolve_range(from, to)?;
    // SQL 与行映射在 core（analytics::load_samples_range：最新 N 条 + 升序，
    // 与 GUI 共用读口径）；本函数只做 JSON 组装。
    let rows = analytics::load_samples_range(conn, &start, &end, limit)?;
    let out: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "timestamp": r.timestamp,
                "cpu_usage": opt_f32(r.cpu_usage),
                "cpu_freq_mhz": opt_f32(r.cpu_freq_mhz),
                "mem_usage_percent": opt_f32(r.mem_usage_percent),
                "mem_used_mb": r.mem_used_mb.unwrap_or(0),
                "mem_total_mb": r.mem_total_mb.unwrap_or(0),
                "mem_available_mb": r.mem_available_mb.unwrap_or(0),
                "swap_usage_percent": opt_f32(r.swap_usage_percent),
                "disk_read_bps": r.disk_read_bps.unwrap_or(0),
                "disk_write_bps": r.disk_write_bps.unwrap_or(0),
                "disk_busy_percent": opt_f32(r.disk_busy_percent),
                "disk_avg_io_ms": opt_f32(r.disk_avg_io_ms),
                "net_sent_bps": r.net_sent_bps.unwrap_or(0),
                "net_recv_bps": r.net_recv_bps.unwrap_or(0),
                "net_sent_total": r.net_sent_total.unwrap_or(0),
                "net_recv_total": r.net_recv_total.unwrap_or(0),
                "gpu_usage": opt_f32(r.gpu_usage),
                "cpu_temp": opt_f32(r.cpu_temp),
                "gpu_temp": opt_f32(r.gpu_temp),
                "process_count": r.process_count.unwrap_or(0),
                "thread_count": r.thread_count.unwrap_or(0),
                "commit_bytes": r.commit_bytes.unwrap_or(0),
                "commit_limit": r.commit_limit.unwrap_or(0),
                "page_reads_per_sec": opt_f32(r.page_reads_per_sec),
                "dpc_percent": opt_f32(r.dpc_percent),
                "interrupt_percent": opt_f32(r.interrupt_percent),
                "context_switches_per_sec": r.context_switches_per_sec.unwrap_or(0),
            })
        })
        .collect();
    Ok(Value::Array(out))
}

/// Option<f64> → Option<f32>（样本表 REAL 列按 f32 口径输出，与 GUI 一致）。
fn opt_f32(v: Option<f64>) -> Option<f32> {
    v.map(|x| x as f32)
}

/// `analysis` 子命令：一次输出 KPI / 趋势 / 元凶榜 / 类型占比 / 最近事件根因报告。
///
/// 聚合全部复用 `find_stutter_core::analytics`（F1~F4 + F-RC5~13），
/// 与 GUI 分析页共用同一口径（ADR-0001 决策 2）。
pub fn analysis_json(
    conn: &Connection,
    from: Option<&str>,
    to: Option<&str>,
) -> anyhow::Result<Value> {
    let (start, end) = resolve_range(from, to)?;
    let range = TimeRange::Custom(start.clone(), end.clone());

    // 1) KPI（任意区间四步口径在 core：analytics::load_kpi；默认范围即「今日」）
    let kpi = {
        let k = analytics::load_kpi(conn, &range)?;
        json!({
            "count": k.today_count,
            "worst_duration_ms": k.worst_duration_ms,
            "peak_hour": k.peak_hour,
            "top_culprit": k.top_culprit,
        })
    };

    // 2) 趋势（本地时区分桶，粒度=小时）
    let trend = analytics::load_trend(conn, &range, TrendBucket::Hour)?;
    let trend_json: Vec<Value> = trend
        .iter()
        .map(|p| {
            json!({
                "bucket": p.bucket,
                "count": p.count,
                "total_ms": p.total_ms,
                "critical": p.critical,
                "major": p.major,
                "minor": p.minor,
            })
        })
        .collect();

    // 3) 元凶榜（同名进程合并，出现次数降序；F2 聚合）
    let culprits = analytics::load_culprits(conn, &range, 10)?;
    let culprits_json: Vec<Value> = culprits
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "count": c.count,
                "total_duration_ms": c.total_duration_ms,
                "max_cpu": c.max_cpu,
                "max_mem_mb": c.max_mem_mb,
            })
        })
        .collect();

    // 4) 卡顿类型占比（F4 关键词归类）
    let cause_types = analytics::load_cause_types(conn, &range)?;
    let cause_types_json: Vec<Value> = cause_types
        .iter()
        .map(|c| json!({ "cause_type": c.cause_type, "count": c.count, "percent": c.percent }))
        .collect();

    // 5) 最近一次事件的根因报告（F-RC6/9/11/13：因果方向 + 置信度 + 链 + 画像匹配）
    let root_cause = latest_root_cause(conn, &range)?;

    Ok(json!({
        "range": { "from": start, "to": end },
        "kpi": kpi,
        "trend": trend_json,
        "culprits": culprits_json,
        "cause_types": cause_types_json,
        "root_cause": root_cause,
    }))
}


/// 最近一次事件的根因报告（无事件时输出 null 字段齐全的占位对象）。
fn latest_root_cause(conn: &Connection, range: &TimeRange) -> anyhow::Result<Value> {
    let events = analytics::load_full_events(conn, range)?;
    let Some(last) = events.last() else {
        return Ok(json!({
            "event_id": null,
            "confidence": null,
            "confidence_label": null,
            "primary_cause": null,
            "trigger": null,
            "amplifiers": [],
            "cause_chain": [],
            "profile_match": null,
        }));
    };
    let (trigger, amplifiers) = analytics::causal_direction(last);
    let (conf, label) = analytics::root_cause_confidence(last);
    let chain: Vec<String> = analytics::cause_chain(last)
        .into_iter()
        .map(|k| format!("{:?}", k))
        .collect();
    let profiles = analytics::cluster_profiles(&events);
    let profile_match = analytics::match_profile(last, &profiles);
    Ok(json!({
        "event_id": last.id,
        "confidence": conf,
        "confidence_label": label,
        "primary_cause": last.primary_cause.map(|k| format!("{:?}", k)),
        "trigger": trigger.map(|k| format!("{:?}", k)),
        "amplifiers": amplifiers.iter().map(|k| format!("{:?}", k)).collect::<Vec<_>>(),
        "cause_chain": chain,
        "profile_match": profile_match,
    }))
}

/// `config` 子命令：当前生效配置（config.toml 加载结果，缺省回退后的有效值）。
pub fn config_json() -> Value {
    let loaded = Config::load("config.toml");
    let (source, cfg, db_path) = match &loaded {
        Ok(c) => ("config.toml", c.clone(), c.storage.db_path.clone()),
        // 加载失败回退默认值（与 GUI / service 启动口径一致：warn 后用默认）
        Err(_) => ("default", Config::default(), Config::default().storage.db_path),
    };
    json!({
        "source": source,        // "config.toml" = 命中配置文件；"default" = 加载失败回退默认
        "db_path": db_path,
        "config": serde_json::to_value(&cfg).unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use find_stutter_core::logger::local_today_bounds_utc;
    use find_stutter_core::{Logger, ProcessBrief, Sample, Severity, StorageConfig};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// `query` 子命令：聚合查询（GROUP BY / COUNT / AVG）返回行数组 JSON，
    /// NULL 列映射为 JSON null。
    #[test]
    fn sql_json_aggregates_rows() {
        let db = unique_db("sqlq");
        seed(&db, 3, 5);
        let conn = open_db(Path::new(&db)).unwrap();

        let v = sql_json(
            &conn,
            "SELECT severity, COUNT(*) AS n FROM stutter_events GROUP BY severity",
        )
        .unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["severity"].as_str().unwrap(), "major");
        assert_eq!(arr[0]["n"].as_i64().unwrap(), 3);

        // 无匹配行的聚合：COUNT=0（整数），MAX=NULL（JSON null）
        let v = sql_json(
            &conn,
            "SELECT COUNT(*) AS c, MAX(cpu_usage) AS m FROM samples WHERE cpu_usage > 100",
        )
        .unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr[0]["c"].as_i64().unwrap(), 0);
        assert!(arr[0]["m"].is_null(), "MAX 无匹配应为 null");

        // 浮点与文本列映射
        let v = sql_json(
            &conn,
            "SELECT ROUND(AVG(cpu_usage), 1) AS avg_cpu FROM samples",
        )
        .unwrap();
        let avg = v.as_array().unwrap()[0]["avg_cpu"].as_f64().unwrap();
        assert!((avg - 30.0).abs() < 0.5, "seed 样本 cpu=30，got {}", avg);
        std::fs::remove_file(&db).ok();
    }

    /// `query` 子命令：写语句在执行前即被语句级 readonly 校验拒绝
    /// （连接本身也只读，双层防护中的第一层先行给出友好错误）。
    #[test]
    fn sql_json_rejects_write_statements() {
        let db = unique_db("sqlw");
        seed(&db, 1, 1);
        let conn = open_db(Path::new(&db)).unwrap();
        for sql in [
            "INSERT INTO stutter_events (timestamp) VALUES ('x')",
            "DELETE FROM samples",
            "UPDATE samples SET cpu_usage = 0",
            "DROP TABLE samples",
            "CREATE TABLE t (x)",
        ] {
            let err = sql_json(&conn, sql);
            assert!(err.is_err(), "写语句应被拒绝: {}", sql);
        }
        std::fs::remove_file(&db).ok();
    }

    /// `query` 子命令：多语句拼接被拒（防止第一条读语句掩护后续写语句）。
    #[test]
    fn sql_json_rejects_multiple_statements() {
        let db = unique_db("sqlm");
        seed(&db, 1, 1);
        let conn = open_db(Path::new(&db)).unwrap();
        assert!(sql_json(&conn, "SELECT 1; DELETE FROM samples").is_err());
        std::fs::remove_file(&db).ok();
    }

    /// `query` 子命令：PRAGMA 只读语句可用（agent 探查 schema 的入口）。
    #[test]
    fn sql_json_allows_pragma_table_info() {
        let db = unique_db("sqlp");
        seed(&db, 1, 1);
        let conn = open_db(Path::new(&db)).unwrap();
        let v = sql_json(&conn, "PRAGMA table_info(stutter_events)").unwrap();
        let arr = v.as_array().unwrap();
        assert!(!arr.is_empty(), "stutter_events 应有列定义");
        assert!(
            arr.iter().any(|r| r["name"].as_str() == Some("timestamp")),
            "应包含 timestamp 列"
        );
        std::fs::remove_file(&db).ok();
    }

    fn unique_db(name: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("fs_cli_{}_{}.db", name, nanos))
            .to_str()
            .unwrap()
            .to_string()
    }

    fn seed(db: &str, events: usize, samples: usize) {
        let cfg = StorageConfig {
            db_path: db.to_string(),
            retention_days: 30,
            event_retention_days: 30,
        };
        let mut logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();
        let base = local_today_bounds_utc().0;
        for i in 0..events {
            let mut s = Sample::default();
            s.cpu_usage = 95.0;
            let ts = base + chrono::Duration::minutes(i as i64);
            let ev = find_stutter_core::StutterEvent {
                timestamp: ts,
                duration_ms: 1000 * (i + 1) as u64,
                severity: Severity::Major,
                causes: vec!["CPU usage 95.0% > 90.0%".into()],
                cause_kinds: vec![find_stutter_core::CauseKind::CpuHigh],
                primary_cause: Some(find_stutter_core::CauseKind::CpuHigh),
                // 模拟真实语义：onset ≈ timestamp - duration（Unix 毫秒）
                onset_ts: Some(ts.timestamp_millis() - 1000 * (i + 1) as i64),
                cause_first_touch: {
                    let mut m = std::collections::HashMap::new();
                    m.insert(find_stutter_core::CauseKind::CpuHigh, 0i64);
                    m
                },
                snapshot: s,
                culprits: vec![ProcessBrief {
                    pid: 100 + i as u32,
                    name: "app.exe".into(),
                    cpu_usage: 80.0,
                    mem_used_mb: 200,
                    ..Default::default()
                }],
                ..Default::default()
            };
            logger.write_event(&ev).unwrap();
        }
        for i in 0..samples {
            let mut s = Sample::default();
            s.timestamp = base + chrono::Duration::seconds(i as i64);
            s.cpu_usage = 30.0;
            s.mem_usage_percent = 40.0;
            logger.write_sample(&s).unwrap();
        }
        logger.flush().unwrap();
    }

    #[test]
    fn events_json_respects_limit_and_fields() {
        let db = unique_db("events");
        seed(&db, 5, 0);
        let conn = open_db(Path::new(&db)).unwrap();
        let v = events_json(&conn, None, None, 3).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 3, "limit=3 应只返回最新 3 条");
        // 升序：duration 递增（最新 3 条 = 1000*3..1000*5）
        assert_eq!(arr[0]["duration_ms"].as_u64().unwrap(), 3000);
        assert_eq!(arr[2]["duration_ms"].as_u64().unwrap(), 5000);
        // 字段契约：causes/cause_kinds/primary_cause/severity/duration_ms/culprits 均在
        assert!(arr[0]["causes"].is_array());
        assert_eq!(arr[0]["cause_kinds"][0].as_str().unwrap(), "CpuHigh");
        assert_eq!(arr[0]["primary_cause"].as_str().unwrap(), "CpuHigh");
        assert_eq!(arr[0]["severity"].as_str().unwrap(), "major");
        assert!(arr[0]["culprits"].as_array().unwrap()[0]["name"]
            .as_str()
            .unwrap()
            .contains("app"));
        // 契约「时间一律 ISO8601」：timestamp 与 onset_ts 都必须是可 RFC3339 解析的字符串
        for key in ["timestamp", "onset_ts"] {
            let s = arr[0][key].as_str().unwrap_or_else(|| panic!("{} 应为字符串", key));
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap_or_else(|e| panic!("{} \"{}\" 应为合法 RFC3339: {}", key, s, e));
        }
        // cause_first_touch：相对 onset 的偏移毫秒（数值），字段名带 _offset_ms 示意
        let touch = arr[0]["cause_first_touch_offset_ms"].as_object().unwrap();
        assert_eq!(touch.get("CpuHigh").and_then(|v| v.as_i64()), Some(0));
        assert!(arr[0].get("cause_first_touch").is_none(), "旧字段名不应存在");
        std::fs::remove_file(&db).ok();
    }

    /// 契约锁定：onset_ts 缺失（旧库）时输出 null 而非裸数值；
    /// 所有「绝对时间点」字段（timestamp/onset_ts）要么 ISO8601 字符串要么 null。
    #[test]
    fn event_to_json_absolute_times_are_iso8601_or_null() {
        let ev = find_stutter_core::StutterEvent {
            id: 7,
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-08-16T09:35:03.167447200+00:00")
                .unwrap()
                .with_timezone(&Utc),
            duration_ms: 3212,
            severity: Severity::Critical,
            onset_ts: Some(1_786_872_868_403), // 实测样例里的 epoch 毫秒
            ..Default::default()
        };
        let v = event_to_json(&ev);
        // onset_ts：Unix 毫秒 → 必须已转 ISO8601 字符串（不再是整数）
        let onset = v["onset_ts"].as_str().expect("onset_ts 应为 ISO8601 字符串");
        let parsed = chrono::DateTime::parse_from_rfc3339(onset).expect("onset_ts 应可 RFC3339 解析");
        assert_eq!(parsed.timestamp_millis(), 1_786_872_868_403);
        // timestamp 同为 RFC3339 字符串
        chrono::DateTime::parse_from_rfc3339(v["timestamp"].as_str().unwrap()).unwrap();

        // onset_ts 缺失 → null（不是裸数值）
        let ev2 = find_stutter_core::StutterEvent {
            onset_ts: None,
            ..ev.clone()
        };
        assert!(event_to_json(&ev2)["onset_ts"].is_null());
    }

    #[test]
    fn events_json_range_filter() {
        let db = unique_db("events_range");
        seed(&db, 5, 0);
        let conn = open_db(Path::new(&db)).unwrap();
        // 只查「今日零点后 2 分钟内」：事件在 0/1/2/3/4 分钟 → 应有 3 条（0,1,2）
        let base = local_today_bounds_utc().0;
        let v = events_json(
            &conn,
            Some(&base.to_rfc3339()),
            Some(&(base + chrono::Duration::minutes(2) + chrono::Duration::seconds(59)).to_rfc3339()),
            100,
        )
        .unwrap();
        assert_eq!(v.as_array().unwrap().len(), 3);
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn events_json_empty_db_returns_empty_array() {
        let db = unique_db("events_empty");
        seed(&db, 0, 0);
        let conn = open_db(Path::new(&db)).unwrap();
        let v = events_json(&conn, None, None, 100).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 0);
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn samples_json_limit_and_order() {
        let db = unique_db("samples");
        seed(&db, 0, 10);
        let conn = open_db(Path::new(&db)).unwrap();
        let v = samples_json(&conn, None, None, 4).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 4, "limit=4 应只返回最新 4 条");
        // 升序排列（SQL 降序取最新 4 条后反转）
        let t0 = arr[0]["timestamp"].as_str().unwrap().to_string();
        let t3 = arr[3]["timestamp"].as_str().unwrap().to_string();
        assert!(t0 < t3, "应按时间升序输出: {} !< {}", t0, t3);
        // 主要字段在
        assert!(arr[0]["cpu_usage"].is_number());
        assert!(arr[0]["mem_usage_percent"].is_number());
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn samples_json_range_filter() {
        let db = unique_db("samples_range");
        seed(&db, 0, 10);
        let conn = open_db(Path::new(&db)).unwrap();
        let base = local_today_bounds_utc().0;
        let v = samples_json(
            &conn,
            Some(&base.to_rfc3339()),
            Some(&(base + chrono::Duration::seconds(3) + chrono::Duration::milliseconds(999)).to_rfc3339()),
            100,
        )
        .unwrap();
        assert_eq!(v.as_array().unwrap().len(), 4, "[0,3.999s] 应含 0/1/2/3 秒四条");
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn analysis_json_empty_db_returns_null_placeholders() {
        // 空库：kpi 全 0/「—」，root_cause 为 null 字段齐全的占位对象（不崩、不缺键）
        let db = unique_db("analysis_empty");
        seed(&db, 0, 0);
        let conn = open_db(Path::new(&db)).unwrap();
        let v = analysis_json(&conn, None, None).unwrap();
        assert_eq!(v["kpi"]["count"].as_u64().unwrap(), 0);
        assert_eq!(v["kpi"]["worst_duration_ms"].as_u64().unwrap(), 0);
        assert_eq!(v["kpi"]["top_culprit"].as_str().unwrap(), "—");
        assert!(v["trend"].as_array().unwrap().is_empty());
        assert!(v["culprits"].as_array().unwrap().is_empty());
        let rc = &v["root_cause"];
        assert!(rc["event_id"].is_null());
        assert!(rc["confidence"].is_null());
        assert!(rc["cause_chain"].as_array().unwrap().is_empty());
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn analysis_json_shape() {
        let db = unique_db("analysis");
        seed(&db, 3, 5);
        let conn = open_db(Path::new(&db)).unwrap();
        let v = analysis_json(&conn, None, None).unwrap();
        // 顶层必有六键
        for key in ["range", "kpi", "trend", "culprits", "cause_types", "root_cause"] {
            assert!(v.get(key).is_some(), "analysis 缺少键 {}", key);
        }
        // kpi 数字口径
        assert_eq!(v["kpi"]["count"].as_u64().unwrap(), 3);
        assert_eq!(v["kpi"]["worst_duration_ms"].as_u64().unwrap(), 3000);
        // 元凶榜非空
        assert_eq!(v["culprits"].as_array().unwrap().len(), 1);
        // 根因报告：最近事件（id=3）置信度 0.9（单 cause；f32 精度用容差比较）
        assert_eq!(v["root_cause"]["event_id"].as_u64().unwrap(), 3);
        assert!((v["root_cause"]["confidence"].as_f64().unwrap() - 0.9).abs() < 1e-6);
        assert_eq!(v["root_cause"]["cause_chain"][0].as_str().unwrap(), "CpuHigh");
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn analysis_json_custom_range() {
        let db = unique_db("analysis_range");
        seed(&db, 3, 0);
        let conn = open_db(Path::new(&db)).unwrap();
        let base = local_today_bounds_utc().0;
        let v = analysis_json(
            &conn,
            Some(&base.to_rfc3339()),
            Some(&(base + chrono::Duration::minutes(1) + chrono::Duration::seconds(30)).to_rfc3339()),
        )
        .unwrap();
        assert_eq!(v["kpi"]["count"].as_u64().unwrap(), 2);
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn resolve_range_defaults_to_today() {
        let (start, _end) = resolve_range(None, None).unwrap();
        let (today_start, _) = TimeRange::Today.bounds();
        assert_eq!(start, today_start);
    }

    #[test]
    fn resolve_range_parses_explicit() {
        let (start, end) = resolve_range(Some("2026-08-16"), Some("2026-08-16T23:59:59Z")).unwrap();
        assert!(start.ends_with("+00:00"));
        assert_eq!(end, "2026-08-16T23:59:59+00:00");
    }

    #[test]
    fn resolve_range_rejects_garbage() {
        assert!(resolve_range(Some("not-a-time"), None).is_err());
        assert!(resolve_range(None, Some("not-a-time")).is_err());
    }

    #[test]
    fn config_json_shape() {
        let v = config_json();
        // 顶层三键：source / db_path / config
        assert!(v.get("source").is_some());
        assert!(v.get("db_path").is_some());
        let cfg = v.get("config").unwrap().as_object().unwrap();
        // 有效值结构：六大节齐全（默认回退也应齐全）
        for section in ["sampling", "detection", "ui", "storage", "notifications", "logging"] {
            assert!(cfg.get(section).is_some(), "config 缺少节 {}", section);
        }
    }
}
