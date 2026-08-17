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
    /// 进程名（exe 文件名，如 `chrome.exe`；用于分组/详情，稳定不变）
    pub name: String,
    /// 展示名（友好名：UWP 包显示名或 exe 的 `FileDescription`；
    /// 取不到时回退 exe 名）。列表/聚合行优先显示它，原始 exe 名作为悬浮/备用。
    pub display_name: String,
    /// CPU 使用率（0.0 ~ 100.0×核数；显示前归一化）
    pub cpu_usage: f32,
    /// 内存字节数（提交大小 Commit Size = `PagefileUsage`，与任务管理器
    /// 「详细信息」页「内存」列口径一致；取不到时回退工作集）
    pub memory_bytes: u64,
    /// 物理内存字节数（专用工作集 Private Working Set =
    /// `PROCESS_MEMORY_COUNTERS_EX2.PrivateWorkingSetSize`，不含与其他进程
    /// 共享的物理页；与任务管理器「进程」页「内存」列口径一致）。
    /// 「物理内存」列用；`memory_bytes` 是提交大小，两者并存展示。
    pub physical_mem_bytes: u64,
    /// 内存占用百分比（相对全机物理内存；高亮判断用）
    pub memory_pct: f32,
    /// 磁盘读速率（B/s）
    pub disk_read_bps: u64,
    /// 磁盘写速率（B/s）
    pub disk_write_bps: u64,
    /// 网络等非磁盘 I/O 速率（B/s）
    pub net_bps: u64,
    /// 累计网络等非磁盘 I/O 字节（OtherTransferCount 当前值）
    pub net_total_bytes: u64,
    /// 监听 TCP 端口 + UDP 端口（去重 + 排序后的端口号列表）。
    /// 空 = 无端口。
    pub ports: Vec<u16>,
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
    /// 全机物理内存（字节；sample() 时更新，聚合行内存占比用）
    total_mem: u64,
    /// 上次采样的 pid → 端口列表（sample() 期间重新枚举；
    /// 用于进程行的「端口」列展示；缓存避免每帧重复 GetExtendedTcpTable）
    port_cache: HashMap<u32, Vec<u16>>,
    /// pid → 展示名（友好名）缓存：避免每 tick 对每个进程重复读 exe 版本信息。
    /// 值同时记录来源 exe 名：每次 sample 后按当前存在的 pid 裁剪，且 exe 名
    /// 不匹配（同周期内 pid 被其他进程复用）时视为失效重查——单独裁剪防不住
    /// 「旧进程退出、新进程立刻复用同一 pid」的串味。
    name_cache: HashMap<u32, (String, String)>,
}

#[derive(Debug, Clone, Copy)]
struct IoSnapshot {
    read: u64,
    write: u64,
    other: u64,
}

/// Chromium 家族进程（WebView2 / Edge / Chrome）的角色，取自命令行 --type 开关。
///
/// 与任务管理器「进程」页的显示口径一致：Chromium 系所有进程都叫同一个 exe
/// （msedgewebview2.exe / msedge.exe / chrome.exe），靠命令行参数区分角色；
/// 内嵌的 WebView2 再沿父进程链标注宿主应用（如「WebView2: MarkFlowy」）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChromiumRole {
    /// 浏览器主进程（无 --type 或 --type=browser）
    Browser,
    /// 网页渲染进程（--type=renderer）
    Renderer,
    /// GPU 进程（--type=gpu-process）
    Gpu,
    /// 实用工具进程（--type=utility）；String 为可读子类型名
    /// （如 "Network Service"），空串表示命令行未给出 --utility-sub-type
    Utility(String),
    /// 崩溃处理（--type=crashpad-handler）
    Crashpad,
    /// 其他未知 --type（原样保留）
    Other(String),
}

/// 进程名 → Chromium 家族展示前缀（WebView2 / Edge / Chrome）。
/// 返回 None 表示非 Chromium 家族进程。
fn chromium_family_prefix(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "msedgewebview2.exe" => Some("WebView2"),
        "msedge.exe" => Some("Edge"),
        "chrome.exe" => Some("Chrome"),
        _ => None,
    }
}

/// 是否属于 Chromium 家族进程（宿主链判断用：同家族进程沿父链向上归到宿主应用）。
fn is_chromium_family(name: &str) -> bool {
    chromium_family_prefix(name).is_some()
}

/// 解析 Chromium 进程命令行 → 角色。
fn chromium_role_from_args(args: &[std::ffi::OsString]) -> ChromiumRole {
    let mut type_val: Option<String> = None;
    let mut sub: Option<String> = None;
    for a in args {
        let s = a.to_string_lossy();
        if let Some(v) = s.strip_prefix("--type=") {
            type_val = Some(v.to_string());
        } else if let Some(v) = s.strip_prefix("--utility-sub-type=") {
            sub = Some(v.to_string());
        }
    }
    match type_val.as_deref() {
        None | Some("browser") => ChromiumRole::Browser,
        Some("renderer") => ChromiumRole::Renderer,
        Some("gpu-process") => ChromiumRole::Gpu,
        Some("crashpad-handler") => ChromiumRole::Crashpad,
        Some("utility") => ChromiumRole::Utility(
            sub.as_deref().map(utility_subtype_label).unwrap_or_default(),
        ),
        Some(other) => ChromiumRole::Other(other.to_string()),
    }
}

/// utility 子类型（如 network.mojom.NetworkService）→ 可读名，
/// 对齐任务管理器「WebView2 实用工具: Network Service」的显示。
fn utility_subtype_label(raw: &str) -> String {
    if raw.contains("NetworkService") {
        "Network Service".into()
    } else if raw.contains("StorageService") {
        "Storage Service".into()
    } else if raw.contains("AudioService") {
        "Audio Service".into()
    } else if raw.contains("VideoCapture") {
        "Video Capture".into()
    } else if raw.contains("PdfService") {
        "PDF Service".into()
    } else if raw.contains("ScreenCapture") {
        "Screen Capture".into()
    } else {
        // 兜底：取最后一个 '.' 之后的段（mojom.XxxService → XxxService）
        raw.rsplit('.').next().unwrap_or(raw).to_string()
    }
}

/// 沿父链向上找 Chromium 家族进程的宿主应用：
/// 一直走到第一个非 Chromium 家族的进程（对 WebView2 来说就是宿主应用，如 MarkFlowy）。
/// 父链断裂 / 环 / 找不到宿主时返回 None。
fn host_display_name(pid: u32, rows: &[ProcessRow], idx: &HashMap<u32, usize>) -> Option<String> {
    let mut cur = pid;
    for _ in 0..16 {
        let row = idx.get(&cur).map(|&i| &rows[i])?;
        if !is_chromium_family(&row.name) {
            let d = row.display_name.trim();
            return Some(if d.is_empty() { row.name.clone() } else { d.to_string() });
        }
        let parent = row.parent_pid;
        if parent == 0 || parent == cur {
            return None;
        }
        cur = parent;
    }
    None
}

/// 按 Chromium 角色改写展示名（任务管理器风格）。
/// 在 display_name 已填充友好名之后调用；roles 是采样时收集的 pid → 角色。
fn annotate_chromium_rows(rows: &mut [ProcessRow], roles: &HashMap<u32, ChromiumRole>) {
    let idx: HashMap<u32, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| (r.pid, i))
        .collect();
    for i in 0..rows.len() {
        let role = roles.get(&rows[i].pid);
        if role.is_none() { continue; }
        let role = role.unwrap();
        let prefix = chromium_family_prefix(&rows[i].name);
        if prefix.is_none() { continue; }
        let prefix = prefix.unwrap();
        rows[i].display_name = match role {
            // 浏览器主进程：
            // - WebView2（内嵌）→ 标注宿主「WebView2: MarkFlowy」
            // - Edge / Chrome（独立应用）→ 保留应用名（FileDescription：Microsoft Edge / Google Chrome）
            ChromiumRole::Browser if prefix == "WebView2" => match host_display_name(rows[i].pid, rows, &idx) {
                Some(host) => format!("{}: {}", prefix, host),
                None => prefix.to_string(),
            },
            ChromiumRole::Browser => rows[i].display_name.clone(),
            ChromiumRole::Renderer => format!("{} 渲染进程", prefix),
            ChromiumRole::Gpu => format!("{} GPU 进程", prefix),
            ChromiumRole::Utility(sub) if !sub.is_empty() => format!("{} 实用工具: {}", prefix, sub),
            ChromiumRole::Utility(_) => format!("{} 实用工具", prefix),
            ChromiumRole::Crashpad => format!("{} 崩溃处理", prefix),
            ChromiumRole::Other(t) => format!("{} {}", prefix, t),
        };
    }
}

/// 沿父链向上找 WebView2 进程的宿主 PID：
/// 一直走到第一个不在 wv_pids 集合中的父进程（即宿主应用）。
/// 父链断裂 / 环 / 找不到时返回 None。
/// `idx`：pid → 行索引（调用方预建；逐层线性 find 会放大成 O(组内进程×链长×总进程数)）。
fn webview2_host_pid(
    pid: u32,
    idx: &std::collections::HashMap<u32, &ProcessRow>,
    wv_pids: &std::collections::HashSet<u32>,
) -> Option<u32> {
    let mut cur = pid;
    for _ in 0..16 {
        let row = idx.get(&cur).copied()?;
        let parent = row.parent_pid;
        if parent == 0 || parent == cur {
            return None;
        }
        if !wv_pids.contains(&parent) {
            return Some(parent);
        }
        cur = parent;
    }
    None
}

/// WebView2 分组：按宿主分组——同一宿主的全部 msedgewebview2.exe 进程
/// （含多个浏览器实例）合并为一个组。宿主 = 父链上第一个非 WebView2 进程；
/// 取不到宿主（父链断裂/进程已退出）的归入一个兜底组。
/// root 优先选浏览器进程（父进程不在 WebView2 集合内），保证组标题能带上宿主。
fn push_webview2_groups(
    group_rows: &[ProcessRow],
    all_rows: &[ProcessRow],
    out: &mut Vec<GroupedProcess>,
) {
    let wv_pids: std::collections::HashSet<u32> = all_rows
        .iter()
        .filter(|r| r.name.eq_ignore_ascii_case("msedgewebview2.exe"))
        .map(|r| r.pid)
        .collect();
    // pid → 行索引：宿主查找沿父链逐级跳转，预建索引避免逐层线性扫描
    let idx: std::collections::HashMap<u32, &ProcessRow> =
        all_rows.iter().map(|r| (r.pid, r)).collect();

    let mut by_host: std::collections::BTreeMap<u32, Vec<&ProcessRow>> = Default::default();
    let mut no_host: Vec<&ProcessRow> = Vec::new();
    for r in group_rows {
        match webview2_host_pid(r.pid, &idx, &wv_pids) {
            Some(host) => by_host.entry(host).or_default().push(r),
            None => no_host.push(r),
        }
    }

    let mut host_groups: Vec<Vec<&ProcessRow>> = by_host.into_values().collect();
    if !no_host.is_empty() {
        host_groups.push(no_host);
    }

    for mut members in host_groups {
        // 组内按 PID 稳定排序；root 优先选浏览器进程（父不在 WebView2 集合内）
        members.sort_by_key(|r| r.pid);
        let root_idx = members
            .iter()
            .position(|r| !wv_pids.contains(&r.parent_pid))
            .unwrap_or(0);
        let root = members[root_idx].clone();
        let children: Vec<ProcessRow> = members
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != root_idx)
            .map(|(_, r)| (*r).clone())
            .collect();
        out.push(GroupedProcess {
            name: root.name.clone(),
            root,
            children,
            services: Default::default(),
        });
    }
}

impl ProcessSampler {
    /// 创建采样器并预热（第一次 refresh 建立 CPU 基线）。
    ///
    /// 只刷新本页所需（进程 + CPU + 内存）：`new_all`/`refresh_all` 还会枚举
    /// 磁盘 / 网络接口 / 硬件组件，而 new() 在 UI 线程（右键菜单回调）执行，
    /// 全量枚举会让窗口打开瞬间冻结数百毫秒。
    pub fn new() -> Self {
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        sys.refresh_cpu_all();
        sys.refresh_memory();
        Self {
            total_mem: sys.total_memory().max(1),
            sys,
            prev_io: HashMap::new(),
            prev_time: None,
            port_cache: HashMap::new(),
            name_cache: HashMap::new(),
        }
    }

    /// 采集一次快照（全量，不截断）。
    /// 第一次调用速率列为 0（无基线）。
    /// 注意：不在采样阶段截断——先完整聚合（任务管理器行为），
    /// 显示阶段再按聚合后的行数限制，避免「同组部分实例被截掉、
    /// 剩下的看起来像独立进程」。
    pub fn sample(&mut self) -> Vec<ProcessRow> {
        // 精确刷新（进程 + CPU + 内存）：refresh_all 每轮还会枚举磁盘 /
        // 网络接口 / 组件，本页不用，纯浪费（每 tick 都付一次）
        self.sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        self.sys.refresh_cpu_all();
        self.sys.refresh_memory();

        let now = Instant::now();
        let dt = self.prev_time.map(|t| now.duration_since(t).as_secs_f64());

        // 全机物理内存（字节）——内存占比/高亮判断用
        self.total_mem = self.sys.total_memory().max(1);
        let total_mem = self.total_mem;

        // 端口快照：每轮重新枚举（监听 TCP + 全部 UDP）；
        // 系统级调用几十毫秒，进程列表刷新间隔 ≥ 500ms，开销可接受。
        self.port_cache = port_map();

        let mut rows: Vec<ProcessRow> = Vec::with_capacity(self.sys.processes().len());
        let mut next_io: HashMap<u32, IoSnapshot> = HashMap::new();
        // Chromium 家族（WebView2/Edge/Chrome）pid → 角色（命令行 --type 解析，供展示名标注）
        let mut roles: HashMap<u32, ChromiumRole> = HashMap::new();

        for (pid, p) in self.sys.processes() {
            let pid_u32 = pid.as_u32();
            // 一次句柄取 IO 计数器 + 内存明细（热路径合并，见函数注释）
            let (cur, detail) = process_io_and_memory(pid_u32);

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

            // Chromium 家族（WebView2/Edge/Chrome）角色：命令行取不到
            // （权限不足/进程已退出）时跳过，该进程保持原展示名不做标注
            let exe_name = p.name().to_string_lossy();
            if is_chromium_family(&exe_name) && !p.cmd().is_empty() {
                roles.insert(pid_u32, chromium_role_from_args(p.cmd()));
            }

            // 内存双口径（任务管理器对齐，一次 GetProcessMemoryInfo 取全）：
            // - 提交大小（Commit Size = PagefileUsage）→「详细信息」页「内存」列
            // - 专用工作集（Private Working Set = PrivateWorkingSetSize）→「进程」页「内存」列
            // 取不到（权限不足 / 进程已退出）时回退 sysinfo 的工作集，避免显示 0。
            let ws = p.memory();
            let mem = detail.map_or(ws, |d| d.0); // 提交大小
            let pws = detail.map_or(ws, |d| d.1); // 专用工作集
            // 监听/UDP 端口：port_map 里有就取，没有就是空（绝大多数进程无端口）
            let ports = self.port_cache.get(&pid_u32).cloned().unwrap_or_default();
            rows.push(ProcessRow {
                pid: pid_u32,
                parent_pid: p.parent().map(|x| x.as_u32()).unwrap_or(0),
                name: p.name().to_string_lossy().into_owned(),
                // 展示名（友好名）在循环结束后统一计算，避免与 self.sys 借用冲突
                display_name: String::new(),
                cpu_usage: p.cpu_usage(),
                memory_bytes: mem,
                physical_mem_bytes: pws,
                memory_pct: mem_pct(mem, total_mem),
                disk_read_bps: read_bps,
                disk_write_bps: write_bps,
                net_bps,
                net_total_bytes: cur.other,
                ports,
                status: format_status(p.status()),
            });
        }

        self.prev_io = next_io;
        self.prev_time = Some(now);

        // 统一填充展示名（友好名）；循环内不调用 &mut self 方法，避免借用冲突
        for r in &mut rows {
            r.display_name = self.cached_display_name(r.pid, &r.name);
        }

        // Chromium 家族标注（任务管理器风格）：
        // - WebView2 浏览器进程显示宿主「WebView2: MarkFlowy」
        // - Edge/Chrome/WebView2 子进程显示角色「WebView2 实用工具: Network Service」等
        if !roles.is_empty() {
            annotate_chromium_rows(&mut rows, &roles);
        }

        // 裁剪展示名缓存：仅保留当前存在的 pid（防内存增长 / pid 复用串味）
        let present: std::collections::HashSet<u32> =
            rows.iter().map(|r| r.pid).collect();
        self.name_cache.retain(|pid, _| present.contains(pid));

        rows
    }

