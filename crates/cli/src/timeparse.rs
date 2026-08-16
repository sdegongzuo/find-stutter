//! `--from` / `--to` 时间参数解析（CLI 契约的一部分）。
//!
//! agent / 人类输入的时间统一按以下优先级解析，**无时区后缀的按本地时区**解释，
//! 再换算为 UTC（库里 `timestamp` 存 UTC RFC3339，`BETWEEN` 比较需 UTC 边界）：
//!
//! 1. RFC3339（`2026-08-16T09:30:00Z` / `2026-08-16T09:30:00+08:00`）
//! 2. `YYYY-MM-DDTHH:MM:SS` 或 `YYYY-MM-DD HH:MM:SS`（本地时区）
//! 3. `YYYY-MM-DDTHH:MM` 或 `YYYY-MM-DD HH:MM`（本地时区，秒=0）
//! 4. `YYYY-MM-DD`（本地时区当天零点）
//!
//! 解析失败返回 `None`，由调用方（clap value_parser）转成报错信息。

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, Utc};

/// 解析时间参数；失败时返回 `None`，由调用方（query::resolve_range）
/// 组装含原始输入与支持格式的中文报错。
pub fn parse_time_arg(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // 1) RFC3339（自带时区后缀，直接解析为 UTC）
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // 2)~4) 无时区后缀：按本地时区解释
    // 2) `YYYY-MM-DD HH:MM:SS` / `YYYY-MM-DDTHH:MM:SS`
    let normalized = s.replace('T', " ");
    if let Ok(ndt) = NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%d %H:%M:%S") {
        return Some(ndt.and_local_timezone(Local).single()?.with_timezone(&Utc));
    }
    // 3) `YYYY-MM-DD HH:MM`
    if let Ok(ndt) = NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%d %H:%M") {
        return Some(ndt.and_local_timezone(Local).single()?.with_timezone(&Utc));
    }
    // 4) `YYYY-MM-DD`（当天零点）
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let ndt = d.and_hms_opt(0, 0, 0)?;
        return Some(ndt.and_local_timezone(Local).single()?.with_timezone(&Utc));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    #[test]
    fn rfc3339_with_z_suffix() {
        let dt = parse_time_arg("2026-08-16T09:30:00Z").unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-08-16T09:30:00+00:00");
    }

    #[test]
    fn rfc3339_with_offset() {
        // +08:00 偏移 → UTC 减 8 小时
        let dt = parse_time_arg("2026-08-16T09:30:00+08:00").unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-08-16T01:30:00+00:00");
    }

    #[test]
    fn date_only_is_local_midnight() {
        let dt = parse_time_arg("2026-08-16").unwrap();
        let local = dt.with_timezone(&Local);
        assert_eq!(
            (local.year(), local.month(), local.day()),
            (2026, 8, 16)
        );
        assert_eq!((local.hour(), local.minute(), local.second()), (0, 0, 0));
    }

    #[test]
    fn datetime_space_and_t_forms() {
        let a = parse_time_arg("2026-08-16 09:30:00").unwrap();
        let b = parse_time_arg("2026-08-16T09:30:00").unwrap();
        assert_eq!(a, b);
        // 本地时区解释：换回本地后时分一致
        let local = a.with_timezone(&Local);
        assert_eq!((local.hour(), local.minute()), (9, 30));
    }

    #[test]
    fn minute_precision_form() {
        let dt = parse_time_arg("2026-08-16 09:30").unwrap();
        let local = dt.with_timezone(&Local);
        assert_eq!((local.hour(), local.minute(), local.second()), (9, 30, 0));
    }

    #[test]
    fn garbage_returns_none() {
        assert!(parse_time_arg("").is_none());
        assert!(parse_time_arg("   ").is_none());
        assert!(parse_time_arg("yesterday").is_none());
        assert!(parse_time_arg("2026/08/16").is_none());
        assert!(parse_time_arg("16-08-2026").is_none());
    }
}
