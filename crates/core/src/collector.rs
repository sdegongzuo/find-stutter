use crate::types::{ProcessBrief, Sample};
use chrono::Utc;
use log::warn;
use std::collections::HashMap;
use sysinfo::{Networks, System};
use wmi::{Variant, WMIConnection};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    ERROR_SUCCESS, ERROR_TIMEOUT, GetLastError, LPARAM, SetLastError, WPARAM,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_BLOCK, WM_NULL,
};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData,     PdhGetFormattedCounterValue,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PDH_FMT_LARGE, PDH_HCOUNTER,
    PDH_HQUERY,
};

/// Windows PDH-based disk I/O sampler.
///
/// Holds the PDH query + counter handles for the `_Total` physical-disk
/// read/write bytes-per-second counters. The query is opened once and the
/// counters are sampled every tick so the values are always fresh (the old
/// WMI approach only ran every 5 ticks AND matched the wrong variant type,
/// which is why disk always showed `0 B/s`). In the windows 0.58 crate PDH
/// windows 0.62 起 PDH 句柄是 `PDH_HQUERY(*mut c_void)` 结构体（不再是 isize）。
struct DiskPdh {
    query: PDH_HQUERY,
    read_counter: PDH_HCOUNTER,
    write_counter: PDH_HCOUNTER,
}

impl DiskPdh {
    fn new() -> Option<Self> {
        unsafe {
            let mut query: PDH_HQUERY = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != ERROR_SUCCESS.0 {
                warn!("PdhOpenQueryW failed");
                return None;
            }

            let mut read_counter: PDH_HCOUNTER = PDH_HCOUNTER::default();
            let mut write_counter: PDH_HCOUNTER = PDH_HCOUNTER::default();

            let read_path = w!(r"\PhysicalDisk(_Total)\Disk Read Bytes/sec");
            let write_path = w!(r"\PhysicalDisk(_Total)\Disk Write Bytes/sec");

            if PdhAddEnglishCounterW(query, read_path, 0, &mut read_counter) != ERROR_SUCCESS.0 {
                warn!("PdhAddEnglishCounterW (read) failed");
                PdhCloseQuery(query);
                return None;
            }
            if PdhAddEnglishCounterW(query, write_path, 0, &mut write_counter) != ERROR_SUCCESS.0 {
                warn!("PdhAddEnglishCounterW (write) failed");
                PdhCloseQuery(query);
                return None;
            }

            // Prime the query so the first real collect has a baseline for the
            // "bytes/sec" rate counter.
            PdhCollectQueryData(query);

            Some(Self {
                query,
                read_counter,
                write_counter,
            })
        }
    }

    /// Collect the current read/write bytes-per-second. Returns `(read_bps, write_bps)`.
    fn sample(&self) -> (u64, u64) {
        unsafe {
            if PdhCollectQueryData(self.query) != ERROR_SUCCESS.0 {
                return (0, 0);
            }

            let mut read_val: PDH_FMT_COUNTERVALUE = PDH_FMT_COUNTERVALUE::default();
            let mut write_val: PDH_FMT_COUNTERVALUE = PDH_FMT_COUNTERVALUE::default();

            if PdhGetFormattedCounterValue(self.read_counter, PDH_FMT_LARGE, None, &mut read_val)
                != ERROR_SUCCESS.0
            {
                return (0, 0);
            }
            if PdhGetFormattedCounterValue(self.write_counter, PDH_FMT_LARGE, None, &mut write_val)
                != ERROR_SUCCESS.0
            {
                return (0, 0);
            }

            // CStatus == 0 means valid data. The large value is an i64; clamp
            // negatives to 0 before casting to u64.
            let read = if read_val.CStatus == 0 {
                read_val.Anonymous.largeValue.max(0) as u64
            } else {
                0
            };
            let write = if write_val.CStatus == 0 {
                write_val.Anonymous.largeValue.max(0) as u64
            } else {
                0
            };

            (read, write)
        }
    }
}