    /// 取进程展示名（友好名），按 pid 缓存（带 exe 名校验防 pid 复用串味）。
    /// 优先用 exe 的 `FileDescription`（如 "Google Chrome"）；取不到回退 exe 名。
    /// 与自由函数 `friendly_name_for`（真正读版本信息）区分：本方法只负责缓存。
    fn cached_display_name(&mut self, pid: u32, exe_name: &str) -> String {
        if let Some((cached_exe, cached)) = self.name_cache.get(&pid) {
            if cached_exe == exe_name {
                return cached.clone();
            }
            // pid 被不同 exe 的进程复用 → 缓存失效，落入下方重查
        }
        let display = friendly_name_for(pid).unwrap_or_else(|| exe_name.to_string());
        self.name_cache
            .insert(pid, (exe_name.to_string(), display.clone()));
        display
    }

    /// 采样时缓存的 pid → 端口列表（聚合行的端口合并、详情面板的端口列表用）。
    /// 返回空 vec 表示该进程无监听端口；不返回 None。
    pub fn ports_of(&self, pid: u32) -> Vec<u16> {
        self.port_cache.get(&pid).cloned().unwrap_or_default()
    }

    /// 逻辑 CPU 核数（用于 CPU 归一化）
    pub fn cpu_count(&self) -> usize {
        self.sys.cpus().len()
    }

    /// 全机物理内存（字节；sample() 后是最新值）。
    pub fn total_memory(&self) -> u64 {
        self.total_mem
    }
}

/// 速率：差值 ÷ 时间（B/s）
fn rate(prev: u64, cur: u64, dt: f64) -> u64 {
    (cur.saturating_sub(prev) as f64 / dt) as u64
}

/// 内存占比（%）：字节数 ÷ 全机物理内存。单行 / 聚合行共用。
fn mem_pct(bytes: u64, total_bytes: u64) -> f32 {
    (bytes as f64 / total_bytes.max(1) as f64 * 100.0) as f32
}

/// 一个聚合组：树状两级结构（主进程 → 子进程）。
///
/// 任务管理器折叠效果的聚合规则：
/// 1. **同名归类**：Name 相同的进程归为一组
/// 2. **找主进程（Root）**：组内 PPID 不属于本组（被其他应用 / explorer
///    拉起）的进程即主进程；**同名多 root 时每个 root 生成独立组**
/// 3. **子进程按 PPID 归属**：组内其余进程沿 PPID 链向上，挂到对应 root 下
///    （孙进程也扁平归入该 root 的 children）
/// 4. **svchost.exe 特殊处理**：不显示 "svchost.exe"，而是查该 PID 绑定的
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

/// 聚合组的展开键（小写 exe 名 + root pid，如 `chrome.exe:100`）。
///
/// 只用 exe 名会导致**同名多组联动展开**（同名多 root 组、WebView2 按宿主
/// 分组都会产生同名组）；带上 root pid 后每组独立。代价是组内 root 进程
/// 重启（pid 变化）后展开状态丢失——与任务管理器行为一致，可接受。
fn group_key(g: &GroupedProcess) -> String {
    format!("{}:{}", g.name.to_ascii_lowercase(), g.root.pid)
}

/// 按「同名 + PPID 父子关系」聚合（纯函数，可单测）。
/// `services`：pid → 服务名（svchost 特殊聚合用；可为空 Map）。
///
/// 同名多 root（如多个被各自父进程拉起的 chrome）时，每个 root 生成独立
/// 聚合组；子进程沿 PPID 链归属到对应 root（孙进程扁平化进 children）。
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
    for (key, group_rows) in by_name {
        // WebView2（msedgewebview2.exe）：按宿主分组——同一宿主的全部
        // WebView2 进程（含多个浏览器实例）合并为一个组；宿主 = 父链上第一个
        // 非 WebView2 进程，取不到时归入一个兜底组。其他进程仍走同名+PPID。
        if key == "msedgewebview2.exe" {
            push_webview2_groups(&group_rows, rows, &mut out);
            continue;
        }
        let pids: std::collections::HashSet<u32> =
            group_rows.iter().map(|r| r.pid).collect();
        let pid_to_row: std::collections::HashMap<u32, &ProcessRow> =
            group_rows.iter().map(|r| (r.pid, r)).collect();

        // 2) 找所有 root：PPID 不在本组 PID 集合中（被外部进程拉起的顶层进程）
        let roots: Vec<&ProcessRow> = group_rows
            .iter()
            .filter(|r| !pids.contains(&r.parent_pid))
            .collect();

        if roots.is_empty() {
            // 罕见环（所有 PPID 都在组内）：取第一个为主进程，其余全为子节点
            let root = group_rows[0].clone();
            out.push(GroupedProcess {
                name: root.name.clone(),
                root,
                children: group_rows[1..].to_vec(),
                services: services.get(&group_rows[0].pid).cloned().unwrap_or_default(),
            });
            continue;
        }

        // 3) 每个 root 生成独立组；其余进程沿 PPID 链归属到对应 root
        let root_pids: std::collections::HashSet<u32> =
            roots.iter().map(|r| r.pid).collect();
        let root_index: std::collections::HashMap<u32, usize> = roots
            .iter()
            .enumerate()
            .map(|(i, r)| (r.pid, i))
            .collect();
        let mut groups: Vec<(ProcessRow, Vec<ProcessRow>)> = roots
            .iter()
            .map(|r| ((*r).clone(), Vec::new()))
            .collect();
        for r in &group_rows {
            if root_index.contains_key(&r.pid) {
                continue; // root 已建组
            }
            // 沿 PPID 链找所属 root；环/异常保底挂第一个 root
            let root_pid = find_root_pid(r.pid, &pid_to_row, &root_pids)
                .unwrap_or(roots[0].pid);
            let idx = root_index.get(&root_pid).copied().unwrap_or(0);
            groups[idx].1.push(r.clone());
        }
        for (root, children) in groups {
            let svc = services.get(&root.pid).cloned().unwrap_or_default();
            out.push(GroupedProcess {
                name: root.name.clone(),
                root,
                children,
                services: svc,
            });
        }
    }
    out
}

/// 沿 PPID 链向上找进程所属的 root pid（父链在组内走到 root）。
/// 环 / 父链断裂（父不在组内且非 root）时返回 None，由调用方保底。
fn find_root_pid(
    pid: u32,
    pid_to_row: &std::collections::HashMap<u32, &ProcessRow>,
    root_pids: &std::collections::HashSet<u32>,
) -> Option<u32> {
    let mut cur = pid;
    let mut visited: std::collections::HashSet<u32> = std::collections::HashSet::new();
    loop {
        if root_pids.contains(&cur) {
            return Some(cur);
        }
        if !visited.insert(cur) {
            return None; // 环
        }
        let next = pid_to_row.get(&cur).map(|r| r.parent_pid)?;
        if next == cur {
            return None; // 自父（异常）
        }
        cur = next;
    }
}

/// 聚合组 → 汇总行（主进程 + 子进程求和的聚合值）。
///
/// 注意 `pid` 保持 root 的 pid（不取组内最小值）：组行的右键「结束进程树」
/// 沿 parent_pid 枚举 root 的子树，若被 pid 回绕后更小的子进程顶替，
/// 枚举出的就不是该组的主树了。
pub fn group_aggregate(g: &GroupedProcess) -> ProcessRow {
    let mut acc = g.root.clone();
    for r in &g.children {
        acc.cpu_usage += r.cpu_usage;
        acc.memory_bytes += r.memory_bytes;
        acc.physical_mem_bytes += r.physical_mem_bytes;
        acc.disk_read_bps = acc.disk_read_bps.saturating_add(r.disk_read_bps);
        acc.disk_write_bps = acc.disk_write_bps.saturating_add(r.disk_write_bps);
        acc.net_bps = acc.net_bps.saturating_add(r.net_bps);
        acc.net_total_bytes = acc.net_total_bytes.saturating_add(r.net_total_bytes);
    }
    acc
}

/// 按列排序聚合组（任务管理器风格；纯函数，可单测）。
/// `column`：pid / name / cpu / mem / disk / net / nettotal / status / port。
///
/// 性能：key（聚合值 + 小写名）先对每个组预计算一次，再对索引排序，
/// 最后单次重排——避免比较器内反复 `group_aggregate`（原实现把聚合成本
/// 放大到 O(N log N × 子进程数)）。排序稳定，行为与原实现一致。
pub fn sort_groups(groups: &mut [GroupedProcess], column: &str, ascending: bool) {
    let by_name = matches!(column, "name" | "status");

    // 1) 预计算 key：每个组只聚合/小写化一次
    let keys: Vec<(f64, String)> = groups
        .iter()
        .map(|g| {
            let agg = group_aggregate(g);
            let num = match column {
            "pid" => agg.pid as f64,
            "cpu" => agg.cpu_usage as f64,
            "mem" => agg.memory_bytes as f64,
            "pmem" => agg.physical_mem_bytes as f64,
                "disk" => (agg.disk_read_bps + agg.disk_write_bps) as f64,
                "net" => agg.net_bps as f64,
                "nettotal" => agg.net_total_bytes as f64,
                // 端口：聚合组的所有端口（root + children）合并去重；
                // 主键 = 端口数量；用 (数量*100000 + 最小端口) 把两个比较
                // 维度压成一个 f64 避免破坏 (f64, String) key 结构。
                // 最小端口的 +MAX 把"无端口"组排到末尾（升序时）。
                "port" => {
                    let mut all: Vec<u16> = g.root.ports.clone();
                    for c in &g.children {
                        all.extend_from_slice(&c.ports);
                    }
                    all.sort();
                    all.dedup();
                    let count = all.len() as f64;
                    let min_port = all.first().copied().unwrap_or(u16::MAX) as f64;
                    // 用 100000 作为端口号最大可能值（1-65535）的放大系数
                    count * 100_000.0 + min_port
                }
                _ => 0.0,
            };
            (num, g.name.to_ascii_lowercase())
        })
        .collect();

    // 2) 排序索引（比较只发生在预计算 key 上；stable → 相等项保持原相对顺序）
    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_by(|&a, &b| {
        let o = if by_name {
            keys[a].1.cmp(&keys[b].1)
        } else {
            keys[a].0.partial_cmp(&keys[b].0).unwrap_or(std::cmp::Ordering::Equal)
        };
        if ascending { o } else { o.reverse() }
    });

    // 3) 按排列重排：沿置换环就地 swap，每个元素只移动一次
    //    （visited 保证每环只处理一遍，语义等价于 stable 重排）
    let n = groups.len();
    let mut visited = vec![false; n];
    for i in 0..n {
        if visited[i] {
            continue;
        }
        visited[i] = true;
        let mut cur = i;
        while order[cur] != i {
            let next = order[cur];
            groups.swap(cur, next);
            visited[next] = true;
            cur = next;
        }
    }
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

/// 终止进程失败原因（#2：失败提示 + 提权重试用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillError {
    /// 进程不存在 / 已退出
    NotFound,
    /// 权限不足（典型：以非管理员运行，进程属 SYSTEM 或高权限）
    Permission,
    /// 其他失败
    Other,
}

/// 终止进程。成功返回 `Ok(())`；失败返回原因（权限不足 / 进程不存在 / 其他）。
#[cfg(windows)]
pub fn kill_process(pid: u32) -> Result<(), KillError> {
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_NOT_FOUND,
    };
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        let handle = match OpenProcess(PROCESS_TERMINATE, false, pid) {
            Ok(h) => h,
            Err(e) => {
                let code = e.code().0 as u32;
                return if code == ERROR_INVALID_PARAMETER.0 || code == ERROR_NOT_FOUND.0 {
                    Err(KillError::NotFound)
                } else if code == ERROR_ACCESS_DENIED.0 {
                    Err(KillError::Permission)
                } else {
                    Err(KillError::Other)
                };
            }
        };
        let ok = TerminateProcess(handle, 1);
        if ok.is_ok() {
            let _ = CloseHandle(handle);
            return Ok(());
        }
        // 细分失败原因（与 OpenProcess 分支同一套映射）：进程在 Open 与
        // Terminate 之间退出时这里会报 ERROR_ACCESS_DENIED/INVALID_PARAMETER，
        // 不应一律归为权限不足（会误弹 UAC 提权重试框）
        let code = ok.err().map(|e| e.code().0 as u32).unwrap_or(0);
        let _ = CloseHandle(handle);
        if code == ERROR_INVALID_PARAMETER.0 || code == ERROR_NOT_FOUND.0 {
            Err(KillError::NotFound)
        } else if code == ERROR_ACCESS_DENIED.0 {
            Err(KillError::Permission)
        } else {
            Err(KillError::Other)
        }
    }
}

#[cfg(not(windows))]
pub fn kill_process(pid: u32) -> Result<(), KillError> {
    let mut sys = sysinfo::System::new_all();
    sys.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
    );
    match sys.process(sysinfo::Pid::from_u32(pid)) {
        Some(p) if p.kill() => Ok(()),
        Some(_) => Err(KillError::Permission),
        None => Err(KillError::NotFound),
    }
}

/// 枚举 `root` 的全部后代（子、孙…），返回**杀戮顺序**（不含 root 自身）：
/// 按「距 root 的层数（深度）降序」排列——任何节点的所有后代都排在它前面，
/// 确保「子先于父」被终止，避免父先死导致子进程被 reparent 残留。
///
/// 纯函数、可单测；防环（沿父链形成的环不会死循环，也不会重复收集）。
#[cfg(windows)]
pub fn collect_process_tree(root: u32, rows: &[ProcessRow]) -> Vec<u32> {
    use std::collections::HashMap;
    // 1) 构建 parent -> children 映射
    let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
    for r in rows {
        children_of.entry(r.parent_pid).or_default().push(r.pid);
    }
    // 2) 栈式遍历（DFS）收集全部后代（不含 root）并记录深度（距 root 的层数），
    //    用集合去重防环（结果最终按深度重排，遍历顺序不影响正确性）
    let mut order: Vec<(u32, u32)> = Vec::new(); // (pid, depth)
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut queue: Vec<(u32, u32)> = vec![(root, 0)];
    while let Some((pid, depth)) = queue.pop() {
        if let Some(kids) = children_of.get(&pid) {
            for k in kids {
                // 跳过 root 自身；用 seen 防环导致的重复/死循环
                if *k != root && seen.insert(*k) {
                    order.push((*k, depth + 1));
                    queue.push((*k, depth + 1));
                }
            }
        }
    }
    // 3) 深度降序排序（稳定）：深层（叶子/更晚代）先杀，保证子先于父
    order.sort_by(|a, b| b.1.cmp(&a.1));
    order.into_iter().map(|(pid, _)| pid).collect()
}

#[cfg(not(windows))]
pub fn collect_process_tree(_root: u32, _rows: &[ProcessRow]) -> Vec<u32> {
    Vec::new()
}

