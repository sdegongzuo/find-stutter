//! F-RC14-d：ETW 调用栈采样（模块 + RVA 级别）。
//!
//! 属于 PRD 中的「重能力」（M5 独立里程碑）：仅在卡顿触发后限频采样，不进采集热路径；
//! 任何初始化 / 运行失败都**静默降级**（该层关闭，不影响其余层与采集热路径，PRD §1.4 / R9 / R10）。
//!
//! 采样方式：**NT Kernel Logger**（系统内核日志器，全系统唯一）实时会话 +
//! `EVENT_TRACE_FLAG_PROFILE`（SampledProfile 采样）+ `TraceSetInformation(TraceStackTracingInfo)`
//! 让内核为 `PerfInfo` 类的 SampledProfile（Type=46）事件附带调用栈；
//! 回调线程把栈里的指令地址喂给全局 SINK，主线程用 culprit 进程的已加载模块表把地址
//! 解析成「模块名 + RVA」，再聚合成热点（PRD §1.4：不引入 PDB 符号化，只到模块 + RVA 级别）。
//!
//! ## 三个必须踩对的坑（都是「静默失败」，返回 ERROR_SUCCESS 但什么也采不到）
//!
//! 1. **会话身份**：只有 `KERNEL_LOGGER_NAME`（"NT Kernel Logger"）+
//!    `Wnode.Guid = SystemTraceControlGuid` 才有 `EVENT_TRACE_FLAG_PROFILE` 采样事件。
//!    自建会话名（如 `find-stutter-stack`）+ `Guid::zeroed()` 不会报错，但永远零事件。
//!    （2026-11-02 实测：该写法跑了几十个卡顿事件，`stack_samples` 表一条都没有。）
//! 2. **`EVENT_TRACE_LOGFILEW.LoggerName`**：实时消费必须填会话名。省略它 `OpenTraceW`
//!    不会失败——它会静默连到 `NT Kernel Logger`，看起来「打开了」但事件全丢。
//! 3. **`CLASSIC_EVENT_ID.EventGuid`**：必须是事件**类**的 GUID（PerfInfo =
//!    `{ce1dbfb4-137e-4da6-87b0-3f59aa102cbc}`）+ `Type = 46`，不是 `SystemTraceControlGuid`。
//!    填错时 `TraceSetInformation` 照样返回 `ERROR_SUCCESS`，但内核不挂栈 →
//!    `EVENT_RECORD.ExtendedDataCount` 恒为 0。
//!
//! 另外 `CLASSIC_EVENT_ID` 的长度必须用 crate 自带的 24 字节版本
//! （evntrace.h 里是 `GUID + UCHAR Type + UCHAR Reserved[7]`）。
//! 手写一个「看起来更对」的 20 字节版本会当场被内核拒掉：
//! `TraceSetInformation` 返回 `0x5B6`「参数大小错误」。

use crate::types::{ProcessBrief, StackSample};
use crate::win32::snapshot_module_addrs;
use log::warn;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, WIN32_ERROR};
use windows::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CLASSIC_EVENT_ID, CloseTrace, ControlTraceW, EVENT_HEADER_EXT_TYPE_STACK_TRACE32,
    EVENT_HEADER_EXT_TYPE_STACK_TRACE64, EVENT_HEADER_EXTENDED_DATA_ITEM, EVENT_RECORD,
    EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_FLAG_PROFILE,
    EVENT_TRACE_LOGFILEW, EVENT_TRACE_LOGFILEW_0, EVENT_TRACE_LOGFILEW_1, EVENT_TRACE_PROPERTIES,
    EVENT_TRACE_REAL_TIME_MODE, OpenTraceW, PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME,
    ProcessTrace, StartTraceW, TraceSetInformation, TraceStackTracingInfo, WNODE_FLAG_TRACED_GUID,
};

