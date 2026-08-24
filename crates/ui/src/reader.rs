//! SQLite 轮询 reader。
//!
//! 1Hz 读取 stutter.db（由 find-stutter-service 持续写入）：
//! - `latest_sample_summary`（CPU/内存/网络/磁盘/GPU/温度）
//! - `latest_heartbeat`（服务健康检测）
//! - `latest_event`（上次卡顿事件，用于「上次闪烁」提示）
//! - `event_count_today`（今日卡顿计数）
//! - 今日 / 本周累计网络流量（悬浮窗流量显示，节流见 `TRAFFIC_REFRESH_EVERY`）
//!
//! 在 WAL 模式下，服务写库不阻塞 GUI 读；GUI 也不需要与 Collector 共享线程。
//!
//! 当 `stutter.db` 不存在 / 服务未启动时返回 `ServiceHealth::NoDatabase` / `Stopped`，
//! UI 显示「服务已停止」提示，不弹错。

use std::path::PathBuf;
use std::time::Duration;

use find_stutter_core::logger::LatestSampleSummary;
use find_stutter_core::{CauseKind, Config, ProcessBrief, StutterEvent};
use parking_lot::Mutex;
use rusqlite::Connection;

/// 服务健康状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceHealth {
    /// 心跳在阈值内新鲜
    Running,
    /// 心跳存在但超时（服务卡了？磁盘 IO 慢？）
    Stale,
    /// 心跳表为空（服务从未启动过）
    Stopped,
    /// stutter.db 不存在或无法打开
    NoDatabase,
}

impl ServiceHealth {
    pub fn is_responsive(self) -> bool {
        matches!(self, ServiceHealth::Running)
    }
}

/// 1Hz 轮询结果
#[derive(Debug, Clone)]
pub struct PollResult {
    pub summary: Option<LatestSampleSummary>,
    pub event: Option<StutterEvent>,
    pub health: ServiceHealth,
    pub today_event_count: u32,
    /// 最近一次心跳时间戳（RFC3339），None 表示从未启动
    pub last_heartbeat: Option<String>,
    /// 当日累计网络流量 `(发送字节, 接收字节)`——samples 每秒速率求和的近似值
    /// （口径见 `find_stutter_core::logger::sum_network_traffic`）
    pub today_net_bytes: (u64, u64),
    /// 当周（本地周一零点起）累计网络流量 `(发送字节, 接收字节)`
    pub week_net_bytes: (u64, u64),
}

/// SQLite reader + 服务健康检测器。
///
/// 启动时打开 stutter.db（失败时不退出，下次 tick 重试），
/// 每次 `poll()` 复用同一连接跑只读查询。
pub struct DbReader {
    db_path: PathBuf,
    conn: Mutex<Option<Connection>>,
    /// 心跳超过此间隔视为 Stale（默认 5s）
    pub stale_threshold: Duration,
    /// 累计流量刷新节流：每 N 次 poll 才真正跑一次 SUM 查询。
    /// 累计值变化慢，而 SUM 需扫过当日全部样本行（一天最多 ~8.6 万行），
    /// 1Hz 全量重算纯属浪费；10s 刷新粒度对「累计流量」显示足够。
    traffic_refresh_every: u32,
    traffic_tick: std::cell::Cell<u32>,
    /// 最近一次流量统计缓存：`(今日 (sent, recv), 本周 (sent, recv))`。
    /// db 打不开时也返回缓存而非清零，避免界面数字闪烁。
    traffic_cache: Mutex<((u64, u64), (u64, u64))>,
}

/// 累计流量查询节流周期（次）：每 N 次 poll 刷一次今日/本周 SUM。
const TRAFFIC_REFRESH_EVERY: u32 = 10;

impl DbReader {
    /// 用 config.toml 默认路径构造 reader（找不到 db 不会立即失败）
    pub fn from_config() -> Self {
        let config = Config::load("config.toml").unwrap_or_default();
        Self::new(config.storage.db_path)
    }