/// 终止进程树（连子进程一起终止）。
///
/// 从给定进程快照 `rows` 中沿 `parent_pid` 递归收集 `root` 的全部后代
/// （子、孙…），按「深者先杀」顺序逐个调用 [`kill_process`]（叶子先死，
/// 避免父先死后子进程被 reparent 残留），最后杀根进程本身。
///
/// 返回 `(尝试终止的进程数, 首个遇到的权限错误)`。权限错误（部分子树属
/// SYSTEM / 高权限）由调用方决定后续是否走 UAC 提权重试（见
/// [`prompt_kill_tree_failure`]），本函数本身不弹窗。
#[cfg(windows)]
pub fn kill_process_tree(root: u32, rows: &[ProcessRow]) -> (usize, Option<KillError>) {
    let to_kill = collect_process_tree(root, rows);
    let mut killed = 0usize;
    // 错误优先级：优先记录 `Permission`（决定后续是否走 UAC 提权）。
    // 否则若靠前的子进程先报 NotFound/Other，会掩盖真正需提权的权限错误，
    // 导致 UAC 提示不触发（spec 要求「权限不足时复用现有 UAC 提权路径」）。
    let mut first_err: Option<KillError> = None;
    let mut record_err = |e: KillError| {
        match e {
            KillError::Permission => first_err = Some(e),
            _ => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    };
    for pid in &to_kill {
        match kill_process(*pid) {
            Ok(()) => killed += 1,
            Err(e) => {
                log::warn!("结束进程树：终止子进程 {} 失败: {:?}", pid, e);
                record_err(e);
            }
        }
    }
    // 最后杀根
    match kill_process(root) {
        Ok(()) => killed += 1,
        Err(e) => {
            log::warn!("结束进程树：终止根进程 {} 失败: {:?}", root, e);
            record_err(e);
        }
    }
    (killed, first_err)
}

#[cfg(not(windows))]
pub fn kill_process_tree(_root: u32, _rows: &[ProcessRow]) -> (usize, Option<KillError>) {
    // 非 Windows 无进程可杀：返回 (0, None)，不误导调用方走 UAC 提权逻辑。
    (0, None)
}

/// 弹「以管理员身份重试？」确认框（UAC 提权前置）。返回用户是否确认重试。
/// UI 线程调用；`title`/`msg` 为 UTF-8 文本（内部转 UTF-16）。
#[cfg(windows)]
fn confirm_elevate(title: &str, msg: &str) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    };
    let ret = unsafe {
        MessageBoxW(
            None, // 无父窗口（独立置顶）
            windows::core::PCWSTR(wide_static(msg).as_ptr()),
            windows::core::PCWSTR(wide_static(title).as_ptr()),
            MB_YESNO | MB_ICONWARNING | MB_TOPMOST | MB_SETFOREGROUND,
        )
    };
    ret == windows::Win32::UI::WindowsAndMessaging::IDYES
}

/// 杀进程失败提示（#2）：权限不足 → 弹「以管理员身份重试」确认框（UAC 提权
/// 运行 taskkill）；进程已退出/其他 → 信息框。UI 线程调用。
#[cfg(windows)]
pub fn prompt_kill_failure(pid: u32, name: &str, err: KillError) {
    match err {
        KillError::Permission => {
            let msg = format!(
                "权限不足，无法结束进程 {} (PID {})\n\n是否以管理员身份重试？",
                name, pid
            );
            if confirm_elevate("停止进程", &msg) {
                elevate_kill_process(pid, false);
            }
        }
        KillError::NotFound | KillError::Other => {
            use windows::Win32::UI::WindowsAndMessaging::{
                MessageBoxW, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
            };
            let msg = format!(
                "无法结束进程 {} (PID {})\n\n进程可能已退出或已停止运行。",
                name, pid
            );
            let _ = unsafe {
                MessageBoxW(
                    None,
                    windows::core::PCWSTR(wide_static(&msg).as_ptr()),
                    windows::core::PCWSTR(wide_static("停止进程").as_ptr()),
                    MB_OK | MB_ICONINFORMATION | MB_TOPMOST | MB_SETFOREGROUND,
                )
            };
        }
    }
}

/// 权限不足时弹「以管理员身份重试」确认框（进程树场景）：
/// 用户确认后走 UAC 提权运行 `taskkill /F /T /PID <pid>`（连同子树一起终止）。
#[cfg(windows)]
pub fn prompt_kill_tree_failure(pid: u32, name: &str) {
    let msg = format!(
        "权限不足，无法结束进程树 {} (PID {})\n\n是否以管理员身份重试？\n（将连同其子进程一起终止）",
        name, pid
    );
    if confirm_elevate("结束进程树", &msg) {
        elevate_kill_process(pid, true);
    }
}

/// UAC 提权运行 `taskkill /F /PID <pid>`（ShellExecuteW "runas"）。
/// `tree=true` 时追加 `/T`，连同子树一起终止（进程树场景）。
#[cfg(windows)]
fn elevate_kill_process(pid: u32, tree: bool) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let params = format!("/F{} /PID {}", if tree { " /T" } else { "" }, pid);
    let result = unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("runas"),
            windows::core::w!("taskkill.exe"),
            windows::core::PCWSTR(wide_static(&params).as_ptr()),
            None,
            SW_HIDE,
        )
    };
    // 返回 HINSTANCE > 32 表示成功；<= 32 是错误码
    if result.0 as usize <= 32 {
        log::warn!("提权 taskkill 启动失败 (code={})", result.0 as usize);
    } else {
        log::info!("已以管理员身份启动 taskkill /F /PID {}", pid);
    }
}

#[cfg(not(windows))]
pub fn prompt_kill_failure(_pid: u32, _name: &str, _err: KillError) {}

/// 把文本写入系统剪贴板（UTF-16 `CF_UNICODETEXT`）。详情面板「复制」按钮用。
/// 剪贴板被其他线程占用时静默放弃（不阻塞 UI）。
#[cfg(windows)]
fn set_clipboard_text(text: &str) {
    use windows::Win32::Foundation::{HANDLE, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    // CF_UNICODETEXT = 13（windows crate 把它放 Win32_System_Ole 下且是 newtype，
    // 为不引入整个 OLE feature，直接用标准值）
    const CF_UNICODETEXT: u32 = 13;
    unsafe {
        if !OpenClipboard(Some(HWND::default())).is_ok() {
            return;
        }
        if EmptyClipboard().is_ok() {
            // UTF-16 编码 + 结尾 NUL（SetClipboardData 要求以 NUL 结尾）
            let mut wide: Vec<u16> = text.encode_utf16().collect();
            wide.push(0);
            if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * std::mem::size_of::<u16>()) {
                let ptr = GlobalLock(h);
                if !ptr.is_null() {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr as *mut u16, wide.len());
                    let _ = GlobalUnlock(h);
                    // SetClipboardData 成功后系统接管内存；失败时调用方负责释放
                    if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(h.0))).is_err() {
                        let _ = windows::Win32::Foundation::GlobalFree(Some(h));
                    }
                } else {
                    let _ = windows::Win32::Foundation::GlobalFree(Some(h));
                }
            }
        }
        let _ = CloseClipboard();
    }
}

#[cfg(not(windows))]
fn set_clipboard_text(_text: &str) {}

/// 按需查询进程详情（双击行时调用，中等成本）：返回多行文本。
/// `row`：当前快照行（CPU/内存/磁盘/网络显示用；None 时跳过这些）。
#[cfg(windows)]
pub fn process_detail(pid: u32, name: &str, row: Option<&ProcessRow>, nb_cpus: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("名称: {}", name));
    lines.push(format!("PID: {}", pid));
    if let Some(r) = row {
        let cpu_pct = (r.cpu_usage / nb_cpus.max(1) as f32).clamp(0.0, 100.0);
        lines.push(format!("CPU: {:.1}%    内存: {}", cpu_pct, format_mem(r.memory_bytes)));
        lines.push(format!("磁盘: {}    网络: {}", format_disk_short(r), format_net_short(r)));
        lines.push(format!("累计网络: {}", format_net_total(r)));
    }
    if let Some(p) = process_exe_path(pid) {
        lines.push(format!("路径: {}", p));
    }
    let user = process_user(pid);
    if !user.is_empty() {
        lines.push(format!("用户: {}", user));
    }
    if let Some(cmdline) = process_cmdline(pid) {
        lines.push(format!("命令行: {}", cmdline));
    }
    lines.push(format!("线程数: {}", process_thread_count(pid)));
    if let Some(h) = process_handle_count(pid) {
        lines.push(format!("句柄数: {}", h));
    }
    if let Some(t) = process_start_time(pid) {
        lines.push(format!("启动时间: {}", t));
    }
    if let Some((commit, pws, _)) = process_memory_detail(pid) {
        lines.push(format!(
            "内存: 提交大小 {} / 专用工作集 {}",
            format_mem(commit),
            format_mem(pws)
        ));
    }
    lines.join("\n")
}

#[cfg(not(windows))]
pub fn process_detail(pid: u32, name: &str, row: Option<&ProcessRow>, nb_cpus: usize) -> String {
    let mut lines = vec![format!("名称: {}\nPID: {}", name, pid)];
    if let Some(r) = row {
        lines.push(format!(
            "CPU: {:.1}% 内存: {}",
            r.cpu_usage / nb_cpus.max(1) as f32,
            format_mem(r.memory_bytes)
        ));
    }
    lines.join("\n")
}

/// 进程命令行（sysinfo `cmd()`；失败返回 None）。
#[cfg(windows)]
fn process_cmdline(pid: u32) -> Option<String> {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
    );
    let p = sys.process(sysinfo::Pid::from_u32(pid))?;
    let args: Vec<String> = p.cmd().iter().map(|s| s.to_string_lossy().into_owned()).collect();
    if args.is_empty() {
        None
    } else {
        Some(args.join(" "))
    }
}

/// 进程线程数（ToolHelp 枚举快照）。
#[cfg(windows)]
fn process_thread_count(pid: u32) -> u64 {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    unsafe {
        let snapshot = match CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) {
            Ok(h) => h,
            Err(_) => return 0,
        };
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut count = 0u64;
        if Thread32First(snapshot, &mut entry).is_ok() {
            loop {
                if entry.th32OwnerProcessID == pid {
                    count += 1;
                }
                if Thread32Next(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        count
    }
}

/// 进程句柄数（`GetProcessHandleCount`）。
#[cfg(windows)]
fn process_handle_count(pid: u32) -> Option<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut count: u32 = 0;
        let ok = GetProcessHandleCount(handle, &mut count);
        let _ = CloseHandle(handle);
        ok.ok()?;
        Some(count)
    }
}

/// 进程启动时间（本地时区字符串；失败返回 None）。
#[cfg(windows)]
fn process_start_time(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut creation: windows::Win32::Foundation::FILETIME = std::mem::zeroed();
        let mut exit: windows::Win32::Foundation::FILETIME = std::mem::zeroed();
        let mut kernel: windows::Win32::Foundation::FILETIME = std::mem::zeroed();
        let mut user: windows::Win32::Foundation::FILETIME = std::mem::zeroed();
        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        let _ = CloseHandle(handle);
        ok.ok()?;
        let mut utc: windows::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
        FileTimeToSystemTime(&creation, &mut utc).ok()?;
        let mut local: windows::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
        Some(format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond
        ))
    }
}

/// 进程内存明细：提交大小 + 专用工作集 + 工作集（一次 `GetProcessMemoryInfo`）。
/// 元组顺序 (提交大小, 专用工作集, 工作集)：
/// - `PagefileUsage` = 提交大小（Commit Size），任务管理器「详细信息」页「内存」列口径
/// - `PrivateWorkingSetSize` = 专用工作集（Private Working Set），任务管理器「进程」页「内存」列口径
/// - `WorkingSetSize` = 工作集（含与其他进程共享的物理页）
/// 需要 `PROCESS_MEMORY_COUNTERS_EX2`（Windows 8.1+，本工具目标平台 Win10/11）。
#[cfg(windows)]
fn process_memory_detail(pid: u32) -> Option<(u64, u64, u64)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut mc: PROCESS_MEMORY_COUNTERS_EX2 = std::mem::zeroed();
        mc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32;
        // cb 传 EX2 结构大小，系统按需填充扩展字段（PrivateWorkingSetSize /
        // SharedCommitUsage）；GetProcessMemoryInfo 签名只接受基结构指针，
        // 以裸指针转换传入（Win32 惯例，Windows 8.1+ 支持 EX2 结构）。
        let p = &mut mc as *mut PROCESS_MEMORY_COUNTERS_EX2 as *mut PROCESS_MEMORY_COUNTERS;
        let ok = GetProcessMemoryInfo(handle, p, mc.cb);
        let _ = CloseHandle(handle);
        ok.ok()?;
        Some((
            mc.PagefileUsage as u64,
            mc.PrivateWorkingSetSize as u64,
            mc.WorkingSetSize as u64,
        ))
    }
}

/// 非 Windows 兜底：无 PSAPI，返回 None（调用方回退 sysinfo 工作集）。
#[cfg(not(windows))]
fn process_memory_detail(_pid: u32) -> Option<(u64, u64, u64)> {
    None
}

/// 生成以 NUL 结尾的 UTF-16 缓冲（MessageBoxW / ShellExecuteW 用）。
/// 注意：内部是静态 Vec 缓存，仅同步调用有效（与本文件 wide 的约定一致）。
#[cfg(windows)]
fn wide_static(s: &str) -> Vec<u16> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<std::sync::Mutex<Vec<u16>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut buf = cache.lock().unwrap();
    buf.clear();
    buf.extend(s.encode_utf16());
    buf.push(0);
    buf.clone()
}

/// 屏幕坐标命中测试：判断 `(sx, sy)`（物理像素）是否落在进程详情列表的
/// 某一行上，返回该行的 `(pid, name)`。
///
/// 用于「连续右键切换」：菜单被右键关闭后，在鼠标新位置重新定位行。
/// 命中条件：鼠标在本窗口内 + y 落在列表行区域（布局硬编码：
/// padding 6 + 标题栏 30 + spacing 6 + 列头 22 + spacing 6 = 列表起始 y=70，
/// 行高 29，均逻辑像素，需按 scale 换算；列表已滚动时再扣除 viewport-y）。
fn hit_test_row(ui: &crate::ProcessList, sx: i32, sy: i32) -> Option<(i32, String)> {
    use slint::Model;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::WindowsAndMessaging::WindowFromPoint;

    let hwnd = crate::window::extract_hwnd_from(ui.window())?;
    // 鼠标必须在本窗口上
    let screen_pt = POINT { x: sx, y: sy };
    if unsafe { WindowFromPoint(screen_pt) } != hwnd {
        return None;
    }
    // 屏幕坐标 → 窗口客户区坐标 → 逻辑坐标
    let mut client_pt = screen_pt;
    let _ = unsafe { ScreenToClient(hwnd, &mut client_pt) };
    let scale = ui.window().scale_factor();
    let y_logical = client_pt.y as f32 / scale;

    // 列表布局：常量见模块级 LIST_TOP / ROW_HEIGHT（与 overlay.slint 双源同步）
    // 列表可滚动：鼠标 y 还需扣除 viewport-y 偏移（未滚动=0，向下滚动为负）
    let rel = y_logical - LIST_TOP - ui.get_list_scroll_y();
    if rel < 0.0 {
        return None;
    }
    let idx = (rel / ROW_HEIGHT) as usize;
    let model = ui.get_process_model();
    if idx >= model.row_count() {
        return None;
    }
    let row = model.row_data(idx)?;
    Some((row.pid, row.name.to_string()))
}

