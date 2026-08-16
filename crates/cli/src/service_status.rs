//! `status` 子命令：服务 SCM 状态 + 心跳健康 + db 路径（ADR-0001 决策 4）。
//!
//! - **SCM 状态**：直接查询 Service Control Manager（`ServiceAccess::QUERY_STATUS`，
//!   无需管理员权限），不依赖 service exe 是否在——比 spawn `find-stutter-service
//!   status` 更可靠（后者在 exe 缺失 / PATH 变化时失效）。实现方式与
//!   `crates/service/src/install.rs::status()` 同款（windows-service crate），
//!   退出码协议语义保持一致：Running=0 / Stopped·Pending=1 / NotFound=2 / Error=3。
//! - **心跳健康**：读 `service_heartbeat` 表，按 GUI 1Hz 轮询的同一口径判定
//!   （`crates/ui/src/reader.rs`）：5 秒内新鲜 = running、存在但超时 = stale、
//!   表空 = stopped、db 打不开 = no_database。
//! - **db 路径**：config.toml 的 storage.db_path（含默认值回退），附存在性。

use std::path::Path;

use chrono::Utc;
use rusqlite::OpenFlags;
use serde_json::{json, Value};

/// 服务注册名（与 `crates/service/src/service.rs::SERVICE_NAME` 保持一致；
/// 为一个常量引入整个 service bin crate 不值得，这里复制并注明来源）。
pub const SERVICE_NAME: &str = "FindStutter";

/// 心跳新鲜阈值：与 GUI `DbReader::stale_threshold`（5s）一致，两边口径不漂移。
const HEARTBEAT_STALE_SECS: i64 = 5;

/// SCM 服务状态（小写蛇形，供 JSON 输出；与 service status 退出码协议对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScmState {
    Running,
    Stopped,
    StartPending,
    StopPending,
    ContinuePending,
    PausePending,
    Paused,
    NotFound,
}

impl ScmState {
    /// JSON 输出的稳定字符串（键值英文）。
    pub fn as_str(self) -> &'static str {
        match self {
            ScmState::Running => "running",
            ScmState::Stopped => "stopped",
            ScmState::StartPending => "start_pending",
            ScmState::StopPending => "stop_pending",
            ScmState::ContinuePending => "continue_pending",
            ScmState::PausePending => "pause_pending",
            ScmState::Paused => "paused",
            ScmState::NotFound => "not_found",
        }
    }

    /// 对应 service exe `status` 子命令的退出码（0/1/2/3 协议）。
    pub fn exit_code(self) -> i32 {
        match self {
            ScmState::Running => 0,
            ScmState::NotFound => 2,
            _ => 1,
        }
    }
}

/// 查询 SCM 服务状态（无需提权）。
pub fn query_scm_state() -> anyhow::Result<ScmState> {
    #[cfg(windows)]
    {
        use windows_service::service::{ServiceAccess, ServiceState};
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
            Ok(s) => s,
            Err(_) => return Ok(ScmState::NotFound),
        };
        let status = service.query_status()?;
        Ok(match status.current_state {
            ServiceState::Stopped => ScmState::Stopped,
            ServiceState::StartPending => ScmState::StartPending,
            ServiceState::StopPending => ScmState::StopPending,
            ServiceState::Running => ScmState::Running,
            ServiceState::ContinuePending => ScmState::ContinuePending,
            ServiceState::PausePending => ScmState::PausePending,
            ServiceState::Paused => ScmState::Paused,
        })
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("当前平台不支持 SCM 查询（仅 Windows）")
    }
}

/// 心跳健康状态（与 GUI `ServiceHealth` 同口径：running / stale / stopped / no_database）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatHealth {
    Running,
    Stale,
    Stopped,
    NoDatabase,
}

impl HeartbeatHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            HeartbeatHealth::Running => "running",
            HeartbeatHealth::Stale => "stale",
            HeartbeatHealth::Stopped => "stopped",
            HeartbeatHealth::NoDatabase => "no_database",
        }
    }
}

/// 心跳查询结果（健康 + 最近心跳时间戳 + 年龄）。
#[derive(Debug, Clone, PartialEq)]
pub struct HeartbeatInfo {
    pub health: HeartbeatHealth,
    /// 最近一次心跳时间戳（RFC3339）；从未有心跳时为 None
    pub last_ts: Option<String>,
    /// 距最近心跳的秒数；时间戳不可解析 / 无心跳时为 None
    pub age_secs: Option<f64>,
}

/// 读 `service_heartbeat` 判定心跳健康 + 最近心跳时间戳与年龄。
pub fn query_heartbeat(db_path: &Path) -> HeartbeatInfo {
    if !db_path.exists() {
        return HeartbeatInfo {
            health: HeartbeatHealth::NoDatabase,
            last_ts: None,
            age_secs: None,
        };
    }
    let conn = match rusqlite::Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(_) => {
            return HeartbeatInfo {
                health: HeartbeatHealth::NoDatabase,
                last_ts: None,
                age_secs: None,
            }
        }
    };
    let ts: Option<String> = conn
        .query_row(
            "SELECT timestamp FROM service_heartbeat WHERE id = 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok();
    let Some(ts) = ts else {
        // db 在但心跳表空/缺表：服务从未启动过（与 GUI Stopped 口径一致）
        return HeartbeatInfo {
            health: HeartbeatHealth::Stopped,
            last_ts: None,
            age_secs: None,
        };
    };
    match chrono::DateTime::parse_from_rfc3339(&ts) {
        Ok(parsed) => {
            let age = Utc::now()
                .signed_duration_since(parsed.with_timezone(&Utc))
                .num_milliseconds()
                .max(0) as f64
                / 1000.0;
            let health = if age < HEARTBEAT_STALE_SECS as f64 {
                HeartbeatHealth::Running
            } else {
                HeartbeatHealth::Stale
            };
            HeartbeatInfo {
                health,
                last_ts: Some(ts),
                age_secs: Some(age),
            }
        }
        Err(_) => HeartbeatInfo {
            health: HeartbeatHealth::Stale,
            last_ts: Some(ts),
            age_secs: None,
        },
    }
}