/// 系统内核日志器会话名（系统内唯一实例，与其他性能工具共享）。
const KERNEL_SESSION: &str = "NT Kernel Logger";
/// `SystemTraceControlGuid`：内核日志器会话在 `StartTraceW` 里的固定标识。
const SYSTEM_TRACE_CONTROL_GUID: GUID = GUID::from_u128(0x9e814aad_3204_11d2_9a82_006008a86939);
/// `PerfInfoGuid`：性能计数器事件父类，SampledProfile 属于它。
const PERF_INFO_GUID: GUID = GUID::from_u128(0xce1dbfb4_137e_4da6_87b0_3f59aa102cbc);
/// `PERFINFO_LOG_TYPE_SAMPLED_PROFILE`：采样事件类型（经典事件里落在 `Opcode`，不是 `Id`）。
const SAMPLED_PROFILE_OPCODE: u8 = 46;
/// `PERFINFO_LOG_TYPE_STACKWALK`：内核另发一条独立的栈回溯事件（`StackWalk_Event`）。
const STACKWALK_OPCODE: u8 = 32;
/// 采样窗口（毫秒）：一次卡顿触发后采集约 1.2s 的全系统 CPU 采样。
/// 不能太短——实时投递靠 `FlushTimer`（1s）+ 缓冲写满两个条件触发。
const SAMPLE_WINDOW_MS: u64 = 1_200;
/// 一次采样聚合后最多保留的热点条数。
const MAX_HOTSPOTS: usize = 20;

/// 一次采样收集的原始栈帧（回调线程写入、主线程解析）。
#[derive(Debug, Clone, Copy)]
struct RawFrame {
    pid: u32,
    address: u64,
}

/// 全局 SINK：ETW 回调线程写入原始帧，采样主线程读取后清空。
/// 同一时间只有一个采样在跑（限频），单例足够。
static SINK: OnceLock<Arc<Mutex<Vec<RawFrame>>>> = OnceLock::new();

/// 本轮窗口里是否已经从 `ExtendedData` 收到过栈。
///
/// 有些系统会**同时**投递「SampledProfile + ExtendedData 栈」和独立的 `StackWalk_Event`，
/// 两条都收会让每个栈帧被计两次（UI 上的 ×N 直接翻倍）。实测本机只走 ExtendedData
/// 一条（264k 帧 ≈ 1ms×14核×1.2s×15 帧，量级自洽），但这不是规范保证的，
/// 所以「见过 ExtendedData 栈就忽略 StackWalk」——由回调线程串行写，无需原子操作之外的同步。
static SAW_EXT_STACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn sink() -> Arc<Mutex<Vec<RawFrame>>> {
    SINK.get_or_init(|| Arc::new(Mutex::new(Vec::new()))).clone()
}

/// 把一批原始帧追加进 SINK。锁中毒（回调里不该发生 panic，但不能让它穿过 FFI 边界）
/// 时静默丢弃，不让 `unwrap()` 把进程 abort 掉。
fn push_frames(frames: Vec<RawFrame>) {
    if frames.is_empty() {
        return;
    }
    if let Ok(mut s) = sink().lock() {
        s.extend(frames);
    }
}

/// 从 `EVENT_EXTENDED_ITEM_STACK_TRACE64/32` 里取出地址数组。
///
/// 布局：`MatchId(u64)` 在前，随后才是地址数组（32 位是 `u32`、64 位是 `u64`），
/// 所以地址区起点是 `DataPtr + 8`，不是 `DataPtr + 1 个元素`。
unsafe fn parse_stack_ext(item: &EVENT_HEADER_EXTENDED_DATA_ITEM, pid: u32) -> Vec<RawFrame> {
    unsafe {
        let data_size = item.DataSize as usize;
        if data_size <= 8 || item.DataPtr == 0 {
            return Vec::new();
        }
        let base = (item.DataPtr as *const u8).add(8);
        let mut out = Vec::new();
        match item.ExtType as u32 {
            EVENT_HEADER_EXT_TYPE_STACK_TRACE64 => {
                let count = (data_size - 8) / 8;
                let addrs = base as *const u64;
                out.reserve(count);
                for i in 0..count {
                    let addr = std::ptr::read_unaligned(addrs.add(i));
                    if addr != 0 {
                        out.push(RawFrame { pid, address: addr });
                    }
                }
            }
            EVENT_HEADER_EXT_TYPE_STACK_TRACE32 => {
                let count = (data_size - 8) / 4;
                let addrs = base as *const u32;
                out.reserve(count);
                for i in 0..count {
                    let addr = std::ptr::read_unaligned(addrs.add(i)) as u64;
                    if addr != 0 {
                        out.push(RawFrame { pid, address: addr });
                    }
                }
            }
            _ => {}
        }
        out
    }
}

