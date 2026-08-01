//! 卡顿通知弹窗（P2）。
//!
//! 检测到 `Major` / `Critical` 卡顿事件时，弹出 Windows 原生通知
//! （任务栏右侧的气泡 toast，由 `Shell_NotifyIconW` + `NIF_INFO` 实现）。
//!
//! ## 为什么不用 WinRT toast
//!
//! WinRT `ToastNotificationManager` 需要 AUMID + shortcut 注册，
//! 在开发环境（无 installer）下不可用；`Shell_NotifyIconW` 的
//! `NIF_INFO` 气泡是同样「原生」的系统通知，零配置即可用，
//! Windows 10/11 上表现为右下角通知。
//!
//! ## 触发条件（纯逻辑，可单测）
//!
//! [`should_notify`]：新事件（事件时间戳比上次已通知的更新）
//! 且严重程度 >= 配置的最低等级，且 `stutter_alert` 开关打开。

use chrono::{DateTime, Utc};
use find_stutter_core::{NotificationConfig, Severity, StutterEvent};

/// 判断某条事件是否值得弹通知。
///
/// - `last_notified_at`：上一次已通知的事件时间戳（None = 从未通知过，
///   此时第一条**达标**事件也弹，用于启动后立刻有历史事件的场景）
/// - `event`：刚轮询到的最新事件
/// - `cfg`：`[notifications]` 配置
///
/// 返回 `true` 表示应该弹 toast。
pub fn should_notify(
    last_notified_at: Option<DateTime<Utc>>,
    event: &StutterEvent,
    cfg: &NotificationConfig,
) -> bool {
    if !cfg.stutter_alert {
        return false;
    }
    if !severity_meets_min(event.severity, &cfg.min_severity) {
        return false;
    }
    // 新事件判定：事件时间戳严格晚于上次已通知的时间戳。
    // 相同时间戳（同一条事件重复轮询到）不算新事件，避免刷屏。
    match last_notified_at {
        Some(last) => event.timestamp > last,
        None => true,
    }
}

/// 严重程度是否达到 `min_severity` 门槛。
///
/// 解析失败（未知字符串）按「最严格」处理：只放行 Critical，
/// 避免配置写错导致刷屏。
pub fn severity_meets_min(sev: Severity, min_severity: &str) -> bool {
    let rank = |s: Severity| match s {
        Severity::Minor => 1,
        Severity::Major => 2,
        Severity::Critical => 3,
    };
    let min_rank = match min_severity.trim().to_ascii_lowercase().as_str() {
        "minor" => 1,
        "major" => 2,
        "critical" => 3,
        _ => 3, // 未知 → 最严格
    };
    rank(sev) >= min_rank
}

/// 弹出系统通知。
///
/// - Windows：`Shell_NotifyIconW` NIF_INFO 气泡（任务栏通知）。
/// - 其他平台：仅写日志（不阻塞）。
///
/// `severity` 决定气泡图标（Major=警告黄 / Critical=错误红）。
pub fn show_stutter_notification(event: &StutterEvent) {
    let title = match event.severity {
        Severity::Major => "find-stutter: 检测到卡顿 (Major)",
        Severity::Critical => "find-stutter: 检测到严重卡顿 (Critical)",
        Severity::Minor => "find-stutter: 检测到卡顿 (Minor)",
    };
    let body = format!(
        "持续 {}ms\n{}",
        event.duration_ms,
        event.causes.join("; ")
    );

    #[cfg(windows)]
    show_balloon(title, &body, event.severity);

    #[cfg(not(windows))]
    log::info!("[notify] {} {}", title, body);
}

