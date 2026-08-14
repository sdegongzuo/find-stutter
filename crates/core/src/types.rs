use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 一次系统指标采样
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub timestamp: DateTime<Utc>,

    // CPU
    pub cpu_usage: f32,
    pub cpu_per_core: Vec<f32>,
    pub cpu_freq_mhz: Option<f32>,

    // 内存
    pub mem_usage_percent: f32,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    pub mem_available_mb: u64,
    pub swap_usage_percent: f32,
    // 提交电荷（commit charge）：已提交虚拟内存字节数 / 提交上限（= 物理内存 + 页面文件）。
    // 仅采集展示，不再作为卡顿触发（存量≠压力，见 DetectionConfig 说明）。
    pub commit_bytes: u64,
    pub commit_limit: u64,

    // 分页活动速率（Page Reads/sec）：每秒因硬页错误而从磁盘（含 pagefile）读入的页数。
    // 是「真正的 swap 卡顿信号」——直接度量换页活动强度（流量），而非 swap 已用存量。
    // 见 DetectionConfig::page_reads_threshold 与 docs/memory-stutter-detection.md。
    pub page_reads_per_sec: f32,

    // 磁盘 I/O (bytes/sec)
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,

    // 系统级信号（F-RC2）：磁盘真繁忙度 + DPC/中断/上下文切换。
    // 这些字段在 3.2 之后加入，旧库 snapshot JSON 若无此字段，
    // 用 serde(default) 回退默认（0.0），避免反序列化失败（reader 读旧事件时用到）。
    // 见 `卡顿根因分析-PRD.md` §3.2。
    /// 磁盘繁忙度：`\PhysicalDisk(_Total)\% Disk Time`（%）。
    /// 比 B/s 吞吐更准确地反映磁盘是否真正饱和（队列里排队的 IO）。
    #[serde(default)]
    pub disk_busy_percent: f32,
    /// 单次 IO 延迟：`\PhysicalDisk(_Total)\Avg Disk sec/Transfer` 换算成毫秒。
    /// 数值高说明磁盘每次 IO 都要等很久（机械盘寻道 / SSD 写放大等）。
    #[serde(default)]
    pub disk_avg_io_ms: f32,
    /// 系统底层卡顿信号：`\Processor(_Total)\% DPC Time`（%）。
    /// DPC（延迟过程调用）长时间占用 CPU 会挤占普通线程，造成「CPU 不忙但系统卡」。
    #[serde(default)]
    pub dpc_percent: f32,
    /// 系统底层卡顿信号：`\Processor(_Total)\% Interrupt Time`（%）。
    /// 中断处理长时间占用 CPU 同样挤占普通线程。
    #[serde(default)]
    pub interrupt_percent: f32,
    /// 上下文切换速率：`\System\Context Switches/sec`（/s）。
    /// 异常飙高（上下文切换风暴）会拖垮调度，是系统级卡顿真信号。
    #[serde(default)]
    pub context_switches_per_sec: f32,

    // 网络 I/O (bytes/sec)
    pub net_sent_bps: u64,
    pub net_recv_bps: u64,
    pub net_sent_total: u64,
    pub net_recv_total: u64,

    // GPU
    pub gpu_usage: Option<f32>,

    // 温度
    pub cpu_temp: Option<f32>,
    pub gpu_temp: Option<f32>,

    // 进程
    pub process_count: usize,
    pub thread_count: usize,

    // 进程快照：top N by CPU 与 top N by 内存的并集（去重），用于卡顿 culprit 归因
    // serde(default)：旧库 snapshot JSON 无此字段时回退为空列表，避免反序列化失败
    #[serde(default)]
    pub top_processes: Vec<ProcessBrief>,
}

impl Default for Sample {
    fn default() -> Self {
        Self {
            timestamp: Utc::now(),
            cpu_usage: 0.0,
            cpu_per_core: Vec::new(),
            cpu_freq_mhz: None,
            mem_usage_percent: 0.0,
            mem_used_mb: 0,
            mem_total_mb: 0,
            mem_available_mb: 0,
            swap_usage_percent: 0.0,
            commit_bytes: 0,
            commit_limit: 0,
            page_reads_per_sec: 0.0,
            disk_read_bps: 0,
            disk_write_bps: 0,
            disk_busy_percent: 0.0,
            disk_avg_io_ms: 0.0,
            dpc_percent: 0.0,
            interrupt_percent: 0.0,
            context_switches_per_sec: 0.0,
            net_sent_bps: 0,
            net_recv_bps: 0,
            net_sent_total: 0,
            net_recv_total: 0,
            gpu_usage: None,
            cpu_temp: None,
            gpu_temp: None,
            process_count: 0,
            thread_count: 0,
            top_processes: Vec::new(),
        }
    }
}

/// 单个进程的资源占用快照（用于卡顿 culprit 归因）。
///
/// 采集器每次采样本地按 CPU / 内存排序取 top 进程，检测器在卡顿持续期间
/// 累积这些快照（按 pid 取最大用量），卡顿结束时提取 top 进程作为 culprits。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProcessBrief {
    pub pid: u32,
    pub name: String,
    /// 该进程 CPU 占用（%，sysinfo 全局口径）
    pub cpu_usage: f32,
    /// 该进程内存占用（MB）
    pub mem_used_mb: u64,
    // ===== F-RC14-a/b：软件根因定位字段（v0.3 新增）=====
    // 仅卡顿帧（collect_with(true)）采集，非卡顿帧跳过（对齐 top_processes 既有节制）。
    // 全部 #[serde(default)]：旧库 culprits JSON 反序列化新结构时不崩（PRD §3.4.1）。
    /// 可执行文件完整路径（如 C:\\...\\browser.exe；来自 sysinfo process.exe()）
    #[serde(default)]
    pub exe_path: Option<String>,
    /// 句柄数（GetProcessHandleCount；句柄泄漏信号）
    #[serde(default)]
    pub handle_count: Option<u32>,
    /// GDI 对象数（GetGuiResources(GR_GDIOBJECTS)；GUI 泄漏经典信号）
    #[serde(default)]
    pub gdi_objects: Option<u32>,
    /// USER 对象数（GetGuiResources(GR_USEROBJECTS)）
    #[serde(default)]
    pub user_objects: Option<u32>,
    /// 该进程磁盘读速率（B/s；GetProcessIoCounters 差分）
    #[serde(default)]
    pub io_read_bps: Option<u64>,
    /// 该进程磁盘写速率（B/s；GetProcessIoCounters 差分）
    #[serde(default)]
    pub io_write_bps: Option<u64>,
}