    /// 显式指定 db 路径
    pub fn new<P: Into<PathBuf>>(db_path: P) -> Self {
        Self {
            db_path: db_path.into(),
            conn: Mutex::new(None),
            stale_threshold: Duration::from_secs(5),
            traffic_refresh_every: TRAFFIC_REFRESH_EVERY,
            traffic_tick: std::cell::Cell::new(0),
            traffic_cache: Mutex::new(((0, 0), (0, 0))),
        }
    }

    /// 读缓存的流量统计（db 打不开 / 未刷新时返回最近一次结果）。
    fn cached_traffic(&self) -> ((u64, u64), (u64, u64)) {
        *self.traffic_cache.lock()
    }

    /// 完整轮询（含事件 snapshot/culprits 反序列化）——供测试与需要事件详情的调用方使用。
    /// 1Hz tick：读最新 sample + 心跳 + 今日事件数，返回 `PollResult`。
    pub fn poll(&self) -> PollResult {
        self.poll_impl(true)
    }

    /// 轻量轮询：事件只读 timestamp（overlay「上次卡顿」提示用，省大 JSON 解析）
    pub fn poll_light(&self) -> PollResult {
        self.poll_impl(false)
    }

    /// 轮询实现。
    ///
    /// `want_event_detail == false` 时事件段只 SELECT timestamp 一列，
    /// 不反序列化 snapshot/culprits 两个大 JSON 字段（overlay 只显示「上次卡顿时间」）。
    fn poll_impl(&self, want_event_detail: bool) -> PollResult {
        // 流量统计缓存：db 打不开的早退分支也带上最近一次结果，避免悬浮窗闪 0
        let cached = self.cached_traffic();
        // 1) 拿到（或建立）连接
        let mut guard = self.conn.lock();
        if guard.is_none() {
            match Connection::open_with_flags(
                &self.db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            ) {
                Ok(c) => {
                    // 强制 WAL 读视图（read-only 连接仍可读 WAL 内的未 checkpoint 数据）
                    let _ = c.execute_batch("PRAGMA journal_mode=WAL;");
                    *guard = Some(c);
                }
                Err(e) => {
                    log::warn!("DbReader: open {:?} failed: {}", self.db_path, e);
                    return PollResult {
                        summary: None,
                        event: None,
                        health: ServiceHealth::NoDatabase,
                        today_event_count: 0,
                        last_heartbeat: None,
                        today_net_bytes: cached.0,
                        week_net_bytes: cached.1,
                    };
                }
            }
        }
        let Some(conn) = guard.as_ref() else {
            return PollResult {
                summary: None,
                event: None,
                health: ServiceHealth::NoDatabase,
                today_event_count: 0,
                last_heartbeat: None,
                today_net_bytes: cached.0,
                week_net_bytes: cached.1,
            };
        };

        // 2) 读 summary（最新 sample）
        let summary = conn
            .query_row(
                "SELECT timestamp, cpu_usage, mem_usage_percent, mem_available_mb, \
                        net_sent_bps, net_recv_bps, disk_read_bps, disk_write_bps, \
                        gpu_usage, cpu_temp \
                 FROM samples ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    let ts: String = row.get(0)?;
                    let cpu: f32 = row.get::<_, Option<f32>>(1)?.unwrap_or(0.0);
                    let mem_pct: f32 = row.get::<_, Option<f32>>(2)?.unwrap_or(0.0);
                    let mem: i64 = row.get::<_, Option<i64>>(3)?.unwrap_or(0);
                    let ns: i64 = row.get::<_, Option<i64>>(4)?.unwrap_or(0);
                    let nr: i64 = row.get::<_, Option<i64>>(5)?.unwrap_or(0);
                    let dr: i64 = row.get::<_, Option<i64>>(6)?.unwrap_or(0);
                    let dw: i64 = row.get::<_, Option<i64>>(7)?.unwrap_or(0);
                    let gpu: Option<f32> = row.get(8)?;
                    let temp: Option<f32> = row.get(9)?;
                    Ok(LatestSampleSummary {
                        timestamp: ts,
                        cpu_usage: cpu,
                        mem_usage_percent: mem_pct,
                        mem_available_mb: mem as u64,
                        net_sent_bps: ns as u64,
                        net_recv_bps: nr as u64,
                        disk_read_bps: dr as u64,
                        disk_write_bps: dw as u64,
                        gpu_usage: gpu,
                        cpu_temp: temp,
                    })
                },
            )
            .ok();

