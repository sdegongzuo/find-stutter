//! 进程详情列表（P2，任务管理器风格）。
//!
//! 右键菜单「进程详情」→ 打开独立置顶窗口，显示进程快照。
//!
//! ## 任务管理器风格特性
//!
//! - **1Hz 实时刷新**：窗口打开期间 `Timer` 每秒重采（非一次性快照）
//! - **列头点击排序**：PID / 名称 / CPU / 内存 / 磁盘 / 网络 六列可排序
//!   （点击切方向；箭头显示当前排序列）
//! - **CPU 归一化**：`cpu_usage / 核数` → 0~100%（占满全部核 = 100%），
//!   与任务管理器「进程」视图语义一致
//! - **磁盘 / 网络显示速率**（B/s）：基于两次采样的 `GetProcessIoCounters`
//!   差分；首次采样无基线，速率显示 0
//! - **搜索**：名称 / PID 过滤（大小写不敏感）
//! - **停止按钮**：`Process::kill()`
//! - **窗口缩放**：右下角 resize 手柄
//!
//! ## 数据来源
//!
//! GUI 是 P3 只读模式（不常驻采集写库），进程列表按需查看：窗口打开时
//! 用 sysinfo + `GetProcessIoCounters` 采样（不写 stutter.db）。
//!
//! ## CPU 基线
//!
//! sysinfo 的 `cpu_usage` 是「两次采样增量 ÷ 系统增量 × 核数」，必须
//! **复用同一个 `System` 实例**持续采样；每次新建会丢基线导致首采失真。
//! 本模块持有 `Mutex<System>` 供刷新循环复用。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, SharedString};

/// 一行进程信息（对应 slint `ProcessRowData` 结构）
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessRow {
    pub pid: u32,
    /// 父进程 PID（sysinfo parent()；用于树状聚合找主进程）
    pub parent_pid: u32,
    pub name: String,
    /// CPU 使用率（0.0 ~ 100.0×核数；显示前归一化）
    pub cpu_usage: f32,
    /// 常驻内存字节数
    pub memory_bytes: u64,
    /// 磁盘读速率（B/s）
    pub disk_read_bps: u64,
    /// 磁盘写速率（B/s）
    pub disk_write_bps: u64,
    /// 网络等非磁盘 I/O 速率（B/s）
    pub net_bps: u64,
    /// 累计网络等非磁盘 I/O 字节（OtherTransferCount 当前值）
    pub net_total_bytes: u64,
    /// 运行状态（中文）
    pub status: String,
}

/// 进程快照采集器：持有 sysinfo System 实例（CPU 基线）+ IO 计数器历史。
pub struct ProcessSampler {
    sys: sysinfo::System,
    /// pid → 上次 IO_COUNTERS（差分算速率）
    prev_io: HashMap<u32, IoSnapshot>,
    /// 上次采样时刻
    prev_time: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct IoSnapshot {
    read: u64,
    write: u64,
    other: u64,
}

impl ProcessSampler {
    /// 创建采样器并预热（第一次 refresh 建立 CPU 基线）。
    pub fn new() -> Self {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_all(); // 预热：只建立基线
        Self {
            sys,
            prev_io: HashMap::new(),
            prev_time: None,
        }
    }

    /// 采集一次快照（全量，不截断）。
    /// 第一次调用速率列为 0（无基线）。
    /// 注意：不在采样阶段截断——先完整聚合（任务管理器行为），
    /// 显示阶段再按聚合后的行数限制，避免「同组部分实例被截掉、
    /// 剩下的看起来像独立进程」。
    pub fn sample(&mut self) -> Vec<ProcessRow> {
        self.sys.refresh_all();

        let now = Instant::now();
        let dt = self.prev_time.map(|t| now.duration_since(t).as_secs_f64());

        let mut rows: Vec<ProcessRow> = Vec::with_capacity(self.sys.processes().len());
        let mut next_io: HashMap<u32, IoSnapshot> = HashMap::new();

        for (pid, p) in self.sys.processes() {
            let pid_u32 = pid.as_u32();
            let io = process_io_counters(pid_u32);
            let cur = IoSnapshot {
                read: io.map(|c| c.ReadTransferCount).unwrap_or(0),
                write: io.map(|c| c.WriteTransferCount).unwrap_or(0),
                other: io.map(|c| c.OtherTransferCount).unwrap_or(0),
            };

            // 差分速率：需要上次基线 + 时间间隔
            let (read_bps, write_bps, net_bps) = match (self.prev_io.get(&pid_u32), dt) {
                (Some(prev), Some(dt)) if dt > 0.0 => (
                    rate(prev.read, cur.read, dt),
                    rate(prev.write, cur.write, dt),
                    rate(prev.other, cur.other, dt),
                ),
                _ => (0, 0, 0),
            };
            next_io.insert(pid_u32, cur);

            rows.push(ProcessRow {
                pid: pid_u32,
                parent_pid: p.parent().map(|x| x.as_u32()).unwrap_or(0),
                name: p.name().to_string_lossy().into_owned(),
                cpu_usage: p.cpu_usage(),
                memory_bytes: p.memory(),
                disk_read_bps: read_bps,
                disk_write_bps: write_bps,
                net_bps,
                net_total_bytes: cur.other,
                status: format_status(p.status()),
            });
        }

        self.prev_io = next_io;
        self.prev_time = Some(now);

        rows
    }

