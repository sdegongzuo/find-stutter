use crate::types::{Sample, StutterEvent, StorageConfig};
use chrono::{DateTime, Duration as ChronoDuration, Local, TimeZone, Utc};
use rusqlite::{params, Connection};
use std::time::{Duration, Instant};

pub struct Logger {
    conn: Connection,
    buffer: Vec<Sample>,
    config: StorageConfig,
    last_flush: Instant,
}

impl Logger {
    pub fn new(config: &StorageConfig) -> anyhow::Result<Self> {
        let conn = Connection::open(&config.db_path)?;
        // 开启 WAL：采集线程每秒写入，GUI / 导出命令并发读取时互不阻塞
        // （P3 服务化采集 + GUI 只读模式的基础）
        let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS samples (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                cpu_usage REAL,
                cpu_per_core TEXT,
                cpu_freq_mhz REAL,
                mem_usage_percent REAL,
                mem_used_mb INTEGER,
                mem_total_mb INTEGER,
                mem_available_mb INTEGER,
                swap_usage_percent REAL,
                disk_read_bps INTEGER,
                disk_write_bps INTEGER,
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
                page_reads_per_sec REAL
            );
            CREATE TABLE IF NOT EXISTS stutter_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                severity TEXT NOT NULL,
                causes TEXT NOT NULL,
                snapshot TEXT NOT NULL
            );
            -- P3：服务心跳表。Service 每次 tick 更新 timestamp，
            -- GUI 用此探活：超过 N 秒未更新即视为服务停止。
            -- 单行表，PRIMARY KEY 固定为 1，UPSERT 语义。
            CREATE TABLE IF NOT EXISTS service_heartbeat (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                timestamp TEXT NOT NULL,
                pid INTEGER NOT NULL
            );",
        )?;

        // 迁移：给历史库里的 stutter_events 补 culprits 列（旧库无此列）。
        // SQLite 不支持 `ADD COLUMN IF NOT EXISTS`，这里忽略「列已存在」错误。
        let _ = conn.execute_batch(
            "ALTER TABLE stutter_events ADD COLUMN culprits TEXT NOT NULL DEFAULT '[]';",
        );

        // 迁移（F-RC1）：结构化根因落库列。每条 ALTER 独立执行并忽略错误
        // （SQLite 不支持 `ADD COLUMN IF NOT EXISTS`，逐条吞错避免前一条报错
        // 中止后续迁移）。列存在性由 reader 在读取时探测，缺失即回退默认值。
        // - cause_kinds：结构化根因枚举数组（JSON）
        // - primary_cause：主因枚举（JSON，可空）
        // - cause_first_touch：各 cause 首触时刻偏移（JSON 对象，key 为枚举字符串）
        // - onset_ts：事件 onset 时刻（Unix 毫秒，可空）
        let _ = conn.execute_batch(
            "ALTER TABLE stutter_events ADD COLUMN cause_kinds TEXT NOT NULL DEFAULT '[]';",
        );
        let _ = conn.execute_batch("ALTER TABLE stutter_events ADD COLUMN primary_cause TEXT;");
        let _ = conn
            .execute_batch("ALTER TABLE stutter_events ADD COLUMN cause_first_touch TEXT NOT NULL DEFAULT '{}';");
        let _ = conn.execute_batch("ALTER TABLE stutter_events ADD COLUMN onset_ts INTEGER;");

        // 迁移：给历史库里的 samples 补 commit_bytes / commit_limit / page_reads_per_sec 列（旧库无此列）。
        // 每条 ALTER 独立执行并忽略错误——SQLite 不支持 `ADD COLUMN IF NOT EXISTS`，
        // 若某列已存在则该条 ALTER 报错；若合并成一条 execute_batch，前一条报错会
        // 中止后续所有 ALTER，导致新列漏加。故逐条执行、各自吞错。
        let _ = conn.execute_batch("ALTER TABLE samples ADD COLUMN commit_bytes INTEGER;");
        let _ = conn.execute_batch("ALTER TABLE samples ADD COLUMN commit_limit INTEGER;");
        let _ = conn.execute_batch("ALTER TABLE samples ADD COLUMN page_reads_per_sec REAL;");

        // M6：时间戳索引真正落地（PRD §3.3）。
        // 分析页严格只读 stutter.db，无法在只读连接上 CREATE INDEX，故索引必须在
        // service 端（本建表逻辑）创建——service 有写权限，重启/重装即生效。
        // 表必须已存在，索引用 IF NOT EXISTS 保证幂等（重复执行不报错）。
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_samples_ts ON samples(timestamp); \
             CREATE INDEX IF NOT EXISTS idx_events_ts ON stutter_events(timestamp);",
        )?;

        Ok(Self {
            conn,
            buffer: Vec::new(),
            config: config.clone(),
            last_flush: Instant::now(),
        })
    }

    pub fn write_sample(&mut self, sample: &Sample) -> anyhow::Result<()> {
        self.buffer.push(sample.clone());
        if self.buffer.len() >= 10 || self.last_flush.elapsed() >= Duration::from_secs(5) {
            self.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> anyhow::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO samples (
                    timestamp, cpu_usage, cpu_per_core, cpu_freq_mhz,
                    mem_usage_percent, mem_used_mb, mem_total_mb, mem_available_mb,
                    swap_usage_percent, disk_read_bps, disk_write_bps,
                    net_sent_bps, net_recv_bps, net_sent_total, net_recv_total,
                    gpu_usage, cpu_temp, gpu_temp, process_count, thread_count,
                    commit_bytes, commit_limit, page_reads_per_sec
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)",
            )?;
            for s in &self.buffer {
                let core_str: String = s
                    .cpu_per_core
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                stmt.execute(params![
                    s.timestamp.to_rfc3339(),
                    s.cpu_usage,
                    core_str,
                    s.cpu_freq_mhz,
                    s.mem_usage_percent,
                    s.mem_used_mb as i64,
                    s.mem_total_mb as i64,
                    s.mem_available_mb as i64,
                    s.swap_usage_percent,
                    s.disk_read_bps as i64,
                    s.disk_write_bps as i64,
                    s.net_sent_bps as i64,
                    s.net_recv_bps as i64,
                    s.net_sent_total as i64,
                    s.net_recv_total as i64,
                    s.gpu_usage,
                    s.cpu_temp,
                    s.gpu_temp,
                    s.process_count as i64,
                    s.thread_count as i64,
                    s.commit_bytes as i64,
                    s.commit_limit as i64,
                    s.page_reads_per_sec,
                ])?;
            }
        }
        tx.commit()?;
        self.buffer.clear();
        self.last_flush = Instant::now();
        Ok(())
    }

    pub fn write_event(&self, event: &StutterEvent) -> anyhow::Result<()> {
        let causes = serde_json::to_string(&event.causes)?;
        let snapshot = serde_json::to_string(&event.snapshot)?;
        let culprits = serde_json::to_string(&event.culprits)?;
        let cause_kinds = serde_json::to_string(&event.cause_kinds)?;
        let primary_cause = serde_json::to_string(&event.primary_cause)?;
        let cause_first_touch = serde_json::to_string(&event.cause_first_touch)?;
        let onset_ts = event.onset_ts.unwrap_or(0);
        self.conn.execute(
            "INSERT INTO stutter_events (timestamp, duration_ms, severity, causes, snapshot, culprits, cause_kinds, primary_cause, cause_first_touch, onset_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                event.timestamp.to_rfc3339(),
                event.duration_ms as i64,
                event.severity.to_string(),
                causes,
                snapshot,
                culprits,
                cause_kinds,
                primary_cause,
                cause_first_touch,
                onset_ts,
            ],
        )?;
        Ok(())
    }

    pub fn export_csv(&self, from: &str, to: &str, output: &str) -> anyhow::Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT timestamp, cpu_usage, cpu_per_core, cpu_freq_mhz, mem_usage_percent,
                    mem_used_mb, mem_total_mb, mem_available_mb, swap_usage_percent,
                    disk_read_bps, disk_write_bps, net_sent_bps, net_recv_bps,
                    net_sent_total, net_recv_total, gpu_usage, cpu_temp, gpu_temp,
                    process_count, thread_count, commit_bytes, commit_limit, page_reads_per_sec
             FROM samples WHERE timestamp BETWEEN ?1 AND ?2 ORDER BY timestamp",
        )?;

        let mut wtr = csv::Writer::from_path(output)?;
        wtr.write_record([
            "时间戳", "CPU 使用率(%)", "CPU 频率(MHz)", "内存使用率(%)",
            "内存已用(MB)", "内存总量(MB)", "内存可用(MB)", "交换分区使用率(%)",
            "磁盘读速率(B/s)", "磁盘写速率(B/s)", "网络发送速率(B/s)", "网络接收速率(B/s)",
            "网络累计发送(B)", "网络累计接收(B)", "GPU 使用率(%)", "CPU 温度(°C)", "GPU 温度(°C)",
            "进程数", "线程数", "提交电荷(MB)", "提交上限(MB)", "分页速率(/s)",
        ])?;

        let rows = stmt.query_map(params![from, to], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<f32>>(1)?,
                row.get::<_, Option<f32>>(3)?,
                row.get::<_, Option<f32>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<f32>>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, Option<i64>>(10)?,
                row.get::<_, Option<i64>>(11)?,
                row.get::<_, Option<i64>>(12)?,
                row.get::<_, Option<i64>>(13)?,
                row.get::<_, Option<i64>>(14)?,
                row.get::<_, Option<f32>>(15)?,
                row.get::<_, Option<f32>>(16)?,
                row.get::<_, Option<f32>>(17)?,
                row.get::<_, Option<i64>>(18)?,
                row.get::<_, Option<i64>>(19)?,
                row.get::<_, Option<i64>>(20)?,
                row.get::<_, Option<i64>>(21)?,
                row.get::<_, Option<f32>>(22)?,
            ))
        })?;

        for row in rows {
            let r = row?;
            wtr.write_record([
                r.0,
                format!("{:.1}", r.1.unwrap_or(0.0)),
                format!("{:.1}", r.2.unwrap_or(0.0)),
                format!("{:.1}", r.3.unwrap_or(0.0)),
                format!("{}", r.4.unwrap_or(0)),
                format!("{}", r.5.unwrap_or(0)),
                format!("{}", r.6.unwrap_or(0)),
                format!("{:.1}", r.7.unwrap_or(0.0)),
                format!("{}", r.8.unwrap_or(0)),
                format!("{}", r.9.unwrap_or(0)),
                format!("{}", r.10.unwrap_or(0)),
                format!("{}", r.11.unwrap_or(0)),
                format!("{}", r.12.unwrap_or(0)),
                format!("{}", r.13.unwrap_or(0)),
                format!("{:.1}", r.14.unwrap_or(0.0)),
                format!("{:.1}", r.15.unwrap_or(0.0)),
                format!("{:.1}", r.16.unwrap_or(0.0)),
                format!("{}", r.17.unwrap_or(0)),
                format!("{}", r.18.unwrap_or(0)),
                format!("{}", r.19.unwrap_or(0) / 1024 / 1024),
                format!("{}", r.20.unwrap_or(0) / 1024 / 1024),
                format!("{:.1}", r.21.unwrap_or(0.0)),
            ])?;
        }
        wtr.flush()?;
        Ok(())
    }

    pub fn cleanup(&self) -> anyhow::Result<()> {
        let cutoff = Utc::now() - ChronoDuration::days(self.config.retention_days as i64);
        let cutoff_str = cutoff.to_rfc3339();
        self.conn
            .execute("DELETE FROM samples WHERE timestamp < ?1", params![cutoff_str])?;
        self.conn.execute(
            "DELETE FROM stutter_events WHERE timestamp < ?1",
            params![cutoff_str],
        )?;
        Ok(())
    }

    pub fn event_count_today(&self) -> anyhow::Result<u32> {
        // 按用户本地时区的「今日」[当地 00:00, 当前时刻] 统计：
        // 库里 timestamp 统一存 UTC（`+00:00`），故把本地零点换算成 UTC 后再用
        // BETWEEN 比较——不能用本地日期前缀去 LIKE（会与 UTC 串前缀对不上）。
        let (start, end) = local_today_bounds();
        let count: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM stutter_events WHERE timestamp BETWEEN ?1 AND ?2",
            params![start, end],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    // --- P3: 服务写心跳、GUI 读心跳 + 最近 sample ---

    /// 服务调用：每 tick 写一次心跳（UPSERT 到 id=1 的单行）。
    /// GUI 用 [`Self::latest_heartbeat`] 探活：返回 None 即数据库从未被服务写入过。
    pub fn touch_heartbeat(&self) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO service_heartbeat (id, timestamp, pid) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET timestamp = excluded.timestamp, pid = excluded.pid",
            params![Utc::now().to_rfc3339(), std::process::id() as i64],
        )?;
        Ok(())
    }

    /// 读最新心跳时间戳（RFC3339）。从未被服务写入过返回 None。
    /// GUI 用此判断服务是否仍在运行。
    pub fn latest_heartbeat(&self) -> anyhow::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT timestamp, pid FROM service_heartbeat WHERE id = 1")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    /// 读最新一条 sample 的时间戳（GUI 健康检测的辅助信号）。
    /// 如果连 samples 表都空，说明服务从未启动过。
    pub fn latest_sample_timestamp(&self) -> anyhow::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT timestamp FROM samples ORDER BY id DESC LIMIT 1")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    /// 读最新一条 sample（GUI 启动时立刻有数据可显示）。
    /// 返回 (timestamp_rfc3339, cpu_usage, mem_usage_percent, mem_available_mb,
    ///       net_sent_bps, net_recv_bps, disk_read_bps, disk_write_bps, gpu_usage, cpu_temp)
    /// 行不存在返回 None。
    pub fn latest_sample_summary(&self) -> anyhow::Result<Option<LatestSampleSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT timestamp, cpu_usage, mem_usage_percent, mem_available_mb, \
                    net_sent_bps, net_recv_bps, disk_read_bps, disk_write_bps, \
                    gpu_usage, cpu_temp \
             FROM samples ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            Ok(Some(LatestSampleSummary {
                timestamp: row.get(0)?,
                cpu_usage: row.get::<_, Option<f32>>(1)?.unwrap_or(0.0),
                mem_usage_percent: row.get::<_, Option<f32>>(2)?.unwrap_or(0.0),
                mem_available_mb: row.get::<_, Option<i64>>(3)?.unwrap_or(0) as u64,
                net_sent_bps: row.get::<_, Option<i64>>(4)?.unwrap_or(0) as u64,
                net_recv_bps: row.get::<_, Option<i64>>(5)?.unwrap_or(0) as u64,
                disk_read_bps: row.get::<_, Option<i64>>(6)?.unwrap_or(0) as u64,
                disk_write_bps: row.get::<_, Option<i64>>(7)?.unwrap_or(0) as u64,
                gpu_usage: row.get::<_, Option<f32>>(8)?,
                cpu_temp: row.get::<_, Option<f32>>(9)?,
            }))
        } else {
            Ok(None)
        }
    }
}

