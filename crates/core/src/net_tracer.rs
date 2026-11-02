//! 进程级网络流量采集（ETW `Microsoft-Windows-Kernel-Network`）。
//!
//! 背景（2026-11-02 修复）：进程详情页原先把 `IO_COUNTERS.OtherTransferCount`
//! 当作网络字节，导致「网络」列几乎恒为 0、累计值还被非网络 I/O 污染。
//! Windows 没有「给我某进程网速」的廉价 API（`GetProcessIoCounters` 只给
//! read/write/other 三类，socket 收发不在其中；`GetExtendedTcpTable` 只有连接
//! 清单、没有字节数），任务管理器那一列本身就是 ETW 得来的，这里走同一条路。
//!
//! 事件来源：manifest provider `Microsoft-Windows-Kernel-Network`
//! `{7DD42A49-5329-4832-8DFD-43D979153A88}`，按 IPv4/IPv6 关键字开启，
//! 只消费「数据收发」事件（payload 前 8 字节固定为 `PID: u32, size: u32`）：
//!
//! | 协议  | 发送 | 接收 |
//! |-------|------|------|
//! | TCPv4 | 10   | 11   |
//! | TCPv6 | 26   | 27   |
//! | UDPv4 | 42   | 43   |
//! | UDPv6 | 58   | 59   |
//!
//! 权限：该 provider 属于内核侧，**普通用户进程启用会被拒**（`ERROR_ACCESS_DENIED`，
//! 实测 0x5）。因此采集必须跑在有内核会话权限的上下文——本项目的
//! `find-stutter-service`（SCM 注册为 LocalSystem）正是这样的上下文。
//! 前台 `Run` 调试时通常拿不到权限，此时 `enabled() == false`，调用方降级
//! （UI 显示「—」而不是 0，避免把「没权限」伪装成「没流量」）。

use std::collections::HashMap;
use log::{info, warn};

/// 事件归属的进程与字节数（payload 首个 DWORD 为 PID，第二个为字节数）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PidBytes {
    pub sent: u64,
    pub recv: u64,
}

/// 一次采样窗口的读数：速率已按实际窗口时长归一化为 B/s + 累计值。
///
/// 速率必须由采集器归一化，不能把「本窗口字节数」直接落库：服务侧 tick 周期是
/// `interval_ms` + 本轮采集耗时（实际略大于标称值），下游若按「每行 = 1 秒」
/// 换算会高估。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetRate {
    pub pid: u32,
    /// 本窗口平均发送速率（B/s）
    pub sent_bps: u64,
    /// 本窗口平均接收速率（B/s）
    pub recv_bps: u64,
    /// 自 ETW 会话开始以来的累计发送字节数
    pub sent_total: u64,
    /// 自 ETW 会话开始以来的累计接收字节数
    pub recv_total: u64,
}

/// 进程网络采集器。`enabled == false` 表示 ETW 初始化失败（无权限 / 会话被占 /
/// 平台不支持），已静默降级，所有取数接口返回空。
pub struct NetTracer {
    enabled: bool,
    /// 上一次快照，用于算增量（diff）。
    last: HashMap<u32, PidBytes>,
    /// 上一次取数时刻（用 Instant：不受系统时钟调整影响，窗口时长更准）。
    last_at: Option<std::time::Instant>,
    #[cfg(windows)]
    control: Option<windows::Win32::System::Diagnostics::Etw::CONTROLTRACE_HANDLE>,
    #[cfg(windows)]
    consumer: Option<std::thread::JoinHandle<()>>,
    /// 消费线程的退出通知：Drop 用它**带超时**地等线程结束（停不掉会话时
    /// `ProcessTrace` 永不返回，直接 join 会把服务停止永久挂住）。
    #[cfg(windows)]
    done: Option<std::sync::mpsc::Receiver<()>>,
}