    /// 逻辑 CPU 核数（用于 CPU 归一化）
    pub fn cpu_count(&self) -> usize {
        self.sys.cpus().len()
    }
}

/// 速率：差值 ÷ 时间（B/s）
fn rate(prev: u64, cur: u64, dt: f64) -> u64 {
    (cur.saturating_sub(prev) as f64 / dt) as u64
}

/// 按 CPU 降序排序并截断（纯函数，可单测）。
pub fn rank_processes(rows: &mut [ProcessRow], limit: usize) -> Vec<ProcessRow> {
    rows.sort_by(|a, b| {
        b.cpu_usage
            .partial_cmp(&a.cpu_usage)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows.iter().take(limit).cloned().collect()
}

/// 按指定列排序（任务管理器风格；纯函数，可单测）。
/// `column`：pid / name / cpu / mem / disk / net；`ascending`：升序。
pub fn sort_processes(rows: &mut [ProcessRow], column: &str, ascending: bool) {
    let cmp = |a: &ProcessRow, b: &ProcessRow| -> std::cmp::Ordering {
        let o = match column {
            "pid" => a.pid.cmp(&b.pid),
            "name" => a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()),
            "cpu" => a.cpu_usage.partial_cmp(&b.cpu_usage).unwrap_or(std::cmp::Ordering::Equal),
            "mem" => a.memory_bytes.cmp(&b.memory_bytes),
            "disk" => (a.disk_read_bps + a.disk_write_bps).cmp(&(b.disk_read_bps + b.disk_write_bps)),
            "net" => a.net_bps.cmp(&b.net_bps),
            _ => a.pid.cmp(&b.pid),
        };
        if ascending { o } else { o.reverse() }
    };
    rows.sort_by(cmp);
}

/// 按关键字过滤进程（名称 / PID 子串匹配，大小写不敏感；纯函数，可单测）。
/// 空关键字返回全部。
pub fn filter_processes(rows: &[ProcessRow], keyword: &str) -> Vec<ProcessRow> {
    let kw = keyword.trim().to_ascii_lowercase();
    if kw.is_empty() {
        return rows.to_vec();
    }
    rows.iter()
        .filter(|r| r.name.to_ascii_lowercase().contains(&kw) || r.pid.to_string().contains(&kw))
        .cloned()
        .collect()
}

/// 一个聚合组：树状两级结构（主进程 → 子进程）。
///
/// 任务管理器折叠效果的聚合规则：
/// 1. **同名归类**：Name 相同的进程归为一组
/// 2. **找主进程（Root）**：组内 PPID 不属于本组（被其他应用 / explorer
///    拉起）的进程即主进程；其余进程塌陷为它的子节点
/// 3. **svchost.exe 特殊处理**：不显示 "svchost.exe"，而是查该 PID 绑定的
///    服务显示名（`EnumServicesStatusEx`），主条目显示「服务宿主: [组名]」，
///    展开显示具体服务列表
#[derive(Debug, Clone, PartialEq)]
pub struct GroupedProcess {
    /// 主进程名（svchost 时为 "svchost.exe"；显示层按服务名改写）
    pub name: String,
    /// 主进程（Root）
    pub root: ProcessRow,
    /// 塌陷到主进程的子进程
    pub children: Vec<ProcessRow>,
    /// 该 PID 绑定的服务显示名（仅 svchost 非空）
    pub services: Vec<String>,
}

impl GroupedProcess {
    pub fn total_count(&self) -> usize {
        1 + self.children.len()
    }
}

/// 按「同名 + PPID 父子关系」聚合（纯函数，可单测）。
/// `services`：pid → 服务名（svchost 特殊聚合用；可为空 Map）。
pub fn group_processes(
    rows: &[ProcessRow],
    services: &std::collections::HashMap<u32, Vec<String>>,
) -> Vec<GroupedProcess> {
    // 1) 同名归组（大小写不敏感）
    let mut by_name: std::collections::BTreeMap<String, Vec<ProcessRow>> = Default::default();
    for r in rows {
        by_name
            .entry(r.name.to_ascii_lowercase())
            .or_default()
            .push(r.clone());
    }

    let mut out: Vec<GroupedProcess> = Vec::new();
    for (key, mut group_rows) in by_name {
        // 2) 找主进程：PPID 不属于本组的进程
        let pids: std::collections::HashSet<u32> =
            group_rows.iter().map(|r| r.pid).collect();
        let root_idx = group_rows
            .iter()
            .position(|r| !pids.contains(&r.parent_pid))
            .unwrap_or(0); // 全在组内（罕见环）→ 取第一个为主
        let root = group_rows.remove(root_idx);
        let children = group_rows;

        // 3) svchost 特殊聚合：查服务名
        let services_of_root = services.get(&root.pid).cloned().unwrap_or_default();
        out.push(GroupedProcess {
            name: root.name.clone(),
            root,
            children,
            services: services_of_root,
        });
        let _ = key;
    }
    out
}

/// 聚合组 → 汇总行（主进程 + 子进程求和的聚合值）。
pub fn group_aggregate(g: &GroupedProcess) -> ProcessRow {
    let mut acc = g.root.clone();
    for r in &g.children {
        acc.cpu_usage += r.cpu_usage;
        acc.memory_bytes += r.memory_bytes;
        acc.disk_read_bps = acc.disk_read_bps.saturating_add(r.disk_read_bps);
        acc.disk_write_bps = acc.disk_write_bps.saturating_add(r.disk_write_bps);
        acc.net_bps = acc.net_bps.saturating_add(r.net_bps);
        acc.net_total_bytes = acc.net_total_bytes.saturating_add(r.net_total_bytes);
        acc.pid = acc.pid.min(r.pid);
    }
    acc
}

/// 按列排序聚合组（任务管理器风格；纯函数，可单测）。
/// `column`：pid / name / cpu / mem / disk / net / nettotal / status。
pub fn sort_groups(groups: &mut [GroupedProcess], column: &str, ascending: bool) {
    let key = |g: &GroupedProcess| -> (f64, String) {
        let agg = group_aggregate(g);
        let num = match column {
            "pid" => agg.pid as f64,
            "cpu" => agg.cpu_usage as f64,
            "mem" => agg.memory_bytes as f64,
            "disk" => (agg.disk_read_bps + agg.disk_write_bps) as f64,
            "net" => agg.net_bps as f64,
            "nettotal" => agg.net_total_bytes as f64,
            _ => 0.0,
        };
        (num, g.name.to_ascii_lowercase())
    };
    groups.sort_by(|a, b| {
        let (an, aname) = key(a);
        let (bn, bname) = key(b);
        let o = match column {
            "name" | "status" => aname.cmp(&bname),
            _ => an.partial_cmp(&bn).unwrap_or(std::cmp::Ordering::Equal),
        };
        if ascending { o } else { o.reverse() }
    });
}

/// `ProcessStatus` → 中文。
pub fn format_status(status: sysinfo::ProcessStatus) -> String {
    use sysinfo::ProcessStatus as S;
    match status {
        S::Idle => "空闲".into(),
        S::Run => "运行中".into(),
        S::Sleep => "睡眠".into(),
        S::Stop => "已停止".into(),
        S::Zombie => "僵尸".into(),
        S::Dead => "已结束".into(),
        S::Tracing => "追踪".into(),
        S::Parked => "驻留".into(),
        S::UninterruptibleDiskSleep => "磁盘等待".into(),
        S::Waking => "唤醒中".into(),
        _ => "未知".into(),
    }
}

/// 终止进程。成功返回 `true`。
pub fn kill_process(pid: u32) -> bool {
    let mut sys = sysinfo::System::new_all();
    sys.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
    );
    match sys.process(sysinfo::Pid::from_u32(pid)) {
        Some(p) => p.kill(),
        None => false,
    }
}

/// 打开进程可执行文件所在的位置（任务管理器「打开文件所在的位置」）。
///
/// 流程：`QueryFullProcessImageNameW` 取完整路径 → 启动 `explorer /select,<path>`
/// 打开资源管理器并选中该文件。失败（权限不足 / 进程已退出 / 无法启动资源管理器）
/// 返回 `Err`。
pub fn open_process_location(pid: u32) -> anyhow::Result<()> {
    let path = process_exe_path(pid).ok_or_else(|| {
        anyhow::anyhow!("无法获取进程 {} 的可执行文件路径（权限不足或进程已退出）", pid)
    })?;
    log::info!("打开文件所在的位置: {} (PID {})", path, pid);
    // explorer 非标准解析命令行参数：把整个 `/select,"path"` 作为单个参数传入，
    // 路径含空格也能正确选中目标文件。
    std::process::Command::new("explorer.exe")
        .arg(format!("/select,\"{}\"", path))
        .spawn()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("启动资源管理器失败: {}", e))
}

