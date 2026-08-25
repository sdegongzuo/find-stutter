//! `maintenance` 子命令：立即完成存量冷数据降采样并按需收缩数据库文件。
//!
//! 背景：SQLite 的 DELETE 不缩小文件——降采样释放的页进入空闲链供复用，
//! 文件高水位不变；真正回收磁盘需要 [`VACUUM`](find_stutter_core::Logger::vacuum)
//! 重写整库。WAL 模式下 VACUUM 需要独占：服务并发写库时会 busy 失败，
//! 因此输出中附带停服指引（status 子命令同款非提权查询）。

use anyhow::Context;
use find_stutter_core::{Config, Logger};
use rusqlite::OpenFlags;

/// 只读统计快照
struct DbStats {
    file_mb: f64,
    page_count: i64,
    freelist_pages: i64,
    minute_rows: i64,
}

fn collect_stats(db_path: &str) -> anyhow::Result<DbStats> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let freelist_pages: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let minute_rows: i64 = conn.query_row(
        "SELECT COUNT(*) FROM samples WHERE length(timestamp) = 19",
        [],
        |r| r.get(0),
    )?;
    let file_mb = std::fs::metadata(db_path)
        .map(|m| m.len() as f64 / (1024.0 * 1024.0))
        .unwrap_or(0.0);
    Ok(DbStats { file_mb, page_count, freelist_pages, minute_rows })
}

fn json(v: serde_json::Value) {
    println!("{}", serde_json::to_string(&v).unwrap_or_default());
}

fn mb(pages: i64, page_size: i64) -> f64 {
    (pages.max(0) * page_size) as f64 / (1024.0 * 1024.0)
}

/// 入口：`find-stutter.exe maintenance [--skip-vacuum]`。
/// 步骤：前置统计 → 全量分批降采样 → 空闲页占比达标则 VACUUM → 收尾统计。
pub fn run(skip_vacuum: bool) -> anyhow::Result<()> {
    let config = Config::load("config.toml").unwrap_or_default();
    let hot_days = config.storage.hot_retention_days as i64;
    let db_path = config.storage.db_path.clone();

    // 服务在跑则提示：降采样可并发，VACUUM 需要独占
    let svc = crate::service_status::status_json();
    let svc_running = svc["scm"]["state"].as_str() == Some("running");

    let before = collect_stats(&db_path)
        .with_context(|| format!("打开数据库失败: {}", db_path))?;
    json(serde_json::json!({
        "phase": "start",
        "db": db_path,
        "file_mb": before.file_mb,
        "minute_rows_before": before.minute_rows,
        "service_running": svc_running,
        "hint": if svc_running { "服务运行中：降采样可与写库并发；VACUUM 需独占，失败请以管理员执行 find-stutter-service stop 后重跑" } else { "" },
    }));

    let logger = Logger::new(&config.storage)?;
    let inserted = if hot_days > 0 {
        let n = logger.downsample_cold_samples_all(hot_days)?;
        json(serde_json::json!({"phase": "downsample", "hot_days": hot_days, "inserted_minute_rows": n}));
        n
    } else {
        json(serde_json::json!({"phase": "downsample", "status": "skipped", "reason": "hot_retention_days=0"}));
        0
    };

    let mid = collect_stats(&db_path)?;
    let page_size: i64 = 4096; // SQLite 默认页大小；占比计算只关心比值，绝对值仅用于展示
    let free_ratio = mid.freelist_pages as f64 / mid.page_count.max(1) as f64;

    if skip_vacuum {
        json(serde_json::json!({"phase": "vacuum", "status": "skipped", "reason": "--skip-vacuum"}));
    } else if mid.freelist_pages == 0 || free_ratio < 0.05 {
        json(serde_json::json!({"phase": "vacuum", "status": "skipped", "reason": "空闲页不足 5%，无可回收空间", "free_ratio": free_ratio}));
    } else {
        json(serde_json::json!({"phase": "vacuum", "status": "started", "reclaimable_mb": mb(mid.freelist_pages, page_size), "free_ratio": free_ratio}));
        match logger.vacuum() {
            Ok(()) => json(serde_json::json!({"phase": "vacuum", "status": "ok"})),
            Err(e) => {
                // busy 非致命：降采样已持久化，收缩可稍后重跑本命令
                json(serde_json::json!({
                    "phase": "vacuum", "status": "failed", "error": e.to_string(),
                    "hint": "服务并发写库导致独占失败：以管理员执行 find-stutter-service stop 后重跑本命令",
                }));
            }
        }
    }

    let after = collect_stats(&db_path)?;
    json(serde_json::json!({
        "phase": "done",
        "file_mb_before": before.file_mb,
        "file_mb_after": after.file_mb,
        "minute_rows_after": after.minute_rows,
        "inserted_minute_rows": inserted,
    }));
    Ok(())
}