impl ProcessBrief {
    /// 从给定进程快照集合中，按 CPU / 内存两个维度各取 top 并去重合并（最多 `max` 个）。
    ///
    /// 供两处复用（避免重复实现同一套「双维度 top + 去重」逻辑）：
    /// - 采集器每 tick 取全局 top（CPU top8 + 内存 top8，≤12）；
    /// - 检测器卡顿结束时提取元凶（CPU top3 + 内存 top3，≤6）。
    pub fn merge_top(
        mut all: Vec<ProcessBrief>,
        cpu_take: usize,
        mem_take: usize,
        max: usize,
    ) -> Vec<ProcessBrief> {
        // CPU 维度降序截取
        all.sort_by(|a, b| {
            b.cpu_usage
                .partial_cmp(&a.cpu_usage)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let cpu_top: Vec<ProcessBrief> = all.iter().take(cpu_take).cloned().collect();
        // 内存维度降序截取
        all.sort_by(|a, b| b.mem_used_mb.cmp(&a.mem_used_mb));
        let mem_top: Vec<ProcessBrief> = all.into_iter().take(mem_take).collect();

        // 按 pid 去重合并（CPU 维度优先），到上限即停
        let mut result: Vec<ProcessBrief> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for p in cpu_top.into_iter().chain(mem_top) {
            if seen.insert(p.pid) {
                result.push(p);
            }
            if result.len() >= max {
                break;
            }
        }
        result
    }
}

/// 卡顿严重程度
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Severity {
    #[default]
    Minor,
    Major,
    Critical,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Minor => write!(f, "minor"),
            Severity::Major => write!(f, "major"),
            Severity::Critical => write!(f, "critical"),
        }
    }
}

/// 结构化根因枚举（F-RC1）。
///
/// 枚举值**直接对齐 `cause_key()` 的稳定类型 key**（不臆造），以字符串形式落库，
/// 便于旧库回读与新库直接 `GROUP BY`。`cause_kinds` 映射随检测器改造分批补齐：
/// - F-RC1：CpuHigh / CpuSpike / MemLow / DiskSpike / NetSpike
/// - F-RC2（本项）：DiskBusy / DpcInterrupt / InterruptStorm / ContextSwitchStorm
/// - F-RC3：UiFrozen
/// - F-RC4：ThermalThrottle（温度高 + cpu_freq 掉档，gpu_temp 不纳入）
/// - 尚未落地映射：`GpuHigh`（检测器未产出 GPU cause）——该槽位已预留，
///   对应检测器改造后 `from_cause` 即生效。
/// 检测器一旦产出对应 cause，即可直接落入 `cause_kinds`。
///
/// 注：`CpuSpike` 对齐 `cause_key()` 现有 `"CPU spike"` key——PRD §3.3 枚举清单未列，
/// 但 `cause_key()` 实际产出该 key，若丢弃会导致「纯 CPU spike」事件 `cause_kinds`
/// 缺项，故显式补齐（保持与 `cause_key()` 严格一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum CauseKind {
    CpuHigh,
    /// CPU 突增 spike（与 `CpuHigh` 区分：硬阈值 vs 滑动基线比率，见 `cause_key`）
    CpuSpike,
    MemLow,
    DiskBusy,
    DiskSpike,
    GpuHigh,
    ThermalThrottle,
    DpcInterrupt,
    InterruptStorm,
    ContextSwitchStorm,
    NetSpike,
    UiFrozen,
    // ===== 软件/驱动/硬件级 cause（v0.3 F-RC14 新增）=====
    // 来源不是 PDH 阈值，而是「进程指纹阈值 + Windows 事件日志回溯」：
    // - ProcessHandleLeak / GdiObjectLeak：句柄 / GDI+USER 对象超阈值（F-RC14-a）
    // - DriverTimeout / ServiceCrash / DiskIoError / HardwareError：事件日志白名单命中（F-RC14-c）
    ProcessHandleLeak,
    /// 句柄数高但无持续增长趋势（中性提示，不参与主因；区别于真正的泄漏）。
    HandleHigh,
    GdiObjectLeak,
    DriverTimeout,
    ServiceCrash,
    DiskIoError,
    HardwareError,
}

/// 稳定类型 key → 结构化 `CauseKind` 的**单一映射表**。
///
/// `cause_key`（去重/更新用）与 `CauseKind::from_cause`（回填用）共用这一份前缀真相，
/// 消除 R2「分类不连续」风险：新增前缀只改这里一处，漏改即在 `from_cause` 落空。
/// 顺序无关紧要（各前缀互不嵌套），与历史 `detector.rs` 的 `PREFIXES` 顺序保持一致。
const PREFIX_TO_KIND: &[(&str, CauseKind)] = &[
    ("CPU usage", CauseKind::CpuHigh),
    ("CPU spike", CauseKind::CpuSpike),
    ("Disk write", CauseKind::DiskSpike),
    ("Disk busy", CauseKind::DiskBusy),
    ("DPC time", CauseKind::DpcInterrupt),
    ("Interrupt time", CauseKind::InterruptStorm),
    ("Context switches", CauseKind::ContextSwitchStorm),
    ("Thermal throttle", CauseKind::ThermalThrottle),
    ("UI frozen", CauseKind::UiFrozen),
    ("Network", CauseKind::NetSpike),
    ("Memory usage", CauseKind::MemLow),
    ("Memory available", CauseKind::MemLow),
    ("Available memory", CauseKind::MemLow),
    ("Commit charge", CauseKind::MemLow),
    ("Memory paging", CauseKind::MemLow),
    ("句柄泄漏", CauseKind::ProcessHandleLeak),
    ("句柄数偏高", CauseKind::HandleHigh),
];