/// 查询进程可执行文件完整路径（`QueryFullProcessImageNameW`，Win32 格式如
/// `C:\Windows\System32\notepad.exe`）。失败（权限不足 / 进程已退出）返回 `None`。
#[cfg(windows)]
fn process_exe_path(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 32 * 1024];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32, // Win32 格式：返回 C:\ 形式的路径
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

#[cfg(not(windows))]
fn process_exe_path(_pid: u32) -> Option<String> {
    None
}

/// 读进程 I/O 计数器（`GetProcessIoCounters`）。
#[cfg(windows)]
fn process_io_counters(pid: u32) -> Option<windows::Win32::System::Threading::IO_COUNTERS> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        GetProcessIoCounters, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut counters: windows::Win32::System::Threading::IO_COUNTERS = std::mem::zeroed();
        let result = GetProcessIoCounters(handle, &mut counters);
        let _ = CloseHandle(handle);
        result.ok()?;
        Some(counters)
    }
}

/// 枚举当前运行的 Windows 服务 → pid → 服务显示名列表。
/// 失败返回空 Map（svchost 退化为按进程名显示）。
#[cfg(windows)]
pub fn service_map() -> std::collections::HashMap<u32, Vec<String>> {
    use windows::Win32::System::Services::{
        EnumServicesStatusExW, OpenSCManagerW, SC_ENUM_PROCESS_INFO, SC_MANAGER_ENUMERATE_SERVICE,
        ENUM_SERVICE_STATUS_PROCESSW, SERVICE_ACTIVE, SERVICE_WIN32,
    };
    let mut out: std::collections::HashMap<u32, Vec<String>> = Default::default();
    unsafe {
        let scm = match OpenSCManagerW(None, None, SC_MANAGER_ENUMERATE_SERVICE) {
            Ok(h) => h,
            Err(e) => {
                log::warn!("OpenSCManagerW 失败: {}", e);
                return out;
            }
        };
        let _scm = scm; // 保持句柄存活

        // 两遍调用：先问需要多大缓冲
        let mut bytes_needed: u32 = 0;
        let mut services_returned: u32 = 0;
        let mut resume: u32 = 0;
        let first = EnumServicesStatusExW(
            scm,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_ACTIVE,
            None,
            &mut bytes_needed,
            &mut services_returned,
            Some(&mut resume),
            None,
        );
        if first.is_err() && bytes_needed == 0 {
            return out;
        }
        if bytes_needed == 0 {
            return out;
        }

        let mut buf: Vec<u8> = vec![0u8; bytes_needed as usize];
        let mut returned: u32 = 0;
        let mut resume2: u32 = 0;
        let ok = EnumServicesStatusExW(
            scm,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_ACTIVE,
            Some(&mut buf),
            &mut bytes_needed,
            &mut returned,
            Some(&mut resume2),
            None,
        );
        if ok.is_err() {
            return out;
        }

        // 解析 ENUM_SERVICE_STATUS_PROCESSW 数组
        let base = buf.as_ptr() as usize;
        let item_size = std::mem::size_of::<ENUM_SERVICE_STATUS_PROCESSW>();
        for i in 0..returned as usize {
            let item_ptr = (base + i * item_size) as *const ENUM_SERVICE_STATUS_PROCESSW;
            let item = &*item_ptr;
            let pid = item.ServiceStatusProcess.dwProcessId;
            if pid == 0 {
                continue;
            }
            let name = item.lpDisplayName.to_string().unwrap_or_default();
            if !name.is_empty() {
                out.entry(pid).or_default().push(name);
            }
        }
    }
    out
}

#[cfg(not(windows))]
pub fn service_map() -> std::collections::HashMap<u32, Vec<String>> {
    Default::default()
}

fn format_bytes(b: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;
    let v = b as f64;
    if v >= TB {
        format!("{:.2}T", v / TB)
    } else if v >= GB {
        format!("{:.1}G", v / GB)
    } else if v >= MB {
        format!("{:.1}M", v / MB)
    } else if v >= KB {
        format!("{:.1}K", v / KB)
    } else {
        format!("{}B", b)
    }
}

/// 磁盘速率短文本（任务管理器风格：每秒）
pub fn format_disk_short(row: &ProcessRow) -> String {
    format!("R {}/s W {}/s", format_bytes(row.disk_read_bps), format_bytes(row.disk_write_bps))
}

pub fn format_disk_full(row: &ProcessRow) -> String {
    format!(
        "磁盘读: {}/s ({} B/s)\n磁盘写: {}/s ({} B/s)",
        format_bytes(row.disk_read_bps),
        row.disk_read_bps,
        format_bytes(row.disk_write_bps),
        row.disk_write_bps
    )
}

/// 网络速率短文本。
pub fn format_net_short(row: &ProcessRow) -> String {
    format!("{}/s", format_bytes(row.net_bps))
}

pub fn format_net_full(row: &ProcessRow) -> String {
    format!(
        "网络等非磁盘 I/O: {}/s ({} B/s)\n说明: Windows 将网络计入 IO_COUNTERS.Other，含管道等",
        format_bytes(row.net_bps),
        row.net_bps
    )
}

/// 累计网络传输量短文本（任务管理器「网络」总列）。
pub fn format_net_total(row: &ProcessRow) -> String {
    format_bytes(row.net_total_bytes)
}

pub fn format_net_total_full(row: &ProcessRow) -> String {
    format!(
        "累计网络等非磁盘 I/O: {} ({} 字节)\n说明: Windows 将网络计入 IO_COUNTERS.Other，含管道等",
        format_bytes(row.net_total_bytes),
        row.net_total_bytes
    )
}

pub fn format_mem(mem_bytes: u64) -> String {
    let mb = mem_bytes / (1024 * 1024);
    format!("{} MB", mb)
}