impl NetTracer {
    /// 启动常驻采集。失败静默降级（不 panic、不阻塞调用方）。
    pub fn start() -> Self {
        #[cfg(windows)]
        {
            match backtrace_impl::start() {
                Ok((control, consumer, done)) => {
                    info!("进程网络采集已启动（Kernel-Network ETW 实时会话）");
                    Self {
                        enabled: true,
                        last: HashMap::new(),
                        last_at: None,
                        control: Some(control),
                        consumer: Some(consumer),
                        done: Some(done),
                    }
                }
                Err(e) => {
                    warn!("进程网络采集初始化失败，该能力静默关闭: {e}");
                    Self {
                        enabled: false,
                        last: HashMap::new(),
                        last_at: None,
                        control: None,
                        consumer: None,
                        done: None,
                    }
                }
            }
        }
        #[cfg(not(windows))]
        {
            warn!("进程网络采集仅支持 Windows，已关闭");
            Self { enabled: false, last: HashMap::new(), last_at: None }
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 取一次读数：返回**当前所有有累计记录的进程**，速率为本窗口平均值（B/s）。
    ///
    /// 不做「速率 > 0」过滤：调用方需要靠 0 速率行维持「累计值不过期」，
    /// 否则静默进程的累计网络会在 UI 上从几 MB 掉成 0。
    /// 首次调用没有基线，速率全为 0（累计值正常）。
    pub fn take_rates(&mut self) -> Vec<NetRate> {
        if !self.enabled {
            return Vec::new();
        }
        let now_at = std::time::Instant::now();
        let dt = self
            .last_at
            .map(|t| now_at.duration_since(t).as_secs_f64())
            .unwrap_or(0.0);
        self.last_at = Some(now_at);

        let cur = current_totals();
        let mut out = Vec::with_capacity(cur.len());
        for (pid, now) in cur.iter() {
            let prev = self.last.get(pid).copied().unwrap_or_default();
            let sent_bytes = now.sent.saturating_sub(prev.sent);
            let recv_bytes = now.recv.saturating_sub(prev.recv);
            let (sent_bps, recv_bps) = if dt > 0.0 {
                ((sent_bytes as f64 / dt) as u64, (recv_bytes as f64 / dt) as u64)
            } else {
                (0, 0)
            };
            out.push(NetRate {
                pid: *pid,
                sent_bps,
                recv_bps,
                sent_total: now.sent,
                recv_total: now.recv,
            });
        }
        self.last = cur;
        out
    }

    /// 剔除已退出进程的累计值（防 pid 复用串味 + 内存增长）。
    ///
    /// 由调用方定期用当前进程表做存活判定传入；不在存活集合里的 pid 会被丢弃，
    /// 其累计值随之归零（进程已退出，UI 不再展示）。
    pub fn retain_alive(&mut self, alive: &impl Fn(u32) -> bool) {
        if !self.enabled {
            return;
        }
        self.last.retain(|pid, _| alive(*pid));
        #[cfg(windows)]
        backtrace_impl::retain_alive(alive);
    }

    /// 当前累计值（不推进 diff 基线），仅用于诊断日志。
    pub fn totals_len(&self) -> usize {
        if !self.enabled {
            return 0;
        }
        current_totals().len()
    }
}

impl Drop for NetTracer {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            // 先按句柄停会话解除 ProcessTrace 阻塞，再回收线程。
            backtrace_impl::stop_session(self.control.take());
            if let Some(done) = self.done.take() {
                // 会话停不掉时 ProcessTrace 永不返回，直接 join 会把服务停止永久挂住
                // （与 stack_sampler 那边同样的 3s 上限）。超时只放弃线程句柄，
                // 进程退出时 OS 会回收，比卡死服务强。
                match done.recv_timeout(std::time::Duration::from_secs(3)) {
                    Ok(()) => {
                        if let Some(h) = self.consumer.take() {
                            let _ = h.join();
                        }
                    }
                    Err(_) => {
                        warn!("ETW 网络消费线程未在 3s 内退出，放弃 join（不阻塞服务停止）");
                        if let Some(h) = self.consumer.take() {
                            std::mem::forget(h);
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 平台实现
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod backtrace_impl {
    use super::PidBytes;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, WIN32_ERROR};
    use windows::Win32::System::Diagnostics::Etw::{
        CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, StartTraceW,
        CONTROLTRACE_HANDLE, EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW,
        EVENT_TRACE_LOGFILEW_0, EVENT_TRACE_LOGFILEW_1, EVENT_TRACE_PROPERTIES,
        EVENT_TRACE_REAL_TIME_MODE, PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME,
        WNODE_FLAG_TRACED_GUID,
    };
    use windows::core::{GUID, PCWSTR};

    /// 会话名（同一时间系统内同名会话仅一个实例）
    const SESSION_NAME: &str = "find-stutter-net";
    /// Microsoft-Windows-Kernel-Network
    const NET_PROVIDER: GUID = GUID::from_u128(0x7dd42a49_5329_4832_8dfd_43d979153a88);
    /// IPv4 / IPv6 关键字（manifest 里这两个位控制数据事件，高位为 provider 基础关键字）
    const KEYWORD_IPV4: u64 = 0x8000_0000_0000_0010;
    const KEYWORD_IPV6: u64 = 0x8000_0000_0000_0020;

    /// 发送类事件：TCPv4 / TCPv6 / UDPv4 / UDPv6
    const SENT_IDS: [u16; 4] = [10, 26, 42, 58];
    /// 接收类事件：TCPv4 / TCPv6 / UDPv4 / UDPv6
    const RECV_IDS: [u16; 4] = [11, 27, 43, 59];

    static STATE: OnceLock<Arc<Mutex<HashMap<u32, PidBytes>>>> = OnceLock::new();

    fn state() -> Arc<Mutex<HashMap<u32, PidBytes>>> {
        STATE.get_or_init(|| Arc::new(Mutex::new(HashMap::new()))).clone()
    }

    /// ETW 实时回调：只处理数据收发事件，累加 (pid, 方向) 字节。
    ///
    /// 用 payload 首字段 PID 而不是 `EventHeader.ProcessId`：网络事件常在内核
    /// 上下文产生，事件头里的 PID 可能是 System（4）；payload 里的 PID 才是
    /// 事件归属的进程（MSDN：Send 事件带发起进程、Receive 事件带目标进程）。
    unsafe extern "system" fn net_event_callback(record: *mut EVENT_RECORD) {
        unsafe {
            if record.is_null() {
                return;
            }
            let rec = &*record;
            let id = rec.EventHeader.EventDescriptor.Id;
            let is_sent = SENT_IDS.contains(&id);
            let is_recv = RECV_IDS.contains(&id);
            if !is_sent && !is_recv {
                return;
            }
            // payload 前 8 字节固定：PID(u32) + size(u32)
            if rec.UserData.is_null() || (rec.UserDataLength as usize) < 8 {
                return;
            }
            let data = std::slice::from_raw_parts(rec.UserData as *const u8, 8);
            let pid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            let size = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as u64;
            if pid == 0 || size == 0 {
                return;
            }
            let st = state();
            // 回调线程绝不 panic 传播：锁中毒时取回内部值继续累加
            let mut guard = match st.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let entry = guard.entry(pid).or_default();
            if is_sent {
                entry.sent = entry.sent.saturating_add(size);
            } else {
                entry.recv = entry.recv.saturating_add(size);
            }
        }
    }

    /// 启动会话 + 启用 provider + 后台消费线程。任一步失败返回可读原因。
    ///
    /// 第三个返回值是消费线程的退出通知（线程结束时发送），供 `Drop` 带超时等待。
    pub(super) fn start() -> Result<
        (
            CONTROLTRACE_HANDLE,
            std::thread::JoinHandle<()>,
            std::sync::mpsc::Receiver<()>,
        ),
        String,
    > {
        // 先停掉可能残留的同名会话：服务崩溃/被强杀时实时会话不会自动注销，
        // 复用那个「没人消费」的会话会导致事件堆积，必须重新建。
        stop_session(None);
        let handle = start_session()?;
        // 先清空历史累计（服务重启后不应继承上一轮的值）
        {
            let st = state();
            if let Ok(mut g) = st.lock() {
                g.clear();
            }
        }
        unsafe {
            // 清理可能存在的残留启用状态（失败可忽略）
            let _ = EnableTraceEx2(handle, &NET_PROVIDER, 0, 0, 0, 0, 0, None);
            let keyword = KEYWORD_IPV4 | KEYWORD_IPV6;
            let rc = EnableTraceEx2(handle, &NET_PROVIDER, 1, 0, keyword, 0, 0, None);
            if rc != WIN32_ERROR(0) {
                stop_session(Some(handle));
                let hint = if rc == ERROR_ACCESS_DENIED {
                    "（需要管理员或 LocalSystem 权限）"
                } else {
                    ""
                };
                return Err(format!("EnableTraceEx2 失败: 0x{:08X}{hint}", rc.0));
            }

            // 消费线程自己完成 OpenTrace → ProcessTrace → CloseTrace：
            // `EVENT_TRACE_LOGFILEW` 与 `LoggerName` 的缓冲必须存活到 CloseTrace，
            // 放在线程里生命周期最清晰（放外面给别的线程用是悬垂指针）。
            let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            let consumer = std::thread::Builder::new()
                .name("etw-net".into())
                .spawn(move || {
                    {
                        // 关键：实时会话必须显式绑定会话名。不设 LoggerName 时
                        // OpenTrace 默认连 "NT Kernel Logger"，拿到的是另一个会话，
                        // 于是 ProcessTrace 立刻返回、回调永远收不到事件
                        // （表现为：会话建好、enabled=true，但一个字节都采不到）。
                        let mut name: Vec<u16> =
                            SESSION_NAME.encode_utf16().chain(std::iter::once(0)).collect();
                        let mut logfile: EVENT_TRACE_LOGFILEW = EVENT_TRACE_LOGFILEW {
                            LoggerName: windows::core::PWSTR(name.as_mut_ptr()),
                            Anonymous1: EVENT_TRACE_LOGFILEW_0 {
                                LogFileMode: PROCESS_TRACE_MODE_REAL_TIME
                                    | PROCESS_TRACE_MODE_EVENT_RECORD,
                            },
                            Anonymous2: EVENT_TRACE_LOGFILEW_1 {
                                EventRecordCallback: Some(net_event_callback),
                            },
                            ..EVENT_TRACE_LOGFILEW::default()
                        };
                        let trace = OpenTraceW(&mut logfile);
                        if trace.Value == 0 || trace.Value == u64::MAX {
                            let _ = ready_tx.send(Err(
                                "OpenTraceW 失败（实时会话无法打开）".to_string()
                            ));
                            // 通知 Drop：线程已退出，不必等满 3s 超时
                            let _ = done_tx.send(());
                            return;
                        }
                        let _ = ready_tx.send(Ok(()));
                        // 阻塞直到会话被停止
                        let _ = ProcessTrace(&[trace], None, None);
                        let _ = CloseTrace(trace);
                    }
                    // 退出通知必须在 CloseTrace 之后：Drop 收到它才认为可以 join
                    let _ = done_tx.send(());
                })
                .map_err(|e| {
                    stop_session(Some(handle));
                    format!("ETW 网络消费线程创建失败: {e}")
                })?;

            // 等消费线程确认 OpenTrace 成功后再返回，避免「enabled=true 但没数据」
            match ready_rx.recv_timeout(std::time::Duration::from_secs(3)) {
                Ok(Ok(())) => Ok((handle, consumer, done_rx)),
                Ok(Err(e)) => {
                    stop_session(Some(handle));
                    Err(e)
                }
                Err(_) => {
                    stop_session(Some(handle));
                    Err("ETW 消费线程启动超时（OpenTrace 未就绪）".to_string())
                }
            }
        }
    }

    fn start_session() -> Result<CONTROLTRACE_HANDLE, String> {
        unsafe {
            let props_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
            let name: Vec<u16> = SESSION_NAME.encode_utf16().chain(std::iter::once(0)).collect();
            let total = props_size + name.len() * 2;
            // 按 8 字节对齐分配：`Vec<u8>` 的 align 是 1，强转成 `*mut EVENT_TRACE_PROPERTIES`
            // （align 8）属于 UB（heap 恰好给对齐地址，但类型系统不保证）
            let mut buf: Vec<u64> = vec![0; total.div_ceil(8)];
            std::ptr::copy_nonoverlapping(
                name.as_ptr() as *const u8,
                (buf.as_mut_ptr() as *mut u8).add(props_size),
                name.len() * 2,
            );
            let props = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
            (*props).Wnode.BufferSize = total as u32;
            (*props).Wnode.Guid = GUID::zeroed();
            (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            // 网络事件量级大（大流量下载每秒可达数千条），缓冲区给足
            (*props).BufferSize = 64;
            (*props).MinimumBuffers = 16;
            (*props).MaximumBuffers = 128;
            (*props).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
            (*props).EnableFlags = Default::default();
            (*props).LoggerNameOffset = props_size as u32;
            (*props).LogFileNameOffset = 0;
            let mut handle = CONTROLTRACE_HANDLE::default();
            let rc = StartTraceW(&mut handle, PCWSTR(name.as_ptr()), props);
            if rc != WIN32_ERROR(0) {
                if rc == ERROR_ALREADY_EXISTS {
                    // 走到这里说明 `start()` 开头的按名 `stop_session(None)` 也没清掉残留会话。
                    // 此时 `handle` 是无效值，继续用它会得到一堆「看不懂的」错误码，
                    // 直接当成失败、把原因说清楚。
                    return Err("会话名 find-stutter-net 已被占用（疑似上一实例残留）".to_string());
                }
                if rc == ERROR_ACCESS_DENIED {
                    return Err(format!(
                        "StartTraceW 拒绝访问（需要管理员或 LocalSystem 权限）: 0x{:08X}",
                        rc.0
                    ));
                }
                return Err(format!("StartTraceW 失败: 0x{:08X}", rc.0));
            }
            Ok(handle)
        }
    }

    /// 停止并清理会话（幂等；失败仅记日志）。
    ///
    /// 有句柄时先按句柄停——只影响我们自己开的这个会话；失败（或本来就没句柄，
    /// 如启动前清残留）再按会话名兜底。按名停会把同名会话一起杀掉，所以不是首选。
    pub(super) fn stop_session(handle: Option<CONTROLTRACE_HANDLE>) {
        unsafe {
            let props_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
            if let Some(h) = handle {
                let mut props: EVENT_TRACE_PROPERTIES = EVENT_TRACE_PROPERTIES::default();
                props.Wnode.BufferSize = props_size as u32;
                props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
                props.LoggerNameOffset = props_size as u32;
                if ControlTraceW(h, PCWSTR::null(), &mut props, EVENT_TRACE_CONTROL_STOP)
                    == WIN32_ERROR(0)
                {
                    return;
                }
            }
            let name: Vec<u16> = SESSION_NAME.encode_utf16().chain(std::iter::once(0)).collect();
            let mut props: EVENT_TRACE_PROPERTIES = EVENT_TRACE_PROPERTIES::default();
            props.Wnode.BufferSize = props_size as u32;
            props.LoggerNameOffset = props_size as u32;
            let _ = ControlTraceW(
                Default::default(),
                PCWSTR(name.as_ptr()),
                &mut props,
                EVENT_TRACE_CONTROL_STOP,
            );
        }
    }

    /// 剔除已退出进程的累计值。
    pub(super) fn retain_alive(alive: &impl Fn(u32) -> bool) {
        let st = state();
        if let Ok(mut g) = st.lock() {
            g.retain(|pid, _| alive(*pid));
        }
    }

    /// 复制当前累计值快照。
    pub(super) fn snapshot() -> HashMap<u32, PidBytes> {
        let st = state();
        match st.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

/// 取当前累计值快照（非 Windows 平台恒为空）。
fn current_totals() -> HashMap<u32, PidBytes> {
    #[cfg(windows)]
    {
        backtrace_impl::snapshot()
    }
    #[cfg(not(windows))]
    {
        HashMap::new()
    }
}

/// 当前存活进程的 pid 集合（供 [`NetTracer::retain_alive`] 做存活判定）。
///
/// 只在清理节拍调用（默认 5 分钟一次），一次性枚举开销可忽略。
pub fn alive_pids() -> std::collections::HashSet<u32> {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    sys.processes().keys().map(|p| p.as_u32()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_snapshot_without_etw_returns_empty() {
        let mut t = NetTracer {
            enabled: false,
            last: HashMap::new(),
            last_at: None,
            #[cfg(windows)]
            control: None,
            #[cfg(windows)]
            consumer: None,
            #[cfg(windows)]
            done: None,
        };
        assert!(t.take_rates().is_empty());
        assert_eq!(t.totals_len(), 0);
        t.retain_alive(&|_| true);
    }

    #[test]
    fn pid_bytes_default_is_zero() {
        let b = PidBytes::default();
        assert_eq!((b.sent, b.recv), (0, 0));
    }
}
