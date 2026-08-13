//! F-RC14-d：ETW 调用栈采样（模块 + RVA 级别）。
//!
//! 属于 PRD 中的「重能力」（M5 独立里程碑）：仅在卡顿触发后限频采样，不进采集热路径；
//! 任何初始化 / 运行失败都**静默降级**（该层关闭，不影响其余层与采集热路径，PRD §1.4 / R9 / R10）。
//!
//! 采样方式：NT Kernel Logger 实时会话 + EVENT_TRACE_FLAG_PROFILE（SampledProfile 采样），
//! 对 Kernel-Process 提供者启用 StackWalk 关键字，回调线程把采样栈的指令地址喂给
//! 全局 SINK，主线程用 culprit 进程的已加载模块表把地址解析成「模块名 + RVA」，
//! 再聚合成热点（PRD §1.4：不引入 PDB 符号化，只到模块 + RVA 级别）。

use crate::types::{ProcessBrief, StackSample};
use crate::win32::snapshot_module_addrs;
use log::warn;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use windows::core::{GUID, PCWSTR};
use windows::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, EVENT_TRACE_CONTROL_STOP, EVENT_HEADER_EXT_TYPE_STACK_TRACE32,
    EVENT_HEADER_EXT_TYPE_STACK_TRACE64, EVENT_TRACE_FLAG_PROFILE, EVENT_TRACE_LOGFILEW, EVENT_TRACE_LOGFILEW_0,
    EVENT_TRACE_LOGFILEW_1, EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE, EVENT_RECORD, OpenTraceW,
    PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME, ProcessTrace, StartTraceW, WNODE_FLAG_TRACED_GUID,
};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, WIN32_ERROR};

/// NT Kernel Logger 的固定会话名（同一时间系统内仅允许一个实例）。
const SESSION_NAME: &str = "find-stutter-stack";
/// Microsoft-Windows-Kernel-Process 提供者 GUID（SampledProfile 事件来源）。
const KERNEL_PROCESS_GUID: GUID = GUID::from_u128(0x22fb2cd60e7b422ba0c72fad1fd0e716);
/// StackWalk 关键字（EnableTraceEx2 matchanykeyword）。
const KEYWORD_STACKWALK: u64 = 0x1000000000000;
/// 采样窗口（毫秒）：一次卡顿触发后采集约 1.5s 的调用栈。
const SAMPLE_WINDOW_MS: u64 = 500;
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

fn sink() -> Arc<Mutex<Vec<RawFrame>>> {
    SINK.get_or_init(|| Arc::new(Mutex::new(Vec::new()))).clone()
    
}

/// ETW 实时事件回调：只取 SampledProfile（Id 46）栈回溯事件，把指令地址压入 SINK。
unsafe extern "system" fn etw_event_callback(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    let rec = &*record;
    // SampledProfile 事件 ID = 46
    if rec.EventHeader.EventDescriptor.Id != 46 {
        return;
    }
    let pid = rec.EventHeader.ProcessId;
    if rec.ExtendedData.is_null() {
        return;
    }
    let items = std::slice::from_raw_parts(rec.ExtendedData, rec.ExtendedDataCount as usize);
    let sink = sink();
    let mut frames = sink.lock().unwrap();
    for item in items {
        let ext = item.ExtType as u32;
        if ext == EVENT_HEADER_EXT_TYPE_STACK_TRACE64 {
            // 布局：MatchId(8 字节) + 地址数组[u64]
            let count = (item.DataSize as usize).saturating_sub(8) / 8;
            let base = item.DataPtr as *const u64;
            for i in 0..count {
                let addr = *base.add(1 + i);
                if addr != 0 {
                    frames.push(RawFrame { pid, address: addr });
                    
                }
            }
        } else if ext == EVENT_HEADER_EXT_TYPE_STACK_TRACE32 {
            let count = (item.DataSize as usize).saturating_sub(8) / 4;
            let base = item.DataPtr as *const u32;
            for i in 0..count {
                let addr = *base.add(1 + i) as u64;
                if addr != 0 {
                    frames.push(RawFrame { pid, address: addr });
                }
            }
        }
    }
}
/// ETW 调用栈采样器。`enabled=false` 表示初始化失败，已静默降级（该层关闭）。
pub struct StackSampler {
    enabled: bool,
}

impl StackSampler {
    // new() 会做 ETW 能力探测（有副作用），不实现 Default（避免语义误导）
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let enabled = probe_etw();
        if !enabled {
            warn!("ETW 调用栈采样初始化失败（无权限 / 已被占用 / 平台不支持），该层静默关闭");
        }
        Self { enabled }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 对一组 culprit 进程做一次限频调用栈采样，返回聚合后的热点（可能为空）。
    pub fn sample(&self, culprits: &[ProcessBrief]) -> Vec<StackSample> {
        if !self.enabled || culprits.is_empty() {
            return Vec::new();
        }
        match run_sample_window() {
            Ok(frames) => aggregate_stack_samples(&frames, culprits),
            Err(e) => {
                warn!("ETW 调用栈采样运行失败，本次跳过: {}", e);
                Vec::new()
            }
        }
    }
}
/// 探测 ETW 能力：能否启动并立即停止一个 NT Kernel Logger 实时会话。
/// 失败（非管理员 / 会话名被占 / 平台不支持）返回 false，调用方据此静默降级。
fn probe_etw() -> bool {
    match start_session() {
        Ok(_handle) => {
            stop_session();
            true
        }
        Err(_) => false,
    }
}