/// 构建 slint 行数据。
#[allow(clippy::too_many_arguments)]
pub fn row_to_slint(
    pid: u32,
    name: String,
    name_full: String,
    group_key: String,
    cpu: String,
    mem: String,
    disk: String,
    disk_full: String,
    net: String,
    net_full: String,
    net_total: String,
    net_total_full: String,
    status: String,
    is_group: bool,
    child_count: i32,
) -> crate::ProcessRowData {
    crate::ProcessRowData {
        pid: pid as i32,
        name: SharedString::from(name),
        name_full: SharedString::from(name_full),
        group_key: SharedString::from(group_key),
        cpu: SharedString::from(cpu),
        mem: SharedString::from(mem),
        disk: SharedString::from(disk),
        disk_full: SharedString::from(disk_full),
        net: SharedString::from(net),
        net_full: SharedString::from(net_full),
        net_total: SharedString::from(net_total),
        net_total_full: SharedString::from(net_total_full),
        status: SharedString::from(status),
        is_group,
        child_count,
    }
}

/// 把单行格式化为 slint 行（普通行/子行共用）。
fn row_display(r: &ProcessRow, nb_cpus: usize, indent: bool) -> crate::ProcessRowData {
    let full_name = format!("PID {}  {}", r.pid, r.name);
    let cpu_pct = (r.cpu_usage / nb_cpus as f32).clamp(0.0, 100.0);
    let mut name = r.name.clone();
    if indent {
        name = format!("    {}", name);
    }
    row_to_slint(
        r.pid,
        name,
        full_name,
        String::new(), // 普通行无 group-key
        format!("{:.1}%", cpu_pct),
        format_mem(r.memory_bytes),
        format_disk_short(r),
        format_disk_full(r),
        format_net_short(r),
        format_net_full(r),
        format_net_total(r),
        format_net_total_full(r),
        r.status.clone(),
        false,
        0,
    )
}

/// 聚合组 → 父节点 slint 行。
/// svchost：主条目显示「服务宿主: [服务组名]」；多服务时列前两个 + 总数。
fn group_display(g: &GroupedProcess, agg: &ProcessRow, nb_cpus: usize) -> crate::ProcessRowData {
    let cpu_pct = (agg.cpu_usage / nb_cpus as f32).clamp(0.0, 100.0);
    let count = g.total_count() as i32;
    let (title, full_name, group_key) = if g.name.eq_ignore_ascii_case("svchost.exe") {
        if g.services.is_empty() {
            (
                format!("服务宿主: svchost ({})", count),
                format!("服务宿主: svchost.exe（{} 个实例）", count),
                "svchost.exe".to_string(),
            )
        } else {
            let shown: Vec<String> = g.services.iter().take(2).cloned().collect();
            let more = if g.services.len() > 2 {
                format!(" 等 {} 个服务", g.services.len())
            } else {
                String::new()
            };
            (
                format!("服务宿主: {}{}", shown.join(", "), more),
                format!(
                    "服务宿主: {}（PID {}, {} 个实例）\n服务: {}",
                    g.services.join(", "),
                    agg.pid,
                    count,
                    g.services.join(", ")
                ),
                "svchost.exe".to_string(),
            )
        }
    } else {
        (
            format!("{} ({})", g.name, count),
            format!("{}（{} 个实例，PID {}）", g.name, count, agg.pid),
            g.name.clone(),
        )
    };
    row_to_slint(
        agg.pid,
        title,
        full_name,
        group_key,
        format!("{:.1}%", cpu_pct),
        format_mem(agg.memory_bytes),
        format_disk_short(agg),
        format_disk_full(agg),
        format_net_short(agg),
        format_net_full(agg),
        format_net_total(agg),
        format_net_total_full(agg),
        "运行中".into(),
        true,
        count,
    )
}

/// 进程列表窗口句柄。
pub struct ProcessListWindow {
    ui: crate::ProcessList,
    /// 最近一次采样的全量进程行缓存（渲染/搜索/展开直接用，避免反复采样）
    cache: Mutex<Vec<ProcessRow>>,
    /// CPU 核数缓存（render 复用）
    nb_cpus: Arc<Mutex<usize>>,
    /// 快照采集器（持有 CPU 基线 + IO 历史，供 1Hz 刷新）
    sampler: Arc<Mutex<ProcessSampler>>,
    /// 当前排序列 + 方向
    sort: Arc<Mutex<(String, bool)>>,
    /// 搜索关键字
    search: Arc<Mutex<String>>,
    /// 已展开的聚合组 key（小写进程名）
    expanded: Arc<Mutex<std::collections::HashSet<String>>>,
    /// 刷新 Timer（持有防 drop）
    _timer: slint::Timer,
}