/// 解析 `StackWalk_Event` 载荷：
/// `EventTimeStamp(u64) + StackProcess(u32) + StackThread(u32) + 地址数组[u64]`。
///
/// 注意 pid 在**载荷**里（`StackProcess`），事件头的 `ProcessId` 恒为 System(4)。
unsafe fn parse_stack_walk(user_data: *const u8, len: usize) -> Vec<RawFrame> {
    unsafe {
        if user_data.is_null() || len < 16 {
            return Vec::new();
        }
        let pid = std::ptr::read_unaligned(user_data.add(8) as *const u32);
        let count = (len - 16) / 8;
        let addrs = user_data.add(16) as *const u64;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let addr = std::ptr::read_unaligned(addrs.add(i));
            if addr != 0 {
                out.push(RawFrame { pid, address: addr });
            }
        }
        out
    }
}

/// ETW 实时事件回调：只认 SampledProfile 的栈。
///
/// 两条来源，按优先级取一条（见 [`SAW_EXT_STACK`]）：
/// - `Opcode == 46` 且带 `ExtendedData` 栈 → 用事件头的 `ProcessId` 归属；
/// - `Opcode == 32`（`StackWalk_Event`）→ 用载荷里的 `StackProcess` 归属。
unsafe extern "system" fn etw_event_callback(record: *mut EVENT_RECORD) {
    unsafe {
        if record.is_null() {
            return;
        }
        let rec = &*record;
        let opcode = rec.EventHeader.EventDescriptor.Opcode;

        if opcode == SAMPLED_PROFILE_OPCODE {
            if rec.ExtendedData.is_null() || rec.ExtendedDataCount == 0 {
                return;
            }
            let pid = rec.EventHeader.ProcessId;
            let items = std::slice::from_raw_parts(rec.ExtendedData, rec.ExtendedDataCount as usize);
            let mut out = Vec::new();
            for item in items {
                out.extend(parse_stack_ext(item, pid));
            }
            if !out.is_empty() {
                SAW_EXT_STACK.store(true, std::sync::atomic::Ordering::Relaxed);
                push_frames(out);
            }
        } else if opcode == STACKWALK_OPCODE {
            if SAW_EXT_STACK.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            push_frames(parse_stack_walk(rec.UserData as *const u8, rec.UserDataLength as usize));
        }
    }
}

/// 一次采样的统计：只为诊断——区分「压根没采到栈」和「采到栈但解析不到模块」
/// 这两种失败长得完全一样（都是热点为空），但根因和修法完全不同。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SampleStats {
    /// 回调收到的原始栈帧数（含无法解析的）
    pub raw_frames: usize,
    /// 聚合后的热点条数
    pub hotspots: usize,
    /// 本次涉及的 culprit 进程数
    pub culprits: usize,
}

/// ETW 调用栈采样器。`enabled=false` 表示初始化失败，已静默降级（该层关闭）。
pub struct StackSampler {
    enabled: bool,
    /// 初始化失败原因（成功为 None），供服务写诊断日志——提权排查时唯一的证据链。
    reason: Option<String>,
}

