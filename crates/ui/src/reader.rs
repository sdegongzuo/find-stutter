//! SQLite 轮询 reader。
//!
//! 1Hz 读取 stutter.db（由 find-stutter-service 持续写入）：
//! - `latest_sample_summary`（CPU/内存/网络/磁盘/GPU/温度）
//! - `latest_heartbeat`（服务健康检测）
//! - `latest_event`（上次卡顿事件，用于「上次闪烁」提示）
//! - `event_count_today`（今日卡顿计数）
//!
//! 在 WAL 模式下，服务写库不阻塞 GUI 读；GUI 也不需要与 Collector 共享线程。
//!
//! 当 `stutter.db` 不存在 / 服务未启动时返回 `ServiceHealth::NoDatabase` / `Stopped`，
//! UI 显示「服务已停止」提示，不弹错。

use std::path::PathBuf;
use std::time::Duration;

use find_stutter_core::logger::LatestSampleSummary;
use find_stutter_core::{Config, StutterEvent};
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
}

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
        }
    }

    /// 1Hz tick：读最新 sample + 心跳 + 今日事件数，返回 `PollResult`。
    pub fn poll(&self) -> PollResult {
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

        // 4) 读今日事件数
        let today_event_count: u32 = {
            let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
            conn.query_row(
                "SELECT COUNT(*) FROM stutter_events WHERE timestamp LIKE ?1",
                rusqlite::params![format!("{}%", today)],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n as u32)
            .unwrap_or(0)
        };

        // 5) 读最近一次事件（用于「上次闪烁」提示）
        let event: Option<StutterEvent> = conn
            .query_row(
                "SELECT timestamp, duration_ms, severity, causes, snapshot \
                 FROM stutter_events ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    let ts_str: String = row.get(0)?;
                    let duration_ms: i64 = row.get(1)?;
                    let severity_str: String = row.get(2)?;
                    let causes_str: String = row.get(3)?;
                    let snapshot_str: String = row.get(4)?;
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
                    Ok(StutterEvent {
                        timestamp,
                        duration_ms: duration_ms as u64,
                        severity,
                        causes,
                        snapshot,
                    })
                },
            )
            .ok();

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
                })
                .unwrap();
        }
        logger.flush().unwrap();

        let reader = DbReader::new(&db);
        let r = reader.poll();
        assert_eq!(r.today_event_count, 3);

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
}