impl ProcessListWindow {
    /// 创建并显示进程详情窗口。
    ///
    /// **拖动延迟**：窗口由右键菜单弹出，`TrackPopupMenu` 的模态循环会
    /// 吞掉 MouseUp 事件，winit 误以为鼠标仍按住 → 新窗口一出现就被拖动
    /// 跟着鼠标走。因此创建后 300ms 内忽略 drag-moved。
    pub fn show() -> anyhow::Result<Self> {
        let ui = crate::ProcessList::new()?;
        let sampler: Arc<Mutex<ProcessSampler>> = Arc::new(Mutex::new(ProcessSampler::new()));
        let sort: Arc<Mutex<(String, bool)>> = Arc::new(Mutex::new(("cpu".to_string(), false)));
        let search: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let expanded: Arc<Mutex<std::collections::HashSet<String>>> =
            Arc::new(Mutex::new(std::collections::HashSet::new()));
        // 进程行缓存（采样一次，渲染/搜索/展开/排序复用）
        let cache: Arc<Mutex<Vec<ProcessRow>>> = Arc::new(Mutex::new(Vec::new()));
        let cache_tick = cache.clone();
        // CPU 核数缓存（render 复用；sysinfo::System::new() 枚举硬件很贵）
        let nb_cpus: Arc<Mutex<usize>> = Arc::new(Mutex::new(
            sampler.lock().unwrap().cpu_count().max(1),
        ));

        // 拖动允许标志：300ms 后置 true
        let drag_allowed = Arc::new(AtomicBool::new(false));

        let weak = ui.as_weak();
        let drag_flag = drag_allowed.clone();
        ui.on_drag_moved(move |dx, dy| {
            if !drag_flag.load(Ordering::SeqCst) {
                return;
            }
            use slint::PhysicalPosition;
            if let Some(ui) = weak.upgrade() {
                let window = ui.window();
                let scale = window.scale_factor();
                let pos = window.position();
                let new_x = pos.x + (dx as f32 * scale) as i32;
                let new_y = pos.y + (dy as f32 * scale) as i32;
                window.set_position(PhysicalPosition::new(new_x, new_y));
            }
        });

        // 关闭按钮 → 隐藏窗口
        let weak_close = ui.as_weak();
        ui.on_close_requested(move || {
            if let Some(ui) = weak_close.upgrade() {
                let _ = ui.hide();
            }
        });

        // 排序：点击列头 → 切换排序列 / 方向 + 用缓存重绘
        let sort_for_cb = sort.clone();
        let weak_sort_ui = ui.as_weak();
        let cache_sort = cache_tick.clone();
        let search_sort = search.clone();
        let expanded_sort = expanded.clone();
        let nb_cpus_sort = nb_cpus.clone();
        ui.on_sort_requested(move |column: slint::SharedString| {
            {
                let mut g = sort_for_cb.lock().unwrap();
                if g.0 == column.as_str() {
                    g.1 = !g.1; // 同列翻转方向
                } else {
                    g.0 = column.to_string();
                    g.1 = column.as_str() == "pid" || column.as_str() == "name";
                }
                log::info!("排序: {} {}", g.0, if g.1 { "升序" } else { "降序" });
            }
            if let Some(ui) = weak_sort_ui.upgrade() {
                render(&ui, &cache_sort, &sort_for_cb, &search_sort, &expanded_sort, *nb_cpus_sort.lock().unwrap());
            }
        });

        // 搜索框 → 保存关键字 + 立即用缓存重绘（不重采样）
        let search_for_cb = search.clone();
        let weak_search = ui.as_weak();
        let cache_search = cache_tick.clone();
        let sort_search = sort.clone();
        let expanded_search = expanded.clone();
        let nb_cpus_search = nb_cpus.clone();
        ui.on_search_changed(move |text: slint::SharedString| {
            *search_for_cb.lock().unwrap() = text.to_string();
            if let Some(ui) = weak_search.upgrade() {
                render(&ui, &cache_search, &sort_search, &search_for_cb, &expanded_search, *nb_cpus_search.lock().unwrap());
            }
        });

        // 刷新按钮 → 立即重采样 + 重绘
        let weak_refresh = ui.as_weak();
        let sampler_refresh = sampler.clone();
        let cache_refresh = cache_tick.clone();
        let sort_refresh = sort.clone();
        let search_refresh = search.clone();
        let expanded_refresh = expanded.clone();
        let nb_cpus_refresh = nb_cpus.clone();
        ui.on_refresh_requested(move || {
            if let Some(ui) = weak_refresh.upgrade() {
                let rows = {
                    let mut s = sampler_refresh.lock().unwrap();
                    s.sample()
                };
                *cache_refresh.lock().unwrap() = rows;
                render(
                    &ui,
                    &cache_refresh,
                    &sort_refresh,
                    &search_refresh,
                    &expanded_refresh,
                    *nb_cpus_refresh.lock().unwrap(),
                );
            }
        });

        // 行右键菜单 → 顶部标题 + 「打开文件所在的位置」/「停止进程」（原生菜单）
        // 弹出位置：show_row_menu 内部用 GetCursorPos() 取鼠标屏幕坐标，
        // 不依赖 Slint 坐标换算（避免 ListView 偏移/缓存导致菜单位置错位）。
        let weak_rowmenu = ui.as_weak();
        ui.on_row_context_menu(move |pid: i32, name: slint::SharedString| {
            if let Some(ui) = weak_rowmenu.upgrade() {
                match crate::window::show_row_menu(ui.window(), pid, name.as_str()) {
                    Some(crate::window::RowMenuCmd::Kill) => {
                        let ok = kill_process(pid as u32);
                        log::info!(
                            "停止进程 {} {}",
                            pid,
                            if ok { "成功" } else { "失败（权限不足或进程已退出）" }
                        );
                        // 下一次刷新自动更新
                    }
                    Some(crate::window::RowMenuCmd::OpenLocation) => {
                        match open_process_location(pid as u32) {
                            Ok(()) => log::info!("打开文件所在的位置 PID {} 成功", pid),
                            Err(e) => log::warn!("打开文件所在的位置 PID {} 失败: {}", pid, e),
                        }
                    }
                    None => {} // 用户取消（点空白 / Esc）
                }
            }
        });

        // 聚合父节点点击 → 展开/收起 + 用缓存重绘
        let expanded_for_cb = expanded.clone();
        let nb_cpus_expand = nb_cpus.clone();
        let weak_expand_ui = ui.as_weak();
        let cache_expand = cache_tick.clone();
        let sort_expand = sort.clone();
        let search_expand = search.clone();
        ui.on_group_toggle(move |key: slint::SharedString| {
            {
                let mut set = expanded_for_cb.lock().unwrap();
                let k = key.to_ascii_lowercase();
                if !set.insert(k.clone()) {
                    set.remove(&k);
                }
            }
            if let Some(ui) = weak_expand_ui.upgrade() {
                render(&ui, &cache_expand, &sort_expand, &search_expand, &expanded_for_cb, *nb_cpus_expand.lock().unwrap());
            }
        });

        // 右下角 resize 手柄 → set_size
        let weak_resize = ui.as_weak();
        ui.on_resize_requested(move |dx: f32, dy: f32| {
            if let Some(ui) = weak_resize.upgrade() {
                let window = ui.window();
                let scale = window.scale_factor();
                let size = window.size(); // 物理像素 u32
                let cur_w = size.width as f32 / scale;
                let cur_h = size.height as f32 / scale;
                let new_w = (cur_w + dx).clamp(500.0, 1200.0);
                let new_h = (cur_h + dy).clamp(300.0, 900.0);
                window.set_size(slint::LogicalSize::new(new_w, new_h));
            }
        });

        ui.show()?;
        // 进程详情页同样不出现在 Windows 系统任务栏（工具窗口样式）
        crate::window::ensure_tool_window_for(ui.window());
        // winit 在 show 后会重算扩展样式（覆盖 WS_EX_TOOLWINDOW / 加回
        // WS_EX_APPWINDOW），延迟 500ms 再补一次；长期由 Overlay 的
        // 1Hz tick 守护（lib.rs）。
        let weak_toolwin = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(500), move || {
            if let Some(ui) = weak_toolwin.upgrade() {
                crate::window::ensure_tool_window_for(ui.window());
            }
        });