impl CauseKind {
    /// 从一条 cause 文本映射到结构化 `CauseKind`（基于稳定前缀 key）。
    ///
    /// 旧事件 `cause_kinds` 为空时，reader 用本函数把自由文本 `causes`
    /// **可靠回填**为枚举（精确映射，非脆弱关键词猜测，见 PRD §3.1）。
    /// 返回 `None` 表示该 cause 文本尚无对应枚举（如尚未落地的
    /// `GpuHigh`——该槽位已预留待后续检测器改造补齐；`UiFrozen` 已在 F-RC3
    /// 映射，`ThermalThrottle` 已在 F-RC4 映射，`DiskBusy` 等已在 F-RC2 映射）。
    pub fn from_cause(cause: &str) -> Option<CauseKind> {
        for (prefix, kind) in PREFIX_TO_KIND {
            if cause.starts_with(prefix) {
                return Some(*kind);
            }
        }
        None
    }

    /// 该 cause 是否为「软件/驱动/硬件级」cause（F-RC14）。
    ///
    /// 软件级 cause 整体优先于资源级 cause 作为 primary_cause（PRD §5.6）——
    /// 驱动超时 / 服务崩溃才是用户能采取行动的真根因。
    pub fn is_software(self) -> bool {
        matches!(
            self,
            CauseKind::ProcessHandleLeak
                | CauseKind::HandleHigh
                | CauseKind::GdiObjectLeak
                | CauseKind::DriverTimeout
                | CauseKind::ServiceCrash
                | CauseKind::DiskIoError
                | CauseKind::HardwareError
        )
    }

    /// 软件级 cause 的严重程度排序（PRD §3.3）：数值越小越严重，用于多软件级 cause 同时
    /// 命中时排序取第一：HardwareError > DriverTimeout > ServiceCrash > DiskIoError
    /// > ProcessHandleLeak > GdiObjectLeak。非软件级 cause 返回 None。
    pub fn software_priority(self) -> Option<u8> {
        match self {
            CauseKind::HardwareError => Some(0),
            CauseKind::DriverTimeout => Some(1),
            CauseKind::ServiceCrash => Some(2),
            CauseKind::DiskIoError => Some(3),
            CauseKind::ProcessHandleLeak => Some(4),
            // 中性提示：优先级最低，且 merge 选主因时会跳过（不抢主因）
            CauseKind::HandleHigh => Some(6),
            CauseKind::GdiObjectLeak => Some(5),
            _ => None,
        }
    }
}

/// cause 的稳定类型 key：按已知前缀匹配，用于同类型去重/更新与 `CauseKind` 映射。
///
/// 滞回带内文案数值会变化（"CPU usage 85%（滞回保持…）" vs "CPU usage 95% > 90%"），
/// 但类型 key 不变；CPU 硬阈值与 CPU spike 是不同的 cause（key 不同）。原定义位于
/// `detector.rs`，F-RC1 起统一迁移到 `types.rs`，与 `CauseKind::from_cause` 共用
/// `PREFIX_TO_KIND` 同一份真相（消除 R2「分类不连续」风险）。
pub fn cause_key(cause: &str) -> &str {
    for (prefix, _) in PREFIX_TO_KIND {
        if cause.starts_with(prefix) {
            return prefix;
        }
    }
    cause
}

/// 卡顿事件
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StutterEvent {
    /// 事件主键（stutter_events 表 autoincrement id；in-memory 构造时默认 0，
    /// 由 reader 从库读出真实值，供 F-RC10 钻取卡精准关联）
    pub id: i64,
    pub timestamp: DateTime<Utc>,
    pub duration_ms: u64,
    pub severity: Severity,
    /// 自由文本根因（保留作兜底/兼容旧库）
    pub causes: Vec<String>,
    /// 结构化根因枚举（对齐 `CauseKind`；旧库无此列时为空，由 reader 用 `cause_key` 回填）
    pub cause_kinds: Vec<CauseKind>,
    /// 主因枚举（多因同发时按首触时刻最早者取第一；F-RC5 进一步按信号强度×持续细化权重）
    pub primary_cause: Option<CauseKind>,
    /// 各 cause 首触时刻（相对 onset 的偏移毫秒：0=与卡顿同时起点，正数=晚于起点），
    /// 供 F-RC6 因果方向（触发者 vs 放大器）使用
    pub cause_first_touch: HashMap<CauseKind, i64>,
    /// 事件 onset 时刻（Unix 毫秒；≈ 卡顿真实起点 = timestamp - duration_ms），
    /// 供 F-RC6 锚定分析窗口
    pub onset_ts: Option<i64>,
    pub snapshot: Sample,
    /// 造成本次卡顿的进程（CPU / 内存维度 top 进程，去重最多 ~6 个）
    pub culprits: Vec<ProcessBrief>,
}