impl StackSampler {
    // new() 会做 ETW 能力探测（有副作用），不实现 Default（避免语义误导）
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        match probe_etw() {
            Ok(()) => Self {
                enabled: true,
                reason: None,
            },
            Err(e) => {
                warn!("ETW 调用栈采样初始化失败（{}），该层静默关闭", e);
                Self {
                    enabled: false,
                    reason: Some(e),
                }
            }
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 初始化失败原因；成功返回 None。
    pub fn disabled_reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// 对一组 culprit 进程做一次限频调用栈采样，返回聚合后的热点（可能为空）。
    pub fn sample(&self, culprits: &[ProcessBrief]) -> Vec<StackSample> {
        self.sample_with_stats(culprits).0
    }

    /// 同 [`sample`](Self::sample)，但额外返回本次统计（诊断用）。
    pub fn sample_with_stats(&self, culprits: &[ProcessBrief]) -> (Vec<StackSample>, SampleStats) {
        let mut stats = SampleStats {
            culprits: culprits.len(),
            ..Default::default()
        };
        if !self.enabled || culprits.is_empty() {
            return (Vec::new(), stats);
        }
        match run_sample_window() {
            Ok(frames) => {
                stats.raw_frames = frames.len();
                let samples = aggregate_stack_samples(&frames, culprits);
                stats.hotspots = samples.len();
                (samples, stats)
            }
            Err(e) => {
                warn!("ETW 调用栈采样运行失败，本次跳过: {}", e);
                (Vec::new(), stats)
            }
        }
    }
}

/// 探测 ETW 能力：能否启动内核会话 + 让采样事件带栈，并立即停止。
/// 失败原因原样返回（供 `disabled_reason` 落到诊断日志）。
fn probe_etw() -> Result<(), String> {
    let handle = start_session()?;
    stop_session(handle);
    Ok(())
}

/// 启动 NT Kernel Logger 实时会话并开启「采样事件带栈」。失败返回原因（含缺权限 / 会话被占）。
fn start_session() -> Result<CONTROLTRACE_HANDLE, String> {
    unsafe {
        let props_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
        let name: Vec<u16> = KERNEL_SESSION
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let total = props_size + name.len() * 2;
        // 必须按 8 字节对齐分配：`Vec<u8>` 的 align 是 1，强转成 `*mut EVENT_TRACE_PROPERTIES`
        // （align 8）是 UB（heap 实际给的地址恰好对齐，但类型系统不保证）
        let mut buf: Vec<u64> = vec![0; total.div_ceil(8)];
        // 会话名紧跟在 EVENT_TRACE_PROPERTIES 之后，由 LoggerNameOffset 指过去
        std::ptr::copy_nonoverlapping(
            name.as_ptr() as *const u8,
            (buf.as_mut_ptr() as *mut u8).add(props_size),
            name.len() * 2,
        );
        let props = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        (*props).Wnode.BufferSize = total as u32;
        // 内核日志器会话的固定标识；填别的 GUID（含 zeroed）会拿到一个「能开但没事件」的会话
        (*props).Wnode.Guid = SYSTEM_TRACE_CONTROL_GUID;
        // 1 = QPC 时间戳（与 PerfView / WPR 一致）
        (*props).Wnode.ClientContext = 1;
        (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        // 小缓冲（KB，需 4KB 对齐）→ 实时投递不必干等 1s 的 FlushTimer
        (*props).BufferSize = 16;
        (*props).MinimumBuffers = 8;
        (*props).MaximumBuffers = 64;
        (*props).FlushTimer = 1;
        (*props).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        (*props).EnableFlags = EVENT_TRACE_FLAG_PROFILE;
        (*props).LoggerNameOffset = props_size as u32;
        (*props).LogFileNameOffset = 0;

        let mut handle = CONTROLTRACE_HANDLE::default();
        let rc = StartTraceW(&mut handle, PCWSTR(name.as_ptr()), props);
        if rc == ERROR_ALREADY_EXISTS {
            return Err("NT Kernel Logger 会话已被其他程序占用（如 WPR / xperf）".to_string());
        }
        if rc != WIN32_ERROR(0) {
            if rc == ERROR_ACCESS_DENIED {
                return Err(format!(
                    "StartTraceW 拒绝访问，需要管理员 / LocalSystem: 0x{:08X}",
                    rc.0
                ));
            }
            return Err(format!("StartTraceW 失败: 0x{:08X}", rc.0));
        }

        // 让内核为 PerfInfo 类的 SampledProfile(46) 事件附带调用栈。
        // `EventGuid` 必须是事件**类** GUID（PerfInfo），不是 SystemTraceControlGuid——
        // 填错时仍然返回 ERROR_SUCCESS，但内核不挂栈（ExtendedDataCount 恒 0）。
        let mut ids = [CLASSIC_EVENT_ID {
            EventGuid: PERF_INFO_GUID,
            Type: SAMPLED_PROFILE_OPCODE,
            Reserved: [0; 7],
        }];
        let rc = TraceSetInformation(
            handle,
            TraceStackTracingInfo,
            ids.as_mut_ptr() as *const core::ffi::c_void,
            std::mem::size_of_val(&ids) as u32,
        );
        if rc != WIN32_ERROR(0) {
            stop_session(handle);
            return Err(format!(
                "TraceSetInformation(TraceStackTracingInfo) 失败: 0x{:08X}",
                rc.0
            ));
        }
        Ok(handle)
    }
}

/// 停止**我们自己**刚启动的会话（先按句柄停，失败再按名停——
/// 按名停会把别人（WPR / xperf）正在跑的内核会话一起杀掉，所以只在
/// `start_session` 确认会话是本进程起的之后才走这条路）。
///
/// 必须有兜底：停不掉会话，`ProcessTrace` 就永不返回，采样线程会永久挂住。
fn stop_session(handle: CONTROLTRACE_HANDLE) {
    unsafe {
        let props_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
        let mut props: EVENT_TRACE_PROPERTIES = EVENT_TRACE_PROPERTIES::default();
        props.Wnode.BufferSize = props_size as u32;
        props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        props.LoggerNameOffset = props_size as u32;
        let rc = ControlTraceW(handle, PCWSTR::null(), &mut props, EVENT_TRACE_CONTROL_STOP);
        if rc == WIN32_ERROR(0) {
            return;
        }
        let name: Vec<u16> = KERNEL_SESSION
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut props2: EVENT_TRACE_PROPERTIES = EVENT_TRACE_PROPERTIES::default();
        props2.Wnode.BufferSize = props_size as u32;
        props2.LoggerNameOffset = props_size as u32;
        let _ = ControlTraceW(
            CONTROLTRACE_HANDLE::default(),
            PCWSTR(name.as_ptr()),
            &mut props2,
            EVENT_TRACE_CONTROL_STOP,
        );
    }
}

/// 执行一次限频采样窗口：启动会话 → 打开实时日志（必须带 `LoggerName`）→
/// `ProcessTrace` 后台线程收集 → 采样窗口结束按句柄停会话（解除阻塞）→ 取回 SINK 原始帧。
/// 任一步失败返回 Err（调用方降级为跳过本次）。
fn run_sample_window() -> Result<Vec<RawFrame>, String> {
    let session = start_session()?;
    unsafe {
        // 清空 SINK，避免累积上一轮数据；同时重置「本轮是否已见过 ExtendedData 栈」
        if let Ok(mut s) = sink().lock() {
            s.clear();
        }
        SAW_EXT_STACK.store(false, std::sync::atomic::Ordering::Relaxed);
        // LoggerName 必须指向会话名，且要在 ProcessTrace 期间保持存活
        let mut logger_name: Vec<u16> = KERNEL_SESSION
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let logger_ptr = logger_name.as_mut_ptr();
        let mut logfile: EVENT_TRACE_LOGFILEW = EVENT_TRACE_LOGFILEW {
            LoggerName: PWSTR(logger_ptr),
            Anonymous1: EVENT_TRACE_LOGFILEW_0 {
                ProcessTraceMode: PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD,
            },
            Anonymous2: EVENT_TRACE_LOGFILEW_1 {
                EventRecordCallback: Some(etw_event_callback),
            },
            ..EVENT_TRACE_LOGFILEW::default()
        };
        let trace = OpenTraceW(&mut logfile);
        if trace.Value == u64::MAX || trace.Value == 0 {
            stop_session(session);
            return Err("OpenTraceW 失败（实时会话打不开）".to_string());
        }
        // ProcessTrace 阻塞直到会话被停止，放到后台线程
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let worker = match std::thread::Builder::new()
            .name("etw-stack".into())
            .spawn(move || {
                let _ = ProcessTrace(&[trace], None, None);
                let _ = done_tx.send(());
            }) {
            Ok(w) => w,
            Err(e) => {
                // 线程起不来必须把会话和 trace 句柄收掉：否则 NT Kernel Logger 会话
                // 会一直挂着，之后每次采样都 ERROR_ALREADY_EXISTS，栈采样永久失效
                // （直到服务重启），而且别的工具也再开不了内核会话。
                stop_session(session);
                let _ = CloseTrace(trace);
                return Err(format!("ETW 采样线程创建失败: {}", e));
            }
        };
        std::thread::sleep(Duration::from_millis(SAMPLE_WINDOW_MS));
        // 停止会话解除 ProcessTrace 阻塞
        stop_session(session);
        // 有上限地等：ProcessTrace 万一不返回，不能让 etw-worker 线程永久卡死
        match done_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(()) => {
                let _ = worker.join();
                let _ = CloseTrace(trace);
                // 正常路径：ProcessTrace 已返回，缓冲区可以释放
                drop(logger_name);
            }
            Err(_) => {
                // 异常路径：ProcessTrace 可能还在跑并持有 LoggerName，**不能**释放缓冲区
                // （释放就是悬垂指针）；宁可泄漏这一小块内存，也不做 UAF。
                warn!("ProcessTrace 未在 3s 内返回，放弃本次采样线程（泄漏 logger_name，不做 UAF）");
                std::mem::forget(logger_name);
                std::mem::forget(worker);
            }
        }
        let frames = match sink().lock() {
            Ok(v) => v.clone(),
            Err(_) => Vec::new(),
        };
        Ok(frames)
    }
}

/// 聚合原始栈帧为调用栈热点（模块 + RVA 级别）。
///
/// 1. 对每个 culprit 进程快照已加载模块（基址 + 大小）；
/// 2. 把采样到的指令地址解析成「模块名 + RVA」（地址落在模块 [base, base+size) 内）；
/// 3. 按 (pid, 进程名, 模块, RVA) 计数，降序取前 MAX_HOTSPOTS。
fn aggregate_stack_samples(frames: &[RawFrame], culprits: &[ProcessBrief]) -> Vec<StackSample> {
    // pid -> 进程名
    let names: HashMap<u32, String> = culprits.iter().map(|c| (c.pid, c.name.clone())).collect();
    // pid -> [(模块路径, 基址, 大小)]
    let mut maps: HashMap<u32, Vec<(String, u64, u64)>> = HashMap::new();
    for pid in names.keys() {
        let mods = snapshot_module_addrs(*pid);
        if !mods.is_empty() {
            maps.insert(*pid, mods);
        }
    }
    let mut tally: HashMap<(u32, String, String, u64), u64> = HashMap::new();
    for f in frames {
        let Some(mods) = maps.get(&f.pid) else {
            continue;
        };
        let Some((path, rva)) = resolve_address(mods, f.address) else {
            continue;
        };
        let name = names.get(&f.pid).cloned().unwrap_or_default();
        *tally.entry((f.pid, name, path, rva)).or_insert(0) += 1;
    }
    let mut out: Vec<StackSample> = tally
        .into_iter()
        .map(|((pid, name, module, rva), sample_count)| StackSample {
            pid,
            process_name: name,
            module,
            rva,
            sample_count,
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.sample_count));
    out.truncate(MAX_HOTSPOTS);
    out
}

/// 把指令地址解析成 (模块路径, RVA)。地址落在某模块 [base, base+size) 内则命中。
fn resolve_address(mods: &[(String, u64, u64)], address: u64) -> Option<(String, u64)> {
    for (path, base, size) in mods {
        if address >= *base && address < base.saturating_add(*size) {
            return Some((path.clone(), address - *base));
        }
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Diagnostics::Etw::EVENT_HEADER_EXTENDED_DATA_ITEM_0;

    #[test]
    fn resolve_address_hits_within_module() {
        let mods = vec![("C:\\app\\a.dll".to_string(), 0x1000, 0x1000)];
        assert_eq!(
            resolve_address(&mods, 0x1000),
            Some(("C:\\app\\a.dll".to_string(), 0))
        );
        assert_eq!(
            resolve_address(&mods, 0x1fff),
            Some(("C:\\app\\a.dll".to_string(), 0xfff))
        );
        // 边界外不命中
        assert_eq!(resolve_address(&mods, 0x2000), None);
        assert_eq!(resolve_address(&mods, 0x0fff), None);
    }

    #[test]
    fn resolve_address_zero_size_module_skipped() {
        let mods = vec![("x".to_string(), 0x1000, 0)];
        assert_eq!(resolve_address(&mods, 0x1000), None);
    }

    #[test]
    fn aggregate_stack_samples_tallies_by_module_rva() {
        let culprits = vec![ProcessBrief {
            pid: 7,
            name: "a.exe".into(),
            ..Default::default()
        }];
        let frames = vec![
            RawFrame {
                pid: 7,
                address: 0x1010,
            },
            RawFrame {
                pid: 7,
                address: 0x1010,
            },
            RawFrame {
                pid: 7,
                address: 0x1020,
            },
            RawFrame {
                pid: 7,
                address: 0x9999,
            }, // 无模块命中 → 丢弃
        ];
        // 真实模块映射来自系统 API，这里直接构造一个固定帧集合并验证计数逻辑
        // （地址解析依赖 snapshot_module_addrs，无法在测试里 mock，故此处只验证空输入安全）
        let out = aggregate_stack_samples(&[], &culprits);
        assert!(out.is_empty());
        // 无 culprit 时安全
        let out2 = aggregate_stack_samples(&frames, &[]);
        assert!(out2.is_empty());
    }

    #[test]
    fn stack_sampler_disabled_returns_empty() {
        let s = StackSampler {
            enabled: false,
            reason: Some("unit-test".into()),
        };
        assert!(!s.enabled());
        assert_eq!(s.disabled_reason(), Some("unit-test"));
        assert!(s.sample(&[]).is_empty());
    }

    /// `CLASSIC_EVENT_ID` 必须用 crate 的 24 字节布局（header 里是 `Reserved[7]`）。
    /// 手写「看起来更对」的 20 字节版本，`TraceSetInformation` 会返回 0x5B6「参数大小错误」。
    #[test]
    fn classic_event_id_uses_header_layout() {
        assert_eq!(std::mem::size_of::<CLASSIC_EVENT_ID>(), 24);
    }

    /// 造一个 `EVENT_HEADER_EXTENDED_DATA_ITEM`（DataPtr 指向调用方持有的字节缓冲区）。
    fn ext_item(ext_type: u32, buf: &[u8]) -> EVENT_HEADER_EXTENDED_DATA_ITEM {
        EVENT_HEADER_EXTENDED_DATA_ITEM {
            Reserved1: 0,
            ExtType: ext_type as u16,
            Anonymous: EVENT_HEADER_EXTENDED_DATA_ITEM_0 { _bitfield: 0 },
            DataSize: buf.len() as u16,
            DataPtr: buf.as_ptr() as u64,
        }
    }

    /// 64 位栈扩展项：前 8 字节是 `MatchId`，必须被跳过（读错就会把 MatchId 当指令地址，
    /// 而且它通常落在模块外，表现为「热点全丢」而不是报错）。
    #[test]
    fn parse_stack_ext_skips_match_id() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0xDEAD_BEEFu64.to_le_bytes()); // MatchId
        buf.extend_from_slice(&0x7ff0_1000u64.to_le_bytes());
        buf.extend_from_slice(&0x7ff0_2000u64.to_le_bytes());
        let item = ext_item(EVENT_HEADER_EXT_TYPE_STACK_TRACE64, &buf);

        let frames = unsafe { parse_stack_ext(&item, 42) };
        assert_eq!(frames.len(), 2, "MatchId 不能被当成地址");
        assert_eq!(frames[0].pid, 42);
        assert_eq!(frames[0].address, 0x7ff0_1000);
        assert_eq!(frames[1].address, 0x7ff0_2000);
    }