impl Drop for DiskPdh {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

/// Windows PDH-based commit-charge sampler.
///
/// 采集 `\Memory\Committed Bytes` 与 `\Memory\Commit Limit`（均为瞬时计数，
/// 非速率计数器），用于判断「提交电荷压力」——已提交虚拟内存接近提交上限
/// （物理内存 + 页面文件）时系统会弹「内存不足」并强制分页，是比「可用物理
/// 内存归零」更早的卡顿预警信号。详见 `DetectionConfig::commit_threshold_percent`。
struct CommitPdh {
    query: PDH_HQUERY,
    committed_counter: PDH_HCOUNTER,
    limit_counter: PDH_HCOUNTER,
}

impl CommitPdh {
    fn new() -> Option<Self> {
        unsafe {
            let mut query: PDH_HQUERY = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != ERROR_SUCCESS.0 {
                warn!("PdhOpenQueryW (commit) failed");
                return None;
            }

            let mut committed_counter: PDH_HCOUNTER = PDH_HCOUNTER::default();
            let mut limit_counter: PDH_HCOUNTER = PDH_HCOUNTER::default();

            let committed_path = w!(r"\Memory\Committed Bytes");
            let limit_path = w!(r"\Memory\Commit Limit");

            if PdhAddEnglishCounterW(query, committed_path, 0, &mut committed_counter)
                != ERROR_SUCCESS.0
            {
                warn!("PdhAddEnglishCounterW (committed) failed");
                PdhCloseQuery(query);
                return None;
            }
            if PdhAddEnglishCounterW(query, limit_path, 0, &mut limit_counter) != ERROR_SUCCESS.0
            {
                warn!("PdhAddEnglishCounterW (limit) failed");
                PdhCloseQuery(query);
                return None;
            }

            // 预采一次，给瞬时计数器建立基线。
            PdhCollectQueryData(query);

            Some(Self {
                query,
                committed_counter,
                limit_counter,
            })
        }
    }

    /// 采集当前已提交字节数与提交上限（字节）。返回 `(committed, limit)`。
    fn sample(&self) -> (u64, u64) {
        unsafe {
            if PdhCollectQueryData(self.query) != ERROR_SUCCESS.0 {
                return (0, 0);
            }

            let mut committed_val: PDH_FMT_COUNTERVALUE = PDH_FMT_COUNTERVALUE::default();
            let mut limit_val: PDH_FMT_COUNTERVALUE = PDH_FMT_COUNTERVALUE::default();

            if PdhGetFormattedCounterValue(
                self.committed_counter,
                PDH_FMT_LARGE,
                None,
                &mut committed_val,
            ) != ERROR_SUCCESS.0
            {
                return (0, 0);
            }
            if PdhGetFormattedCounterValue(self.limit_counter, PDH_FMT_LARGE, None, &mut limit_val)
                != ERROR_SUCCESS.0
            {
                return (0, 0);
            }

            // CStatus == 0 表示数据有效；largeValue 为 i64，负数钳到 0 再转 u64。
            let committed = if committed_val.CStatus == 0 {
                committed_val.Anonymous.largeValue.max(0) as u64
            } else {
                0
            };
            let limit = if limit_val.CStatus == 0 {
                limit_val.Anonymous.largeValue.max(0) as u64
            } else {
                0
            };

            (committed, limit)
        }
    }
}

impl Drop for CommitPdh {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

/// Windows PDH-based paging-activity sampler.
///
/// 采集 `\Memory\Page Reads/sec`（速率计数器）：每秒因硬页错误（hard page fault）
/// 而从磁盘（含 pagefile）读入的页数。它度量「换页活动强度」这一**流量**口径，
/// 而非 swap 已用存量——前者才是真正的 swap 卡顿信号（见
/// `docs/memory-stutter-detection.md` 阶段 C）。作为瞬时速率计数器，PDH 在两次
/// `PdhCollectQueryData` 之间自动计算每秒速率，故 `sample()` 返回的是 per-second 值。
struct PagingPdh {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
}

impl PagingPdh {
    fn new() -> Option<Self> {
        unsafe {
            let mut query: PDH_HQUERY = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != ERROR_SUCCESS.0 {
                warn!("PdhOpenQueryW (paging) failed");
                return None;
            }

            let mut counter: PDH_HCOUNTER = PDH_HCOUNTER::default();
            let path = w!(r"\Memory\Page Reads/sec");

            if PdhAddEnglishCounterW(query, path, 0, &mut counter) != ERROR_SUCCESS.0 {
                warn!("PdhAddEnglishCounterW (page reads) failed");
                PdhCloseQuery(query);
                return None;
            }

            // 预采一次，给速率计数器建立基线（首个真实 sample 才能算出 per-second 速率）。
            PdhCollectQueryData(query);

            Some(Self { query, counter })
        }
    }