/// 检测器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionConfig {
    pub cpu_threshold: f32,
    /// CPU 滞回：进入卡顿需 > cpu_threshold；退出需 < cpu_threshold - cpu_hysteresis。
    /// 滞回带内维持当前状态，避免阈值附近反复横跳反复记录。
    #[serde(default = "default_cpu_hysteresis")]
    pub cpu_hysteresis: f32,
    pub mem_threshold_percent: f32,
    pub mem_threshold_mb: u64,
    /// 提交电荷压力阈值（%）：已提交虚拟内存 / 提交上限（= 物理内存 + 页面文件）。
    /// 接近上限时系统弹「内存不足」并强制分页，往往比「可用物理内存归零」更早预警。
    /// 与 mem 两个口径互补（任一成立即记内存压力）。
    pub commit_threshold_percent: f32,
    /// 分页活动速率阈值（/s）：\Memory\Page Reads/sec 超过该值即记为换页抖动
    /// （真正的 swap 卡顿信号，度量换页活动强度而非 swap 已用存量）。
    /// 正常负载通常 < 10/s，抖动（thrashing）时飙升到数百/数千 /s。
    /// 修正（用户实测）：Page Reads/sec 瞬时抖动极大，开发机/模拟器正常负载也常超 50/s，
    /// 故默认调高到 300/s，且需与「内存/磁盘压力证据」同时成立才触发（见 detector.rs）。
    /// 详见 docs/memory-stutter-detection.md 的阶段 C 说明。
    pub page_reads_threshold: f32,
    pub disk_rate_spike_ratio: f32,
    pub spike_ratio: f32,
    /// 网络/磁盘 spike 的绝对下限（B/s）：当前速率低于该值不判定 spike，
    /// 避免空闲时几 B/s ~ 几十 KB/s 的零头波动按倍数误报。
    #[serde(default = "default_spike_min_bps")]
    pub spike_min_bps: u64,
    pub sustained_seconds: u32,
    // ===== F-RC2 系统级信号阈值（带滞回）=====
    /// 磁盘繁忙度阈值（% Disk Time）：超过即记为 `DiskBusy` cause。
    /// 替代原来的磁盘 B/s spike——繁忙度才是磁盘真正饱和的真信号。
    #[serde(default = "default_disk_busy_threshold_percent")]
    pub disk_busy_threshold_percent: f32,
    /// 单次 IO 延迟阈值（ms，来自 Avg Disk sec/Transfer）：超过即记为 `DiskBusy`。
    /// 与 `disk_busy_threshold_percent` 为「或」关系：任一成立即磁盘繁忙。
    #[serde(default = "default_disk_io_threshold_ms")]
    pub disk_io_threshold_ms: f32,
    /// `% DPC Time` 阈值（%）：超过即记为 `DpcInterrupt` cause（DPC 风暴）。
    #[serde(default = "default_dpc_threshold_percent")]
    pub dpc_threshold_percent: f32,
    /// `% Interrupt Time` 阈值（%）：超过即记为 `InterruptStorm` cause（中断风暴）。
    #[serde(default = "default_interrupt_threshold_percent")]
    pub interrupt_threshold_percent: f32,
    /// `Context Switches/sec` 阈值（/s）：超过即记为 `ContextSwitchStorm` cause（切换风暴）。
    #[serde(default = "default_context_switch_threshold_per_sec")]
    pub context_switch_threshold_per_sec: f32,
    /// 系统级信号滞回比例（退出线 = 阈值 × 该比例）。避免在阈值附近反复横跳：
    /// 触发后需明显回落（降到阈值的一半，默认 0.5）才解除激活。
    #[serde(default = "default_sys_signal_hysteresis_ratio")]
    pub sys_signal_hysteresis_ratio: f32,
    /// F-RC3 前台窗口冻结探测超时（ms）：`SendMessageTimeout(WM_NULL, N)` 的 N。
    /// 仅在前台窗口真正挂起时才可能等满 N；正常响应近乎立即返回，故低频
    /// （2s 限频）探测的常态开销极小，不会进入采集热路径。供 F-RC12 what-if 调参。
    #[serde(default = "default_ui_freeze_timeout_ms")]
    pub ui_freeze_timeout_ms: u32,
    // ===== F-RC4 温度→降频根因阈值 =====
    /// 温度降频阈值（℃）：`cpu_temp` 超过即视为「温度高」，进入降频判定。
    /// `gpu_temp` 从未填充（collector 恒为 None），不纳入。
    #[serde(default = "default_thermal_threshold_celsius")]
    pub thermal_threshold_celsius: f32,
    /// 频率掉档比例（0~1）：当前 `cpu_freq_mhz` 低于近期观测峰值 × 该比例
    /// 即视为「疑似降频」（负载下频率不升反降）。与高温**同时**成立才记
    /// `ThermalThrottle` cause（单一高温但频率正常不算降频）。
    #[serde(default = "default_thermal_freq_drop_ratio")]
    pub thermal_freq_drop_ratio: f32,
    // ===== F-RC14-a 软件根因：句柄 / GDI 泄漏阈值 =====
    /// 句柄泄漏阈值：单进程 handle_count 超过即进入句柄趋势判定（F-RC14-a 方案 B）。
    /// 仅绝对值超过还不够——必须窗口内句柄数持续增长（后半段均值较前半段净增
    /// >= handle_leak_growth_threshold）才判为 ProcessHandleLeak；否则只标 HandleHigh
    /// （句柄数偏高，中性提示）。默认 10000（正常 Chrome 即可上万句柄，需实际校准）。
    #[serde(default = "default_handle_leak_threshold")]
    pub handle_leak_threshold: u32,
    /// 句柄增长阈值：卡顿窗口内后半段句柄均值较前半段净增超过该值才算「泄漏」。
    /// 防止把稳定占用大量句柄的进程（AI/数据库/杀毒等常驻服务）误判为泄漏。
    #[serde(default = "default_handle_leak_growth_threshold")]
    pub handle_leak_growth_threshold: u32,
    /// GDI+USER 对象泄漏阈值：单进程 gdi_objects + user_objects 超过即记
    /// GdiObjectLeak cause。默认 10000。
    #[serde(default = "default_gdi_leak_threshold")]
    pub gdi_leak_threshold: u32,
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self {
            cpu_threshold: 90.0,
            cpu_hysteresis: 10.0,
            mem_threshold_percent: 90.0,
            mem_threshold_mb: 500,
            commit_threshold_percent: 90.0,
            page_reads_threshold: 300.0,
            disk_rate_spike_ratio: 10.0,
            spike_ratio: 3.0,
            spike_min_bps: 2_000_000,
            sustained_seconds: 3,
            disk_busy_threshold_percent: 95.0,
            disk_io_threshold_ms: 50.0,
            dpc_threshold_percent: 10.0,
            interrupt_threshold_percent: 10.0,
            context_switch_threshold_per_sec: 50_000.0,
            sys_signal_hysteresis_ratio: 0.5,
            ui_freeze_timeout_ms: 200,
            thermal_threshold_celsius: 85.0,
            thermal_freq_drop_ratio: 0.85,
            handle_leak_threshold: default_handle_leak_threshold(),
            handle_leak_growth_threshold: default_handle_leak_growth_threshold(),
            gdi_leak_threshold: default_gdi_leak_threshold(),
        }
    }
}

