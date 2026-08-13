//! F-RC14 软件根因定位：纯逻辑层（PRD §3.4.3 / F-RC14-c / §5.6）。
//!
//! 与 Win32 采集解耦：map_win_event / enrich_software_causes / merge_software_causes
//! 均为纯函数、可直接单测；win32.rs 只负责把系统原始数据（事件日志 / 模块快照）喂进来。

use crate::types::{CauseKind, ProcessBrief, StutterEvent, WindowsEventRecord};

/// 白名单事件映射（PRD §3.4.3）：(channel, provider 关键词, 事件 ID, 软件级 cause)。
///
/// - provider 用「包含」匹配（如 Display 覆盖 nvlddmkm / igdkmd64 / amdkmdag 等所有显卡驱动）；
/// - id == 0 表示该 provider 下任意事件 ID 都命中（用于 WHEA，硬件错误 ID 因机器而异）。
pub const WIN_EVENT_WHITELIST: &[(&str, &str, u32, CauseKind)] = &[
    // 显卡驱动 TDR 超时 / 驱动重置
    ("System", "Display", 4101, CauseKind::DriverTimeout),
    // 磁盘坏块 / 分页错误 / IO 超时
    ("System", "disk", 7, CauseKind::DiskIoError),
    ("System", "disk", 51, CauseKind::DiskIoError),
    ("System", "disk", 153, CauseKind::DiskIoError),
    // 意外断电 / 系统崩溃（Kernel-Power）
    ("System", "Microsoft-Windows-Kernel-Power", 41, CauseKind::HardwareError),
    // 服务意外终止 / 崩溃（Service Control Manager）
    ("System", "Service Control Manager", 7031, CauseKind::ServiceCrash),
    ("System", "Service Control Manager", 7034, CauseKind::ServiceCrash),
    // WHEA 硬件错误（ID 因机器而异，provider 命中即可）
    ("System", "Microsoft-Windows-WHEA-Logger", 0, CauseKind::HardwareError),
];

/// 把一条原始 Windows 事件映射到软件级 cause。白名单命中返回对应 cause，否则 None。
pub fn map_win_event(ev: &WindowsEventRecord) -> Option<CauseKind> {
    for (channel, provider_kw, id, kind) in WIN_EVENT_WHITELIST {
        if ev.channel == *channel
            && ev.provider.contains(provider_kw)
            && (*id == 0 || ev.win_event_id == *id)
        {
            return Some(*kind);
        }
    }
    None
}

/// 把一条原始 Windows 事件映射为「是否白名单命中」（供写入 windows_events 表时过滤）。
pub fn is_whitelisted_win_event(ev: &WindowsEventRecord) -> bool {
    map_win_event(ev).is_some()
}

/// F-RC14-a + F-RC14-c：汇总软件级 cause（进程指纹阈值 + 事件日志命中），
/// 按严重程度排序（software_priority 升序）并去重。
///
/// - 进程指纹：句柄数超 handle_leak_threshold → ProcessHandleLeak；
///   GDI+USER 对象数超 gdi_leak_threshold → GdiObjectLeak。
/// - 事件日志：白名单命中的事件映射为 DriverTimeout / ServiceCrash /
///   DiskIoError / HardwareError（同类型去重，不按条数重复计）。
pub fn enrich_software_causes(
    culprits: &[ProcessBrief],
    win_events: &[WindowsEventRecord],
    handle_leak_threshold: u32,
    gdi_leak_threshold: u32,
) -> Vec<CauseKind> {
    let mut out: Vec<CauseKind> = Vec::new();
    let mut push_once = |k: CauseKind| {
        if !out.contains(&k) {
            out.push(k);
        }
    };
    for c in culprits {
        if c.handle_count.unwrap_or(0) > handle_leak_threshold {
            push_once(CauseKind::ProcessHandleLeak);
        }
        let gdi_user = c.gdi_objects.unwrap_or(0) as u64 + c.user_objects.unwrap_or(0) as u64;
        if gdi_user > gdi_leak_threshold as u64 {
            push_once(CauseKind::GdiObjectLeak);
        }
    }
    for ev in win_events {
        if let Some(kind) = map_win_event(ev) {
            push_once(kind);
        }
    }
    out.sort_by_key(|k| k.software_priority().unwrap_or(99));
    out
}

