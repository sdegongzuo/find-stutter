use crate::types::{ProcessBrief, Sample};
use chrono::Utc;
use log::{error, warn};
use std::collections::HashMap;
use sysinfo::{Networks, System};
use wmi::{Variant, WMIConnection};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, ERROR_TIMEOUT, GetLastError, HANDLE, LPARAM, SetLastError,
    WPARAM,
};
use windows::Win32::System::Threading::{
    GetGuiResources, GetProcessHandleCount, GetProcessIoCounters, OpenProcess, GR_GDIOBJECTS,
    GR_USEROBJECTS, IO_COUNTERS, PROCESS_QUERY_LIMITED_INFORMATION,
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
/// F-RC14-a/b：进程指纹采样器（句柄 / GDI / USER / 进程级 IO 速率）。
///
/// 仅在卡顿帧（collect_with(true)）调用，进程打开开销一次性、限频；
/// 非卡顿帧完全跳过（对齐 top_processes 既有节制，PRD §3.4.2）。
///
/// IO 速率由 GetProcessIoCounters 的累积字节计数跨帧差分得到
/// （io_read_bps / io_write_bps），故需保留上次计数与采样时刻；
/// 句柄 / GDI / USER 为瞬时值，取当前读数即可。
struct ProcessFingerprintSampler {
    /// pid -> (read_bytes, write_bytes) 上次 IO 累积计数
    io_prev: HashMap<u32, (u64, u64)>,
    /// 上次 IO 采样时刻（首采为 None，不产出速率）
    io_last: Option<std::time::Instant>,
}

/// 单个进程的软件指纹（F-RC14-a/b 采集结果，合并进 ProcessBrief）。
#[derive(Debug, Clone, Default)]
struct ProcessFingerprint {
    exe_path: Option<String>,
    handle_count: Option<u32>,
    gdi_objects: Option<u32>,
    user_objects: Option<u32>,
    io_read_bps: Option<u64>,
    io_write_bps: Option<u64>,
}

impl ProcessFingerprint {
    /// 把指纹一次性合并进 ProcessBrief（六字段合并收拢到一处，避免散落拷贝）。
    fn apply_to(&self, p: &mut ProcessBrief) {
        p.exe_path = self.exe_path.clone();
        p.handle_count = self.handle_count;
        p.gdi_objects = self.gdi_objects;
        p.user_objects = self.user_objects;
        p.io_read_bps = self.io_read_bps;
        p.io_write_bps = self.io_write_bps;
    }
}

impl ProcessFingerprintSampler {
    fn new() -> Self {
        Self {
            io_prev: HashMap::new(),
            io_last: None,
        }
    }

    /// 打开进程句柄（只读、限权 PROCESS_QUERY_LIMITED_INFORMATION）。失败（如拒绝访问）返回 None。
    fn open_process(pid: u32) -> Option<HANDLE> {
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok() }
    }

    /// 句柄数（GetProcessHandleCount）
    fn handle_count(h: HANDLE) -> Option<u32> {
        unsafe {
            let mut n = 0u32;
            GetProcessHandleCount(h, &mut n).ok()?;
            Some(n)
        }
    }

    /// GDI 对象数（GetGuiResources GR_GDIOBJECTS）
    fn gdi_objects(h: HANDLE) -> u32 {
        unsafe { GetGuiResources(h, GR_GDIOBJECTS) }
    }

    /// USER 对象数（GetGuiResources GR_USEROBJECTS）
    fn user_objects(h: HANDLE) -> u32 {
        unsafe { GetGuiResources(h, GR_USEROBJECTS) }
    }

    /// IO 累积字节计数（读、写）
    fn io_bytes(h: HANDLE) -> Option<(u64, u64)> {
        unsafe {
            let mut io = IO_COUNTERS::default();
            GetProcessIoCounters(h, &mut io).ok()?;
            Some((io.ReadTransferCount, io.WriteTransferCount))
        }
    }

    /// 采样一组进程的软件指纹。仅对 pids 内进程打开句柄（限频）；
    /// IO 速率为跨帧差分，首采该 pid 时为 None。
    fn sample(&mut self, sys: &System, pids: &[u32]) -> HashMap<u32, ProcessFingerprint> {
        let now = std::time::Instant::now();
        let elapsed = self.io_last.map(|t| t.elapsed().as_secs_f64());
        let mut out = HashMap::new();
        for &pid in pids {
            let mut fp = ProcessFingerprint::default();
            // 可执行文件完整路径：来自 sysinfo（无需再打开进程）
            if let Some(proc) = sys.process(sysinfo::Pid::from_u32(pid)) {
                fp.exe_path = proc.exe().map(|p| p.to_string_lossy().into_owned());
            }
            // 打开进程一次，句柄 / GDI / USER / IO 一起取，随后关闭
            if let Some(h) = Self::open_process(pid) {
                fp.handle_count = Self::handle_count(h);
                fp.gdi_objects = Some(Self::gdi_objects(h));
                fp.user_objects = Some(Self::user_objects(h));
                if let Some((cur_r, cur_w)) = Self::io_bytes(h) {
                    if let (Some((pr, pw)), Some(dt)) = (self.io_prev.get(&pid), elapsed) {
                        let dt = dt.max(0.001);
                        fp.io_read_bps = Some((cur_r.saturating_sub(*pr) as f64 / dt) as u64);
                        fp.io_write_bps = Some((cur_w.saturating_sub(*pw) as f64 / dt) as u64);
                    }
                    self.io_prev.insert(pid, (cur_r, cur_w));
                }
                unsafe {
                    let _ = CloseHandle(h);
                }
            }
            out.insert(pid, fp);
        }
        self.io_last = Some(now);
        out
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
///
/// **健壮性（修复前为「全有或全无」）**：任一计数器在本机不可用（如某些系统
/// `\PhysicalDisk(_Total)\Avg Disk sec/Transfer` 找不到 `_Total` 实例）时，**仅禁用
/// 该字段**并打日志，其余计数器照常工作——避免单个计数器失败拖垮 DPC/中断/磁盘
/// 繁忙/上下文切换全部静默失效（此前因此导致 detector 的 DiskBusy/Dpc/Interrupt/
/// ContextSwitch 判定从未触发，属于假阴性）。
struct SysPdh {
    query: PDH_HQUERY,
    disk_time: Option<PDH_HCOUNTER>,
    disk_avg_io: Option<PDH_HCOUNTER>,
    dpc_time: Option<PDH_HCOUNTER>,
    interrupt_time: Option<PDH_HCOUNTER>,
    ctx_switch: Option<PDH_HCOUNTER>,
}

impl SysPdh {
    fn new() -> Option<Self> {
        unsafe {
            let mut query: PDH_HQUERY = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != ERROR_SUCCESS.0 {
                error!("SysPdh: PdhOpenQueryW failed — 系统级信号采集整体不可用");
                return None;
            }

            // 逐计数器独立添加：失败的仅禁用该字段（记 warn），不影响其余计数器。
            // 修复此前「任一失败整体 return None」导致其余可用计数器也全被禁用的 bug。
            let mut disk_time: Option<PDH_HCOUNTER> = None;
            let mut disk_avg_io: Option<PDH_HCOUNTER> = None;
            let mut dpc_time: Option<PDH_HCOUNTER> = None;
            let mut interrupt_time: Option<PDH_HCOUNTER> = None;
            let mut ctx_switch: Option<PDH_HCOUNTER> = None;

            let mut c: PDH_HCOUNTER;
            c = PDH_HCOUNTER::default();
            if PdhAddEnglishCounterW(query, w!(r"\PhysicalDisk(_Total)\% Disk Time"), 0, &mut c)
                == ERROR_SUCCESS.0
            {
                disk_time = Some(c);
            } else {
                warn!("SysPdh: 计数器不可用（已禁用该字段）: \\PhysicalDisk(_Total)\\% Disk Time");
            }
            c = PDH_HCOUNTER::default();
            if PdhAddEnglishCounterW(query, w!(r"\PhysicalDisk(_Total)\Avg Disk sec/Transfer"), 0, &mut c)
                == ERROR_SUCCESS.0
            {
                disk_avg_io = Some(c);
            } else {
                warn!("SysPdh: 计数器不可用（已禁用该字段）: \\PhysicalDisk(_Total)\\Avg Disk sec/Transfer");
            }
            c = PDH_HCOUNTER::default();
            if PdhAddEnglishCounterW(query, w!(r"\Processor(_Total)\% DPC Time"), 0, &mut c)
                == ERROR_SUCCESS.0
            {
                dpc_time = Some(c);
            } else {
                warn!("SysPdh: 计数器不可用（已禁用该字段）: \\Processor(_Total)\\% DPC Time");
            }
            c = PDH_HCOUNTER::default();
            if PdhAddEnglishCounterW(query, w!(r"\Processor(_Total)\% Interrupt Time"), 0, &mut c)
                == ERROR_SUCCESS.0
            {
                interrupt_time = Some(c);
            } else {
                warn!("SysPdh: 计数器不可用（已禁用该字段）: \\Processor(_Total)\\% Interrupt Time");
            }
            c = PDH_HCOUNTER::default();
            if PdhAddEnglishCounterW(query, w!(r"\System\Context Switches/sec"), 0, &mut c)
                == ERROR_SUCCESS.0
            {
                ctx_switch = Some(c);
            } else {
                warn!("SysPdh: 计数器不可用（已禁用该字段）: \\System\\Context Switches/sec");
            }

            // 全部计数器都不可用 → 整体放弃（关闭 query，返回 None）
            if disk_time.is_none()
                && disk_avg_io.is_none()
                && dpc_time.is_none()
                && interrupt_time.is_none()
                && ctx_switch.is_none()
            {
                warn!("SysPdh: 所有系统级计数器均不可用，放弃采集");
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
    ///
    /// 每个计数器独立取数：未添加（`None`）或取数失败 / CStatus != 0 的字段返回 0，
    /// 不影响其他计数器——与 `new()` 的「逐计数器独立」策略一致。
    fn sample(&self) -> SysSample {
        unsafe {
            if PdhCollectQueryData(self.query) != ERROR_SUCCESS.0 {
                return SysSample::default();
            }

            let get = |c: Option<PDH_HCOUNTER>| -> f32 {
                let Some(c) = c else { return 0.0 };
                let mut v: PDH_FMT_COUNTERVALUE = PDH_FMT_COUNTERVALUE::default();
                if PdhGetFormattedCounterValue(c, PDH_FMT_DOUBLE, None, &mut v) != ERROR_SUCCESS.0 {
                    return 0.0;
                }
                if v.CStatus == 0 {
                    v.Anonymous.doubleValue.max(0.0) as f32
                } else {
                    0.0
                }
            };

            let dt = get(self.disk_time);
            let dio = get(self.disk_avg_io);
            let dpc = get(self.dpc_time);
            let intr = get(self.interrupt_time);
            let ctx = get(self.ctx_switch);

            SysSample {
                disk_busy_percent: dt,
                // Avg Disk sec/Transfer 是秒，×1000 转毫秒（该计数器本机不可用时恒为 0）
                disk_avg_io_ms: (dio * 1000.0),
                dpc_percent: dpc,
                interrupt_percent: intr,
                context_switches_per_sec: ctx,
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
    /// F-RC14-a/b：进程指纹采样器（句柄/GDI/USER/进程级 IO 速率）
    fingerprint: ProcessFingerprintSampler,
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
            fingerprint: ProcessFingerprintSampler::new(),
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
            self.collect_top_processes()
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
    fn collect_top_processes(&mut self) -> Vec<ProcessBrief> {
        let all: Vec<ProcessBrief> = self
            .sys
            .processes()
            .iter()
            .map(|(pid, p)| ProcessBrief {
                pid: pid.as_u32(),
                name: p.name().to_string_lossy().into_owned(),
                cpu_usage: p.cpu_usage(),
                mem_used_mb: p.memory() / (1024 * 1024),
                ..Default::default()
            })
            .collect();

        let top = ProcessBrief::merge_top(all, 8, 8, 12);

        // F-RC14-a/b：仅对进入 top 的进程采样软件指纹（限频、一次性打开）。
        // IO 速率需跨帧差分，故非卡顿帧（不调本函数）不更新基线，避免陈旧计数污染。
        let pids: Vec<u32> = top.iter().map(|p| p.pid).collect();
        let fps = self.fingerprint.sample(&self.sys, &pids);
        top.into_iter()
            .map(|mut p| {
                if let Some(fp) = fps.get(&p.pid) {
                    fp.apply_to(&mut p);
                }
                p
            })
            .collect()
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

        // 诊断：温度查询成功连接但无数据（本机无热区 / WMI 类不可用）时显式告警，
        // 避免 cpu_temp 静默恒为 None（此前无任何日志，属于假阴性）。
        if cpu_temp.is_none() {
            warn!("WMI 温度查询无数据：cpu_temp 将保持 None（连接成功但热区/WMI 类可能不可用）");
        }

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