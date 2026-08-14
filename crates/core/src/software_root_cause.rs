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
/// - 进程指纹（方案 B）：句柄趋势判定——窗口内句柄数持续增长（后半段净增
///   >= handle_growth_threshold）→ ProcessHandleLeak；绝对值高但无增长 → HandleHigh
///   （中性提示，不参与主因）；GDI+USER 对象数超 gdi_leak_threshold → GdiObjectLeak。
/// - 事件日志：白名单命中的事件映射为 DriverTimeout / ServiceCrash /
///   DiskIoError / HardwareError（同类型去重，不按条数重复计）。
/// 句柄趋势判定结果：None（正常）/ High（偏高，中性）/ Leak（持续增长=真泄漏）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandleTrend {
    None,
    High,
    Leak,
}

/// 方案 B：句柄是否「泄漏」看趋势而非绝对值。
///
/// 输入是卡顿窗口内某进程的句柄数采样序列 `history`（为空时回退到单帧 `current`）。
/// - 全程未超 `abs_threshold` → None（正常占用）；
/// - 后半段句柄均值较前半段净增 >= `growth_threshold` → Leak（持续增长不回落 = 真泄漏）；
/// - 绝对值高但无增长 → High（稳定大句柄进程如 AI/数据库服务，中性提示非泄漏）。
fn handle_trend(
    history: &[u32],
    current: u32,
    abs_threshold: u32,
    growth_threshold: u32,
) -> HandleTrend {
    let values: Vec<u32> = if history.is_empty() {
        vec![current]
    } else {
        history.to_vec()
    };
    let max = values.iter().copied().max().unwrap_or(0);
    if max <= abs_threshold {
        return HandleTrend::None;
    }
    let n = values.len();
    if n < 2 {
        // 仅一帧无法判断趋势，按「偏高」中性处理，不武断判泄漏
        return HandleTrend::High;
    }
    let half = n / 2;
    let first: f64 = values[..half].iter().map(|&v| v as f64).sum::<f64>() / half as f64;
    let second: f64 = values[half..].iter().map(|&v| v as f64).sum::<f64>() / (n - half) as f64;
    if second - first >= growth_threshold as f64 {
        HandleTrend::Leak
    } else {
        HandleTrend::High
    }
}
pub fn enrich_software_causes(
    culprits: &[ProcessBrief],
    win_events: &[WindowsEventRecord],
    handle_history: &std::collections::HashMap<u32, Vec<u32>>,
    handle_leak_threshold: u32,
    handle_growth_threshold: u32,
    gdi_leak_threshold: u32,
) -> Vec<CauseKind> {
    let mut out: Vec<CauseKind> = Vec::new();
    let mut push_once = |k: CauseKind| {
        if !out.contains(&k) {
            out.push(k);
        }
    };
    for c in culprits {
        // 方案 B：句柄泄漏按趋势判定（绝对值高但稳定 → HandleHigh 中性提示，不判泄漏）
        let history = handle_history.get(&c.pid).map(|v| v.as_slice()).unwrap_or(&[]);
        match handle_trend(
            history,
            c.handle_count.unwrap_or(0),
            handle_leak_threshold,
            handle_growth_threshold,
        ) {
            HandleTrend::Leak => push_once(CauseKind::ProcessHandleLeak),
            HandleTrend::High => push_once(CauseKind::HandleHigh),
            HandleTrend::None => {}
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
        CauseKind::HandleHigh => {
            let p = culprits
                .iter()
                .filter(|c| c.handle_count.unwrap_or(0) > 0)
                .max_by_key(|c| c.handle_count.unwrap_or(0));
            match p {
                Some(c) => format!("句柄数偏高: {} {} 句柄（无增长趋势）", c.name, c.handle_count.unwrap_or(0)),
                None => "句柄数偏高 (进程句柄数高但无增长)".to_string(),
            }
        }
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
    // 主因选择跳过 HandleHigh：句柄数偏高只是中性提示，不抢真实主因。
    // 真泄漏（ProcessHandleLeak）仍是软件级主因候选。
    if let Some(top) = ev.cause_kinds.iter().find(|k| **k != CauseKind::HandleHigh) {
        ev.primary_cause = Some(*top);
    }
    ev
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
        let mut history = HashMap::new();
        // a.exe 句柄从 16000 持续增长到 20000（后半段净增 > 2000 → 真泄漏）
        history.insert(1u32, vec![16000, 18000, 20000]);
        let culprits = vec![
            culprit(1, "a.exe", 20000, 100, 100),
            culprit(2, "b.exe", 100, 12000, 100), // GDI+USER 超 10000
        ];
        let causes = enrich_software_causes(&culprits, &[], &history, 10_000, 2_000, 10_000);
        assert!(causes.contains(&CauseKind::ProcessHandleLeak));
        assert!(causes.contains(&CauseKind::GdiObjectLeak));
        // 严重度排序：同为泄漏级，ProcessHandleLeak(4) 在 GdiObjectLeak(5) 前
        assert_eq!(causes[0], CauseKind::ProcessHandleLeak);
    }

    #[test]
    fn enrich_no_leak_below_threshold() {
        let culprits = vec![culprit(1, "a.exe", 5000, 100, 100)];
        let causes = enrich_software_causes(&culprits, &[], &HashMap::new(), 10_000, 2_000, 10_000);
        assert!(causes.is_empty());
    }

    #[test]
    fn enrich_event_log_causes_sorted_by_severity() {
        let evs = vec![
            win_event("System", "Display", 4101),
            win_event("System", "Service Control Manager", 7031),
            win_event("System", "Microsoft-Windows-WHEA-Logger", 18),
        ];
        let causes = enrich_software_causes(&[], &evs, &HashMap::new(), 10_000, 2_000, 10_000);
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
        let causes2 = enrich_software_causes(&[], &dup, &HashMap::new(), 10_000, 2_000, 10_000);
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

    #[test]
    fn handle_trend_classifies() {
        // 未超绝对值阈值 → None
        assert_eq!(handle_trend(&[100, 200, 300], 0, 10_000, 2_000), HandleTrend::None);
        // 稳定高位（如常驻 AI/数据库服务）→ High，不武断判泄漏
        assert_eq!(handle_trend(&[28000, 28100, 27900], 0, 10_000, 2_000), HandleTrend::High);
        // 持续增长不回落 → Leak
        assert_eq!(handle_trend(&[16000, 18000, 20000], 0, 10_000, 2_000), HandleTrend::Leak);
        // 仅单帧且高 → High（无趋势不武断）
        assert_eq!(handle_trend(&[], 20000, 10_000, 2_000), HandleTrend::High);
        // 增长但未超增长阈值 → High
        assert_eq!(handle_trend(&[18000, 18500, 19000], 0, 10_000, 2_000), HandleTrend::High);
    }

    #[test]
    fn enrich_stable_high_handle_is_high_not_leak() {
        // 模拟 HnPCAIService 这类稳定大句柄进程：绝对值 2.8 万但无增长 → HandleHigh 而非泄漏
        let mut history = HashMap::new();
        history.insert(2u32, vec![28000, 28100, 27900]);
        let culprits = vec![culprit(2, "b.exe", 28000, 0, 0)];
        let causes = enrich_software_causes(&culprits, &[], &history, 10_000, 2_000, 10_000);
        assert!(!causes.contains(&CauseKind::ProcessHandleLeak), "稳定大句柄不得判泄漏");
        assert!(causes.contains(&CauseKind::HandleHigh), "应标 HandleHigh 中性提示");
    }

    #[test]
    fn merge_handle_high_does_not_take_primary() {
        let mut ev = StutterEvent::default();
        ev.cause_kinds = vec![CauseKind::CpuHigh];
        ev.primary_cause = Some(CauseKind::CpuHigh);
        ev.culprits = vec![culprit(1, "a.exe", 20000, 0, 0)];
        let out = merge_software_causes(ev, vec![CauseKind::HandleHigh]);
        // 中性提示不抢主因
        assert_eq!(out.primary_cause, Some(CauseKind::CpuHigh));
        assert!(out.causes.iter().any(|c| c.contains("句柄数偏高: a.exe")));
    }
}