/// Windows 气泡通知实现（message-only 隐藏窗口 + NIF_INFO）。
#[cfg(windows)]
fn show_balloon(title: &str, body: &str, severity: Severity) {
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIIF_ERROR, NIIF_WARNING,
        NOTIFYICONDATAW, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, LoadIconW, HWND_MESSAGE, IDI_APPLICATION,
        WINDOW_EX_STYLE, WS_OVERLAPPED,
    };

    unsafe {
        // message-only 隐藏窗口（HWND_MESSAGE 父窗口），用于承载通知图标
        let class_name = windows::core::PCWSTR("STATIC".encode_utf16().collect::<Vec<_>>().as_ptr());
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            windows::core::PCWSTR::null(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                log::warn!("notify: CreateWindowExW failed: {}", e);
                return;
            }
        };

        let icon = LoadIconW(None, IDI_APPLICATION).unwrap_or_default();
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_INFO | NIF_MESSAGE;
        nid.uCallbackMessage = 0x8000; // WM_APP
        nid.hIcon = icon;

        write_wide(&mut nid.szInfo, body);
        write_wide(&mut nid.szInfoTitle, title);
        nid.dwInfoFlags = match severity {
            Severity::Critical => NIIF_ERROR,
            Severity::Major => NIIF_WARNING,
            Severity::Minor => NIIF_WARNING,
        };

        // 先 ADD 再 MODIFY 才能触发气泡显示
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        // 短暂延迟后删除图标，避免残留
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        let _ = DestroyWindow(hwnd);
    }
}

/// 把字符串写入定长 wide 数组（截断 + NUL 结尾）。
#[cfg(windows)]
fn write_wide(dst: &mut [u16], s: &str) {
    let mut it = s.encode_utf16();
    for slot in dst.iter_mut() {
        *slot = it.next().unwrap_or(0);
        if *slot == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use find_stutter_core::Sample;

    fn make_event(sev: Severity, ts: DateTime<Utc>) -> StutterEvent {
        StutterEvent {
            timestamp: ts,
            duration_ms: 5000,
            severity: sev,
            causes: vec!["CPU usage 95.0% > 90.0%".into()],
            snapshot: Sample::default(),
        }
    }

    #[test]
    fn alert_disabled_never_notifies() {
        let cfg = NotificationConfig {
            stutter_alert: false,
            min_severity: "minor".into(),
        };
        let ev = make_event(Severity::Critical, Utc::now());
        assert!(!should_notify(None, &ev, &cfg));
    }

    #[test]
    fn below_min_severity_not_notified() {
        let cfg = NotificationConfig {
            stutter_alert: true,
            min_severity: "major".into(),
        };
        let ev = make_event(Severity::Minor, Utc::now());
        assert!(!should_notify(None, &ev, &cfg));
    }

    #[test]
    fn meeting_min_severity_notified() {
        let cfg = NotificationConfig {
            stutter_alert: true,
            min_severity: "major".into(),
        };
        let ev = make_event(Severity::Major, Utc::now());
        assert!(should_notify(None, &ev, &cfg));
        let ev_crit = make_event(Severity::Critical, Utc::now());
        assert!(should_notify(None, &ev_crit, &cfg));
    }

    #[test]
    fn same_timestamp_not_resent() {
        let cfg = NotificationConfig {
            stutter_alert: true,
            min_severity: "major".into(),
        };
        let ts = Utc::now();
        let ev = make_event(Severity::Major, ts);
        assert!(should_notify(None, &ev, &cfg));
        // 同一时间戳再次轮询到 → 不弹（防刷屏）
        assert!(!should_notify(Some(ts), &ev, &cfg));
    }

    #[test]
    fn newer_timestamp_resent() {
        let cfg = NotificationConfig {
            stutter_alert: true,
            min_severity: "major".into(),
        };
        let old = Utc::now() - chrono::Duration::minutes(1);
        let new = Utc::now();
        let ev = make_event(Severity::Critical, new);
        assert!(should_notify(Some(old), &ev, &cfg));
    }

    #[test]
    fn severity_meets_min_matrix() {
        assert!(severity_meets_min(Severity::Major, "minor"));
        assert!(severity_meets_min(Severity::Critical, "major"));
        assert!(!severity_meets_min(Severity::Minor, "major"));
        assert!(!severity_meets_min(Severity::Major, "critical"));
        assert!(severity_meets_min(Severity::Critical, "critical"));
    }

    #[test]
    fn unknown_min_severity_is_strictest() {
        // 未知等级 → 只放行 Critical
        assert!(!severity_meets_min(Severity::Major, "bogus_level"));
        assert!(severity_meets_min(Severity::Critical, "bogus_level"));
    }

    #[test]
    fn min_severity_case_insensitive() {
        assert!(severity_meets_min(Severity::Major, "MAJOR"));
        assert!(severity_meets_min(Severity::Major, "  Major  "));
    }
}