    /// 采集当前分页读取速率（页/秒）。返回 f32（低活动下可能为小数，用 DOUBLE 格式化保留精度）。
    fn sample(&self) -> f32 {
        unsafe {
            if PdhCollectQueryData(self.query) != ERROR_SUCCESS.0 {
                return 0.0;
            }

            let mut val: PDH_FMT_COUNTERVALUE = PDH_FMT_COUNTERVALUE::default();
            if PdhGetFormattedCounterValue(self.counter, PDH_FMT_DOUBLE, None, &mut val)
                != ERROR_SUCCESS.0
            {
                return 0.0;
            }

            // CStatus == 0 表示数据有效；doubleValue 已为每秒速率，负数钳到 0。
            if val.CStatus == 0 {
                val.Anonymous.doubleValue.max(0.0) as f32
            } else {
                0.0
            }
        }
    }
}

impl Drop for PagingPdh {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

/// 系统级信号采样结果（F-RC2）。
///
/// 各字段含义见 `Sample` 对应字段与 `卡顿根因分析-PRD.md` §3.2。
pub struct SysSample {
    pub disk_busy_percent: f32,
    pub disk_avg_io_ms: f32,
    pub dpc_percent: f32,
    pub interrupt_percent: f32,
    pub context_switches_per_sec: f32,
}

impl Default for SysSample {
    fn default() -> Self {
        Self {
            disk_busy_percent: 0.0,
            disk_avg_io_ms: 0.0,
            dpc_percent: 0.0,
            interrupt_percent: 0.0,
            context_switches_per_sec: 0.0,
        }
    }
}

/// Windows PDH-based system-level signal sampler（F-RC2）。
///
/// 单个 PDH query 同时挂载 5 个计数器，每 tick 采样一次：
/// - `\PhysicalDisk(_Total)\% Disk Time`：磁盘繁忙度（%）
/// - `\PhysicalDisk(_Total)\Avg Disk sec/Transfer`：单次 IO 延迟（秒，转 ms）
/// - `\Processor(_Total)\% DPC Time`：DPC 占用（%）
/// - `\Processor(_Total)\% Interrupt Time`：中断处理占用（%）
/// - `\System\Context Switches/sec`：上下文切换速率（/s）
///
/// 这些都是瞬时/速率型计数器，开销极低（与 `DiskPdh` 同为 PDH 句柄，不走 WMI）。
/// 数值全部用 `PDH_FMT_DOUBLE` 格式化以保留精度；CStatus != 0 或负数一律钳为 0。
struct SysPdh {
    query: PDH_HQUERY,
    disk_time: PDH_HCOUNTER,
    disk_avg_io: PDH_HCOUNTER,
    dpc_time: PDH_HCOUNTER,
    interrupt_time: PDH_HCOUNTER,
    ctx_switch: PDH_HCOUNTER,
}

impl SysPdh {
    fn new() -> Option<Self> {
        unsafe {
            let mut query: PDH_HQUERY = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != ERROR_SUCCESS.0 {
                warn!("PdhOpenQueryW (sys) failed");
                return None;
            }

            let mut disk_time: PDH_HCOUNTER = PDH_HCOUNTER::default();
            let mut disk_avg_io: PDH_HCOUNTER = PDH_HCOUNTER::default();
            let mut dpc_time: PDH_HCOUNTER = PDH_HCOUNTER::default();
            let mut interrupt_time: PDH_HCOUNTER = PDH_HCOUNTER::default();
            let mut ctx_switch: PDH_HCOUNTER = PDH_HCOUNTER::default();

            let disk_time_path = w!(r"\PhysicalDisk(_Total)\% Disk Time");
            if PdhAddEnglishCounterW(query, disk_time_path, 0, &mut disk_time) != ERROR_SUCCESS.0 {
                warn!("PdhAddEnglishCounterW (sys disk time) failed");
                PdhCloseQuery(query);
                return None;
            }
            let disk_avg_io_path = w!(r"\PhysicalDisk(_Total)\Avg Disk sec/Transfer");
            if PdhAddEnglishCounterW(query, disk_avg_io_path, 0, &mut disk_avg_io)
                != ERROR_SUCCESS.0
            {
                warn!("PdhAddEnglishCounterW (sys disk avg io) failed");
                PdhCloseQuery(query);
                return None;
            }
            let dpc_time_path = w!(r"\Processor(_Total)\% DPC Time");
            if PdhAddEnglishCounterW(query, dpc_time_path, 0, &mut dpc_time) != ERROR_SUCCESS.0 {
                warn!("PdhAddEnglishCounterW (sys dpc time) failed");
                PdhCloseQuery(query);
                return None;
            }
            let interrupt_time_path = w!(r"\Processor(_Total)\% Interrupt Time");
            if PdhAddEnglishCounterW(query, interrupt_time_path, 0, &mut interrupt_time)
                != ERROR_SUCCESS.0
            {
                warn!("PdhAddEnglishCounterW (sys interrupt time) failed");
                PdhCloseQuery(query);
                return None;
            }
            let ctx_switch_path = w!(r"\System\Context Switches/sec");
            if PdhAddEnglishCounterW(query, ctx_switch_path, 0, &mut ctx_switch) != ERROR_SUCCESS.0
            {
                warn!("PdhAddEnglishCounterW (sys ctx switch) failed");
                PdhCloseQuery(query);
                return None;
            }

            // 预采一次，给速率计数器（Context Switches/sec）建立基线。
            PdhCollectQueryData(query);

            Some(Self {
                query,
                disk_time,
                disk_avg_io,
                dpc_time,
                interrupt_time,
                ctx_switch,
            })
        }
    }