fn default_cpu_hysteresis() -> f32 {
    10.0
}

fn default_spike_min_bps() -> u64 {
    2_000_000
}

// F-RC2 系统级信号阈值默认值
fn default_disk_busy_threshold_percent() -> f32 {
    95.0
}
fn default_disk_io_threshold_ms() -> f32 {
    50.0
}
fn default_dpc_threshold_percent() -> f32 {
    10.0
}
fn default_interrupt_threshold_percent() -> f32 {
    10.0
}
fn default_context_switch_threshold_per_sec() -> f32 {
    50_000.0
}
fn default_sys_signal_hysteresis_ratio() -> f32 {
    0.5
}
fn default_ui_freeze_timeout_ms() -> u32 {
    200
}

// F-RC4 温度→降频根因阈值默认值
fn default_thermal_threshold_celsius() -> f32 {
    85.0
}
fn default_thermal_freq_drop_ratio() -> f32 {
    0.85
}

// F-RC14-a 句柄 / GDI 泄漏阈值默认值
fn default_handle_leak_threshold() -> u32 {
    10_000
}

fn default_handle_leak_growth_threshold() -> u32 {
    2_000
}
fn default_gdi_leak_threshold() -> u32 {
    10_000
}

/// 采样配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamplingConfig {
    pub interval_ms: u64,
    pub slow_interval_factor: u32,
}

impl Default for SamplingConfig {
    fn default() -> Self {
        Self {
            interval_ms: 1000,
            slow_interval_factor: 5,
        }
    }
}

/// 存储配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub db_path: String,
    pub retention_days: u32,
    /// stutter_events（及其软件根因子表）独立保留天数：与 samples 的 30 天不同周期，
    /// 按 PRD §3.4.6 卡顿事件保留 7 天（同机制、不同周期）。缺省 7，旧配置无此项也可解析。
    #[serde(default = "default_event_retention_days")]
    pub event_retention_days: u32,
}

fn default_event_retention_days() -> u32 {
    7
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            db_path: "stutter.db".to_string(),
            retention_days: 30,
            event_retention_days: 7,
        }
    }
}

/// UI 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub skin: String,
    pub always_on_top: bool,
    pub show_upload: bool,
    pub show_download: bool,
    pub show_cpu: bool,
    pub show_memory: bool,
    pub show_gpu: bool,
    pub show_disk: bool,
    pub show_cpu_freq: bool,
    pub show_temperature: bool,
    pub mouse_transparent: bool,
    pub click_through: bool,
    /// 启动 GUI 时是否自动检测 + 启动后台服务（含 UAC 提权）。
    /// 自动测试 / CI 环境建议关掉（或设环境变量 FIND_STUTTER_SKIP_SERVICE=1），
    /// 避免每次启动都弹 UAC。
    #[serde(default = "default_true")]
    pub auto_start_service: bool,
    /// P2：任务栏嵌入模式（伪任务栏窗口，显示在屏幕底部，可拖动到任务栏位置）
    #[serde(default)]
    pub taskbar: bool,
    /// 进程详情页：CPU/内存使用率超过该百分比（%）的行高亮标红（默认 30）
    #[serde(default = "default_highlight_pct")]
    pub process_highlight_pct: f32,
    /// 进程详情页：自动刷新间隔（毫秒）。默认 30000 = 30 秒
    #[serde(default = "default_process_refresh_ms")]
    pub process_refresh_ms: u64,
}

fn default_true() -> bool { true }

fn default_highlight_pct() -> f32 { 30.0 }

fn default_process_refresh_ms() -> u64 { 30_000 }

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            skin: "default".to_string(),
            always_on_top: true,
            show_upload: true,
            show_download: true,
            show_cpu: true,
            show_memory: true,
            show_gpu: true,
            show_disk: true,
            show_cpu_freq: false,
            show_temperature: false,
            mouse_transparent: false,
            click_through: false,
            auto_start_service: true,
            taskbar: false,
            process_highlight_pct: default_highlight_pct(),
            process_refresh_ms: default_process_refresh_ms(),
        }
    }
}

/// 通知配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationConfig {
    pub stutter_alert: bool,
    pub min_severity: String,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            stutter_alert: true,
            min_severity: "major".to_string(),
        }
    }
}

/// 日志配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

/// 全局配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub sampling: SamplingConfig,
    pub detection: DetectionConfig,
    pub ui: UiConfig,
    pub storage: StorageConfig,
    pub notifications: NotificationConfig,
    pub logging: LoggingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sampling: SamplingConfig::default(),
            detection: DetectionConfig::default(),
            ui: UiConfig::default(),
            storage: StorageConfig::default(),
            notifications: NotificationConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