        // 30s 自动刷新：重采样 + 重绘
        let weak_tick = ui.as_weak();
        let sampler_tick = sampler.clone();
        let sort_tick = sort.clone();
        let search_tick = search.clone();
        let expanded_tick = expanded.clone();
        let nb_cpus_tick = nb_cpus.clone();
        let tick_timer = slint::Timer::default();
        tick_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(30_000),
            move || {
                if let Some(ui) = weak_tick.upgrade() {
                    // 重采样更新缓存
                    {
                        let mut s = sampler_tick.lock().unwrap();
                        let rows = s.sample();
                        *cache_tick.lock().unwrap() = rows;
                    }
                    render(
                        &ui,
                        &cache_tick,
                        &sort_tick,
                        &search_tick,
                        &expanded_tick,
                        *nb_cpus_tick.lock().unwrap(),
                    );
                }
            },
        );

        let enable_flag = drag_allowed.clone();
        slint::Timer::single_shot(Duration::from_millis(300), move || {
            enable_flag.store(true, Ordering::SeqCst);
        });

        Ok(Self {
            ui,
            cache: Mutex::new(Vec::new()),
            nb_cpus,
            sampler,
            sort,
            search,
            expanded,
            _timer: tick_timer,
        })
    }

    /// 立即重采样 + 重绘（若窗口已关闭则先显示）。
    pub fn refresh(&self) {
        if !self.ui.window().is_visible() {
            let _ = self.ui.show();
            // show 可能触发 winit 重算样式 → 重新确保不在任务栏显示
            crate::window::ensure_tool_window_for(self.ui.window());
        }
        {
            let mut s = self.sampler.lock().unwrap();
            let rows = s.sample();
            *self.cache.lock().unwrap() = rows;
        }
        render(&self.ui, &self.cache, &self.sort, &self.search, &self.expanded, *self.nb_cpus.lock().unwrap());
    }

    /// 底层 Slint 窗口（供 tick 守护重新设置任务栏样式）。
    pub fn window(&self) -> &slint::Window {
        self.ui.window()
    }
}

/// 用缓存渲染列表（不采样）：过滤 + 分组 + 排序 + 展开 + 填充。
/// 搜索 / 展开 / 收起 / 排序都走这里 —— 只读缓存，毫秒级。
/// 30s 定时器和手动刷新先更新缓存再调这里。
fn render(
    ui: &crate::ProcessList,
    cache: &Mutex<Vec<ProcessRow>>,
    sort: &Mutex<(String, bool)>,
    search: &Mutex<String>,
    expanded: &Mutex<std::collections::HashSet<String>>,
    nb_cpus: usize,
) {
    let rows = cache.lock().unwrap().clone();

    // 服务映射（svchost 特殊聚合用）
    let services = service_map();

    // 分组 + 组排序
    let (column, ascending) = {
        let s = sort.lock().unwrap();
        (s.0.clone(), s.1)
    };
    let mut groups = group_processes(&rows, &services);
    sort_groups(&mut groups, &column, ascending);
    ui.set_sort_column(SharedString::from(column));
    ui.set_sort_ascending(ascending);

    // 搜索过滤（按名称 / PID；过滤发生在分组后，保留组结构）
    let keyword = search.lock().unwrap().clone();
    if !keyword.trim().is_empty() {
        groups = filter_groups(&groups, &keyword);
    }

    // 展开状态集合
    let expanded_set = expanded.lock().unwrap().clone();

    // 构建显示列表：父节点（聚合）+ 展开的子进程 / 服务
    let mut items: Vec<crate::ProcessRowData> = Vec::new();
    for g in &groups {
        if g.children.is_empty() && g.services.is_empty() {
            // 单实例：普通行
            items.push(row_display(&g.root, nb_cpus, false));
        } else {
            // 多实例 / 服务宿主：父节点
            let agg = group_aggregate(g);
            items.push(group_display(g, &agg, nb_cpus));
            if expanded_set.contains(&g.name.to_ascii_lowercase()) {
                // svchost：先展开服务列表
                if !g.services.is_empty() {
                    for svc in &g.services {
                        let full = format!("服务: {}", svc);
                        items.push(row_to_slint(
                            0,
                            format!("  ▶ {}", svc),
                            full,
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                            "服务".into(),
                            false,
                            0,
                        ));
                    }
                }
                // 再展开子进程
                for r in &g.children {
                    items.push(row_display(r, nb_cpus, true));
                }
            }
        }
    }
    let model = slint::VecModel::from(items);
    ui.set_process_model(slint::ModelRc::new(model));
}