/// 软件级 cause 的人类可读文本（追加进事件 causes 列表，兼容只读 causes 的旧 reader）。
///
/// 句柄 / GDI 泄漏带上最可疑的进程名与数值；事件日志类带上 provider + 事件 ID。
pub fn software_cause_text(kind: CauseKind, culprits: &[ProcessBrief]) -> String {
    match kind {
        CauseKind::ProcessHandleLeak => {
            let p = culprits
                .iter()
                .filter(|c| c.handle_count.unwrap_or(0) > 0)
                .max_by_key(|c| c.handle_count.unwrap_or(0));
            match p {
                Some(c) => format!("句柄泄漏: {} {} 句柄", c.name, c.handle_count.unwrap_or(0)),
                None => "句柄泄漏 (进程句柄数超阈值)".to_string(),
            }
        }
        CauseKind::GdiObjectLeak => {
            let p = culprits
                .iter()
                .filter(|c| c.gdi_objects.unwrap_or(0) + c.user_objects.unwrap_or(0) > 0)
                .max_by_key(|c| c.gdi_objects.unwrap_or(0) as u64 + c.user_objects.unwrap_or(0) as u64);
            match p {
                Some(c) => format!("GDI 对象泄漏: {} GDI{}+USER{}", c.name, c.gdi_objects.unwrap_or(0), c.user_objects.unwrap_or(0)),
                None => "GDI 对象泄漏 (GDI/USER 对象超阈值)".to_string(),
            }
        }
        CauseKind::DriverTimeout => "显卡驱动 TDR 超时 (Display 4101)".to_string(),
        CauseKind::ServiceCrash => "服务崩溃 (SCM 7031/7034)".to_string(),
        CauseKind::DiskIoError => "磁盘 IO 错误 (disk 7/51/153)".to_string(),
        CauseKind::HardwareError => "硬件错误 (Kernel-Power/WHEA)".to_string(),
        _ => format!("{:?}", kind),
    }
}