/// 打开进程可执行文件所在的位置（任务管理器「打开文件所在的位置」）。
///
/// 流程：`QueryFullProcessImageNameW` 取完整路径 → `ShellExecuteW` 启动
/// `explorer /select,<path>` 打开资源管理器并选中该文件。失败（权限不足 /
/// 进程已退出 / 无法启动资源管理器）返回 `Err`。
#[cfg(windows)]
pub fn open_process_location(pid: u32) -> anyhow::Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let path = process_exe_path(pid).ok_or_else(|| {
        anyhow::anyhow!("无法获取进程 {} 的可执行文件路径（权限不足或进程已退出）", pid)
    })?;
    log::info!("打开文件所在的位置: {} (PID {})", path, pid);
    // explorer 对命令行的解析非标准：`/select,"path"` 的引号必须原样保留，
    // 路径含空格时才能正确选中目标文件。
    // 用 ShellExecuteW 直接传 UTF-16 参数字符串（不经 CreateProcessW 命令行
    // 转义），避免 `Command::arg` 把引号转义成 `\"` 导致路径被破坏。
    // 与 elevate_kill_process 的写法保持一致。
    let params = format!("/select,\"{}\"", path);
    let result = unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("open"),
            windows::core::w!("explorer.exe"),
            windows::core::PCWSTR(wide_static(&params).as_ptr()),
            None,
            SW_SHOWNORMAL,
        )
    };
    // 返回 HINSTANCE > 32 表示成功；<= 32 是错误码（与 elevate_kill_process 一致）
    if result.0 as usize <= 32 {
        Err(anyhow::anyhow!(
            "ShellExecuteW 启动资源管理器失败 (code={})",
            result.0 as usize
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
pub fn open_process_location(_pid: u32) -> anyhow::Result<()> {
    Err(anyhow::anyhow!("当前平台不支持打开文件所在位置"))
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

/// 取 exe 的「文件描述」（版本资源里的 `FileDescription`，如 "Google Chrome"）。
/// 失败 / 无描述返回 `None`。仅在 Windows 有意义。
#[cfg(windows)]
fn file_description(path: &str) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };
    let path_w: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // 版本信息缓冲区大小（0 = 失败 / 无版本资源）
    let size = unsafe { GetFileVersionInfoSizeW(windows::core::PCWSTR(path_w.as_ptr()), None) };
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let ok = unsafe {
        GetFileVersionInfoW(
            windows::core::PCWSTR(path_w.as_ptr()),
            Some(0),
            size,
            buf.as_mut_ptr() as *mut _,
        )
    };
    if ok.is_err() {
        return None;
    }
    // 1) 取翻译（lang-codepage），用于拼出正确的 StringFileInfo 子块路径
    let trans_block: Vec<u16> = "\\VarFileInfo\\Translation"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut trans_len = 0u32;
    let mut trans_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    let ok_t = unsafe {
        VerQueryValueW(
            buf.as_ptr() as *const _,
            windows::core::PCWSTR(trans_block.as_ptr()),
            &mut trans_ptr,
            &mut trans_len,
        )
    };
    if !ok_t.as_bool() || trans_len == 0 {
        return None;
    }
    let trans =
        unsafe { std::slice::from_raw_parts(trans_ptr as *const u16, (trans_len as usize) / 2) };
    if trans.is_empty() {
        return None;
    }
    let (lang, codepage) = (trans[0], trans[1]);
    let sub = format!("\\StringFileInfo\\{:04X}{:04X}\\FileDescription", lang, codepage);
    let sub_w: Vec<u16> = sub.encode_utf16().chain(std::iter::once(0)).collect();
    let mut desc_len = 0u32;
    let mut desc_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    let ok_d = unsafe {
        VerQueryValueW(
            buf.as_ptr() as *const _,
            windows::core::PCWSTR(sub_w.as_ptr()),
            &mut desc_ptr,
            &mut desc_len,
        )
    };
    if !ok_d.as_bool() || desc_len == 0 {
        return None;
    }
    // desc_len 含结尾 NUL，去掉后转 UTF-16
    let desc_slice = unsafe {
        std::slice::from_raw_parts(desc_ptr as *const u16, (desc_len as usize).saturating_sub(1))
    };
    let s = String::from_utf16_lossy(desc_slice).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(not(windows))]
fn file_description(_path: &str) -> Option<String> {
    None
}

/// 进程展示名（友好名）：取 exe 的 `FileDescription`；取不到（无版本信息 /
/// 系统进程 / 权限不足 / 非 Windows）回退 `None`（调用方回退 exe 名）。
///
/// 注：spec 要求「UWP 包显示名 **或** exe 的 `FileDescription`」二选一，
/// 本实现走 `FileDescription` 路径（覆盖绝大多数桌面应用）。UWP 包显示名需
/// WinRT `PackageManager`（按 PackageFamilyName 取显示名），成本更高且 windows
/// crate 需额外 feature，留作后续扩展；UWP 应用当前回退 exe 名，不影响主流程。
#[cfg(windows)]
fn friendly_name_for(pid: u32) -> Option<String> {
    let path = process_exe_path(pid)?;
    file_description(&path)
}

#[cfg(not(windows))]
fn friendly_name_for(_pid: u32) -> Option<String> {
    None
}

/// 查询进程所属用户（`OpenProcessToken` + `GetTokenInformation(TokenUser)` +
/// `LookupAccountSidW`）。返回 "SYSTEM" / 用户名（不带域）；失败（权限不足 /
/// 进程已退出 / 无法解析 SID）返回空字符串。
///
/// 注意：进程较多时逐进程查询 token 有开销，放在采样路径上即可
/// （#12 采样移入后台线程后不阻塞 UI）。
#[cfg(windows)]
fn process_user(pid: u32) -> String {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        GetTokenInformation, LookupAccountSidW, SID_NAME_USE, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(_) => return String::new(),
        };
        let mut token = windows::Win32::Foundation::HANDLE::default();
        let token_ok = OpenProcessToken(handle, TOKEN_QUERY, &mut token).is_ok();
        let _ = CloseHandle(handle);
        if !token_ok {
            return String::new();
        }

        // 先查所需 buffer 长度，再分配
        let mut need: u32 = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut need);
        if need == 0 {
            let _ = CloseHandle(token);
            return String::new();
        }
        let mut buf = vec![0u8; need as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut _),
            need,
            &mut need,
        );
        let _ = CloseHandle(token);
        if !ok.is_ok() {
            return String::new();
        }
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let sid = tu.User.Sid;
        if sid.0.is_null() {
            return String::new();
        }

        let mut name = [0u16; 256];
        let mut name_len = name.len() as u32;
        let mut domain = [0u16; 256];
        let mut domain_len = domain.len() as u32;
        let mut use_kind = SID_NAME_USE::default();
        let ok = LookupAccountSidW(
            None,
            sid,
            Some(windows::core::PWSTR(name.as_mut_ptr())),
            &mut name_len,
            Some(windows::core::PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut use_kind,
        );
        if !ok.is_ok() {
            return String::new();
        }
        // "SYSTEM" / "Administrator" 等直接显示用户名；有域时显示 "域\用户"
        let user = String::from_utf16_lossy(&name[..name_len as usize]);
        if user.is_empty() {
            String::new()
        } else if use_kind == windows::Win32::Security::SidTypeUser
            && domain_len > 0
            && !user.eq_ignore_ascii_case("SYSTEM")
        {
            let dom = String::from_utf16_lossy(&domain[..domain_len as usize]);
            format!("{}\\{}", dom, user)
        } else {
            user
        }
    }
}

#[cfg(not(windows))]
fn process_user(_pid: u32) -> String {
    String::new()
}

/// 查询占用指定端口（本地端口）的进程：TCP（监听/连接）+ UDP。
///
/// 返回 `(pid, 协议, TCP 状态, 本地地址)`。毫秒级（`GetExtendedTcpTable` /
/// `GetExtendedUdpTable` 两遍调用）。pid=0 的系统条目会过滤掉。
/// 用于搜索框按端口号查找进程（例如输入 `8080`）。
#[cfg(windows)]
pub fn port_owners(port: u16) -> Vec<(u32, String, String, String)> {
    use windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCPTABLE_OWNER_PID,
        MIB_UDPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
    };
    const AF_INET: u32 = 2; // 不引入 WinSock feature，直接传地址族常量
    const NO_ERROR: u32 = 0;

    let mut out: Vec<(u32, String, String, String)> = Vec::new();
    unsafe {
        // ---- TCP ----
        let mut size: u32 = 0;
        let r = GetExtendedTcpTable(None, &mut size, false, AF_INET, TCP_TABLE_OWNER_PID_ALL, 0);
        if r == ERROR_INSUFFICIENT_BUFFER.0 && size > 0 {
            let mut buf = vec![0u8; size as usize];
            let r2 = GetExtendedTcpTable(
                Some(buf.as_mut_ptr() as *mut _),
                &mut size,
                false,
                AF_INET,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );
            if r2 == NO_ERROR {
                let table = &*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID);
                let rows = std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize);
                for row in rows {
                    // dwLocalPort 是网络字节序（大端）
                    let lp = u16::from_be((row.dwLocalPort & 0xffff) as u16);
                    if lp == port && row.dwOwningPid != 0 {
                        out.push((
                            row.dwOwningPid,
                            "TCP".to_string(),
                            tcp_state_text(row.dwState),
                            ip4_text(row.dwLocalAddr),
                        ));
                    }
                }
            }
        }
        // ---- UDP ----
        let mut size: u32 = 0;
        let r = GetExtendedUdpTable(None, &mut size, false, AF_INET, UDP_TABLE_OWNER_PID, 0);
        if r == ERROR_INSUFFICIENT_BUFFER.0 && size > 0 {
            let mut buf = vec![0u8; size as usize];
            let r2 = GetExtendedUdpTable(
                Some(buf.as_mut_ptr() as *mut _),
                &mut size,
                false,
                AF_INET,
                UDP_TABLE_OWNER_PID,
                0,
            );
            if r2 == NO_ERROR {
                let table = &*(buf.as_ptr() as *const MIB_UDPTABLE_OWNER_PID);
                let rows = std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize);
                for row in rows {
                    let lp = u16::from_be((row.dwLocalPort & 0xffff) as u16);
                    if lp == port && row.dwOwningPid != 0 {
                        out.push((row.dwOwningPid, "UDP".to_string(), String::new(), ip4_text(row.dwLocalAddr)));
                    }
                }
            }
        }
    }
    out
}

#[cfg(not(windows))]
pub fn port_owners(_port: u16) -> Vec<(u32, String, String, String)> {
    Vec::new()
}

/// 枚举当前所有「进程 → 端口号列表」：监听 TCP + 全部 UDP（去重 + 升序）。
///
/// 用于进程详情页的「端口」列：
/// - TCP 只取 `LISTEN` 状态（避免每个连接都占一行；任务管理器只显示监听端口）
/// - UDP 全部（UDP 无状态，所有条目都是「占用」）
/// - pid=0 的系统条目过滤掉
/// - 同一 pid 的端口去重 + 升序，方便显示稳定（端口号在调用间不会跳来跳去）
#[cfg(windows)]
pub fn port_map() -> std::collections::HashMap<u32, Vec<u16>> {
    use std::collections::{BTreeSet, HashMap};
    use windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCPTABLE_OWNER_PID, MIB_TCP_STATE_LISTEN,
        MIB_UDPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
    };
    const AF_INET: u32 = 2;
    const NO_ERROR: u32 = 0;

    let mut out: HashMap<u32, BTreeSet<u16>> = HashMap::new();

    unsafe {
        // ---- TCP LISTEN ----
        let mut size: u32 = 0;
        let r = GetExtendedTcpTable(None, &mut size, false, AF_INET, TCP_TABLE_OWNER_PID_ALL, 0);
        if r == ERROR_INSUFFICIENT_BUFFER.0 && size > 0 {
            let mut buf = vec![0u8; size as usize];
            let r2 = GetExtendedTcpTable(
                Some(buf.as_mut_ptr() as *mut _),
                &mut size,
                false,
                AF_INET,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );
            if r2 == NO_ERROR {
                let table = &*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID);
                let rows = std::slice::from_raw_parts(
                    table.table.as_ptr(),
                    table.dwNumEntries as usize,
                );
                for row in rows {
                    // MIB_TCP_STATE 是 i32 newtype；row.dwState 是 u32，
                    // 不能直接 ==（类型不匹配）。用 .0 取内部 i32 再比较。
                    if row.dwState != MIB_TCP_STATE_LISTEN.0 as u32 {
                        continue;
                    }
                    if row.dwOwningPid == 0 {
                        continue;
                    }
                    let lp = u16::from_be((row.dwLocalPort & 0xffff) as u16);
                    out.entry(row.dwOwningPid).or_default().insert(lp);
                }
            }
        }
        // ---- UDP 全部 ----
        let mut size: u32 = 0;
        let r = GetExtendedUdpTable(None, &mut size, false, AF_INET, UDP_TABLE_OWNER_PID, 0);
        if r == ERROR_INSUFFICIENT_BUFFER.0 && size > 0 {
            let mut buf = vec![0u8; size as usize];
            let r2 = GetExtendedUdpTable(
                Some(buf.as_mut_ptr() as *mut _),
                &mut size,
                false,
                AF_INET,
                UDP_TABLE_OWNER_PID,
                0,
            );
            if r2 == NO_ERROR {
                let table = &*(buf.as_ptr() as *const MIB_UDPTABLE_OWNER_PID);
                let rows = std::slice::from_raw_parts(
                    table.table.as_ptr(),
                    table.dwNumEntries as usize,
                );
                for row in rows {
                    if row.dwOwningPid == 0 {
                        continue;
                    }
                    let lp = u16::from_be((row.dwLocalPort & 0xffff) as u16);
                    out.entry(row.dwOwningPid).or_default().insert(lp);
                }
            }
        }
    }

    // BTreeSet → Vec（小集合，多次 to_vec 比 HashMap 排序便宜）
    out.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect()
}

#[cfg(not(windows))]
pub fn port_map() -> std::collections::HashMap<u32, Vec<u16>> {
    Default::default()
}

/// 端口列表 → 显示字符串。
/// - 空 → "—"
/// - 1 个 → "80"
/// - 2 个 → "80, 443"
/// - 3+ 个 → "80, 443 +N"（前两个 + 剩余数量；悬浮详情用 port-full 看完整）
pub fn format_ports_short(ports: &[u16]) -> String {
    match ports.len() {
        0 => "—".to_string(),
        1 => ports[0].to_string(),
        2 => format!("{}, {}", ports[0], ports[1]),
        _ => format!("{}, {} +{}", ports[0], ports[1], ports.len() - 2),
    }
}