impl Config {
    /// 加载配置文件。
    ///
    /// 查找顺序：
    /// 1. 用户指定的 `path`（通常是 `config.toml`）
    /// 2. **当前可执行文件所在目录**下的 `path`（关键！SCM 启动 service 时
    ///    CWD 是 `C:\Windows\System32`，那里没 config.toml；fallback 到
    ///    binary 同目录 `target\release\config.toml`）
    /// 3. 从 binary 目录**逐级向上**查找 `path`（开发布局：binary 在
    ///    `target/release/`，config.toml 在项目根；SCM 服务需要这个回退）
    /// 4. 最后再尝试原路径返回原始错误
    ///
    /// 同时把 `db_path` 相对路径**解析为绝对路径**（基于 config 所在目录），
    /// 避免 SCM service 写到 `C:\Windows\System32\stutter.db`。
    pub fn load(path: &str) -> anyhow::Result<Self> {
        // 1) 尝试给定路径
        if let Ok(content) = std::fs::read_to_string(path) {
            let base = std::path::Path::new(path)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            return Self::parse_with_base(&content, &base);
        }
        // 2) fallback 到 binary 同目录 + 3) 逐级向上
        if let Ok(me) = std::env::current_exe() {
            if let Some(dir) = me.parent() {
                // binary 同目录（如 target/release/config.toml）
                let alt = dir.join(path);
                if let Ok(content) = std::fs::read_to_string(&alt) {
                    log::info!("config 加载自 binary 同目录: {}", alt.display());
                    return Self::parse_with_base(&content, dir);
                }
                // 从 binary 目录逐级向上找（target/release → target → 项目根）
                for ancestor in dir.ancestors().skip(1) {
                    let candidate = ancestor.join(path);
                    if let Ok(content) = std::fs::read_to_string(&candidate) {
                        log::info!(
                            "config 加载自 binary 上级目录: {}",
                            candidate.display()
                        );
                        return Self::parse_with_base(&content, ancestor);
                    }
                }
            }
        }
        // 4) 原路径再试一次让调用方看到原始错误
        let content = std::fs::read_to_string(path)?;
        Self::parse_with_base(
            &content,
            std::path::Path::new(path)
                .parent()
                .unwrap_or_else(|| std::path::Path::new(".")),
        )
    }

    /// 解析 TOML 字符串并把 `db_path` 相对路径转为绝对路径。
    fn parse_with_base(content: &str, base: &std::path::Path) -> anyhow::Result<Self> {
        let mut config: Config = toml::from_str(content)?;
        let p = std::path::Path::new(&config.storage.db_path);
        if p.is_relative() {
            // base 本身是相对路径（如 CWD 下的 "."）时，先转成绝对路径，
            // 否则 base.join("stutter.db") 仍是相对的（日志里出现
            // "db_path 解析为绝对路径: stutter.db" 就是这种情况）。
            let base_abs = if base.is_absolute() {
                base.to_path_buf()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| std::path::PathBuf::from("."))
                    .join(base)
            };
            let abs = base_abs.join(p);
            config.storage.db_path = abs.to_string_lossy().to_string();
            log::info!("db_path 解析为绝对路径: {}", config.storage.db_path);
        }
        Ok(config)
    }

    pub fn save(&self, path: &str) -> anyhow::Result<()> {
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

// ===================== F-RC14 软件根因定位数据结构 =====================

/// 某 culprit 进程已加载的模块列表快照（F-RC14-b，卡顿事件生成时 snap 一次）。
/// 落 `process_modules` 表；识别注入的可疑 DLL / 第三方驱动模块。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessModule {
    /// 进程 ID
    pub pid: u32,
    /// 进程名
    pub process_name: String,
    /// 模块完整路径（如 C:\\Windows\\System32\\foo.dll）
    pub module_path: String,
    /// 模块大小（字节）
    pub module_size: u64,
}

/// Windows 事件日志回溯命中记录（F-RC14-c，落 `windows_events` 表）。
/// 卡顿事件生成时回溯 [onset-30s, now] 窗口的高价值白名单事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowsEventRecord {
    /// 日志通道（System / Application）
    pub channel: String,
    /// 事件源（如 Display / disk / Service Control Manager / Microsoft-Windows-WHEA-Logger）
    pub provider: String,
    /// Windows 事件 ID（如 4101 / 7 / 51 / 7031 / 41）
    pub win_event_id: u32,
    /// 级别（Error / Warning）
    pub level: String,
    /// 事件消息（截断到 512 字符，防膨胀）
    pub message: String,
    /// 事件发生时刻（RFC3339）
    pub ts: String,
}

/// ETW 调用栈采样聚合热点（F-RC14-d，落 `stack_samples` 表）。
/// 只解析到「模块名 + RVA 偏移」级别（不做完整 PDB 符号化，PRD §1.4）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackSample {
    /// 进程 ID
    pub pid: u32,
    /// 进程名
    pub process_name: String,
    /// 热点模块名（exe / dll）
    pub module: String,
    /// 模块内相对偏移（RVA）
    pub rva: u64,
    /// 该 (process, module, rva) 热点采样命中次数（聚合后）
    pub sample_count: u64,
}

