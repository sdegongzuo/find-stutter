use crate::types::{Sample, StutterEvent, StorageConfig};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
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
                thread_count INTEGER
            );
            CREATE TABLE IF NOT EXISTS stutter_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                severity TEXT NOT NULL,
                causes TEXT NOT NULL,
                snapshot TEXT NOT NULL
            );",
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
                    gpu_usage, cpu_temp, gpu_temp, process_count, thread_count
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
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
                    s.mem_used_mb,
                    s.mem_total_mb,
                    s.mem_available_mb,
                    s.swap_usage_percent,
                    s.disk_read_bps,
                    s.disk_write_bps,
                    s.net_sent_bps,
                    s.net_recv_bps,
                    s.net_sent_total,
                    s.net_recv_total,
                    s.gpu_usage,
                    s.cpu_temp,
                    s.gpu_temp,
                    s.process_count as u64,
                    s.thread_count as u64,
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
        self.conn.execute(
            "INSERT INTO stutter_events (timestamp, duration_ms, severity, causes, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event.timestamp.to_rfc3339(),
                event.duration_ms,
                event.severity.to_string(),
                causes,
                snapshot,
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
                    process_count, thread_count
             FROM samples WHERE timestamp BETWEEN ?1 AND ?2 ORDER BY timestamp",
        )?;

        let mut wtr = csv::Writer::from_path(output)?;
        wtr.write_record([
            "timestamp", "cpu_usage", "cpu_freq_mhz", "mem_usage_percent",
            "mem_used_mb", "mem_total_mb", "mem_available_mb", "swap_usage_percent",
            "disk_read_bps", "disk_write_bps", "net_sent_bps", "net_recv_bps",
            "net_sent_total", "net_recv_total", "gpu_usage", "cpu_temp", "gpu_temp",
            "process_count", "thread_count",
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
        let today = Utc::now().format("%Y-%m-%d").to_string();
        let count: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM stutter_events WHERE timestamp LIKE ?1",
            params![format!("{}%", today)],
            |row| row.get(0),
        )?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Severity;

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

        let mem_avail: u64 = logger
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
        assert!(content.contains("cpu_usage"));
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