    /// 采集当前系统级信号快照。
    fn sample(&self) -> SysSample {
        unsafe {
            if PdhCollectQueryData(self.query) != ERROR_SUCCESS.0 {
                return SysSample::default();
            }

            let mut dt: PDH_FMT_COUNTERVALUE = PDH_FMT_COUNTERVALUE::default();
            let mut dio: PDH_FMT_COUNTERVALUE = PDH_FMT_COUNTERVALUE::default();
            let mut dpc: PDH_FMT_COUNTERVALUE = PDH_FMT_COUNTERVALUE::default();
            let mut intr: PDH_FMT_COUNTERVALUE = PDH_FMT_COUNTERVALUE::default();
            let mut ctx: PDH_FMT_COUNTERVALUE = PDH_FMT_COUNTERVALUE::default();

            if PdhGetFormattedCounterValue(self.disk_time, PDH_FMT_DOUBLE, None, &mut dt)
                != ERROR_SUCCESS.0
                || PdhGetFormattedCounterValue(self.disk_avg_io, PDH_FMT_DOUBLE, None, &mut dio)
                    != ERROR_SUCCESS.0
                || PdhGetFormattedCounterValue(self.dpc_time, PDH_FMT_DOUBLE, None, &mut dpc)
                    != ERROR_SUCCESS.0
                || PdhGetFormattedCounterValue(self.interrupt_time, PDH_FMT_DOUBLE, None, &mut intr)
                    != ERROR_SUCCESS.0
                || PdhGetFormattedCounterValue(self.ctx_switch, PDH_FMT_DOUBLE, None, &mut ctx)
                    != ERROR_SUCCESS.0
            {
                return SysSample::default();
            }

            let d = |v: &PDH_FMT_COUNTERVALUE| -> f32 {
                if v.CStatus == 0 {
                    v.Anonymous.doubleValue.max(0.0) as f32
                } else {
                    0.0
                }
            };

            SysSample {
                disk_busy_percent: d(&dt),
                // Avg Disk sec/Transfer 是秒，×1000 转毫秒
                disk_avg_io_ms: (d(&dio) * 1000.0),
                dpc_percent: d(&dpc),
                interrupt_percent: d(&intr),
                context_switches_per_sec: d(&ctx),
            }
        }
    }
}

impl Drop for SysPdh {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

/// F-RC3：探测前台窗口是否真无响应（挂起）。
///
/// 通过 `SendMessageTimeout(WM_NULL, timeout_ms)` 向前台窗口投递一条空消息，
/// 若窗口消息循环挂起（`SMTO_ABORTIFHUNG` 命中）则在超时前返回 0（冻结）。
/// 无前台窗口（桌面 / 锁屏 / 无焦点）返回 `false`——无法判断时按「未冻结」处理，
/// 避免误报。此函数本身有 ~timeout_ms 的阻塞成本，**调用方必须限频、
/// 不得进入采集热路径**（PRD §F-RC3 / R5）。
pub(crate) fn probe_foreground_window_frozen(timeout_ms: u32) -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return false;
        }
        // 清空残留错误，避免上一次 API 的 ERROR_TIMEOUT 残留干扰「窗口是否挂起」判定
        SetLastError(ERROR_SUCCESS);
        let mut presult: usize = 0;
        let ret = SendMessageTimeoutW(
            hwnd,
            WM_NULL,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            timeout_ms,
            Some(&mut presult),
        );
        if ret.0 == 0 {
            // 返回 0 可能是 WM_NULL 成功（DefWindowProc 返回 0）**或**超时/失败。
            // 仅靠 GetLastError == ERROR_TIMEOUT 区分：只有前台窗口真挂起才会置该错误；
            // 否则（窗口正常响应 WM_NULL 返回 0）GetLastError 非 ERROR_TIMEOUT → 视为未冻结。
            // 这是 SendMessageTimeout 的经典坑：不能仅凭 LRESULT==0 判冻结（违背 PRD §F-RC3
            // 「区分资源高但还能动与真卡死」的初衷，否则会大量误报）。
            return GetLastError() == ERROR_TIMEOUT;
        }
        // ret != 0：窗口正常响应消息（明确成功），未冻结
        false
    }
}

pub struct Collector {
    sys: System,
    networks: Networks,
    prev_net_sent: u64,
    prev_net_recv: u64,
    tick: u32,
    disk_pdh: Option<DiskPdh>,
    commit_pdh: Option<CommitPdh>,
    paging_pdh: Option<PagingPdh>,
    /// 系统级信号采样器（F-RC2）：磁盘繁忙度 + DPC/中断/上下文切换
    sys_pdh: Option<SysPdh>,
}

impl Collector {
    pub fn new() -> Self {
        let mut sys = System::new_all();
        sys.refresh_all();
        let networks = Networks::new_with_refreshed_list();

        let mut prev_net_sent = 0u64;
        let mut prev_net_recv = 0u64;
        for (_name, data) in networks.iter() {
            prev_net_sent += data.total_transmitted();
            prev_net_recv += data.total_received();
        }

        let disk_pdh = DiskPdh::new();
        let commit_pdh = CommitPdh::new();
        let paging_pdh = PagingPdh::new();
        let sys_pdh = SysPdh::new();

        Self {
            sys,
            networks,
            prev_net_sent,
            prev_net_recv,
            tick: 0,
            disk_pdh,
            commit_pdh,
            paging_pdh,
            sys_pdh,
        }
    }