/// P3：GUI 只读模式读到的最新一条 sample 的精简视图。
/// 完整 `Sample` 含 `cpu_per_core` 数组（序列化到 TEXT）反序列化成本高，
/// GUI 只需显示几个关键指标，用专用结构体省一次 JSON roundtrip。
#[derive(Debug, Clone)]
pub struct LatestSampleSummary {
    pub timestamp: String, // RFC3339
    pub cpu_usage: f32,
    /// 内存使用率（0.0 ~ 100.0）
    pub mem_usage_percent: f32,
    pub mem_available_mb: u64,
    pub net_sent_bps: u64,
    pub net_recv_bps: u64,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
    pub gpu_usage: Option<f32>,
    pub cpu_temp: Option<f32>,
}

/// 本地时区「今日」的边界，返回 `(当地 00:00 对应的 UTC 时刻, 当前 UTC 时刻)`。
///
/// 这是「今日」口径的**单一来源**：悬浮窗 `event_count_today` 与分析页
/// `load_kpi_today` / `TimeRange::Today` 都应调用它，保证「今日卡顿 N 次」一致。
/// 库里 `timestamp` 统一存 UTC（`+00:00`），故这里把本地零点换算成 UTC 后返回
/// `DateTime<Utc>`，调用方按需格式化为 RFC3339 即可用 `BETWEEN` 比较，避免直接拿
/// 本地日期前缀去 `LIKE`（会与 `+00:00` 的 UTC 串前缀不匹配，导致跨时区错位）。
pub fn local_today_bounds_utc() -> (DateTime<Utc>, DateTime<Utc>) {
    let now_local = Local::now();
    let midnight_local = now_local.date_naive().and_hms_opt(0, 0, 0).unwrap();
    let start = Local
        .from_local_datetime(&midnight_local)
        .single()
        .unwrap_or(now_local)
        .with_timezone(&Utc);
    let end = Utc::now();
    (start, end)
}