/// 按关键字过滤聚合组（组内名称 / PID 匹配即保留；纯函数，可单测）。
fn filter_groups(groups: &[GroupedProcess], keyword: &str) -> Vec<GroupedProcess> {
    let kw = keyword.trim().to_ascii_lowercase();
    groups
        .iter()
        .filter(|g| {
            g.name.to_ascii_lowercase().contains(&kw)
                || g.root.pid.to_string().contains(&kw)
                || g.children.iter().any(|c| c.pid.to_string().contains(&kw))
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, name: &str, cpu: f32, mem_mb: u64) -> ProcessRow {
        ProcessRow {
            pid,
            parent_pid: 0,
            name: name.into(),
            cpu_usage: cpu,
            memory_bytes: mem_mb * 1024 * 1024,
            disk_read_bps: 0,
            disk_write_bps: 0,
            net_bps: 0,
            net_total_bytes: 0,
            status: "运行中".into(),
        }
    }

    #[test]
    fn rank_sorts_by_cpu_desc() {
        let mut rows = vec![
            row(1, "a", 10.0, 100),
            row(2, "b", 50.0, 200),
            row(3, "c", 30.0, 300),
        ];
        let ranked = rank_processes(&mut rows, 10);
        assert_eq!(ranked[0].pid, 2);
        assert_eq!(ranked[1].pid, 3);
        assert_eq!(ranked[2].pid, 1);
    }

    #[test]
    fn rank_truncates_to_limit() {
        let mut rows = vec![row(1, "a", 10.0, 100), row(2, "b", 50.0, 200)];
        let ranked = rank_processes(&mut rows, 1);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].pid, 2);
    }

    #[test]
    fn sort_by_pid_asc() {
        let mut rows = vec![row(3, "c", 10.0, 100), row(1, "a", 50.0, 200), row(2, "b", 30.0, 300)];
        sort_processes(&mut rows, "pid", true);
        assert_eq!(rows[0].pid, 1);
        assert_eq!(rows[2].pid, 3);
    }

    #[test]
    fn sort_by_pid_desc() {
        let mut rows = vec![row(3, "c", 10.0, 100), row(1, "a", 50.0, 200)];
        sort_processes(&mut rows, "pid", false);
        assert_eq!(rows[0].pid, 3);
    }

    #[test]
    fn sort_by_cpu_asc() {
        let mut rows = vec![row(1, "a", 50.0, 100), row(2, "b", 10.0, 200)];
        sort_processes(&mut rows, "cpu", true);
        assert_eq!(rows[0].pid, 2);
        assert_eq!(rows[1].pid, 1);
    }

    #[test]
    fn sort_by_name_case_insensitive() {
        let mut rows = vec![row(1, "Bob", 10.0, 100), row(2, "alice", 50.0, 200)];
        sort_processes(&mut rows, "name", true);
        assert_eq!(rows[0].name, "alice");
        assert_eq!(rows[1].name, "Bob");
    }

    #[test]
    fn sort_by_net_uses_net_bps() {
        let mut a = row(1, "a", 10.0, 100);
        a.net_bps = 1000;
        let mut b = row(2, "b", 10.0, 100);
        b.net_bps = 500;
        let mut rows = vec![a, b];
        sort_processes(&mut rows, "net", false);
        assert_eq!(rows[0].pid, 1); // 网络大 → 降序在前
    }

    #[test]
    fn filter_by_name_case_insensitive() {
        let rows = vec![
            row(1, "explorer.exe", 10.0, 100),
            row(2, "Code.exe", 50.0, 200),
            row(3, "chrome.exe", 30.0, 300),
        ];
        let f = filter_processes(&rows, "CODE");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].pid, 2);
    }

    #[test]
    fn filter_by_pid() {
        let rows = vec![row(1234, "a", 10.0, 100), row(5678, "b", 50.0, 200)];
        let f = filter_processes(&rows, "567");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].pid, 5678);
    }

    #[test]
    fn filter_empty_keyword_returns_all() {
        let rows = vec![row(1, "a", 10.0, 100), row(2, "b", 50.0, 200)];
        assert_eq!(filter_processes(&rows, "").len(), 2);
    }

    #[test]
    fn format_status_chinese() {
        use sysinfo::ProcessStatus as S;
        assert_eq!(format_status(S::Run), "运行中");
        assert_eq!(format_status(S::Sleep), "睡眠");
        assert_eq!(format_status(S::Stop), "已停止");
    }

    #[test]
    fn format_rate_units() {
        assert_eq!(format_bytes(0), "0B");
        assert_eq!(format_bytes(2048), "2.0K");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0M");
    }

    #[test]
    fn format_rate_units_auto_upgrade_to_gb_tb() {
        // 累计网络 / 磁盘等大字节场景：>= 1GB 升 G，>= 1TB 升 T
        let gb = 1024_u64 * 1024 * 1024;
        let tb = gb * 1024;
        assert_eq!(format_bytes(gb + 512 * 1024 * 1024), "1.5G");
        assert_eq!(format_bytes(900 * gb), "900.0G");
        assert_eq!(format_bytes(tb), "1.00T");
        assert_eq!(format_bytes(2 * tb + 100 * gb), "2.10T");
    }

    #[test]
    fn format_disk_short_rates() {
        let mut r = row(1, "a", 10.0, 100);
        r.disk_read_bps = 1024 * 1024;
        r.disk_write_bps = 2048;
        assert_eq!(format_disk_short(&r), "R 1.0M/s W 2.0K/s");
    }

    #[test]
    fn format_net_short_rate() {
        let mut r = row(1, "a", 10.0, 100);
        r.net_bps = 5 * 1024 * 1024;
        assert_eq!(format_net_short(&r), "5.0M/s");
    }

    #[test]
    fn rate_computes_delta_over_time() {
        assert_eq!(rate(1000, 3000, 1.0), 2000);
        assert_eq!(rate(1000, 500, 1.0), 0); // 回退（计数器重置）→ 0
        assert_eq!(rate(0, 1024, 0.5), 2048);
    }

    #[test]
    fn sort_state_toggle_semantics() {
        // 验证同列翻转 / 换列默认方向的逻辑（与回调一致）
        let mut g = ("cpu".to_string(), false);
        if g.0 == "cpu" { g.1 = !g.1; }
        assert!(g.1);
        let col = "mem";
        if g.0 == col { g.1 = !g.1; } else { g.0 = col.to_string(); g.1 = false; }
        assert_eq!(g.0, "mem");
        assert!(!g.1);
    }

    // ===== 进程聚合（任务管理器式树状分组）=====

    fn no_services() -> std::collections::HashMap<u32, Vec<String>> {
        Default::default()
    }

    /// 构造指定 parent 的进程行
    fn row_p(pid: u32, parent: u32, name: &str, cpu: f32, mem_mb: u64) -> ProcessRow {
        let mut r = row(pid, name, cpu, mem_mb);
        r.parent_pid = parent;
        r
    }

    #[test]
    fn group_same_name_single_root() {
        // 两个 chrome.exe：PID 100 的 PPID 是 explorer(不在组内) → 主进程；
        // PID 200 的 PPID 是 100（组内）→ 塌陷为子进程
        let rows = vec![
            row_p(100, 500, "chrome.exe", 10.0, 100), // 500 = explorer（外部）
            row_p(200, 100, "chrome.exe", 20.0, 200), // PPID 在组内
            row_p(300, 0, "explorer.exe", 5.0, 50),
        ];
        let groups = group_processes(&rows, &no_services());
        assert_eq!(groups.len(), 2);
        let chrome = groups.iter().find(|g| g.name == "chrome.exe").unwrap();
        assert_eq!(chrome.root.pid, 100); // 主进程 = PPID 不在组内者
        assert_eq!(chrome.children.len(), 1);
        assert_eq!(chrome.children[0].pid, 200);
        assert_eq!(chrome.total_count(), 2);
    }

    #[test]
    fn group_case_insensitive() {
        let rows = vec![
            row_p(1, 0, "Code.exe", 10.0, 100),
            row_p(2, 1, "code.EXE", 20.0, 200),
        ];
        let groups = group_processes(&rows, &no_services());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].total_count(), 2);
    }

    #[test]
    fn group_all_internal_parent_falls_back_first() {
        // 罕见环：所有 PPID 都在组内 → 取第一个为主进程
        let rows = vec![
            row_p(1, 2, "a.exe", 10.0, 100),
            row_p(2, 1, "a.exe", 30.0, 300),
        ];
        let groups = group_processes(&rows, &no_services());
        assert_eq!(groups[0].root.pid, 1);
        assert_eq!(groups[0].children.len(), 1);
    }

    #[test]
    fn group_aggregate_sums_fields() {
        let rows = vec![
            row_p(1, 0, "a.exe", 10.0, 100),
            row_p(2, 1, "a.exe", 30.0, 300),
        ];
        let groups = group_processes(&rows, &no_services());
        let agg = group_aggregate(&groups[0]);
        assert_eq!(agg.cpu_usage, 40.0);
        assert_eq!(agg.memory_bytes, 400 * 1024 * 1024);
        assert_eq!(agg.pid, 1); // 取最小 PID
    }

    #[test]
    fn group_aggregate_rates_and_total() {
        let mut a = row_p(1, 0, "a.exe", 10.0, 100);
        a.disk_read_bps = 1000;
        a.net_bps = 500;
        a.net_total_bytes = 10000;
        let mut b = row_p(2, 1, "a.exe", 10.0, 100);
        b.disk_read_bps = 2000;
        b.net_bps = 1500;
        b.net_total_bytes = 20000;
        let groups = group_processes(&[a, b], &no_services());
        let agg = group_aggregate(&groups[0]);
        assert_eq!(agg.disk_read_bps, 3000);
        assert_eq!(agg.net_bps, 2000);
        assert_eq!(agg.net_total_bytes, 30000);
    }

    #[test]
    fn svchost_gets_service_names() {
        // svchost 绑定多个服务 → services 字段填充（显示层改写为「服务宿主」）
        let rows = vec![row_p(123, 4, "svchost.exe", 1.0, 10)];
        let mut services: std::collections::HashMap<u32, Vec<String>> = Default::default();
        services.insert(123, vec!["Windows Update".into(), "BITS".into()]);
        let groups = group_processes(&rows, &services);
        assert_eq!(groups[0].services.len(), 2);
        assert_eq!(groups[0].services[0], "Windows Update");
    }

    #[test]
    fn svchost_without_services_stays_group() {
        let rows = vec![
            row_p(1, 4, "svchost.exe", 1.0, 10),
            row_p(2, 1, "svchost.exe", 2.0, 20),
        ];
        let groups = group_processes(&rows, &no_services());
        assert!(groups[0].services.is_empty());
        assert_eq!(groups[0].total_count(), 2); // 仍按进程组折叠
    }

    #[test]
    fn sort_groups_by_cpu_desc() {
        let rows = vec![
            row(1, "a.exe", 10.0, 100),
            row(2, "b.exe", 50.0, 200),
            row(3, "c.exe", 30.0, 300),
        ];
        let mut groups = group_processes(&rows, &no_services());
        sort_groups(&mut groups, "cpu", false);
        assert_eq!(groups[0].name, "b.exe");
        assert_eq!(groups[2].name, "a.exe");
    }

    #[test]
    fn sort_groups_by_name_asc() {
        let rows = vec![row(1, "b.exe", 10.0, 100), row(2, "a.exe", 50.0, 200)];
        let mut groups = group_processes(&rows, &no_services());
        sort_groups(&mut groups, "name", true);
        assert_eq!(groups[0].name, "a.exe");
    }

    #[test]
    fn sort_groups_by_mem() {
        let rows = vec![
            row(1, "a.exe", 10.0, 100),
            row(2, "b.exe", 10.0, 900),
            row(3, "c.exe", 10.0, 500),
        ];
        let mut groups = group_processes(&rows, &no_services());
        sort_groups(&mut groups, "mem", false);
        assert_eq!(groups[0].name, "b.exe"); // 内存大 → 降序在前
        assert_eq!(groups[2].name, "a.exe");
    }

    #[test]
    fn sort_groups_by_disk() {
        let mut a = row(1, "a.exe", 10.0, 100);
        a.disk_read_bps = 1000;
        let mut b = row(2, "b.exe", 10.0, 100);
        b.disk_write_bps = 5000;
        let mut groups = group_processes(&[a, b], &no_services());
        sort_groups(&mut groups, "disk", false);
        assert_eq!(groups[0].name, "b.exe"); // 磁盘 R+W 大 → 降序在前
    }

    #[test]
    fn sort_groups_by_net() {
        let mut a = row(1, "a.exe", 10.0, 100);
        a.net_bps = 3000;
        let mut b = row(2, "b.exe", 10.0, 100);
        b.net_bps = 1000;
        let mut groups = group_processes(&[a, b], &no_services());
        sort_groups(&mut groups, "net", false);
        assert_eq!(groups[0].name, "a.exe"); // 网络速率大 → 降序在前
    }

    #[test]
    fn sort_groups_by_pid() {
        let rows = vec![row(300, "a.exe", 10.0, 100), row(100, "b.exe", 10.0, 100)];
        let mut groups = group_processes(&rows, &no_services());
        sort_groups(&mut groups, "pid", true);
        assert_eq!(groups[0].name, "b.exe"); // PID 100 升序在前
    }

    #[test]
    fn sort_groups_by_nettotal() {
        let mut a = row(1, "a.exe", 10.0, 100);
        a.net_total_bytes = 5000;
        let mut b = row(2, "b.exe", 10.0, 100);
        b.net_total_bytes = 9000;
        let mut groups = group_processes(&[a, b], &no_services());
        sort_groups(&mut groups, "nettotal", false);
        assert_eq!(groups[0].name, "b.exe"); // 累计网络大 → 降序在前
    }

    // ===== 组过滤（搜索在分组后，保留组结构）=====

    fn build_chrome_group() -> GroupedProcess {
        GroupedProcess {
            name: "chrome.exe".into(),
            root: row_p(100, 500, "chrome.exe", 10.0, 100),
            children: vec![row_p(200, 100, "chrome.exe", 20.0, 200)],
            services: vec![],
        }
    }

    #[test]
    fn filter_groups_by_name() {
        let mut g = build_chrome_group();
        g.children.push(row_p(300, 100, "chrome.exe", 5.0, 50));
        let groups = vec![g, GroupedProcess {
            name: "explorer.exe".into(),
            root: row_p(400, 0, "explorer.exe", 1.0, 10),
            children: vec![],
            services: vec![],
        }];
        let f = filter_groups(&groups, "chrome");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].name, "chrome.exe");
    }

    #[test]
    fn filter_groups_by_child_pid() {
        let g = build_chrome_group();
        let groups = vec![g];
        let f = filter_groups(&groups, "200"); // 子进程 PID
        assert_eq!(f.len(), 1);
    }

    #[test]
    fn filter_groups_empty_keyword_all() {
        let groups = vec![build_chrome_group()];
        assert_eq!(filter_groups(&groups, "").len(), 1);
        assert_eq!(filter_groups(&groups, "  ").len(), 1);
    }

    #[test]
    fn filter_groups_no_match_empty() {
        let groups = vec![build_chrome_group()];
        assert!(filter_groups(&groups, "zzz").is_empty());
    }

    #[test]
    fn sort_groups_by_status() {
        // status 按名称排序（纯字符串比较）
        let rows = vec![row(1, "zzz.exe", 10.0, 100), row(2, "aaa.exe", 50.0, 200)];
        let mut groups = group_processes(&rows, &no_services());
        sort_groups(&mut groups, "status", true);
        assert_eq!(groups[0].name, "aaa.exe");
    }
}