/// 端口列表 → 完整字符串（悬浮详情用）：按逗号分隔全部。
pub fn format_ports_full(ports: &[u16]) -> String {
    if ports.is_empty() {
        "无监听端口".to_string()
    } else {
        ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// 合并多个进程的端口（去重 + 升序）。聚合行显示用。
pub fn merge_ports(pids_ports: &[Vec<u16>]) -> Vec<u16> {
    use std::collections::BTreeSet;
    let mut set: BTreeSet<u16> = BTreeSet::new();
    for v in pids_ports {
        for p in v {
            set.insert(*p);
        }
    }
    set.into_iter().collect()
}

/// TCP 状态码 → 中文（MIB_TCP_STATE）。
#[cfg(windows)]
fn tcp_state_text(state: u32) -> String {
    use windows::Win32::NetworkManagement::IpHelper::{
        MIB_TCP_STATE, MIB_TCP_STATE_CLOSE_WAIT, MIB_TCP_STATE_CLOSED, MIB_TCP_STATE_CLOSING,
        MIB_TCP_STATE_ESTAB, MIB_TCP_STATE_FIN_WAIT1, MIB_TCP_STATE_FIN_WAIT2,
        MIB_TCP_STATE_LAST_ACK, MIB_TCP_STATE_LISTEN, MIB_TCP_STATE_SYN_RCVD,
        MIB_TCP_STATE_SYN_SENT, MIB_TCP_STATE_TIME_WAIT,
    };
    let s = MIB_TCP_STATE(state as i32);
    match s {
        MIB_TCP_STATE_CLOSED => "已关闭",
        MIB_TCP_STATE_LISTEN => "监听中",
        MIB_TCP_STATE_SYN_SENT => "SYN已发送",
        MIB_TCP_STATE_SYN_RCVD => "SYN已接收",
        MIB_TCP_STATE_ESTAB => "已建立",
        MIB_TCP_STATE_FIN_WAIT1 => "FIN等待1",
        MIB_TCP_STATE_FIN_WAIT2 => "FIN等待2",
        MIB_TCP_STATE_CLOSE_WAIT => "关闭等待",
        MIB_TCP_STATE_CLOSING => "关闭中",
        MIB_TCP_STATE_LAST_ACK => "最后ACK",
        MIB_TCP_STATE_TIME_WAIT => "时间等待",
        _ => "未知",
    }
    .to_string()
}

#[cfg(not(windows))]
fn tcp_state_text(_state: u32) -> String {
    String::new()
}

/// IPv4（小端 u32）→ 点分十进制。
#[cfg(windows)]
fn ip4_text(addr: u32) -> String {
    format!(
        "{}.{}.{}.{}",
        addr & 0xff,
        (addr >> 8) & 0xff,
        (addr >> 16) & 0xff,
        (addr >> 24) & 0xff
    )
}

/// 读进程 I/O 计数器 + 内存明细（一次 `OpenProcess`，同句柄取两类信息）。
///
/// 采样热路径专用：原来 IO 计数器与内存明细各开一次句柄，每 tick 每进程
/// 两对 Open/Close；合并后减半。打开失败时返回 `(全 0, None)`——两个 API
/// 的权限要求相同（PROCESS_QUERY_LIMITED_INFORMATION），失败高度相关，
/// 独立退路的收益可以忽略。
#[cfg(windows)]
fn process_io_and_memory(pid: u32) -> (IoSnapshot, Option<(u64, u64, u64)>) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
    };
    use windows::Win32::System::Threading::{
        GetProcessIoCounters, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(_) => return (IoSnapshot { read: 0, write: 0, other: 0 }, None),
        };
        let mut counters: windows::Win32::System::Threading::IO_COUNTERS =
            std::mem::zeroed();
        let io = if GetProcessIoCounters(handle, &mut counters).is_ok() {
            IoSnapshot {
                read: counters.ReadTransferCount,
                write: counters.WriteTransferCount,
                other: counters.OtherTransferCount,
            }
        } else {
            IoSnapshot { read: 0, write: 0, other: 0 }
        };
        // EX2 结构传基结构指针（Win32 惯例，与 process_memory_detail 一致）
        let mut mc: PROCESS_MEMORY_COUNTERS_EX2 = std::mem::zeroed();
        mc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32;
        let p = &mut mc as *mut PROCESS_MEMORY_COUNTERS_EX2 as *mut PROCESS_MEMORY_COUNTERS;
        let mem = if GetProcessMemoryInfo(handle, p, mc.cb).is_ok() {
            Some((
                mc.PagefileUsage as u64,
                mc.PrivateWorkingSetSize as u64,
                mc.WorkingSetSize as u64,
            ))
        } else {
            None
        };
        let _ = CloseHandle(handle);
        (io, mem)
    }
}

#[cfg(not(windows))]
fn process_io_and_memory(_pid: u32) -> (IoSnapshot, Option<(u64, u64, u64)>) {
    (IoSnapshot { read: 0, write: 0, other: 0 }, None)
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
    cpu_high: bool,
    mem: String,
    physical_mem: String,
    mem_high: bool,
    disk: String,
    disk_full: String,
    net: String,
    net_full: String,
    net_total: String,
    net_total_full: String,
    port: String,
    port_full: String,
    is_group: bool,
    child_count: i32,
) -> crate::ProcessRowData {
    crate::ProcessRowData {
        pid: pid as i32,
        name: SharedString::from(name),
        name_full: SharedString::from(name_full),
        group_key: SharedString::from(group_key),
        cpu: SharedString::from(cpu),
        cpu_high,
        mem: SharedString::from(mem),
        physical_mem: SharedString::from(physical_mem),
        mem_high,
        disk: SharedString::from(disk),
        disk_full: SharedString::from(disk_full),
        net: SharedString::from(net),
        net_full: SharedString::from(net_full),
        net_total: SharedString::from(net_total),
        net_total_full: SharedString::from(net_total_full),
        port: SharedString::from(port),
        port_full: SharedString::from(port_full),
        is_group,
        child_count,
    }
}

/// 把单行格式化为 slint 行（普通行/子行共用）。
/// `highlight_pct`：CPU/内存占用超过该百分比即高亮（标红加粗）。
fn row_display(
    r: &ProcessRow,
    nb_cpus: usize,
    highlight_pct: f32,
    indent: bool,
) -> crate::ProcessRowData {
    // 展示名优先用友好名；原始 exe 名作为悬浮/备用（差异时在 full_name 标明）
    let display = &r.display_name;
    let full_name = if r.display_name != r.name {
        format!("{}（{}，PID {}）", r.display_name, r.name, r.pid)
    } else {
        format!("PID {}  {}", r.pid, r.name)
    };
    let cpu_pct = (r.cpu_usage / nb_cpus as f32).clamp(0.0, 100.0);
    let cpu_high = cpu_pct > highlight_pct;
    let mem_high = r.memory_pct > highlight_pct;
    let mut name = display.clone();
    if indent {
        name = format!("    {}", name);
    }
    row_to_slint(
        r.pid,
        name,
        full_name,
        String::new(), // 普通行无 group-key
        format!("{:.1}%", cpu_pct),
        cpu_high,
        format_mem(r.memory_bytes),
        format_mem(r.physical_mem_bytes),
        mem_high,
        format_disk_short(r),
        format_disk_full(r),
        format_net_short(r),
        format_net_full(r),
        format_net_total(r),
        format_net_total_full(r),
        format_ports_short(&r.ports),
        format_ports_full(&r.ports),
        false,
        0,
    )
}

/// 聚合组 → 父节点 slint 行。
/// svchost：主条目显示「服务宿主: [服务组名]」；多服务时列前两个 + 总数。
/// `highlight_pct`：CPU/内存占用超阈值高亮；`total_mem`：全机物理内存（字节，算聚合内存占比）。
fn group_display(
    g: &GroupedProcess,
    agg: &ProcessRow,
    nb_cpus: usize,
    highlight_pct: f32,
    total_mem: u64,
) -> crate::ProcessRowData {
    let cpu_pct = (agg.cpu_usage / nb_cpus as f32).clamp(0.0, 100.0);
    let mem_pct = mem_pct(agg.memory_bytes, total_mem);
    let cpu_high = cpu_pct > highlight_pct;
    let mem_high = mem_pct > highlight_pct;
    let count = g.total_count() as i32;
    // 展开键唯一化（exe:root_pid）：见 group_key 注释
    let gkey = group_key(g);
    let (title, full_name, group_key) = if g.name.eq_ignore_ascii_case("svchost.exe") {
        if g.services.is_empty() {
            (
                format!("服务宿主: svchost ({})", count),
                format!("服务宿主: svchost.exe（{} 个实例）", count),
                gkey,
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
                gkey,
            )
        }
    } else {
        // 非 svchost：展示名优先用友好名（组内同 exe，取 root 的 display_name）；
        // 原始 exe 名作为悬浮/备用，展开键用 group_key（exe:root_pid 唯一）
        if g.root.display_name != g.name {
            (
                format!("{} ({})", g.root.display_name, count),
                format!(
                    "{}（{}，{} 个实例，PID {}）",
                    g.root.display_name, g.name, count, agg.pid
                ),
                gkey,
            )
        } else {
            (
                format!("{} ({})", g.name, count),
                format!("{}（{} 个实例，PID {}）", g.name, count, agg.pid),
                gkey,
            )
        }
    };
    // 聚合端口：合并 root + 所有 children 的端口（去重升序）
    let mut all_ports: Vec<Vec<u16>> = Vec::with_capacity(1 + g.children.len());
    all_ports.push(g.root.ports.clone());
    for c in &g.children {
        all_ports.push(c.ports.clone());
    }
    let merged_ports = merge_ports(&all_ports);
    row_to_slint(
        agg.pid,
        title,
        full_name,
        group_key,
        format!("{:.1}%", cpu_pct),
        cpu_high,
        format_mem(agg.memory_bytes),
        format_mem(agg.physical_mem_bytes),
        mem_high,
        format_disk_short(agg),
        format_disk_full(agg),
        format_net_short(agg),
        format_net_full(agg),
        format_net_total(agg),
        format_net_total_full(agg),
        format_ports_short(&merged_ports),
        format_ports_full(&merged_ports),
        true,
        count,
    )
}

/// 进程列表窗口句柄。
pub struct ProcessListWindow {
    ui: crate::ProcessList,
    /// 最近一次采样的全量进程行缓存（渲染/搜索/展开直接用，避免反复采样）。
    /// 与采样线程共享同一个 Arc（构造时由 `cache` 传入，非新建），
    /// 否则 `refresh()` 首帧读到的永远是空列表。
    cache: Arc<Mutex<Vec<ProcessRow>>>,
    /// 快照采集器（采样线程持有操作；本字段仅作 Arc 持有者，
    /// 与采样线程闭包共享同一实例）
    #[allow(dead_code)]
    sampler: Arc<Mutex<ProcessSampler>>,
    /// 当前排序列 + 方向
    sort: Arc<Mutex<(String, bool)>>,
    /// 搜索关键字
    search: Arc<Mutex<String>>,
    /// 已展开的聚合组 key（小写进程名）
    expanded: Arc<Mutex<std::collections::HashSet<String>>>,
    /// 渲染共享状态（CPU 核数 / 高亮阈值 / 总内存 / 增量 model）
    shared: Arc<RenderShared>,
    /// 最近一次打开的进程详情文本（详情面板「复制」按钮用；双击行时更新）。
    /// 仅作 Arc 持有者（复制回调持有同一 Arc 的 clone），本字段不直接读取。
    #[allow(dead_code)]
    detail_text: Arc<Mutex<String>>,
    /// 自动刷新间隔（毫秒，来自 config.ui.process_refresh_ms；下拉可调）。
    /// 仅作为 Arc 持有者（采样线程 / UI timer / 下拉回调共享同一 Arc），
    /// 本结构体本身不读取。
    #[allow(dead_code)]
    _refresh_ms: Arc<Mutex<u64>>,
    /// 采样线程停止标志（Drop 时置位）
    stop_sampling: Arc<AtomicBool>,
    /// 手动刷新置位 → 采样线程立即采样（refresh() 用）
    sample_now: Arc<AtomicBool>,
    /// 采样完成版本号（每次 +1；采样线程局部逻辑保留）。
    /// refresh() 已改为非阻塞（不再同步等版本变化），此字段仅作 Arc 持有者。
    #[allow(dead_code)]
    cache_version: Arc<Mutex<u64>>,
    /// 采样线程句柄（#12：采样移出 UI 线程）
    sampler_handle: Option<std::thread::JoinHandle<()>>,
    /// 刷新 Timer（Arc 共享：下拉改间隔时重启；持有防 drop）
    _timer: Arc<slint::Timer>,
}

impl ProcessListWindow {
    /// 创建并显示进程详情窗口。
    ///
    /// `highlight_pct`：CPU/内存高亮阈值（%，来自 config.ui.process_highlight_pct）；
    /// `refresh_ms`：自动刷新间隔（毫秒，来自 config.ui.process_refresh_ms）。
    ///
    /// **拖动延迟**：窗口由右键菜单弹出，`TrackPopupMenu` 的模态循环会
    /// 吞掉 MouseUp 事件，winit 误以为鼠标仍按住 → 新窗口一出现就被拖动
    /// 跟着鼠标走。因此创建后 300ms 内忽略 drag-moved。
    pub fn show(highlight_pct: f32, refresh_ms: u64) -> anyhow::Result<Self> {
        let ui = crate::ProcessList::new()?;
        let sampler: Arc<Mutex<ProcessSampler>> = Arc::new(Mutex::new(ProcessSampler::new()));
        let sort: Arc<Mutex<(String, bool)>> = Arc::new(Mutex::new(("cpu".to_string(), false)));
        let search: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let expanded: Arc<Mutex<std::collections::HashSet<String>>> =
            Arc::new(Mutex::new(std::collections::HashSet::new()));
        let highlight_pct: Arc<Mutex<f32>> = Arc::new(Mutex::new(highlight_pct));
        let total_mem: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let refresh_ms: Arc<Mutex<u64>> = Arc::new(Mutex::new(refresh_ms.max(200)));
        // 渲染增量更新持有的 VecModel（None = 首次渲染前）
        let model_arc: Arc<Mutex<Option<std::rc::Rc<slint::VecModel<crate::ProcessRowData>>>>> =
            Arc::new(Mutex::new(None));
        // 采样线程停止标志
        let stop_sampling = Arc::new(AtomicBool::new(false));
        // 手动刷新：置位后采样线程立即采样（不等剩余间隔）
        let sample_now = Arc::new(AtomicBool::new(false));
        // 采样完成版本号（每次 +1；手动刷新等它变化后重绘）
        let cache_version: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        // 进程行缓存（采样一次，渲染/搜索/展开/排序复用）
        let cache: Arc<Mutex<Vec<ProcessRow>>> = Arc::new(Mutex::new(Vec::new()));
        let cache_tick = cache.clone();
        // CPU 核数缓存（render 复用；sysinfo::System::new() 枚举硬件很贵）
        let nb_cpus: Arc<Mutex<usize>> = Arc::new(Mutex::new(
            sampler.lock().unwrap().cpu_count().max(1),
        ));
        // 服务映射缓存（svchost 聚合用；采样线程 30s TTL 刷新，render 只读）
        let services_cache: Arc<Mutex<std::collections::HashMap<u32, Vec<String>>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        // 渲染共享状态（各回调 clone 一份 Arc，避免每个闭包 clone 4 个 Arc）
        let shared: Arc<RenderShared> = Arc::new(RenderShared {
            nb_cpus: nb_cpus.clone(),
            highlight_pct: highlight_pct.clone(),
            total_mem: total_mem.clone(),
            model_arc: model_arc.clone(),
            services: services_cache.clone(),
        });
        // 最近一次打开的详情文本（详情面板「复制」按钮用）
        let detail_text: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

        // #12 采样/渲染分离：采样线程独立跑（UI 只消费 cache 快照），
        // 间隔跟随 refresh_ms（下拉可调，线程每轮读取）。
        // - `sample_now`：手动刷新置位 → 线程立即采样（不等剩余间隔）
        // - `cache_version`：每次采样完成 +1，手动刷新等它变化再重绘
        //   （避免 UI 线程直接 sample() 与后台线程交错污染速率差分基线）
        let stop_thread = stop_sampling.clone();
        let sampler_thread = sampler.clone();
        let cache_thread = cache_tick.clone();
        let refresh_thread = refresh_ms.clone();
        let total_thread = total_mem.clone();
        let sample_now_thread = sample_now.clone();
        let version_thread = cache_version.clone();
        let services_thread = services_cache.clone();
        let sampler_handle = std::thread::Builder::new()
            .name("process-sampler".into())
            .spawn(move || {
                let mut deadline = Instant::now(); // 首轮立即采样
                let mut svc_deadline = Instant::now(); // 首轮立即枚举服务
                loop {
                    if stop_thread.load(Ordering::SeqCst) {
                        break;
                    }
                    let forced = sample_now_thread.swap(false, Ordering::SeqCst);
                    if Instant::now() >= deadline || forced {
                        let rows = {
                            let mut s = sampler_thread.lock().unwrap();
                            let rows = s.sample();
                            *total_thread.lock().unwrap() = s.total_memory();
                            rows
                        };
                        *cache_thread.lock().unwrap() = rows;
                        *version_thread.lock().unwrap() += 1;
                        deadline = Instant::now()
                            + Duration::from_millis(
                                (*refresh_thread.lock().unwrap()).clamp(200, 60_000),
                            );
                    }
                    // 服务映射（SCM 枚举）独立于采样节奏：30s 刷一次足够
                    //（服务启停罕见；首轮回填保证窗口打开即有 svchost 服务名）
                    if Instant::now() >= svc_deadline {
                        *services_thread.lock().unwrap() = service_map();
                        svc_deadline = Instant::now() + Duration::from_secs(30);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            })?;

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
        let shared_sort = shared.clone();
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
                render(&ui, &cache_sort, &sort_for_cb, &search_sort, &expanded_sort, &shared_sort);
            }
        });