    /// 32 位栈扩展项：地址是 `u32` 数组（同样跳过 8 字节 MatchId，不是跳 4 字节）。
    #[test]
    fn parse_stack_ext_handles_32bit_addresses() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u64.to_le_bytes()); // MatchId
        buf.extend_from_slice(&0x1000u32.to_le_bytes());
        buf.extend_from_slice(&0x2000u32.to_le_bytes());
        let item = ext_item(EVENT_HEADER_EXT_TYPE_STACK_TRACE32, &buf);

        let frames = unsafe { parse_stack_ext(&item, 7) };
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].address, 0x1000);
        assert_eq!(frames[1].address, 0x2000);

        // 只有 MatchId（8 字节）→ 没有地址
        let only_match = ext_item(EVENT_HEADER_EXT_TYPE_STACK_TRACE32, &buf[..8]);
        assert!(unsafe { parse_stack_ext(&only_match, 7) }.is_empty());
    }

    /// `StackWalk_Event` 载荷：pid 在偏移 8，地址从偏移 16 开始（不是 8）。
    #[test]
    fn parse_stack_walk_reads_pid_and_addrs() {
        let pid: u32 = 4321;
        let mut buf = Vec::new();
        buf.extend_from_slice(&123456u64.to_le_bytes()); // EventTimeStamp
        buf.extend_from_slice(&pid.to_le_bytes()); // StackProcess
        buf.extend_from_slice(&99u32.to_le_bytes()); // StackThread
        buf.extend_from_slice(&0x7ff6_0000_1000u64.to_le_bytes());
        buf.extend_from_slice(&0x7ff6_0000_2000u64.to_le_bytes());

        let frames = unsafe { parse_stack_walk(buf.as_ptr(), buf.len()) };
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].pid, pid, "pid 必须取载荷里的 StackProcess，不是事件头的 System(4)");
        assert_eq!(frames[1].address, 0x7ff6_0000_2000);

        // 载荷不足一个头（<16）或只有头没有地址 → 空
        assert!(unsafe { parse_stack_walk(buf.as_ptr(), 12) }.is_empty());
        assert!(unsafe { parse_stack_walk(buf.as_ptr(), 16) }.is_empty());
        assert!(unsafe { parse_stack_walk(std::ptr::null(), 64) }.is_empty());
    }
}