    /// 兼容入口：完整采集（含 top_processes 快照）。
    /// 等价于 `collect_with(true)`，现有调用方（测试等）行为不变。
    pub fn collect(&mut self) -> Sample {
        self.collect_with(true)
    }

    /// 采集一帧系统指标。
    ///
    /// `need_processes == false` 时跳过全进程遍历构建 top_processes（空列表）——
    /// 平时（无卡顿）detector 不消费它，省掉每 tick 的进程排序开销；
    /// 卡顿进行中或刚结束一帧才需要快照（见 `Detector::needs_process_snapshot`）。
    pub fn collect_with(&mut self, need_processes: bool) -> Sample {
        self.sys.refresh_all();
        self.networks.refresh(true);

        let tick = self.tick;
        self.tick = self.tick.wrapping_add(1);

        let cpu_usage = self.sys.global_cpu_usage();
        let cpu_per_core: Vec<f32> = self.sys.cpus().iter().map(|c| c.cpu_usage()).collect();

        let mem_total = self.sys.total_memory();
        let mem_used = self.sys.used_memory();
        let mem_available = self.sys.available_memory();
        let mem_total_mb = mem_total / (1024 * 1024);
        let mem_used_mb = mem_used / (1024 * 1024);
        let mem_available_mb = mem_available / (1024 * 1024);
        let mem_usage_percent = if mem_total > 0 {
            (mem_used as f32 / mem_total as f32) * 100.0
        } else {
            0.0
        };

        let swap_total = self.sys.total_swap();
        let swap_used = self.sys.used_swap();
        let swap_usage_percent = if swap_total > 0 {
            (swap_used as f32 / swap_total as f32) * 100.0
        } else {
            0.0
        };

        let mut net_sent_total = 0u64;
        let mut net_recv_total = 0u64;
        for (_name, data) in self.networks.iter() {
            net_sent_total += data.total_transmitted();
            net_recv_total += data.total_received();
        }
        let net_sent_bps = net_sent_total.saturating_sub(self.prev_net_sent);
        let net_recv_bps = net_recv_total.saturating_sub(self.prev_net_recv);
        self.prev_net_sent = net_sent_total;
        self.prev_net_recv = net_recv_total;

        let process_count = self.sys.processes().len();

        // Disk I/O: sampled every tick via PDH (accurate, never 0 due to the
        // old every-5-tick WMI bug).
        let (disk_read_bps, disk_write_bps) = match &self.disk_pdh {
            Some(d) => d.sample(),
            None => (0, 0),
        };

        // 提交电荷（commit charge）：瞬时计数器，每 tick 采样。
        let (commit_bytes, commit_limit) = match &self.commit_pdh {
            Some(c) => c.sample(),
            None => (0, 0),
        };

        // 分页活动速率（Page Reads/sec）：速率计数器，每 tick 采样。
        let page_reads_per_sec = match &self.paging_pdh {
            Some(p) => p.sample(),
            None => 0.0,
        };

        // 系统级信号（F-RC2）：磁盘繁忙度 + DPC/中断/上下文切换，每 tick 采样。
        let sys = match &self.sys_pdh {
            Some(s) => s.sample(),
            None => SysSample::default(),
        };

        // Slow channel (every 5 ticks): CPU freq, GPU usage, temperature via WMI.
        // These are expensive/rarely-changing, so leaving them on the slow
        // channel is fine.
        let (cpu_freq, gpu_usage, cpu_temp) = if tick % 5 == 0 {
            self.collect_wmi_slow()
        } else {
            (None, None, None)
        };

        // 进程快照：取 top by CPU 与 top by 内存的并集（去重），最多 12 个，
        // 用于卡顿 culprit 归因（detector 在卡顿持续期间累积，结束时提取 top）。
        // 非卡顿时跳过（need_processes == false），省掉每 tick 的全进程遍历排序。
        let top_processes = if need_processes {
            Self::collect_top_processes(&self.sys)
        } else {
            Vec::new()
        };

        Sample {
            timestamp: Utc::now(),
            cpu_usage,
            cpu_per_core,
            cpu_freq_mhz: cpu_freq,
            mem_usage_percent,
            mem_used_mb,
            mem_total_mb,
            mem_available_mb,
            swap_usage_percent,
            commit_bytes,
            commit_limit,
            page_reads_per_sec,
            disk_read_bps,
            disk_write_bps,
            disk_busy_percent: sys.disk_busy_percent,
            disk_avg_io_ms: sys.disk_avg_io_ms,
            dpc_percent: sys.dpc_percent,
            interrupt_percent: sys.interrupt_percent,
            context_switches_per_sec: sys.context_switches_per_sec,
            net_sent_bps,
            net_recv_bps,
            net_sent_total,
            net_recv_total,
            gpu_usage,
            cpu_temp,
            gpu_temp: None,
            process_count,
            thread_count: 0,
            top_processes,
        }
    }