        // 搜索框 → 保存关键字 + 立即用缓存重绘（不重采样）
        let search_for_cb = search.clone();
        let weak_search = ui.as_weak();
        let cache_search = cache_tick.clone();
        let sort_search = sort.clone();
        let expanded_search = expanded.clone();
        let shared_search = shared.clone();
        ui.on_search_changed(move |text: slint::SharedString| {
            *search_for_cb.lock().unwrap() = text.to_string();
            if let Some(ui) = weak_search.upgrade() {
                render(&ui, &cache_search, &sort_search, &search_for_cb, &expanded_search, &shared_search);
            }
        });

        // 刷新按钮 → 通知采样线程立即采样 + 等完成 + 重绘
        let weak_refresh = ui.as_weak();
        let cache_refresh = cache_tick.clone();
        let sort_refresh = sort.clone();
        let search_refresh = search.clone();
        let expanded_refresh = expanded.clone();
        let shared_refresh = shared.clone();
        let sample_now_btn = sample_now.clone();
        ui.on_refresh_requested(move || {
            // 非阻塞（与 refresh() 一致）：立即用当前缓存重绘 + 通知采样线程
            // 尽快采样；不在 UI 线程死等采样完成（旧实现轮询等 3s，首轮
            // sysinfo 初始化慢时窗口整个冻结）。
            if let Some(ui) = weak_refresh.upgrade() {
                render(&ui, &cache_refresh, &sort_refresh, &search_refresh, &expanded_refresh, &shared_refresh);
                sample_now_btn.store(true, Ordering::SeqCst);
                // 采样线程 50ms 轮询 + 采样耗时：延迟几次重绘把新数据补上
                // （常规采样 <500ms；首轮初始化慢时由 tick timer 兜底）
                for delay_ms in [300u64, 1000, 2000] {
                    let weak_late = weak_refresh.clone();
                    let cache_late = cache_refresh.clone();
                    let sort_late = sort_refresh.clone();
                    let search_late = search_refresh.clone();
                    let expanded_late = expanded_refresh.clone();
                    let shared_late = shared_refresh.clone();
                    slint::Timer::single_shot(Duration::from_millis(delay_ms), move || {
                        if let Some(ui) = weak_late.upgrade() {
                            render(&ui, &cache_late, &sort_late, &search_late, &expanded_late, &shared_late);
                        }
                    });
                }
            }
        });

        // 行右键菜单 → 顶部标题 + 「打开文件所在的位置」/「停止进程」（原生菜单）
        // 弹出位置：show_row_menu_once 内部用 GetCursorPos() 取鼠标屏幕坐标。
        // 连续右键切换：菜单被右键点击外部关闭时，命中测试鼠标新位置，
        // 若落在另一行则立即重弹该行菜单（无需先关闭再右键）。
        let weak_rowmenu = ui.as_weak();
        let cache_rowmenu = cache_tick.clone();
        ui.on_row_context_menu(move |pid: i32, name: slint::SharedString| {
            use crate::window::{RowMenuCmd, RowMenuOutcome};
            log::info!("row-context-menu: pid={} name={}", pid, name);
            if let Some(ui) = weak_rowmenu.upgrade() {
                let mut cur_pid = pid;
                let mut cur_name = name.to_string();
                loop {
                    match crate::window::show_row_menu_once(ui.window(), cur_pid, &cur_name) {
                        RowMenuOutcome::Command(RowMenuCmd::Kill) => {
                            match kill_process(cur_pid as u32) {
                                Ok(()) => log::info!("停止进程 {} 成功", cur_pid),
                                Err(e) => {
                                    log::warn!("停止进程 {} 失败: {:?}", cur_pid, e);
                                    // 失败提示：权限不足 → 弹管理员提权重试框
                                    prompt_kill_failure(cur_pid as u32, &cur_name, e);
                                }
                            }
                            break; // 下一次刷新自动更新
                        }
                        RowMenuOutcome::Command(RowMenuCmd::KillTree) => {
                            // 结束进程树：沿 parent_pid 递归枚举后代并逐个终止
                            let rows = cache_rowmenu.lock().unwrap().clone();
                            let (killed, err) =
                                kill_process_tree(cur_pid as u32, &rows);
                            log::info!(
                                "结束进程树 {} ({}): 已终止 {} 个进程",
                                cur_name,
                                cur_pid,
                                killed
                            );
                            if matches!(err, Some(KillError::Permission)) {
                                // 部分子树权限不足 → 弹 UAC 提权重试
                                // （连同子树一起终止）
                                prompt_kill_tree_failure(cur_pid as u32, &cur_name);
                            }
                            break; // 下一次刷新自动更新
                        }
                        RowMenuOutcome::Command(RowMenuCmd::OpenLocation) => {
                            match open_process_location(cur_pid as u32) {
                                Ok(()) => log::info!("打开文件所在的位置 PID {} 成功", cur_pid),
                                Err(e) => {
                                    log::warn!("打开文件所在的位置 PID {} 失败: {}", cur_pid, e)
                                }
                            }
                            break;
                        }
                        RowMenuOutcome::Switch => {
                            // 右键点击菜单外关闭 → 尝试切换到新位置的行
                            // 等右键释放（若用户按住右键移动）
                            crate::window::wait_rbutton_release();
                            // 命中测试：鼠标当前位置落在哪一行 → 重弹该行菜单
                            let mut pt = windows::Win32::Foundation::POINT::default();
                            let _ = unsafe {
                                windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt)
                            };
                            match hit_test_row(&ui, pt.x, pt.y) {
                                Some((p, n)) => {
                                    cur_pid = p;
                                    cur_name = n;
                                    log::info!("菜单切换到 pid={} name={}", cur_pid, cur_name);
                                    continue; // 重弹新行菜单
                                }
                                None => break,
                            }
                        }
                        RowMenuOutcome::Cancelled => break, // 左键/Esc 关闭
                    }
                }
            }
        });

        // 聚合父节点点击 → 展开/收起 + 用缓存重绘
        let expanded_for_cb = expanded.clone();
        let weak_expand_ui = ui.as_weak();
        let cache_expand = cache_tick.clone();
        let sort_expand = sort.clone();
        let search_expand = search.clone();
        let shared_expand = shared.clone();
        ui.on_group_toggle(move |key: slint::SharedString| {
            {
                let mut set = expanded_for_cb.lock().unwrap();
                let k = key.to_ascii_lowercase();
                if !set.insert(k.clone()) {
                    set.remove(&k);
                }
            }
            if let Some(ui) = weak_expand_ui.upgrade() {
                render(&ui, &cache_expand, &sort_expand, &search_expand, &expanded_for_cb, &shared_expand);
            }
        });

        // 双击行 → 进程详情面板（350ms 内同 pid 再次单击判定为双击）
        let last_click: Arc<Mutex<(u32, Instant)>> =
            Arc::new(Mutex::new((0, Instant::now() - Duration::from_secs(1))));
        let last_click_for_cb = last_click.clone();
        let cache_click = cache_tick.clone();
        let nb_cpus_click = nb_cpus.clone();
        let detail_text_click = detail_text.clone();
        let weak_click_ui = ui.as_weak();
        ui.on_row_clicked(move |pid: i32| {
            let pid_u32 = pid as u32;
            let now = Instant::now();
            let is_double = {
                let mut lc = last_click_for_cb.lock().unwrap();
                let dbl = lc.0 == pid_u32 && now.duration_since(lc.1) < Duration::from_millis(350);
                lc.0 = pid_u32;
                lc.1 = now;
                dbl
            };
            if !is_double {
                return; // 单击无动作（等第二次点击判定双击）
            }
            if let Some(ui) = weak_click_ui.upgrade() {
                let row = cache_click
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|r| r.pid == pid_u32)
                    .cloned();
                let name = row
                    .as_ref()
                    .map(|r| r.name.clone())
                    .unwrap_or_else(|| format!("PID {}", pid_u32));
                let detail = process_detail(
                    pid_u32,
                    &name,
                    row.as_ref(),
                    *nb_cpus_click.lock().unwrap(),
                );
                *detail_text_click.lock().unwrap() = detail.clone(); // 供「复制」按钮使用
                ui.set_detail_title(SharedString::from(format!("{} (PID {})", name, pid_u32)));
                ui.set_detail_text(SharedString::from(detail));
                ui.set_detail_visible(true);
                log::info!("双击进程 {} 打开详情", pid_u32);
            }
        });

        // 关闭详情面板
        let weak_detail_ui = ui.as_weak();
        ui.on_detail_close(move || {
            if let Some(ui) = weak_detail_ui.upgrade() {
                ui.set_detail_visible(false);
            }
        });

        // 详情面板「复制」按钮 → 全量复制详情文本到剪贴板
        let detail_text_copy = detail_text.clone();
        ui.on_detail_copy(move || {
            let text = detail_text_copy.lock().unwrap().clone();
            if text.is_empty() {
                log::warn!("复制详情：无可复制内容");
                return;
            }
            set_clipboard_text(&text);
            log::info!("已复制进程详情到剪贴板（{} 字节）", text.len());
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
        // 初始化标题栏：刷新间隔下拉索引 + header 显示当前间隔
        ui.set_refresh_interval_index(interval_ms_to_index(*refresh_ms.lock().unwrap()));
        ui.set_header_text(SharedString::from(format!(
            "进程详情（{} 自动刷新）",
            interval_ms_to_text(*refresh_ms.lock().unwrap())
        )));
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

        // 自动刷新：首帧用 1s 快速间隔。采样线程首轮 sysinfo 初始化可能
        // 耗时数秒，快速 tick 保证窗口一打开、数据一就绪就立即渲染；
        // 一旦渲染到非空数据，切换回用户配置的刷新间隔（标题栏下拉可调）。
        let weak_tick = ui.as_weak();
        let sort_tick = sort.clone();
        let search_tick = search.clone();
        let expanded_tick = expanded.clone();
        let shared_tick = shared.clone();
        let refresh_ms_tick = refresh_ms.clone();
        let cache_data_tick = cache_tick.clone();
        let tick_timer = Arc::new(slint::Timer::default());
        let timer_for_first = tick_timer.clone();
        let first_done = Arc::new(AtomicBool::new(false));
        let first_done_cb = first_done.clone();
        tick_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(1000),
            move || {
                if let Some(ui) = weak_tick.upgrade() {
                    render(
                        &ui,
                        &cache_tick,
                        &sort_tick,
                        &search_tick,
                        &expanded_tick,
                        &shared_tick,
                    );
                    // 首帧渲染到非空数据 → 恢复正常刷新节奏（只切一次，
                    // 之后间隔由标题栏下拉回调管理）
                    let has_data = !cache_data_tick.lock().unwrap().is_empty();
                    if has_data && !first_done_cb.swap(true, Ordering::SeqCst) {
                        timer_for_first.set_interval(Duration::from_millis(
                            *refresh_ms_tick.lock().unwrap(),
                        ));
                    }
                }
            },
        );

        let enable_flag = drag_allowed.clone();
        slint::Timer::single_shot(Duration::from_millis(300), move || {
            enable_flag.store(true, Ordering::SeqCst);
        });

        // 刷新间隔下拉 → 更新状态 + 重启 timer（slint Timer 可改 interval 后 restart）
        let refresh_ms_for_cb = refresh_ms.clone();
        let timer_for_cb = tick_timer.clone();
        let weak_interval_ui = ui.as_weak();
        ui.on_refresh_interval_changed(move |value: slint::SharedString| {
            let ms = interval_text_to_ms(value.as_str());
            *refresh_ms_for_cb.lock().unwrap() = ms;
            timer_for_cb.set_interval(Duration::from_millis(ms));
            timer_for_cb.restart();
            log::info!("刷新间隔改为 {}ms", ms);
            if let Some(ui) = weak_interval_ui.upgrade() {
                ui.set_header_text(SharedString::from(format!(
                    "进程详情（{} 自动刷新）",
                    interval_ms_to_text(ms)
                )));
            }
        });

        Ok(Self {
            ui,
            // 复用与采样线程共享的 cache Arc（而非 Mutex::new(Vec::new())）：
            // 否则 refresh() 用 self.cache 渲染时读不到采样线程写入的数据，
            // 进程详情页第一次打开（lib.rs 创建后立即 refresh()）会显示空列表。
            cache,
            sampler,
            sort,
            search,
            expanded,
            shared,
            detail_text,
            _refresh_ms: refresh_ms,
            stop_sampling,
            sample_now,
            cache_version,
            sampler_handle: Some(sampler_handle),
            _timer: tick_timer,
        })
    }

    /// 立即刷新：用当前缓存重绘 + 通知采样线程尽快采样（若窗口已关闭则先显示）。
    ///
    /// 非阻塞：不在 UI 线程同步死等采样完成（旧实现等 3s，首轮 sysinfo
    /// 初始化可能耗时数秒 → 超时后渲染空缓存 → 首次显示空列表，且 3s 阻塞
    /// 发生在菜单回调所在线程导致窗口卡顿）。数据未就绪时由 show() 里注册的
    /// 1s 快速 tick 在采样完成后自动补齐渲染。
    pub fn refresh(&self) {
        if !self.ui.window().is_visible() {
            let _ = self.ui.show();
            // show 可能触发 winit 重算样式 → 重新确保不在任务栏显示
            crate::window::ensure_tool_window_for(self.ui.window());
        }
        render(
            &self.ui,
            &*self.cache,
            &self.sort,
            &self.search,
            &self.expanded,
            &self.shared,
        );
        // 通知采样线程尽快采样（异步，不等待）；不直接 sample()（避免与
        // 后台采样线程共享 ProcessSampler 交错污染速率差分基线）
        self.sample_now.store(true, Ordering::SeqCst);
    }

    /// 底层 Slint 窗口（供 tick 守护重新设置任务栏样式）。
    pub fn window(&self) -> &slint::Window {
        self.ui.window()
    }
}

impl Drop for ProcessListWindow {
    fn drop(&mut self) {
        // 停止采样线程（线程内 50ms 分段 sleep，join 很快返回，不阻塞 UI）
        self.stop_sampling.store(true, Ordering::SeqCst);
        if let Some(h) = self.sampler_handle.take() {
            let _ = h.join();
        }
    }
}

/// 刷新间隔档位（毫秒 → 下拉显示文本）。
/// 下拉 ComboBox（overlay.slint）、索引换算、header 显示都从这张表派生，
/// 改档位只动这一处。
const INTERVAL_OPTIONS: [(u64, &str); 5] = [
    (500, "0.5s"),
    (1_000, "1s"),
    (2_000, "2s"),
    (5_000, "5s"),
    (30_000, "30s"),
];

/// 刷新间隔毫秒 → 下拉索引（取最接近的档位）。
fn interval_ms_to_index(ms: u64) -> i32 {
    INTERVAL_OPTIONS
        .iter()
        .enumerate()
        .min_by_key(|(_, (opt, _))| opt.abs_diff(ms))
        .map(|(i, _)| i as i32)
        .unwrap_or((INTERVAL_OPTIONS.len() - 1) as i32)
}

/// 下拉文本 → 毫秒（未知文本回退 30s）。
fn interval_text_to_ms(text: &str) -> u64 {
    INTERVAL_OPTIONS
        .iter()
        .find(|(_, label)| *label == text)
        .map(|(ms, _)| *ms)
        .unwrap_or(30_000)
}