        // 3) 读心跳（service_heartbeat 单行）
        let heartbeat: Option<(String, i64)> = conn
            .query_row(
                "SELECT timestamp, pid FROM service_heartbeat WHERE id = 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .ok();

        // 4) 读今日事件数（按用户本地时区「今日」：本地零点 → 现在，BETWEEN UTC 边界；
        //     与 logger.event_count_today 共用 local_today_bounds，保证悬浮窗与分析页一致）
        let today_event_count: u32 = {
            let (start, end) = find_stutter_core::logger::local_today_bounds();
            conn.query_row(
                "SELECT COUNT(*) FROM stutter_events WHERE timestamp BETWEEN ?1 AND ?2",
                rusqlite::params![start, end],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n as u32)
            .unwrap_or(0)
        };

        // 4b) 今日 / 本周累计网络流量（悬浮窗原「磁盘读写速率」位改显累计流量）。
        // 口径：samples 约 1Hz 落库，net_sent_bps / net_recv_bps 是该秒传输字节数
        // （网卡计数差分），窗口内求和即近似累计字节量——跨重启成立，故不用
        // net_*_total（开机累计、重启清零）。每 traffic_refresh_every 次 poll 才真正
        // 查库一次，其余 tick 读缓存（SUM 需扫当日全部样本行，逐秒重算太浪费）。
        let (today_net_bytes, week_net_bytes) = {
            let refresh = self.traffic_tick.get() % self.traffic_refresh_every == 0;
            self.traffic_tick.set(self.traffic_tick.get().wrapping_add(1));
            let mut cache = self.traffic_cache.lock();
            if refresh {
                let (t0, t1) = find_stutter_core::logger::local_today_bounds();
                if let Some(v) = find_stutter_core::logger::sum_network_traffic(conn, &t0, &t1).ok() {
                    cache.0 = v;
                }
                let (w0, w1) = find_stutter_core::logger::local_this_week_bounds();
                if let Some(v) = find_stutter_core::logger::sum_network_traffic(conn, &w0, &w1).ok() {
                    cache.1 = v;
                }
            }
            *cache
        };

        // 5) 读最近一次事件（用于「上次闪烁」提示）。
        // 旧库（P5 之前）没有 culprits 列，先探测列是否存在，存在才在 SELECT 里带上；
        // 缺失时 culprits 回退为空列表，避免整条事件因缺列而读不出来（Spec 回归兜底）。
        //
        // overlay 只展示「上次卡顿时间」：轻量路径（want_event_detail == false）只取
        // timestamp 列，避免每 tick 反序列化 snapshot/culprits 两个大 JSON 字段。
        let event: Option<StutterEvent> = if want_event_detail {
            let columns: Vec<String> = conn
                .prepare("PRAGMA table_info(stutter_events)")
                .and_then(|mut stmt| {
                    let names: Vec<String> = stmt
                        .query_map([], |row| row.get::<_, String>(1))?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(names)
                })
                .unwrap_or_default();
            let has_culprits = columns.iter().any(|n| n == "culprits");
            // F-RC1 新列：检测是否存在（与 culprits 同理）。四列随同一迁移批次写入，
            // 存在性以 lead 列 cause_kinds 代表即可，缺失则回退默认值。
            let has_cause_kinds = columns.iter().any(|n| n == "cause_kinds");

            let mut select =
                "id, timestamp, duration_ms, severity, causes, snapshot".to_string();
            if has_culprits {
                select.push_str(", culprits");
            }
            if has_cause_kinds {
                select.push_str(", cause_kinds, primary_cause, cause_first_touch, onset_ts");
            }
            let event_sql = format!(
                "SELECT {} FROM stutter_events ORDER BY id DESC LIMIT 1",
                select
            );
            conn.query_row(&event_sql, [], |row| {
                let id: i64 = row.get(0)?;
                let ts_str: String = row.get(1)?;
                let duration_ms: i64 = row.get(2)?;
                let severity_str: String = row.get(3)?;
                let causes_str: String = row.get(4)?;
                let snapshot_str: String = row.get(5)?;
                let timestamp = chrono::DateTime::parse_from_rfc3339(&ts_str)
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now());
                let severity = match severity_str.as_str() {
                    "critical" => find_stutter_core::Severity::Critical,
                    "major" => find_stutter_core::Severity::Major,
                    _ => find_stutter_core::Severity::Minor,
                };
                let causes: Vec<String> =
                    serde_json::from_str(&causes_str).unwrap_or_default();
                let snapshot: find_stutter_core::Sample =
                    serde_json::from_str(&snapshot_str).unwrap_or_default();
                let mut idx = 6usize;
                let culprits: Vec<ProcessBrief> = if has_culprits {
                    let culprits_str: String = row.get(idx)?;
                    idx += 1;
                    serde_json::from_str(&culprits_str).unwrap_or_default()
                } else {
                    Vec::new()
                };
                let (cause_kinds, primary_cause, cause_first_touch, onset_ts) = if has_cause_kinds {
                    let cause_kinds_str: String = row.get(idx)?;
                    idx += 1;
                    let primary_cause_str: String = row.get(idx)?;
                    idx += 1;
                    let cause_first_touch_str: String = row.get(idx)?;
                    idx += 1;
                    let onset_ts: Option<i64> = row.get(idx)?;
                    (
                        serde_json::from_str(&cause_kinds_str).unwrap_or_default(),
                        serde_json::from_str(&primary_cause_str).unwrap_or_default(),
                        serde_json::from_str(&cause_first_touch_str).unwrap_or_default(),
                        onset_ts,
                    )
                } else {
                    (Vec::new(), None, std::collections::HashMap::new(), None)
                };
                let mut event = StutterEvent {
                    id,
                    timestamp,
                    duration_ms: duration_ms as u64,
                    severity,
                    causes,
                    cause_kinds,
                    primary_cause,
                    cause_first_touch,
                    onset_ts,
                    snapshot,
                    culprits,
                };
                // 旧库 cause_kinds 为空时用 cause_key 可靠回填（精确映射，非脆弱关键词）：
                // 仅当结构化字段为空、但自由文本 causes 非空时触发（见 PRD §3.1）。
                if event.cause_kinds.is_empty() && !event.causes.is_empty() {
                    event.cause_kinds = event
                        .causes
                        .iter()
                        .filter_map(|c| CauseKind::from_cause(c))
                        .collect();
                }
                Ok(event)
            })
            .ok()
        } else {
            // overlay 只展示「上次卡顿时间」：轻量路径只取 timestamp，
            // 避免每 tick 反序列化 snapshot/culprits 两个大 JSON。
            conn.query_row(
                "SELECT timestamp FROM stutter_events ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    let ts_str: String = row.get(0)?;
                    let timestamp = chrono::DateTime::parse_from_rfc3339(&ts_str)
                        .map(|d| d.with_timezone(&chrono::Utc))
                        .unwrap_or_else(|_| chrono::Utc::now());
                    Ok(StutterEvent {
                        timestamp,
                        ..Default::default()
                    })
                },
            )
            .ok()
        };

        // 6) 推算 health
        let health = if let Some((ts, _)) = &heartbeat {
            match chrono::DateTime::parse_from_rfc3339(ts) {
                Ok(parsed) => {
                    let age = chrono::Utc::now()
                        .signed_duration_since(parsed.with_timezone(&chrono::Utc));
                    let age_dur = Duration::from_millis(age.num_milliseconds().max(0) as u64);
                    if age_dur < self.stale_threshold {
                        ServiceHealth::Running
                    } else {
                        ServiceHealth::Stale
                    }
                }
                Err(_) => ServiceHealth::Stale,
            }
        } else {
            ServiceHealth::Stopped
        };

        PollResult {
            summary,
            event,
            health,
            today_event_count,
            last_heartbeat: heartbeat.map(|(ts, _)| ts),
            today_net_bytes,
            week_net_bytes,
        }
    }

    /// 关闭底层连接（UI 退出时调用）
    pub fn close(&self) {
        *self.conn.lock() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use find_stutter_core::{Logger, Sample, Severity, StorageConfig};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_db(name: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("fs_reader_{}_{}.db", name, nanos))
            .to_str()
            .unwrap()
            .to_string()
    }

    /// 验证：心跳写后 5s 内 poll 返回 Running
    #[test]
    fn poll_running_when_heartbeat_fresh() {
        let db = unique_db("running");
        let cfg = StorageConfig {
            db_path: db.clone(),
            retention_days: 30,
            event_retention_days: 30, hot_retention_days: 0, // 测试关闭降采样，保持既有行为口径
        };
        let mut logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();
        // 写入带 mem_usage_percent 的 sample，验证 GUI 读到的百分比正确
        let mut s = Sample::default();
        s.cpu_usage = 30.0;
        s.mem_usage_percent = 62.5;
        s.mem_available_mb = 4096;
        logger.write_sample(&s).unwrap();
        logger.flush().unwrap();

        let reader = DbReader::new(&db);
        let r = reader.poll();
        assert_eq!(r.health, ServiceHealth::Running);
        let summary = r.summary.expect("应有 summary");
        assert_eq!(summary.cpu_usage, 30.0);
        assert_eq!(summary.mem_usage_percent, 62.5);
        assert_eq!(summary.mem_available_mb, 4096);
        assert!(r.last_heartbeat.is_some());

        std::fs::remove_file(&db).ok();
    }

    /// 验证：心跳缺失时返回 Stopped
    #[test]
    fn poll_stopped_when_no_heartbeat() {
        let db = unique_db("stopped");
        let cfg = StorageConfig {
            db_path: db.clone(),
            retention_days: 30,
            event_retention_days: 30, hot_retention_days: 0, // 测试关闭降采样，保持既有行为口径
        };
        // 只建表 + 写一条 sample，不写心跳
        let mut logger = Logger::new(&cfg).unwrap();
        logger.write_sample(&Sample::default()).unwrap();
        logger.flush().unwrap();

        let reader = DbReader::new(&db);
        let r = reader.poll();
        assert_eq!(r.health, ServiceHealth::Stopped);
        assert!(r.summary.is_some());
        assert!(r.last_heartbeat.is_none());

        std::fs::remove_file(&db).ok();
    }

    /// 验证：db 不存在时返回 NoDatabase
    #[test]
    fn poll_no_database_when_missing() {
        let reader = DbReader::new("C:/__definitely_not_exists__/nope.db");
        let r = reader.poll();
        assert_eq!(r.health, ServiceHealth::NoDatabase);
        assert!(r.summary.is_none());
        assert!(r.last_heartbeat.is_none());
    }

    /// 验证：自定义 stale_threshold 影响状态判定
    #[test]
    fn poll_uses_stale_threshold() {
        let db = unique_db("threshold");
        let cfg = StorageConfig {
            db_path: db.clone(),
            retention_days: 30,
            event_retention_days: 30, hot_retention_days: 0, // 测试关闭降采样，保持既有行为口径
        };
        let logger = Logger::new(&cfg).unwrap();
        // 写一个 2 小时前的心跳
        let two_hours_ago = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO service_heartbeat (id, timestamp, pid) VALUES (1, ?1, ?2)",
            rusqlite::params![two_hours_ago, 0i64],
        )
        .unwrap();
        drop(conn);
        drop(logger);

        let mut reader = DbReader::new(&db);
        // 1s 阈值 → 2h 前的心跳算 Stale
        reader.stale_threshold = Duration::from_secs(1);
        assert_eq!(reader.poll().health, ServiceHealth::Stale);
        // 3h 阈值 → 2h 前的心跳算 Running
        reader.stale_threshold = Duration::from_secs(3600 * 3);
        assert_eq!(reader.poll().health, ServiceHealth::Running);

        std::fs::remove_file(&db).ok();
    }

    /// 验证：今日事件数可读
    #[test]
    fn poll_today_event_count() {
        let db = unique_db("count");
        let cfg = StorageConfig {
            db_path: db.clone(),
            retention_days: 30,
            event_retention_days: 30, hot_retention_days: 0, // 测试关闭降采样，保持既有行为口径
        };
        let mut logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();
        for i in 0..3 {
            let mut s = Sample::default();
            s.cpu_usage = 100.0; // 触发卡顿
            logger
                .write_event(&find_stutter_core::StutterEvent {
                    timestamp: chrono::Utc::now(),
                    duration_ms: 100 + i,
                    severity: Severity::Major,
                    causes: vec!["test".into()],
                    snapshot: s,
                    culprits: vec![],
                    ..Default::default()
                })
                .unwrap();
        }
        logger.flush().unwrap();

        let reader = DbReader::new(&db);
        let r = reader.poll();
        assert_eq!(r.today_event_count, 3);

        std::fs::remove_file(&db).ok();
    }

    /// 验证：今日 / 本周累计网络流量按「速率求和」口径统计，
    /// 且窗口外（10 天前，必在本 ISO 周之外）的样本不计入。
    #[test]
    fn poll_reads_network_traffic_windows() {
        let db = unique_db("net");
        let cfg = StorageConfig {
            db_path: db.clone(),
            retention_days: 30,
            event_retention_days: 30, hot_retention_days: 0, // 测试关闭降采样，保持既有行为口径
        };
        let logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();
        // 直接插入带控制时间戳的样本：现在（必属今日 & 本周）与 10 天前
        // （ISO 周最多回溯到本周一即至多 6 天前，故必在今日/本周窗口之外）
        let conn = Connection::open(&db).unwrap();
        let now = chrono::Utc::now();
        let old = now - chrono::Duration::days(10);
        for (ts, sent, recv) in [
            (now.to_rfc3339(), 1000_i64, 2000_i64),
            (old.to_rfc3339(), 500_000_i64, 900_000_i64),
        ] {
            conn.execute(
                "INSERT INTO samples (timestamp, net_sent_bps, net_recv_bps) VALUES (?1, ?2, ?3)",
                rusqlite::params![ts, sent, recv],
            )
            .unwrap();
        }
        drop(conn);

        let reader = DbReader::new(&db);
        let r = reader.poll();
        assert_eq!(r.today_net_bytes, (1000, 2000), "今日只应计入当前样本");
        assert_eq!(r.week_net_bytes, (1000, 2000), "10 天前样本不应计入本周");

        std::fs::remove_file(&db).ok();
    }

    /// 验证 ServiceHealth::is_responsive
    #[test]
    fn service_health_is_responsive() {
        assert!(ServiceHealth::Running.is_responsive());
        assert!(!ServiceHealth::Stale.is_responsive());
        assert!(!ServiceHealth::Stopped.is_responsive());
        assert!(!ServiceHealth::NoDatabase.is_responsive());
    }

    /// 验证：reader 失败后 close 再 poll 能重新建连接（模拟服务挂掉后重启）
    #[test]
    fn poll_reconnects_after_close() {
        let db = unique_db("reconnect");
        let cfg = StorageConfig {
            db_path: db.clone(),
            retention_days: 30,
            event_retention_days: 30, hot_retention_days: 0, // 测试关闭降采样，保持既有行为口径
        };
        let mut logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();
        logger.flush().unwrap();

        let reader = DbReader::new(&db);
        assert_eq!(reader.poll().health, ServiceHealth::Running);
        reader.close();
        // 关闭后心跳「过期」→ 模拟服务挂掉后无新数据
        let two_hours_ago = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE service_heartbeat SET timestamp = ?1 WHERE id = 1",
            rusqlite::params![two_hours_ago],
        )
        .unwrap();
        drop(conn);
        // 再次 poll：默认 5s 阈值，2h 前 → Stale
        let r = reader.poll();
        assert_eq!(r.health, ServiceHealth::Stale);

        std::fs::remove_file(&db).ok();
    }

    /// 验证：事件里的 culprits（卡顿元凶进程）能被 reader 正确读回
    #[test]
    fn poll_reads_culprits() {
        let db = unique_db("culprits");
        let cfg = StorageConfig {
            db_path: db.clone(),
            retention_days: 30,
            event_retention_days: 30, hot_retention_days: 0, // 测试关闭降采样，保持既有行为口径
        };
        let logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();

        let mut s = Sample::default();
        s.cpu_usage = 100.0;
        let ev = find_stutter_core::StutterEvent {
            timestamp: chrono::Utc::now(),
            duration_ms: 5000,
            severity: Severity::Major,
            causes: vec!["CPU usage 95.0% > 90.0%".into()],
            snapshot: s,
            culprits: vec![find_stutter_core::ProcessBrief {
                pid: 777,
                name: "hog.exe".into(),
                cpu_usage: 90.0,
                mem_used_mb: 512,
                ..Default::default()
            }],
            ..Default::default()
        };
        logger.write_event(&ev).unwrap();

        let reader = DbReader::new(&db);
        let event = reader.poll().event.expect("应读到最近事件");
        assert_eq!(event.culprits.len(), 1);
        assert_eq!(event.culprits[0].pid, 777);
        assert_eq!(event.culprits[0].name, "hog.exe");

        std::fs::remove_file(&db).ok();
    }

    /// F-RC1：结构化根因字段（cause_kinds / primary_cause / onset_ts /
    /// cause_first_touch / id）应能随事件落库并回读。
    #[test]
    fn poll_reads_cause_kinds_and_primary_cause() {
        let db = unique_db("cause_kinds");
        let cfg = StorageConfig {
            db_path: db.clone(),
            retention_days: 30,
            event_retention_days: 30, hot_retention_days: 0, // 测试关闭降采样，保持既有行为口径
        };
        let logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();

        let mut s = Sample::default();
        s.cpu_usage = 100.0;
        let ev = find_stutter_core::StutterEvent {
            timestamp: chrono::Utc::now(),
            duration_ms: 5000,
            severity: Severity::Major,
            causes: vec![
                "CPU usage 95.0% > 90.0%".into(),
                "Available memory 100MB < 500MB".into(),
            ],
            snapshot: s,
            cause_kinds: vec![CauseKind::CpuHigh, CauseKind::MemLow],
            primary_cause: Some(CauseKind::CpuHigh),
            onset_ts: Some(1_700_000_000_000),
            cause_first_touch: {
                let mut m = std::collections::HashMap::new();
                m.insert(CauseKind::CpuHigh, 0i64);
                m.insert(CauseKind::MemLow, 1200i64);
                m
            },
            ..Default::default()
        };
        logger.write_event(&ev).unwrap();

        let reader = DbReader::new(&db);
        let event = reader.poll().event.expect("应读到最近事件");
        assert_eq!(event.id, 1, "应读出事件主键 id");
        assert_eq!(
            event.cause_kinds,
            vec![CauseKind::CpuHigh, CauseKind::MemLow]
        );
        assert_eq!(event.primary_cause, Some(CauseKind::CpuHigh));
        assert_eq!(event.onset_ts, Some(1_700_000_000_000));
        assert_eq!(
            event.cause_first_touch.get(&CauseKind::MemLow).copied(),
            Some(1200)
        );

        std::fs::remove_file(&db).ok();
    }

    /// 回归：旧库（stutter_events 无 cause_kinds 列）事件，cause_kinds 为空时
    /// 用 `cause_key` 可靠回填（精确映射，非脆弱关键词），primary_cause 回退 None。
    #[test]
    fn poll_refills_cause_kinds_from_legacy_causes() {
        let db = unique_db("legacy_cause_kinds");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE stutter_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                severity TEXT NOT NULL,
                causes TEXT NOT NULL,
                snapshot TEXT NOT NULL
            );
            CREATE TABLE service_heartbeat (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                timestamp TEXT NOT NULL,
                pid INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO stutter_events (timestamp, duration_ms, severity, causes, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                chrono::Utc::now().to_rfc3339(),
                3000i64,
                "major",
                r#"["CPU usage 95.0% > 90.0%","Available memory 100MB < 500MB"]"#,
                r#"{"cpu_usage":95.0}"#,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO service_heartbeat (id, timestamp, pid) VALUES (1, ?1, ?2)",
            rusqlite::params![chrono::Utc::now().to_rfc3339(), 0i64],
        )
        .unwrap();
        drop(conn);

        let reader = DbReader::new(&db);
        let event = reader.poll().event.expect("旧库事件应能读回");
        assert_eq!(event.duration_ms, 3000);
        // cause_kinds 无列 → 用 cause_key 可靠回填
        assert!(
            event.cause_kinds.contains(&CauseKind::CpuHigh),
            "应回填 CpuHigh: {:?}",
            event.cause_kinds
        );
        assert!(
            event.cause_kinds.contains(&CauseKind::MemLow),
            "应回填 MemLow: {:?}",
            event.cause_kinds
        );
        assert_eq!(
            event.primary_cause, None,
            "旧库无 primary_cause 列应回退 None"
        );

        std::fs::remove_file(&db).ok();
    }

    /// 回归：旧库（stutter_events 无 culprits 列）也能读回事件，culprits 为空列表
    #[test]
    fn poll_reads_event_from_legacy_schema_without_culprits() {
        let db = unique_db("legacy_events");
        // 手工构造旧版本库结构：stutter_events 不含 culprits 列，心跳表保留
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE stutter_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                severity TEXT NOT NULL,
                causes TEXT NOT NULL,
                snapshot TEXT NOT NULL
            );
            CREATE TABLE service_heartbeat (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                timestamp TEXT NOT NULL,
                pid INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO stutter_events (timestamp, duration_ms, severity, causes, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                chrono::Utc::now().to_rfc3339(),
                3000i64,
                "major",
                r#"["CPU usage 95.0% > 90.0%"]"#,
                r#"{"cpu_usage":95.0}"#,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO service_heartbeat (id, timestamp, pid) VALUES (1, ?1, ?2)",
            rusqlite::params![chrono::Utc::now().to_rfc3339(), 0i64],
        )
        .unwrap();
        drop(conn);

        let reader = DbReader::new(&db);
        let event = reader.poll().event.expect("旧库事件应能读回");
        assert_eq!(event.duration_ms, 3000);
        assert!(event.culprits.is_empty(), "旧库无 culprits 列应回退为空列表");

        std::fs::remove_file(&db).ok();
    }

    /// 轻量轮询（overlay 用）：事件只读 timestamp 一列，
    /// duration_ms==0 / culprits 为空证明没有走全量反序列化路径。
    #[test]
    fn poll_light_returns_timestamp_only() {
        let db = unique_db("light");
        let cfg = StorageConfig {
            db_path: db.clone(),
            retention_days: 30,
            event_retention_days: 30, hot_retention_days: 0, // 测试关闭降采样，保持既有行为口径
        };
        let logger = Logger::new(&cfg).unwrap();
        logger.touch_heartbeat().unwrap();

        let timestamp = chrono::Utc::now();
        let ev = find_stutter_core::StutterEvent {
            timestamp,
            duration_ms: 5000,
            severity: Severity::Major,
            causes: vec!["CPU usage 95.0% > 90.0%".into()],
            snapshot: Sample::default(),
            culprits: vec![find_stutter_core::ProcessBrief {
                pid: 777,
                name: "hog.exe".into(),
                cpu_usage: 90.0,
                mem_used_mb: 512,
                ..Default::default()
            }],
            ..Default::default()
        };
        logger.write_event(&ev).unwrap();

        let reader = DbReader::new(&db);
        let event = reader
            .poll_light()
            .event
            .expect("轻量轮询应读到最近事件");
        assert_eq!(event.timestamp, timestamp, "timestamp 应正确读回");
        assert_eq!(event.duration_ms, 0, "轻量路径不读 duration_ms");
        assert!(event.causes.is_empty(), "轻量路径不读 causes");
        assert!(event.culprits.is_empty(), "轻量路径不读 culprits");

        std::fs::remove_file(&db).ok();
    }
}