/// 本地时区「今日」的 UTC RFC3339 边界：`(当地 00:00 对应的 UTC 时刻, 当前 UTC 时刻)`。
///
/// 供「今日卡顿次数」等按用户本地日统计使用。见 [`local_today_bounds_utc`]——
/// 本函数即其返回值格式化为 RFC3339 字符串的便捷包装。
pub fn local_today_bounds() -> (String, String) {
    let (start, end) = local_today_bounds_utc();
    (start.to_rfc3339(), end.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ProcessBrief, Severity};

    fn temp_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "find_stutter_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn make_sample() -> Sample {
        let mut s = Sample::default();
        s.cpu_usage = 45.5;
        s.cpu_per_core = vec![40.0, 50.0];
        s.mem_usage_percent = 60.0;
        s.mem_used_mb = 4096;
        s.mem_total_mb = 8192;
        s.mem_available_mb = 4096;
        s.swap_usage_percent = 10.0;
        s.disk_read_bps = 1024;
        s.disk_write_bps = 2048;
        s.net_sent_bps = 512;
        s.net_recv_bps = 1024;
        s.net_sent_total = 100000;
        s.net_recv_total = 200000;
        s.gpu_usage = Some(30.0);
        s.cpu_temp = Some(55.0);
        s.gpu_temp = Some(60.0);
        s.process_count = 150;
        s.thread_count = 1200;
        s
    }

    fn make_event() -> StutterEvent {
        StutterEvent {
            timestamp: Utc::now(),
            duration_ms: 5000,
            severity: Severity::Major,
            causes: vec!["CPU usage 95.0% > 90.0%".to_string()],
            snapshot: make_sample(),
            culprits: vec![],
            ..Default::default()
        }
    }

    // --- Logger::new ---

    #[test]
    fn logger_new_creates_database() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_new.db").to_str().unwrap().to_string();
        let config = StorageConfig {
            db_path: db_path.clone(),
            retention_days: 30,
        };

        let result = Logger::new(&config);
        assert!(result.is_ok());

        // Database file should exist
        assert!(std::path::Path::new(&db_path).exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- 时间戳索引（M6 / PRD §3.3）---

    #[test]
    fn logger_new_creates_timestamp_indexes() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir
            .join("test_indexes.db")
            .to_str()
            .unwrap()
            .to_string();
        let config = StorageConfig {
            db_path: db_path.clone(),
            retention_days: 30,
        };

        // Logger::new 建表 + 迁移 + 建索引后，sqlite_master 里应存在两个时间戳索引
        let logger = Logger::new(&config).unwrap();

        let idx_samples: u32 = logger
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='idx_samples_ts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let idx_events: u32 = logger
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='idx_events_ts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(idx_samples, 1, "samples 时间戳索引应存在");
        assert_eq!(idx_events, 1, "stutter_events 时间戳索引应存在");

        // 幂等：再次打开同一库（已含索引）不应报错
        let logger2 = Logger::new(&config).unwrap();
        let idx_events2: u32 = logger2
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='idx_events_ts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(idx_events2, 1, "重启后索引应存在且唯一");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn logger_new_creates_tables() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_tables.db").to_str().unwrap().to_string();
        let config = StorageConfig {
            db_path: db_path.clone(),
            retention_days: 30,
        };

        let logger = Logger::new(&config).unwrap();

        // Verify tables exist by querying them
        let sample_count: u32 = logger
            .conn
            .query_row("SELECT COUNT(*) FROM samples", [], |row| row.get(0))
            .unwrap();
        let event_count: u32 = logger
            .conn
            .query_row("SELECT COUNT(*) FROM stutter_events", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(sample_count, 0);
        assert_eq!(event_count, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- write_sample + flush ---

    #[test]
    fn logger_write_sample_and_flush() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_flush.db").to_str().unwrap().to_string();
        let config = StorageConfig {
            db_path,
            retention_days: 30,
        };

        let mut logger = Logger::new(&config).unwrap();
        let sample = make_sample();

        logger.write_sample(&sample).unwrap();
        logger.flush().unwrap();

        // Verify data was written
        let count: u32 = logger
            .conn
            .query_row("SELECT COUNT(*) FROM samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn logger_write_multiple_samples() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_multi.db").to_str().unwrap().to_string();
        let config = StorageConfig {
            db_path,
            retention_days: 30,
        };

        let mut logger = Logger::new(&config).unwrap();
        let sample = make_sample();

        for _ in 0..5 {
            logger.write_sample(&sample).unwrap();
        }
        logger.flush().unwrap();

        let count: u32 = logger
            .conn
            .query_row("SELECT COUNT(*) FROM samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 5);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn logger_flush_empty_buffer_is_noop() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_empty_flush.db").to_str().unwrap().to_string();
        let config = StorageConfig {
            db_path,
            retention_days: 30,
        };

        let mut logger = Logger::new(&config).unwrap();
        // Flush with no samples — should succeed without error
        logger.flush().unwrap();

        let count: u32 = logger
            .conn
            .query_row("SELECT COUNT(*) FROM samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn logger_write_sample_data_integrity() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_integrity.db").to_str().unwrap().to_string();
        let config = StorageConfig {
            db_path,
            retention_days: 30,
        };

        let mut logger = Logger::new(&config).unwrap();
        let sample = make_sample();
        logger.write_sample(&sample).unwrap();
        logger.flush().unwrap();

        // Read back and verify cpu_usage
        let cpu: f32 = logger
            .conn
            .query_row("SELECT cpu_usage FROM samples LIMIT 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(cpu, 45.5);

        let mem_avail: i64 = logger
            .conn
            .query_row(
                "SELECT mem_available_mb FROM samples LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mem_avail, 4096);

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- write_event ---

    #[test]
    fn logger_write_event() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_event.db").to_str().unwrap().to_string();
        let config = StorageConfig {
            db_path,
            retention_days: 30,
        };

        let logger = Logger::new(&config).unwrap();
        let event = make_event();
        logger.write_event(&event).unwrap();

        let count: u32 = logger
            .conn
            .query_row(
                "SELECT COUNT(*) FROM stutter_events",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn logger_write_multiple_events() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_multi_event.db").to_str().unwrap().to_string();
        let config = StorageConfig {
            db_path,
            retention_days: 30,
        };

        let logger = Logger::new(&config).unwrap();
        for _ in 0..3 {
            let event = make_event();
            logger.write_event(&event).unwrap();
        }

        let count: u32 = logger
            .conn
            .query_row(
                "SELECT COUNT(*) FROM stutter_events",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn logger_write_event_stores_culprits() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir
            .join("test_culprits.db")
            .to_str()
            .unwrap()
            .to_string();
        let config = StorageConfig {
            db_path,
            retention_days: 30,
        };
        let logger = Logger::new(&config).unwrap();

        let mut ev = make_event();
        ev.culprits = vec![ProcessBrief {
            pid: 42,
            name: "x.exe".into(),
            cpu_usage: 50.0,
            mem_used_mb: 100,
        }];
        logger.write_event(&ev).unwrap();

        let culprits_json: String = logger
            .conn
            .query_row(
                "SELECT culprits FROM stutter_events ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            culprits_json.contains("x.exe"),
            "culprits 应写入 culprits 列: {}",
            culprits_json
        );
        assert!(culprits_json.contains("42"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- event_count_today ---

    #[test]
    fn logger_event_count_today_zero() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_count_zero.db").to_str().unwrap().to_string();
        let config = StorageConfig {
            db_path,
            retention_days: 30,
        };

        let logger = Logger::new(&config).unwrap();
        let count = logger.event_count_today().unwrap();
        assert_eq!(count, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn logger_event_count_today_matches_written() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_count_match.db").to_str().unwrap().to_string();
        let config = StorageConfig {
            db_path,
            retention_days: 30,
        };

        let logger = Logger::new(&config).unwrap();
        for _ in 0..3 {
            logger.write_event(&make_event()).unwrap();
        }

        let count = logger.event_count_today().unwrap();
        assert_eq!(count, 3);

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- export_csv ---

    #[test]
    fn logger_export_csv_success() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_export_fail.db").to_str().unwrap().to_string();
        let csv_path = dir.join("export.csv").to_str().unwrap().to_string();
        let config = StorageConfig {
            db_path,
            retention_days: 30,
        };

        let mut logger = Logger::new(&config).unwrap();
        logger.write_sample(&make_sample()).unwrap();
        logger.flush().unwrap();

        let from = "2000-01-01T00:00:00+00:00";
        let to = "2100-01-01T00:00:00+00:00";
        let result = logger.export_csv(from, to, &csv_path);
        assert!(result.is_ok(), "CSV export should succeed now that Vec<f32> is handled");

        let content = std::fs::read_to_string(&csv_path).unwrap();
        assert!(content.contains("CPU 使用率")); // 表头已中文化（AGENTS.md：导出表头用中文）
        assert!(content.contains("45.5"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn logger_export_csv_empty_range_no_data() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_export_empty.db").to_str().unwrap().to_string();
        let csv_path = dir.join("export_empty.csv").to_str().unwrap().to_string();
        let config = StorageConfig {
            db_path,
            retention_days: 30,
        };

        let mut logger = Logger::new(&config).unwrap();
        logger.write_sample(&make_sample()).unwrap();
        logger.flush().unwrap();

        // Date range that excludes our sample — no rows returned,
        // so csv::serialize is never called, and the export should succeed
        let from = "2000-01-01T00:00:00+00:00";
        let to = "2000-01-02T00:00:00+00:00";
        let result = logger.export_csv(from, to, &csv_path);
        assert!(result.is_ok());

        // File may or may not exist when no rows match; either is acceptable
        if std::path::Path::new(&csv_path).exists() {
            let content = std::fs::read_to_string(&csv_path).unwrap();
            let lines: Vec<&str> = content.lines().collect();
            assert!(lines.len() <= 1);
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- cleanup ---

    #[test]
    fn logger_cleanup_removes_old_data() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_cleanup.db").to_str().unwrap().to_string();
        let config = StorageConfig {
            db_path,
            retention_days: 0, // 0 days = everything is old
        };

        let mut logger = Logger::new(&config).unwrap();
        logger.write_sample(&make_sample()).unwrap();
        logger.flush().unwrap();

        // Verify data exists before cleanup
        let count_before: u32 = logger
            .conn
            .query_row("SELECT COUNT(*) FROM samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_before, 1);

        logger.cleanup().unwrap();

        let count_after: u32 = logger
            .conn
            .query_row("SELECT COUNT(*) FROM samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_after, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn logger_cleanup_keeps_recent_data() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_cleanup_recent.db").to_str().unwrap().to_string();
        let config = StorageConfig {
            db_path,
            retention_days: 30, // 30 days = recent data stays
        };

        let mut logger = Logger::new(&config).unwrap();
        logger.write_sample(&make_sample()).unwrap();
        logger.flush().unwrap();

        logger.cleanup().unwrap();

        let count: u32 = logger
            .conn
            .query_row("SELECT COUNT(*) FROM samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn logger_cleanup_removes_old_events() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir
            .join("test_cleanup_events.db")
            .to_str()
            .unwrap()
            .to_string();
        let config = StorageConfig {
            db_path,
            retention_days: 0,
        };

        let logger = Logger::new(&config).unwrap();
        logger.write_event(&make_event()).unwrap();

        let before: u32 = logger
            .conn
            .query_row(
                "SELECT COUNT(*) FROM stutter_events",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(before, 1);

        logger.cleanup().unwrap();

        let after: u32 = logger
            .conn
            .query_row(
                "SELECT COUNT(*) FROM stutter_events",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(after, 0);

        std::fs::remove_dir_all(&dir).ok();
    }
}