/// 毫秒 → 下拉显示文本（header 用）。
fn interval_ms_to_text(ms: u64) -> &'static str {
    INTERVAL_OPTIONS[interval_ms_to_index(ms) as usize].1
}

/// 进程列表布局常量（逻辑像素）——与 overlay.slint `ProcessList` 布局**双源**：
/// 列表从标题栏（~70px）下方开始，行高 29px。改动 slint 布局时需同步此处
/// （hit_test_row 命中测试依赖）。
const LIST_TOP: f32 = 70.0;
const ROW_HEIGHT: f32 = 29.0;

/// 渲染共享的只读状态（各回调 clone 一份 `Arc<RenderShared>` 传入 render，
/// 避免每个回调各自 clone 4 个 Arc）。
struct RenderShared {
    nb_cpus: Arc<Mutex<usize>>,
    highlight_pct: Arc<Mutex<f32>>,
    total_mem: Arc<Mutex<u64>>,
    model_arc: Arc<Mutex<Option<std::rc::Rc<slint::VecModel<crate::ProcessRowData>>>>>,
    /// 服务映射缓存（pid → 服务名列表，svchost 聚合用）。
    /// 由采样线程按 30s TTL 刷新（SCM 枚举 10~100ms，不能每 render 都查——
    /// 搜索框每敲一个字符都会触发 render）。
    services: Arc<Mutex<std::collections::HashMap<u32, Vec<String>>>>,
}

/// 用缓存渲染列表（不采样）：过滤 + 分组 + 排序 + 展开 + 填充。
/// 搜索 / 展开 / 收起 / 排序 / 定时刷新都走这里 —— 只读缓存，毫秒级。
/// `shared`：渲染共享状态（CPU 核数 / 高亮阈值 / 总内存 / 增量 model）。
/// model 增量更新：行数一致时只 `set_row_data` 变化行，行数变化时全量重建
/// ——避免每 tick 全量替换 model 的卡顿。
#[allow(clippy::too_many_arguments)]
fn render(
    ui: &crate::ProcessList,
    cache: &Mutex<Vec<ProcessRow>>,
    sort: &Mutex<(String, bool)>,
    search: &Mutex<String>,
    expanded: &Mutex<std::collections::HashSet<String>>,
    shared: &RenderShared,
) {
    use slint::Model as _; // row_count / row_data（trait 方法）
    let nb_cpus = *shared.nb_cpus.lock().unwrap();
    let highlight_pct = *shared.highlight_pct.lock().unwrap();
    let total_mem = *shared.total_mem.lock().unwrap();
    let rows = cache.lock().unwrap().clone();

    // 服务映射（svchost 特殊聚合用）：读采样线程维护的缓存（SCM 枚举
    // 10~100ms 且服务列表变化极慢，不应每 render/每按键都查一次）
    let services = shared.services.lock().unwrap().clone();

    // 分组 + 组排序
    let (column, ascending) = {
        let s = sort.lock().unwrap();
        (s.0.clone(), s.1)
    };
    let mut groups = group_processes(&rows, &services);
    sort_groups(&mut groups, &column, ascending);
    ui.set_sort_column(SharedString::from(column));
    ui.set_sort_ascending(ascending);

    // 搜索过滤（按名称 / PID；端口号关键字 → 查占用进程；过滤发生在分组后，保留组结构）
    let keyword = search.lock().unwrap().clone();
    if !keyword.trim().is_empty() {
        // 纯数字关键字（如 8080）→ 先按端口号搜索；端口无命中时回退普通
        // 名称/PID 搜索（避免端口搜索覆盖 PID 搜索——例如输入 PID 135 而
        // 端口 135 空闲时，应仍能按 PID 匹配到进程）
        let mut port_pids = std::collections::HashSet::new();
        if let Some(port) = parse_port(&keyword) {
            if port > 0 {
                for (pid, _, _, _) in port_owners(port) {
                    port_pids.insert(pid);
                }
            }
        }
        groups = filter_groups(&groups, &keyword, &port_pids);
    }

    // 展开状态集合
    let expanded_set = expanded.lock().unwrap().clone();

    // 构建显示列表：父节点（聚合）+ 展开的子进程 / 服务
    let mut items: Vec<crate::ProcessRowData> = Vec::new();
    for g in &groups {
        if g.children.is_empty() && g.services.is_empty() {
            // 单实例：普通行
            items.push(row_display(&g.root, nb_cpus, highlight_pct, false));
        } else {
            // 多实例 / 服务宿主：父节点
            let agg = group_aggregate(g);
            items.push(group_display(g, &agg, nb_cpus, highlight_pct, total_mem));
            if expanded_set.contains(&group_key(g)) {
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
                            false,
                            String::new(),
                            String::new(),
                            false,
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                            false,
                            0,
                        ));
                    }
                }
                // 再展开子进程
                for r in &g.children {
                    items.push(row_display(r, nb_cpus, highlight_pct, true));
                }
            }
        }
    }
    // 增量更新 model：行数一致 → 只 set_row_data 变化行；行数变化 → 全量重建
    let mut guard = shared.model_arc.lock().unwrap();
    match guard.as_ref() {
        Some(model) if model.row_count() == items.len() => {
            let mut changed = 0;
            for (i, item) in items.iter().enumerate() {
                if model.row_data(i).as_ref() != Some(item) {
                    model.set_row_data(i, item.clone());
                    changed += 1;
                }
            }
            log::trace!("增量更新 {} 行", changed);
        }
        _ => {
            let vec_model = slint::VecModel::from(items);
            let rc = std::rc::Rc::new(vec_model);
            ui.set_process_model(slint::ModelRc::from(rc.clone()));
            *guard = Some(rc);
        }
    }
}

/// 按关键字过滤聚合组（组内名称 / PID 匹配即保留；纯函数，可单测）。
/// `port_pids`：端口号搜索命中进程的 PID 集合；非空时按该集合过滤
/// （此时 keyword 是端口号，忽略名称匹配），空集合并入名称/PID 匹配。
fn filter_groups(
    groups: &[GroupedProcess],
    keyword: &str,
    port_pids: &std::collections::HashSet<u32>,
) -> Vec<GroupedProcess> {
    if !port_pids.is_empty() {
        // 端口搜索：保留含命中 PID 的组（命中子进程也保留整组）
        return groups
            .iter()
            .filter(|g| {
                port_pids.contains(&g.root.pid)
                    || g.children.iter().any(|c| port_pids.contains(&c.pid))
            })
            .cloned()
            .collect();
    }
    let kw = keyword.trim().to_ascii_lowercase();
    groups
        .iter()
        .filter(|g| {
            g.name.to_ascii_lowercase().contains(&kw)
                || g.root.pid.to_string().contains(&kw)
                || g.root.display_name.to_ascii_lowercase().contains(&kw)
                || g.children.iter().any(|c| {
                    c.pid.to_string().contains(&kw)
                        || c.display_name.to_ascii_lowercase().contains(&kw)
                })
        })
        .cloned()
        .collect()
}