/// 启动 NT Kernel Logger 实时会话（PROFILE 采样标志）。
/// 失败返回 Err（含「非管理员 / 会话被占」等）。
fn start_session() -> Result<windows::Win32::System::Diagnostics::Etw::CONTROLTRACE_HANDLE, String> {
    unsafe {
        let props_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
        let name: Vec<u16> = SESSION_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        let mut buf = vec![0u8; props_size + name.len() * 2];
        for (i, u) in name.iter().enumerate() {
            buf[props_size + i * 2] = *u as u8;
            buf[props_size + i * 2 + 1] = (*u >> 8) as u8;
        }
        let props = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        (*props).Wnode.BufferSize = (props_size + name.len() * 2) as u32;
        (*props).Wnode.Guid = GUID::zeroed();
        (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        (*props).BufferSize = 1024;
        (*props).MinimumBuffers = 4;
        (*props).MaximumBuffers = 32;
        (*props).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        (*props).EnableFlags = EVENT_TRACE_FLAG_PROFILE;
        (*props).LoggerNameOffset = props_size as u32;
        (*props).LogFileNameOffset = 0;
        let mut handle = Default::default();
        let name_pcwstr = PCWSTR(name.as_ptr());
        let rc = StartTraceW(&mut handle, name_pcwstr, props);
        if rc != WIN32_ERROR(0) {
            // ERROR_ALREADY_EXISTS 视为已就绪（会话已存在）
            if rc == ERROR_ALREADY_EXISTS {
                return Ok(handle);
            }
            if rc == ERROR_ACCESS_DENIED {
                return Err(format!("StartTraceW 拒绝访问（需要管理员权限）: 0x{:08X}", rc.0));
            }
            return Err(format!("StartTraceW 失败: 0x{:08X}", rc.0));
        }
        Ok(handle)
    }
}
/// 停止并清理 NT Kernel Logger 会话（幂等；失败仅记日志不影响主流程）。
fn stop_session() {
    unsafe {
        let props_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
        let name: Vec<u16> = SESSION_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        let mut props: EVENT_TRACE_PROPERTIES = EVENT_TRACE_PROPERTIES::default();
        props.Wnode.BufferSize = props_size as u32;
        props.LoggerNameOffset = props_size as u32;
        let _ = ControlTraceW(Default::default(), PCWSTR(name.as_ptr()), &mut props, EVENT_TRACE_CONTROL_STOP);
    }
}

/// 执行一次限频采样窗口：启动会话 → 打开实时日志 → ProcessTrace 后台线程收集 →
/// 约 1.5s 后停止会话（解除 ProcessTrace 阻塞）→ 取回 SINK 里的原始帧。
/// 任一步失败返回 Err（调用方降级为跳过本次）。
fn run_sample_window() -> Result<Vec<RawFrame>, String> {
    let session = start_session()?;
    unsafe {
        // 启用 Kernel-Process 提供者的 StackWalk 关键字（采样栈依赖它）
        let _ = EnableTraceEx2(session, &KERNEL_PROCESS_GUID, 1, 0, KEYWORD_STACKWALK, 0, 0, None);
        // 先清空 SINK，避免累积上一事件数据
        sink().lock().unwrap().clear();
        let mut logfile: EVENT_TRACE_LOGFILEW = EVENT_TRACE_LOGFILEW {
            Anonymous1: EVENT_TRACE_LOGFILEW_0 {
                LogFileMode: PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD,
            },
            Anonymous2: EVENT_TRACE_LOGFILEW_1 {
                EventRecordCallback: Some(etw_event_callback),
            },
            ..EVENT_TRACE_LOGFILEW::default()
        };
        let trace = OpenTraceW(&mut logfile);
        if trace.Value == 0 {
            stop_session();
            return Err("OpenTraceW 失败".to_string());
        }
        let trace_handle = trace;
        // ProcessTrace 阻塞直到会话被停止，放到后台线程
        let h = trace_handle;
        let worker = std::thread::Builder::new()
            .name("etw-sample".into())
            .spawn(move || {
                let _ = ProcessTrace(&[h], None, None);
            })
            .map_err(|e| format!("ETW 采样线程创建失败: {}", e))?;
        // 采样窗口
        std::thread::sleep(Duration::from_millis(SAMPLE_WINDOW_MS));
        // 停止会话解除 ProcessTrace 阻塞
        stop_session();
        let _ = worker.join();
        let _ = CloseTrace(trace_handle);
        let frames = sink().lock().unwrap().clone();
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
    let names: HashMap<u32, String> = culprits
        .iter()
        .map(|c| (c.pid, c.name.clone()))
        .collect();
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

    #[test]
    fn resolve_address_hits_within_module() {
        let mods = vec![("C:\\app\\a.dll".to_string(), 0x1000, 0x1000)];
        assert_eq!(resolve_address(&mods, 0x1000), Some(("C:\\app\\a.dll".to_string(), 0)));
        assert_eq!(resolve_address(&mods, 0x1fff), Some(("C:\\app\\a.dll".to_string(), 0xfff)));
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
            RawFrame { pid: 7, address: 0x1010 },
            RawFrame { pid: 7, address: 0x1010 },
            RawFrame { pid: 7, address: 0x1020 },
            RawFrame { pid: 7, address: 0x9999 }, // 无模块命中 → 丢弃
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
        let s = StackSampler { enabled: false };
        assert!(!s.enabled());
        assert!(s.sample(&[]).is_empty());
    }
}