    /// 从 `sysinfo::System` 取 top 进程快照（CPU / 内存维度各取前 8，去重合并最多 12）。
    ///
    /// sysinfo 0.39：`process.memory()` 返回字节，`/ (1024*1024)` 转 MB；
    /// `process.cpu_usage()` 为全局 CPU 百分比；`pid.as_u32()` 取进程 ID。
    fn collect_top_processes(sys: &System) -> Vec<ProcessBrief> {
        let all: Vec<ProcessBrief> = sys
            .processes()
            .iter()
            .map(|(pid, p)| ProcessBrief {
                pid: pid.as_u32(),
                name: p.name().to_string_lossy().into_owned(),
                cpu_usage: p.cpu_usage(),
                mem_used_mb: p.memory() / (1024 * 1024),
            })
            .collect();

        ProcessBrief::merge_top(all, 8, 8, 12)
    }

    fn collect_wmi_slow(&self) -> (Option<f32>, Option<f32>, Option<f32>) {
        // wmi 0.18：WMIConnection::new() 自动初始化 COM，无需 COMLibrary
        let wmi_con = match WMIConnection::new() {
            Ok(c) => c,
            Err(e) => {
                warn!("WMI connection failed: {}", e);
                return (None, None, None);
            }
        };

        let cpu_freq = wmi_con
            .raw_query("SELECT CurrentClockSpeed FROM Win32_Processor")
            .ok()
            .and_then(|r: Vec<HashMap<String, Variant>>| r.first().cloned())
            .and_then(|row| {
                if let Some(Variant::UI4(v)) = row.get("CurrentClockSpeed") {
                    Some(*v as f32)
                } else {
                    None
                }
            });

        // Fixed: the correct class is Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine.
        // It has multiple rows (one per GPU engine), so we sum UtilizationPercentage
        // across all rows and cap at 100%.
        let gpu_usage = wmi_con
            .raw_query(
                "SELECT UtilizationPercentage \
                 FROM Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine",
            )
            .ok()
            .and_then(|r: Vec<HashMap<String, Variant>>| {
                let per_engine: Vec<Option<u64>> = r
                    .iter()
                    .map(|row| match row.get("UtilizationPercentage") {
                        Some(Variant::UI8(v)) => Some(*v),
                        Some(Variant::UI4(v)) => Some(*v as u64),
                        _ => None,
                    })
                    .collect();
                aggregate_gpu_utilization(&per_engine)
            });

        let cpu_temp = wmi_con
            .raw_query("SELECT CurrentTemperature FROM Win32_PerfFormattedData_ThermalZoneInformation")
            .ok()
            .and_then(|r: Vec<HashMap<String, Variant>>| r.first().cloned())
            .and_then(|row| {
                if let Some(Variant::UI4(v)) = row.get("CurrentTemperature") {
                    Some((*v as f32) / 10.0 - 273.15)
                } else {
                    None
                }
            });

        (cpu_freq, gpu_usage, cpu_temp)
    }
}

/// 聚合各 GPU 引擎的利用率：累加并封顶 100%。
///
/// - 无引擎（空数组）→ `None`（无法从 WMI 拿到数据）
/// - 单引擎 → 该引擎值（封顶 100）
/// - 多引擎 → 求和（封顶 100），兼容 UI8 / UI4 混用
/// - 某引擎取不到值（`None`）→ 视作 0 参与累加，不丢弃其它引擎
pub fn aggregate_gpu_utilization(per_engine: &[Option<u64>]) -> Option<f32> {
    if per_engine.is_empty() {
        return None;
    }
    let sum: u64 = per_engine
        .iter()
        .fold(0u64, |acc, v| acc.saturating_add(v.unwrap_or(0)));
    Some((sum as f32).min(100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_new() {
        let collector = Collector::new();
        assert_eq!(collector.tick, 0);
    }

    #[test]
    fn collector_collect_returns_valid_sample() {
        let mut collector = Collector::new();
        let sample = collector.collect();

        assert!(sample.cpu_usage >= 0.0 && sample.cpu_usage <= 100.0);
        assert!(sample.mem_total_mb > 0);
        assert!(sample.process_count > 0);
    }

    #[test]
    fn collector_collect_increments_tick() {
        let mut collector = Collector::new();
        assert_eq!(collector.tick, 0);
        collector.collect();
        assert_eq!(collector.tick, 1);
        collector.collect();
        assert_eq!(collector.tick, 2);
    }

    #[test]
    fn collector_collect_covers_per_core() {
        let mut collector = Collector::new();
        let sample = collector.collect();
        assert!(!sample.cpu_per_core.is_empty());
        for usage in &sample.cpu_per_core {
            assert!(*usage >= 0.0 && *usage <= 100.0);
        }
    }

    /// `collect_with(false)`：非卡顿时跳过 top_processes 构建（空列表）；
    /// `collect_with(true)`：完整快照（真实进程数 > 0）。
    #[test]
    fn collector_collect_with_controls_top_processes() {
        let mut collector = Collector::new();

        let sparse = collector.collect_with(false);
        assert!(sparse.top_processes.is_empty(), "非卡顿帧不应构建 top_processes");

        let full = collector.collect_with(true);
        assert!(
            !full.top_processes.is_empty(),
            "卡顿帧应构建 top_processes（真实进程数 > 0）"
        );
    }

    /// F-RC2：每 tick 采样的系统级信号字段应存在且为有限非负数
    /// （PDH 不可用时回落 0.0，不崩溃）。
    #[test]
    fn collector_collect_includes_sys_signals() {
        let mut collector = Collector::new();
        let sample = collector.collect();

        // 全部有限、非负（磁盘繁忙度/IO 延迟/DPC/中断/上下文切换）
        for v in [
            sample.disk_busy_percent,
            sample.disk_avg_io_ms,
            sample.dpc_percent,
            sample.interrupt_percent,
            sample.context_switches_per_sec,
        ] {
            assert!(v.is_finite(), "系统信号应为有限值，got: {}", v);
            assert!(v >= 0.0, "系统信号应非负，got: {}", v);
        }
        // 磁盘繁忙度上限 100%（% Disk Time 不可能超过 100）
        assert!(
            sample.disk_busy_percent <= 100.0,
            "磁盘繁忙度不应超过 100%，got: {}",
            sample.disk_busy_percent
        );
    }

    // ===== aggregate_gpu_utilization（P2 GPU 采集，纯逻辑）=====

    #[test]
    fn gpu_empty_returns_none() {
        assert_eq!(aggregate_gpu_utilization(&[]), None);
    }

    #[test]
    fn gpu_single_engine() {
        assert_eq!(aggregate_gpu_utilization(&[Some(42)]), Some(42.0));
    }

    #[test]
    fn gpu_multi_engine_sum() {
        // 两个引擎 30 + 40 = 70
        assert_eq!(aggregate_gpu_utilization(&[Some(30), Some(40)]), Some(70.0));
    }

    #[test]
    fn gpu_caps_at_100() {
        // 三个引擎 50 + 40 + 30 = 120 → 封顶 100
        assert_eq!(
            aggregate_gpu_utilization(&[Some(50), Some(40), Some(30)]),
            Some(100.0)
        );
    }

    #[test]
    fn gpu_missing_engine_treated_as_zero() {
        // 一个引擎取不到值（None）视作 0，不丢弃其它引擎
        assert_eq!(
            aggregate_gpu_utilization(&[Some(30), None, Some(40)]),
            Some(70.0)
        );
    }

    #[test]
    fn gpu_all_missing_returns_zero() {
        // 全部取不到值 → 0%（有引擎但无数据）
        assert_eq!(aggregate_gpu_utilization(&[None, None]), Some(0.0));
    }

    #[test]
    fn gpu_overflow_saturates() {
        // u64 溢出用 saturating 语义 → 封顶 100
        assert_eq!(
            aggregate_gpu_utilization(&[Some(u64::MAX), Some(u64::MAX)]),
            Some(100.0)
        );
    }
}