/// 判断关键字是否是合法端口号（1-65535 的纯数字）。
fn parse_port(kw: &str) -> Option<u16> {
    let t = kw.trim();
    if t.is_empty() || t.len() > 5 {
        return None;
    }
    if !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    t.parse::<u16>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, name: &str, cpu: f32, mem_mb: u64) -> ProcessRow {
        ProcessRow {
            pid,
            parent_pid: 0,
            name: name.into(),
            display_name: name.into(),
            cpu_usage: cpu,
            memory_bytes: mem_mb * 1024 * 1024,
            physical_mem_bytes: mem_mb * 1024 * 1024,
            memory_pct: 0.0,
            disk_read_bps: 0,
            disk_write_bps: 0,
            net_bps: 0,
            net_total_bytes: 0,
            ports: Vec::new(),
            status: "运行中".into(),
        }
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
    fn group_multi_root_creates_separate_groups() {
        // 同名多 root：3 个 chrome 都被外部进程（explorer/不同父）拉起 → 3 个独立组
        let rows = vec![
            row_p(100, 500, "chrome.exe", 10.0, 100), // 500 外部
            row_p(200, 501, "chrome.exe", 20.0, 200), // 501 外部
            row_p(300, 502, "chrome.exe", 30.0, 300), // 502 外部
        ];
        let groups = group_processes(&rows, &no_services());
        let chromes: Vec<&GroupedProcess> =
            groups.iter().filter(|g| g.name == "chrome.exe").collect();
        assert_eq!(chromes.len(), 3, "每个 root 应生成独立组");
        for g in &chromes {
            assert!(g.children.is_empty());
        }
        // root PID 集合完整
        let mut pids: Vec<u32> = chromes.iter().map(|g| g.root.pid).collect();
        pids.sort();
        assert_eq!(pids, vec![100, 200, 300]);
    }

    #[test]
    fn group_children_attach_by_ppid_chain() {
        // 两个 chrome root（100、300，父进程 900/901 不在组内）+ 各自子进程
        // + 孙进程（500 → 200 → 100）：子进程按 PPID 链归属，孙进程扁平化
        let rows = vec![
            row_p(100, 900, "chrome.exe", 10.0, 100),  // root（900 外部）
            row_p(200, 100, "chrome.exe", 20.0, 200),  // 100 的子
            row_p(300, 901, "chrome.exe", 30.0, 300),  // root（901 外部）
            row_p(400, 300, "chrome.exe", 40.0, 400),  // 300 的子
            row_p(500, 200, "chrome.exe", 50.0, 500),  // 孙：200 → 100
        ];
        let groups = group_processes(&rows, &no_services());
        let chromes: Vec<&GroupedProcess> =
            groups.iter().filter(|g| g.name == "chrome.exe").collect();
        assert_eq!(chromes.len(), 2);
        let g100 = chromes.iter().find(|g| g.root.pid == 100).unwrap();
        let mut c100: Vec<u32> = g100.children.iter().map(|c| c.pid).collect();
        c100.sort();
        assert_eq!(c100, vec![200, 500], "200 直接子、500 孙进程都应归 100");
        let g300 = chromes.iter().find(|g| g.root.pid == 300).unwrap();
        assert_eq!(g300.children.len(), 1);
        assert_eq!(g300.children[0].pid, 400);
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
        assert_eq!(agg.physical_mem_bytes, 400 * 1024 * 1024);
        assert_eq!(agg.pid, 1); // 保持 root pid（组行右键「结束进程树」沿 root 枚举）
    }

    #[test]
    fn group_aggregate_keeps_root_pid_even_if_child_smaller() {
        // pid 回绕后子进程 pid 可能比 root 小：聚合行仍必须是 root pid，
        // 否则组行右键「结束进程树」会枚举错子树
        let rows = vec![
            row_p(500, 0, "a.exe", 10.0, 100), // root
            row_p(50, 500, "a.exe", 30.0, 300), // 子进程 pid 更小
        ];
        let groups = group_processes(&rows, &no_services());
        let g = groups.iter().find(|g| g.root.pid == 500).unwrap();
        assert_eq!(group_aggregate(g).pid, 500);
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
    fn sort_groups_by_pmem() {
        // 物理内存列：提交大小相同、专用工作集不同 → 按专用工作集排序
        let mut rows = vec![
            row(1, "a.exe", 10.0, 100),
            row(2, "b.exe", 10.0, 100),
            row(3, "c.exe", 10.0, 100),
        ];
        rows[1].physical_mem_bytes = 900 * 1024 * 1024; // b 专用工作集最大
        rows[2].physical_mem_bytes = 500 * 1024 * 1024; // c 次之
        let mut groups = group_processes(&rows, &no_services());
        sort_groups(&mut groups, "pmem", false);
        assert_eq!(groups[0].name, "b.exe");
        assert_eq!(groups[1].name, "c.exe");
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
        let f = filter_groups(&groups, "chrome", &std::collections::HashSet::new());
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].name, "chrome.exe");
    }

    #[test]
    fn filter_groups_by_child_pid() {
        let g = build_chrome_group();
        let groups = vec![g];
        let f = filter_groups(&groups, "200", &std::collections::HashSet::new()); // 子进程 PID
        assert_eq!(f.len(), 1);
    }

    #[test]
    fn filter_groups_empty_keyword_all() {
        let groups = vec![build_chrome_group()];
        assert_eq!(filter_groups(&groups, "", &std::collections::HashSet::new()).len(), 1);
        assert_eq!(filter_groups(&groups, "  ", &std::collections::HashSet::new()).len(), 1);
    }

    #[test]
    fn filter_groups_no_match_empty() {
        let groups = vec![build_chrome_group()];
        assert!(filter_groups(&groups, "zzz", &std::collections::HashSet::new()).is_empty());
    }

    #[test]
    fn parse_port_accepts_valid_numbers() {
        assert_eq!(parse_port("80"), Some(80));
        assert_eq!(parse_port(" 8080 "), Some(8080));
        assert_eq!(parse_port("65535"), Some(65535));
        assert_eq!(parse_port("1"), Some(1));
    }

    #[test]
    fn parse_port_rejects_invalid() {
        assert_eq!(parse_port(""), None);
        assert_eq!(parse_port("abc"), None);
        assert_eq!(parse_port("80a"), None);
        assert_eq!(parse_port("65536"), None); // 超出 u16
        assert_eq!(parse_port("123456"), None); // 超长
        assert_eq!(parse_port("12.5"), None);
    }

    #[test]
    fn filter_groups_by_port_pids() {
        // 端口搜索：命中 PID 集合 → 保留含命中进程的组
        let mut g = build_chrome_group();
        g.children.push(row_p(300, 100, "chrome.exe", 5.0, 50));
        let groups = vec![
            g,
            GroupedProcess {
                name: "explorer.exe".into(),
                root: row_p(400, 0, "explorer.exe", 1.0, 10),
                children: vec![],
                services: vec![],
            },
        ];
        // 命中子进程 300 → chrome 组保留
        let mut pids = std::collections::HashSet::new();
        pids.insert(300);
        let f = filter_groups(&groups, "8080", &pids);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].name, "chrome.exe");
        // 命中 400 → explorer 组保留
        let mut pids2 = std::collections::HashSet::new();
        pids2.insert(400);
        let f2 = filter_groups(&groups, "8080", &pids2);
        assert_eq!(f2.len(), 1);
        assert_eq!(f2[0].name, "explorer.exe");
        // 无命中 → 空
        let f3 = filter_groups(&groups, "8080", &std::collections::HashSet::new());
        assert!(f3.is_empty());
    }

    #[test]
    fn sort_groups_by_status() {
        // status 按名称排序（纯字符串比较）
        let rows = vec![row(1, "zzz.exe", 10.0, 100), row(2, "aaa.exe", 50.0, 200)];
        let mut groups = group_processes(&rows, &no_services());
        sort_groups(&mut groups, "status", true);
        assert_eq!(groups[0].name, "aaa.exe");
    }

    // ===== 端口（port_map / format_ports_* / sort by port）=====

    #[test]
    fn format_ports_short_covers_all_cases() {
        assert_eq!(format_ports_short(&[]), "—");
        assert_eq!(format_ports_short(&[80]), "80");
        assert_eq!(format_ports_short(&[80, 443]), "80, 443");
        assert_eq!(format_ports_short(&[80, 443, 8080]), "80, 443 +1");
        assert_eq!(format_ports_short(&[1, 2, 3, 4, 5]), "1, 2 +3");
    }

    #[test]
    fn format_ports_full_joins_all() {
        assert_eq!(format_ports_full(&[]), "无监听端口");
        assert_eq!(format_ports_full(&[80]), "80");
        assert_eq!(format_ports_full(&[80, 443, 8080]), "80, 443, 8080");
    }

    #[test]
    fn merge_ports_dedups_and_sorts() {
        assert_eq!(merge_ports(&[]), Vec::<u16>::new());
        assert_eq!(merge_ports(&[vec![443, 80]]), vec![80, 443]);
        // 跨多进程去重 + 升序
        assert_eq!(
            merge_ports(&[vec![443, 80], vec![80, 8080], vec![9090]]),
            vec![80, 443, 8080, 9090]
        );
    }

    #[test]
    fn sort_groups_by_port_merges_children() {
        // 聚合端口：root + children 合并去重升序 → 排序
        let mut r1 = row_p(1, 0, "svc.exe", 10.0, 100);
        r1.ports = vec![80];
        let mut c1 = row_p(2, 1, "svc.exe", 5.0, 50);
        c1.ports = vec![443, 8080]; // 与 root 合并 → 3 端口
        let mut r2 = row_p(3, 0, "empty.exe", 20.0, 200);
        r2.ports = vec![]; // 0 端口
        let rows = vec![r1, c1, r2];
        let mut groups = group_processes(&rows, &no_services());
        sort_groups(&mut groups, "port", false);
        // svc.exe(3 端口, 最小 80) 排第一，empty.exe(0 端口) 排第二
        assert_eq!(groups[0].name, "svc.exe");
        assert_eq!(groups[1].name, "empty.exe");
    }

    // ===== collect_process_tree（结束进程树的后代枚举）=====

    /// 构造一行进程（仅 pid / parent_pid / name 参与树枚举）。
    fn row_tree(pid: u32, ppid: u32) -> ProcessRow {
        let name = format!("p{}.exe", pid);
        ProcessRow {
            pid,
            parent_pid: ppid,
            name: name.clone(),
            display_name: name,
            cpu_usage: 0.0,
            memory_bytes: 0,
            physical_mem_bytes: 0,
            memory_pct: 0.0,
            disk_read_bps: 0,
            disk_write_bps: 0,
            net_bps: 0,
            net_total_bytes: 0,
            ports: vec![],
            status: String::new(),
        }
    }

    #[test]
    fn collect_process_tree_basic_chain() {
        // 1 -> 2 -> 3 -> 4 （线性链）
        let rows = vec![row_tree(1, 0), row_tree(2, 1), row_tree(3, 2), row_tree(4, 3)];
        let order = collect_process_tree(1, &rows);
        // 全部后代（2,3,4）被收集
        assert_eq!(order.len(), 3);
        assert!(order.contains(&2));
        assert!(order.contains(&3));
        assert!(order.contains(&4));
        // 叶子(4) 先于其父(3) 先于其祖父(2)：子先于父
        let pos = |p: u32| order.iter().position(|&x| x == p).unwrap();
        assert!(pos(4) < pos(3));
        assert!(pos(3) < pos(2));
        // root(1) 不在列表（最后单独杀）
        assert!(!order.contains(&1));
    }

    #[test]
    fn collect_process_tree_fanout_and_cycle_safe() {
        // 1 有两个子 2、3；3 又有子 4；另造一个指向自身的环 9 -> 9
        let rows = vec![
            row_tree(1, 0),
            row_tree(2, 1),
            row_tree(3, 1),
            row_tree(4, 3),
            row_tree(9, 9), // 自环，不在 1 的子树内，不应被收集
        ];
        let order = collect_process_tree(1, &rows);
        // 仅 1 的子树：2,3,4
        assert_eq!(order.len(), 3);
        assert!(order.contains(&2));
        assert!(order.contains(&3));
        assert!(order.contains(&4));
        // 叶子(2,4) 先于父(3)
        let pos = |p: u32| order.iter().position(|&x| x == p).unwrap();
        assert!(pos(4) < pos(3));
        assert!(pos(2) < pos(3));
        // 自环节点不被收集
        assert!(!order.contains(&9));
    }

    #[test]
    fn collect_process_tree_no_children() {
        // 孤立进程：无任何后代
        let rows = vec![row_tree(7, 0), row_tree(8, 1)];
        let order = collect_process_tree(7, &rows);
        assert!(order.is_empty());
    }

    // ===== 应用友好名（P6）：展示逻辑 =====

    #[test]
    fn row_display_uses_friendly_name() {
        // 展示名优先用友好名；悬浮 full_name 保留原始 exe 名作为备用
        let mut r = row(1, "chrome.exe", 10.0, 100);
        r.display_name = "Google Chrome".to_string();
        let d = row_display(&r, 8, 30.0, false);
        assert_eq!(d.name, "Google Chrome");
        assert!(d.name_full.contains("chrome.exe"));
        // 无友好名时回退 exe 名
        let r2 = row(2, "svchost.exe", 1.0, 10);
        let d2 = row_display(&r2, 8, 30.0, false);
        assert_eq!(d2.name, "svchost.exe");
    }

    #[test]
    fn group_display_uses_friendly_name() {
        // 聚合行标题优先用友好名，full_name 保留原始 exe 名；group_key 仍用 exe 名
        let mut root = row_p(100, 0, "chrome.exe", 10.0, 100);
        root.display_name = "Google Chrome".to_string();
        let g = GroupedProcess {
            name: "chrome.exe".into(),
            root,
            children: vec![],
            services: vec![],
        };
        let agg = group_aggregate(&g);
        let d = group_display(&g, &agg, 8, 30.0, 16 * 1024 * 1024 * 1024);
        assert!(d.name.contains("Google Chrome"));
        assert!(d.name_full.contains("chrome.exe"));
        assert_eq!(d.group_key, "chrome.exe:100", "展开键应含 root pid（唯一化）");
    }

    // ===== Chromium 家族标注（WebView2 / Edge / Chrome 角色 + 宿主）=====

    /// 命令行参数辅助：&str 切片 → OsString 数组
    fn cmd(args: &[&str]) -> Vec<std::ffi::OsString> {
        args.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn chromium_role_browser_when_no_type() {
        // 无 --type 或 --type=browser 都是浏览器主进程
        assert_eq!(chromium_role_from_args(&cmd(&[])), ChromiumRole::Browser);
        assert_eq!(
            chromium_role_from_args(&cmd(&["--type=browser", "--foo"])),
            ChromiumRole::Browser
        );
    }

    #[test]
    fn chromium_role_renderer_gpu_crashpad() {
        assert_eq!(
            chromium_role_from_args(&cmd(&["--type=renderer"])),
            ChromiumRole::Renderer
        );
        assert_eq!(
            chromium_role_from_args(&cmd(&["--type=gpu-process"])),
            ChromiumRole::Gpu
        );
        assert_eq!(
            chromium_role_from_args(&cmd(&["--type=crashpad-handler"])),
            ChromiumRole::Crashpad
        );
    }

    #[test]
    fn chromium_role_utility_maps_subtype() {
        // 常见 utility 子类型 → 可读名（对齐任务管理器）
        assert_eq!(
            chromium_role_from_args(&cmd(&[
                "--type=utility",
                "--utility-sub-type=network.mojom.NetworkService",
            ])),
            ChromiumRole::Utility("Network Service".into())
        );
        assert_eq!(
            chromium_role_from_args(&cmd(&[
                "--type=utility",
                "--utility-sub-type=storage.mojom.StorageService",
            ])),
            ChromiumRole::Utility("Storage Service".into())
        );
        // 未知子类型 → 取最后一个 '.' 后的段
        assert_eq!(
            chromium_role_from_args(&cmd(&[
                "--type=utility",
                "--utility-sub-type=mojom.SomeOddThing",
            ])),
            ChromiumRole::Utility("SomeOddThing".into())
        );
        // 无子类型 → 空串
        assert_eq!(
            chromium_role_from_args(&cmd(&["--type=utility"])),
            ChromiumRole::Utility(String::new())
        );
    }

    #[test]
    fn chromium_role_unknown_type_kept() {
        assert_eq!(
            chromium_role_from_args(&cmd(&["--type=zygote"])),
            ChromiumRole::Other("zygote".into())
        );
    }

    #[test]
    fn chromium_family_prefix_and_check() {
        assert_eq!(chromium_family_prefix("msedgewebview2.exe"), Some("WebView2"));
        assert_eq!(chromium_family_prefix("MSEDGEWEBVIEW2.EXE"), Some("WebView2"));
        assert_eq!(chromium_family_prefix("msedge.exe"), Some("Edge"));
        assert_eq!(chromium_family_prefix("chrome.exe"), Some("Chrome"));
        assert_eq!(chromium_family_prefix("markflowy.exe"), None);
        assert!(is_chromium_family("chrome.exe"));
        assert!(!is_chromium_family("explorer.exe"));
    }

    #[test]
    fn annotate_webview2_browser_with_host() {
        // markflowy(宿主, PID 100) → WebView2 浏览器(200) → 实用工具子进程(201)
        let mut rows = vec![
            row_p(100, 0, "markflowy.exe", 1.0, 10),
            row_p(200, 100, "msedgewebview2.exe", 1.0, 10),
            row_p(201, 200, "msedgewebview2.exe", 1.0, 10),
        ];
        rows[0].display_name = "MarkFlowy".into();
        let mut roles = std::collections::HashMap::new();
        roles.insert(200, ChromiumRole::Browser);
        roles.insert(201, ChromiumRole::Utility("Network Service".into()));
        annotate_chromium_rows(&mut rows, &roles);
        // 浏览器 → 「WebView2: 宿主」；子进程 → 角色标签；宿主本身不受影响
        assert_eq!(rows[1].display_name, "WebView2: MarkFlowy");
        assert_eq!(rows[2].display_name, "WebView2 实用工具: Network Service");
        assert_eq!(rows[0].display_name, "MarkFlowy");
    }

    #[test]
    fn annotate_chrome_keeps_app_name_and_labels_children() {
        // explorer(宿主) → chrome 浏览器(300) → GPU 子进程(301)
        let mut rows = vec![
            row_p(300, 500, "chrome.exe", 1.0, 10),
            row_p(301, 300, "chrome.exe", 1.0, 10),
        ];
        rows[0].display_name = "Google Chrome".into();
        rows[1].display_name = "Google Chrome".into();
        let mut roles = std::collections::HashMap::new();
        roles.insert(300, ChromiumRole::Browser);
        roles.insert(301, ChromiumRole::Gpu);
        annotate_chromium_rows(&mut rows, &roles);
        // Chrome 是独立应用：浏览器保留应用名，子进程标注角色
        assert_eq!(rows[0].display_name, "Google Chrome");
        assert_eq!(rows[1].display_name, "Chrome GPU 进程");
    }

    #[test]
    fn annotate_webview2_no_host_falls_back() {
        // 浏览器父链断裂（parent=0）→ 找不到宿主，回退为「WebView2」
        let mut rows = vec![row_p(400, 0, "msedgewebview2.exe", 1.0, 10)];
        let mut roles = std::collections::HashMap::new();
        roles.insert(400, ChromiumRole::Browser);
        annotate_chromium_rows(&mut rows, &roles);
        assert_eq!(rows[0].display_name, "WebView2");
    }

    #[test]
    fn annotate_skips_non_chromium() {
        // 非 Chromium 进程（无角色记录）保持原展示名
        let mut rows = vec![row_p(500, 0, "explorer.exe", 1.0, 10)];
        rows[0].display_name = "Explorer".into();
        annotate_chromium_rows(&mut rows, &std::collections::HashMap::new());
        assert_eq!(rows[0].display_name, "Explorer");
    }

    #[test]
    fn filter_groups_by_display_name() {
        // 按展示名搜索：输入宿主名可找到 WebView2 组
        let mut wv = GroupedProcess {
            name: "msedgewebview2.exe".into(),
            root: row_p(200, 100, "msedgewebview2.exe", 1.0, 10),
            children: vec![],
            services: vec![],
        };
        wv.root.display_name = "WebView2: MarkFlowy".into();
        let groups = vec![wv];
        let f = filter_groups(&groups, "markflowy", &std::collections::HashSet::new());
        assert_eq!(f.len(), 1);
        // 不匹配的展示名 → 空
        let f2 = filter_groups(&groups, "notexist", &std::collections::HashSet::new());
        assert!(f2.is_empty());
    }

    // ===== WebView2 按宿主分组 ===== 

    #[test]
    fn group_webview2_grouped_by_host() {
        // markflowy(100) → WebView2 浏览器(200) → 子进程(201)；
        // markflowy 还有第二个浏览器实例(202) → 应与 200/201 合并进同一组；
        // widgets(300) → WebView2 浏览器(400) → 子进程(401) → 独立一组
        let rows = vec![
            row_p(100, 0, "markflowy.exe", 1.0, 10),
            row_p(200, 100, "msedgewebview2.exe", 1.0, 10),
            row_p(201, 200, "msedgewebview2.exe", 1.0, 10),
            row_p(202, 100, "msedgewebview2.exe", 1.0, 10), // 同宿主的第二个浏览器
            row_p(300, 0, "Widgets.exe", 1.0, 10),
            row_p(400, 300, "msedgewebview2.exe", 1.0, 10),
            row_p(401, 400, "msedgewebview2.exe", 1.0, 10),
        ];
        let groups = group_processes(&rows, &no_services());
        let wv: Vec<&GroupedProcess> = groups
            .iter()
            .filter(|g| g.name == "msedgewebview2.exe")
            .collect();
        // 两个宿主 → 两个 WebView2 组（不是把所有 WebView2 合成一组）
        assert_eq!(wv.len(), 2, "按宿主分组：应有两个 WebView2 组");
        // markflowy 的组应含其两个浏览器实例 + 子进程 = 3 个
        let mf = wv.iter().find(|g| g.root.parent_pid == 100).unwrap();
        assert_eq!(mf.total_count(), 3, "同宿主多个浏览器实例应合并为一组");
        // 宿主进程仍各自独立成组：markflowy + Widgets
        assert_eq!(groups.len(), 4);
    }

    #[test]
    fn group_webview2_distinct_hosts_separate() {
        // 不同宿主 → 各自独立组，不合并
        let rows = vec![
            row_p(100, 0, "markflowy.exe", 1.0, 10),
            row_p(200, 100, "msedgewebview2.exe", 1.0, 10),
            row_p(300, 0, "Widgets.exe", 1.0, 10),
            row_p(400, 300, "msedgewebview2.exe", 1.0, 10),
        ];
        let groups = group_processes(&rows, &no_services());
        let wv: Vec<&GroupedProcess> = groups
            .iter()
            .filter(|g| g.name == "msedgewebview2.exe")
            .collect();
        assert_eq!(wv.len(), 2);
        assert_eq!(wv[0].total_count(), 1);
        assert_eq!(wv[1].total_count(), 1);
    }

    #[test]
    fn group_webview2_orphaned_fallback_single_group() {
        // 父进程为 0（无法确定宿主）：所有 WebView2 归入一个兜底组
        let rows = vec![
            row_p(200, 0, "msedgewebview2.exe", 1.0, 10),
            row_p(201, 200, "msedgewebview2.exe", 1.0, 10),
        ];
        let groups = group_processes(&rows, &no_services());
        let wv: Vec<&GroupedProcess> = groups
            .iter()
            .filter(|g| g.name == "msedgewebview2.exe")
            .collect();
        assert_eq!(wv.len(), 1);
        assert_eq!(wv[0].total_count(), 2);
    }

    #[test]
    fn group_display_webview2_host_title() {
        // 组标题带宿主（来自 root 浏览器进程的标注展示名）
        let rows = vec![
            row_p(200, 100, "msedgewebview2.exe", 1.0, 10),
            row_p(201, 200, "msedgewebview2.exe", 1.0, 10),
        ];
        let mut groups = group_processes(&rows, &no_services());
        groups[0].root.display_name = "WebView2: MarkFlowy".into();
        let agg = group_aggregate(&groups[0]);
        let d = group_display(&groups[0], &agg, 8, 30.0, 16 * 1024 * 1024 * 1024);
        assert!(d.name.contains("WebView2: MarkFlowy"));
        assert!(d.name.contains("(2)"));
        assert_eq!(d.group_key, "msedgewebview2.exe:200");
    }
}