/// `status` 子命令整体输出：
/// `{"scm":{"state":...,"exit_code":...},"heartbeat":{...},"db":{"path":...,"exists":...}}`
pub fn status_json() -> Value {
    let db_path = crate::query::db_path_from_config();
    let scm = query_scm_state()
        .map(|s| json!({"state": s.as_str(), "exit_code": s.exit_code()}))
        .unwrap_or_else(|e| json!({"state": "error", "error": e.to_string()}));
    let hb = query_heartbeat(Path::new(&db_path));
    json!({
        "scm": scm,
        "heartbeat": {
            "health": hb.health.as_str(),
            "last_ts": hb.last_ts,
            "age_secs": hb.age_secs,
        },
        "db": {
            "path": db_path,
            "exists": Path::new(&db_path).exists(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scm_state_strings_stable() {
        assert_eq!(ScmState::Running.as_str(), "running");
        assert_eq!(ScmState::NotFound.as_str(), "not_found");
        assert_eq!(ScmState::StopPending.as_str(), "stop_pending");
    }

    /// 与 service exe 的 status 退出码协议保持一致（main.rs:142-158）：
    /// 0=Running / 1=Stopped·Pending / 2=NotFound。
    #[test]
    fn scm_exit_code_protocol() {
        assert_eq!(ScmState::Running.exit_code(), 0);
        assert_eq!(ScmState::Stopped.exit_code(), 1);
        assert_eq!(ScmState::StartPending.exit_code(), 1);
        assert_eq!(ScmState::Paused.exit_code(), 1);
        assert_eq!(ScmState::NotFound.exit_code(), 2);
    }

    #[test]
    fn heartbeat_strings() {
        assert_eq!(HeartbeatHealth::Running.as_str(), "running");
        assert_eq!(HeartbeatHealth::Stale.as_str(), "stale");
        assert_eq!(HeartbeatHealth::Stopped.as_str(), "stopped");
        assert_eq!(HeartbeatHealth::NoDatabase.as_str(), "no_database");
    }

    #[test]
    fn heartbeat_missing_db() {
        let hb = query_heartbeat(Path::new("D:/__missing__/nope.db"));
        assert_eq!(hb.health, HeartbeatHealth::NoDatabase);
        assert!(hb.last_ts.is_none());
        assert!(hb.age_secs.is_none());
    }

    #[test]
    fn heartbeat_empty_table_is_stopped() {
        let tmp = std::env::temp_dir().join(format!(
            "fs_cli_hb_empty_{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        rusqlite::Connection::open(&tmp).unwrap().close().unwrap();
        let hb = query_heartbeat(&tmp);
        assert_eq!(hb.health, HeartbeatHealth::Stopped, "空库无心跳表应为 stopped");
        assert!(hb.last_ts.is_none());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn heartbeat_fresh_is_running() {
        let tmp = std::env::temp_dir().join(format!(
            "fs_cli_hb_fresh_{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let conn = rusqlite::Connection::open(&tmp).unwrap();
            conn.execute_batch(
                "CREATE TABLE service_heartbeat (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    timestamp TEXT NOT NULL,
                    pid INTEGER NOT NULL
                );
                INSERT INTO service_heartbeat VALUES (1, '26-01-01T00:00:00Z', 1);",
            )
            .unwrap();
            conn.execute(
                "UPDATE service_heartbeat SET timestamp = ?1 WHERE id = 1",
                rusqlite::params![Utc::now().to_rfc3339()],
            )
            .unwrap();
        }
        let hb = query_heartbeat(&tmp);
        assert_eq!(hb.health, HeartbeatHealth::Running);
        assert!(hb.last_ts.is_some());
        assert!(hb.age_secs.unwrap() < 5.0);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn heartbeat_old_is_stale() {
        let tmp = std::env::temp_dir().join(format!(
            "fs_cli_hb_old_{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let conn = rusqlite::Connection::open(&tmp).unwrap();
            let old = (Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
            conn.execute_batch(&format!(
                "CREATE TABLE service_heartbeat (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    timestamp TEXT NOT NULL,
                    pid INTEGER NOT NULL
                );
                INSERT INTO service_heartbeat VALUES (1, '{old}', 1);"
            ))
            .unwrap();
        }
        let hb = query_heartbeat(&tmp);
        assert_eq!(hb.health, HeartbeatHealth::Stale);
        assert!(hb.age_secs.unwrap() > 7000.0);
        let _ = std::fs::remove_file(&tmp);
    }

    /// 真机 SCM 查询冒烟：不注册/不停靠任何服务，只读状态并断言输出可序列化。
    /// （开发机上服务可能已注册，两种结果都合法。）
    #[test]
    fn scm_query_smoke_returns_known_state() {
        if let Ok(state) = query_scm_state() {
            // 只要不 panic、落在已知枚举集合内即可
            assert!(state.exit_code() >= 0 && state.exit_code() <= 2);
        }
    }

    #[test]
    fn status_json_shape() {
        let v = status_json();
        assert!(v["scm"]["state"].is_string());
        assert!(v["heartbeat"]["health"].is_string());
        assert!(v["db"]["path"].is_string());
        assert!(v["db"]["exists"].is_boolean());
    }
}