/// 分析结论落库记录（F-RC15，落 `root_cause_reports` 表，按 event_id UPSERT）。
/// 可回溯、可审计：algorithm_version 记录算法版本，升级后可对比新旧结论。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootCauseReport {
    /// 关联卡顿事件 id（stutter_events.id，一事件一条，UNIQUE）
    pub event_id: i64,
    /// 分析算法版本（如 rc5-rc14.v1）
    pub algorithm_version: String,
    /// 主因枚举（字符串）
    pub primary_cause: String,
    /// 置信度 0..1
    pub confidence: f32,
    /// 因果链（CauseKind 枚举字符串数组，F-RC9 结果）
    pub cause_chain: Vec<String>,
    /// 软件根因定位结论（F-RC14 摘要：进程 / 模块 / 事件 ID）
    pub software_root_cause: serde_json::Value,
    /// 偏离基线摘要（F-RC7 结果）
    pub baseline_delta: serde_json::Value,
    /// 计算时刻（RFC3339）
    pub computed_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_default_values() {
        let s = Sample::default();
        assert_eq!(s.cpu_usage, 0.0);
        assert!(s.cpu_per_core.is_empty());
        assert!(s.cpu_freq_mhz.is_none());
        assert_eq!(s.mem_usage_percent, 0.0);
        assert_eq!(s.mem_used_mb, 0);
        assert_eq!(s.mem_total_mb, 0);
        assert_eq!(s.mem_available_mb, 0);
        assert_eq!(s.swap_usage_percent, 0.0);
        assert_eq!(s.disk_read_bps, 0);
        assert_eq!(s.disk_write_bps, 0);
        assert_eq!(s.disk_busy_percent, 0.0);
        assert_eq!(s.disk_avg_io_ms, 0.0);
        assert_eq!(s.dpc_percent, 0.0);
        assert_eq!(s.interrupt_percent, 0.0);
        assert_eq!(s.context_switches_per_sec, 0.0);
        assert_eq!(s.net_sent_bps, 0);
        assert_eq!(s.net_recv_bps, 0);
        assert_eq!(s.net_sent_total, 0);
        assert_eq!(s.net_recv_total, 0);
        assert!(s.gpu_usage.is_none());
        assert!(s.cpu_temp.is_none());
        assert!(s.gpu_temp.is_none());
        assert_eq!(s.process_count, 0);
        assert_eq!(s.thread_count, 0);
        assert!(s.top_processes.is_empty());
    }

    #[test]
    fn severity_display_minor() {
        assert_eq!(Severity::Minor.to_string(), "minor");
    }

    #[test]
    fn severity_display_major() {
        assert_eq!(Severity::Major.to_string(), "major");
    }

    #[test]
    fn severity_display_critical() {
        assert_eq!(Severity::Critical.to_string(), "critical");
    }

    #[test]
    fn detection_config_defaults() {
        let c = DetectionConfig::default();
        assert_eq!(c.cpu_threshold, 90.0);
        assert_eq!(c.mem_threshold_percent, 90.0);
        assert_eq!(c.mem_threshold_mb, 500);
        assert_eq!(c.commit_threshold_percent, 90.0);
        assert_eq!(c.page_reads_threshold, 300.0);
        assert_eq!(c.disk_rate_spike_ratio, 10.0);
        assert_eq!(c.spike_ratio, 3.0);
        assert_eq!(c.spike_min_bps, 2_000_000);
        assert_eq!(c.sustained_seconds, 3);
        // F-RC2 系统级信号阈值默认值
        assert_eq!(c.disk_busy_threshold_percent, 95.0);
        assert_eq!(c.disk_io_threshold_ms, 50.0);
        assert_eq!(c.dpc_threshold_percent, 10.0);
        assert_eq!(c.interrupt_threshold_percent, 10.0);
        assert_eq!(c.context_switch_threshold_per_sec, 50_000.0);
        assert_eq!(c.sys_signal_hysteresis_ratio, 0.5);
        assert_eq!(c.ui_freeze_timeout_ms, 200);
        // F-RC4 温度→降频根因阈值默认值
        assert_eq!(c.thermal_threshold_celsius, 85.0);
        assert_eq!(c.thermal_freq_drop_ratio, 0.85);
        // F-RC14-a 泄漏阈值默认值
        assert_eq!(c.handle_leak_threshold, 10_000);
        assert_eq!(c.gdi_leak_threshold, 10_000);
    }

    #[test]
    fn sampling_config_defaults() {
        let c = SamplingConfig::default();
        assert_eq!(c.interval_ms, 1000);
        assert_eq!(c.slow_interval_factor, 5);
    }

    #[test]
    fn storage_config_defaults() {
        let c = StorageConfig::default();
        assert_eq!(c.db_path, "stutter.db");
        assert_eq!(c.retention_days, 30);
    }

    #[test]
    fn config_defaults() {
        let c = Config::default();
        assert_eq!(c.sampling.interval_ms, 1000);
        assert_eq!(c.detection.cpu_threshold, 90.0);
        assert_eq!(c.ui.skin, "default");
        assert!(c.ui.always_on_top);
        assert_eq!(c.storage.retention_days, 30);
        assert!(c.notifications.stutter_alert);
        assert_eq!(c.notifications.min_severity, "major");
        assert_eq!(c.logging.level, "info");
    }

    fn temp_path(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "find_stutter_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{}.toml", name))
            .to_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn config_save_and_load_roundtrip() {
        let path = temp_path("config_roundtrip");
        let config = Config::default();
        config.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.sampling.interval_ms, config.sampling.interval_ms);
        assert_eq!(
            loaded.detection.cpu_threshold,
            config.detection.cpu_threshold
        );
        assert_eq!(loaded.storage.retention_days, config.storage.retention_days);
        assert_eq!(loaded.ui.skin, config.ui.skin);
        assert_eq!(
            loaded.notifications.min_severity,
            config.notifications.min_severity
        );
        assert_eq!(loaded.logging.level, config.logging.level);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn config_save_and_load_custom_values() {
        let path = temp_path("config_custom");
        let mut config = Config::default();
        config.sampling.interval_ms = 500;
        config.detection.cpu_threshold = 80.0;
        config.storage.retention_days = 7;
        config.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.sampling.interval_ms, 500);
        assert_eq!(loaded.detection.cpu_threshold, 80.0);
        assert_eq!(loaded.storage.retention_days, 7);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn config_load_nonexistent_file_fails() {
        let result = Config::load("/nonexistent/path/config.toml");
        assert!(result.is_err());
    }

    #[test]
    fn severity_equality() {
        assert_eq!(Severity::Minor, Severity::Minor);
        assert_ne!(Severity::Minor, Severity::Major);
        assert_ne!(Severity::Major, Severity::Critical);
    }

    #[test]
    fn sample_clone() {
        let mut s = Sample::default();
        s.cpu_usage = 75.5;
        s.cpu_per_core = vec![50.0, 60.0];
        let cloned = s.clone();
        assert_eq!(cloned.cpu_usage, 75.5);
        assert_eq!(cloned.cpu_per_core, vec![50.0, 60.0]);
    }

    // --- CauseKind / cause_key（F-RC1）---

    #[test]
    fn cause_kind_from_cause_maps_known_keys() {
        // 对齐 cause_key() 现有稳定 key
        assert_eq!(
            CauseKind::from_cause("CPU usage 95.0% > 90%"),
            Some(CauseKind::CpuHigh)
        );
        assert_eq!(
            CauseKind::from_cause("CPU usage 85.0%（滞回保持，阈值 90%）"),
            Some(CauseKind::CpuHigh)
        );
        assert_eq!(
            CauseKind::from_cause("CPU spike: 1.0% → 3.0%"),
            Some(CauseKind::CpuSpike)
        );
        assert_eq!(
            CauseKind::from_cause("Disk write spike: 1B/s → 3B/s"),
            Some(CauseKind::DiskSpike)
        );
        // F-RC2：系统级信号映射
        assert_eq!(
            CauseKind::from_cause("Disk busy 98.0% (IO 12.5ms)"),
            Some(CauseKind::DiskBusy)
        );
        assert_eq!(
            CauseKind::from_cause("DPC time 12.0% > 10%"),
            Some(CauseKind::DpcInterrupt)
        );
        assert_eq!(
            CauseKind::from_cause("Interrupt time 14.0% > 10%"),
            Some(CauseKind::InterruptStorm)
        );
        assert_eq!(
            CauseKind::from_cause("Context switches 60000/s > 50000/s"),
            Some(CauseKind::ContextSwitchStorm)
        );
        assert_eq!(
            CauseKind::from_cause("Network spike: 1B/s → 3B/s"),
            Some(CauseKind::NetSpike)
        );
        // 内存多口径归并为同一 MemLow
        assert_eq!(
            CauseKind::from_cause("Available memory 100MB < 500MB"),
            Some(CauseKind::MemLow)
        );
        assert_eq!(
            CauseKind::from_cause("Memory usage 95.0% > 90%"),
            Some(CauseKind::MemLow)
        );
        assert_eq!(
            CauseKind::from_cause("Memory paging 200.0/s > 50/s"),
            Some(CauseKind::MemLow)
        );
        assert_eq!(
            CauseKind::from_cause("Commit charge 95.0% > 90%"),
            Some(CauseKind::MemLow)
        );
    }

    #[test]
    fn cause_kind_from_cause_none_for_unmapped() {
        // F-RC3 的 UiFrozen、F-RC4 的 ThermalThrottle 已在 PREFIX_TO_KIND 映射
        // （见下方 maps_ui_frozen / maps_thermal_throttle 用例）；
        // 这里仅验证尚未落地的 GpuHigh 仍返回 None，不臆造枚举，避免 R2「分类不连续」。
        assert_eq!(CauseKind::from_cause("GPU usage 99%"), None);
        // 完全无关文本也返回 None
        assert_eq!(CauseKind::from_cause("something else"), None);
    }

    #[test]
    fn cause_kind_from_cause_maps_ui_frozen() {
        // F-RC3：前台窗口冻结 cause 文本应映射到 UiFrozen 枚举。
        assert_eq!(
            CauseKind::from_cause("UI frozen (前台窗口无响应 200ms)"),
            Some(CauseKind::UiFrozen)
        );
    }

    #[test]
    fn cause_kind_from_cause_maps_thermal_throttle() {
        // F-RC4：温度降频 cause 文本应映射到 ThermalThrottle 枚举。
        assert_eq!(
            CauseKind::from_cause("Thermal throttle: CPU 95°C, freq 2000MHz < 3000MHz (drop 33%)"),
            Some(CauseKind::ThermalThrottle)
        );
    }

    #[test]
    fn cause_key_relocated_groups_hysteresis_variants() {
        // cause_key 已迁移到 types.rs，语义必须与原 detector 实现一致
        assert_eq!(
            cause_key("CPU usage 95.0% > 90%"),
            cause_key("CPU usage 85.0%（滞回保持，阈值 90%）")
        );
        assert_eq!(
            cause_key("Commit charge 95.0% > 90%"),
            cause_key("Commit charge 85.0%")
        );
        assert_eq!(
            cause_key("Memory paging 200.0/s > 50/s"),
            cause_key("Memory paging 80.5/s > 50/s")
        );
        // 硬阈值与 spike 是不同 cause
        assert_ne!(
            cause_key("CPU usage 95.0% > 90%"),
            cause_key("CPU spike: 1.0% → 3.0%")
        );
        assert_ne!(
            cause_key("Disk write spike: 1B/s → 3B/s"),
            cause_key("Network spike: 1B/s → 3B/s")
        );
        assert_ne!(
            cause_key("Memory available spike: 1MB → 3MB"),
            cause_key("Available memory 100MB < 500MB")
        );
    }

    #[test]
    fn cause_kind_serde_roundtrip() {
        let kinds = vec![CauseKind::CpuHigh, CauseKind::MemLow, CauseKind::NetSpike];
        let json = serde_json::to_string(&kinds).unwrap();
        assert_eq!(json, r#"["CpuHigh","MemLow","NetSpike"]"#);
        let back: Vec<CauseKind> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kinds);

        // 单值与 Option 形态（落库存 primary_cause）
        let p: Option<CauseKind> = serde_json::from_str(r#""DiskSpike""#).unwrap();
        assert_eq!(p, Some(CauseKind::DiskSpike));
        let n: Option<CauseKind> = serde_json::from_str("null").unwrap();
        assert_eq!(n, None);
    }

    #[test]
    fn cause_first_touch_serde_roundtrip() {
        // HashMap<CauseKind, i64> 以枚举字符串为 key 落库（供 F-RC6 按 cause 查首触时刻）
        let mut map: HashMap<CauseKind, i64> = HashMap::new();
        map.insert(CauseKind::CpuHigh, 0);
        map.insert(CauseKind::MemLow, 1200);
        let json = serde_json::to_string(&map).unwrap();
        let back: HashMap<CauseKind, i64> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, map);
        assert_eq!(back.get(&CauseKind::MemLow).copied(), Some(1200));
    }

    #[test]
    fn severity_default_is_minor() {
        assert_eq!(Severity::default(), Severity::Minor);
    }

    #[test]
    fn stutter_event_default_has_empty_structured_fields() {
        let e = StutterEvent::default();
        assert_eq!(e.id, 0);
        assert!(e.cause_kinds.is_empty());
        assert_eq!(e.primary_cause, None);
        assert!(e.cause_first_touch.is_empty());
        assert_eq!(e.onset_ts, None);
        assert!(e.culprits.is_empty());
    }
}