/// 把软件级 cause 合并进卡顿事件（F-RC14 收尾 + PRD §5.6「主因软件级优先」）。
///
/// 软件级 cause 来自事件生成后的系统回溯（事件日志 / 进程指纹），无法进 detector 的
/// 文本 cause 池，故在此：
/// 1. 追加 cause_kinds（结构化，软件级按严重度在前、资源级按原首触顺序在后）；
/// 2. 追加 causes（可读文本，兼容只读 causes 的旧 reader）；
/// 3. 重设 primary_cause：有软件级 cause 时取最严重者，否则保持原资源级主因。
pub fn merge_software_causes(mut ev: StutterEvent, sw: Vec<CauseKind>) -> StutterEvent {
    if sw.is_empty() {
        return ev;
    }
    // 只追加尚未出现过的软件级 cause
    let mut fresh: Vec<CauseKind> = Vec::new();
    for k in sw {
        if !ev.cause_kinds.contains(&k) && !fresh.contains(&k) {
            fresh.push(k);
        }
    }
    if fresh.is_empty() {
        return ev;
    }
    for k in &fresh {
        ev.causes.push(software_cause_text(*k, &ev.culprits));
    }
    let mut software = fresh;
    software.sort_by_key(|k| k.software_priority().unwrap_or(99));
    let mut resource: Vec<CauseKind> = ev
        .cause_kinds
        .iter()
        .copied()
        .filter(|k| !k.is_software())
        .collect();
    resource.retain(|k| !software.contains(k));
    ev.cause_kinds = software.into_iter().chain(resource).collect();
    if let Some(top) = ev.cause_kinds.first() {
        ev.primary_cause = Some(*top);
    }
    ev
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win_event(channel: &str, provider: &str, id: u32) -> WindowsEventRecord {
        WindowsEventRecord {
            channel: channel.to_string(),
            provider: provider.to_string(),
            win_event_id: id,
            level: "Error".to_string(),
            message: String::new(),
            ts: "2026-08-13T00:00:00Z".to_string(),
        }
    }

    fn culprit(pid: u32, name: &str, handle: u32, gdi: u32, user: u32) -> ProcessBrief {
        ProcessBrief {
            pid,
            name: name.to_string(),
            cpu_usage: 10.0,
            mem_used_mb: 100,
            handle_count: Some(handle),
            gdi_objects: Some(gdi),
            user_objects: Some(user),
            ..Default::default()
        }
    }

    #[test]
    fn map_display_4101_is_driver_timeout() {
        let ev = win_event("System", "Display", 4101);
        assert_eq!(map_win_event(&ev), Some(CauseKind::DriverTimeout));
    }

    #[test]
    fn map_disk_ids_map_to_disk_io_error() {
        for id in [7, 51, 153] {
            let ev = win_event("System", "disk", id);
            assert_eq!(map_win_event(&ev), Some(CauseKind::DiskIoError), "id={}", id);
        }
    }

    #[test]
    fn map_kernel_power_41_is_hardware_error() {
        let ev = win_event("System", "Microsoft-Windows-Kernel-Power", 41);
        assert_eq!(map_win_event(&ev), Some(CauseKind::HardwareError));
    }

    #[test]
    fn map_whea_any_id_is_hardware_error() {
        for id in [17, 18, 19, 20, 41] {
            let ev = win_event("System", "Microsoft-Windows-WHEA-Logger", id);
            assert_eq!(map_win_event(&ev), Some(CauseKind::HardwareError), "id={}", id);
        }
    }

    #[test]
    fn map_service_crash_ids() {
        for id in [7031, 7034] {
            let ev = win_event("System", "Service Control Manager", id);
            assert_eq!(map_win_event(&ev), Some(CauseKind::ServiceCrash), "id={}", id);
        }
    }

    #[test]
    fn map_non_whitelist_is_none() {
        let ev = win_event("Application", "SomeApp", 1000);
        assert_eq!(map_win_event(&ev), None);
        // provider 不同但 ID 撞车也不命中（磁盘 7 vs Display 7）
        let ev2 = win_event("System", "Display", 7);
        assert_eq!(map_win_event(&ev2), None);
    }

    #[test]
    fn enrich_leak_causes_by_threshold() {
        let culprits = vec![
            culprit(1, "a.exe", 20000, 100, 100), // 句柄超 10000
            culprit(2, "b.exe", 100, 12000, 100), // GDI+USER 超 10000
        ];
        let causes = enrich_software_causes(&culprits, &[], 10_000, 10_000);
        assert!(causes.contains(&CauseKind::ProcessHandleLeak));
        assert!(causes.contains(&CauseKind::GdiObjectLeak));
        // 严重度排序：同为泄漏级，ProcessHandleLeak(4) 在 GdiObjectLeak(5) 前
        assert_eq!(causes[0], CauseKind::ProcessHandleLeak);
    }

    #[test]
    fn enrich_no_leak_below_threshold() {
        let culprits = vec![culprit(1, "a.exe", 5000, 100, 100)];
        let causes = enrich_software_causes(&culprits, &[], 10_000, 10_000);
        assert!(causes.is_empty());
    }

    #[test]
    fn enrich_event_log_causes_sorted_by_severity() {
        let evs = vec![
            win_event("System", "Display", 4101),
            win_event("System", "Service Control Manager", 7031),
            win_event("System", "Microsoft-Windows-WHEA-Logger", 18),
        ];
        let causes = enrich_software_causes(&[], &evs, 10_000, 10_000);
        // 严重度：HardwareError(0) > DriverTimeout(1) > ServiceCrash(2)
        assert_eq!(causes, vec![
            CauseKind::HardwareError,
            CauseKind::DriverTimeout,
            CauseKind::ServiceCrash,
        ]);
        // 同类型多条只计一次
        let dup = vec![
            win_event("System", "disk", 7),
            win_event("System", "disk", 51),
        ];
        let causes2 = enrich_software_causes(&[], &dup, 10_000, 10_000);
        assert_eq!(causes2, vec![CauseKind::DiskIoError]);
    }

    #[test]
    fn merge_sets_software_primary_and_appends_text() {
        let mut ev = StutterEvent::default();
        ev.cause_kinds = vec![CauseKind::CpuHigh, CauseKind::MemLow];
        ev.primary_cause = Some(CauseKind::CpuHigh);
        ev.culprits = vec![culprit(1, "a.exe", 20000, 0, 0)];
        let out = merge_software_causes(ev, vec![CauseKind::ProcessHandleLeak]);
        // 软件级主因优先
        assert_eq!(out.primary_cause, Some(CauseKind::ProcessHandleLeak));
        assert_eq!(out.cause_kinds[0], CauseKind::ProcessHandleLeak);
        // 资源级仍在后面
        assert!(out.cause_kinds.contains(&CauseKind::CpuHigh));
        // 追加了可读文本
        assert!(out.causes.iter().any(|c| c.contains("句柄泄漏: a.exe")));
    }

    #[test]
    fn merge_empty_keeps_resource_primary() {
        let mut ev = StutterEvent::default();
        ev.cause_kinds = vec![CauseKind::MemLow];
        ev.primary_cause = Some(CauseKind::MemLow);
        let out = merge_software_causes(ev, vec![]);
        assert_eq!(out.primary_cause, Some(CauseKind::MemLow));
        assert_eq!(out.cause_kinds, vec![CauseKind::MemLow]);
    }
}