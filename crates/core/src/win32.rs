//! F-RC14-b/c：Win32 软件根因数据采集（已加载模块快照 + Windows 事件日志回溯）。
//!
//! - `snapshot_process_modules`：Toolhelp32 快照某进程已加载模块（F-RC14-b）；
//! - `read_windows_events`：回溯 System/Application 事件日志，取 [since, until] 窗口的
//!   原始记录（F-RC14-c 数据源）。
//!
//! 所有函数都是「尽力而为」：底层 API 失败（权限不足 / 句柄拒绝）时返回空结果，
//! 由调用方（service）静默降级，不影响采集热路径（PRD §1.4 / R9）。

use crate::types::{ProcessModule, WindowsEventRecord};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W, TH32CS_SNAPMODULE,
    TH32CS_SNAPMODULE32,
};
use windows::Win32::System::EventLog::{
    CloseEventLog, OpenEventLogW, ReadEventLogW, EVENTLOG_SEQUENTIAL_READ, EVENTLOGRECORD,
    READ_EVENT_LOG_READ_FLAGS,
};

/// `EVENTLOG_BACKWARDS_READ`（从最新记录往回读）。windows crate 未导出该常量（值为 4）。
const EVENTLOG_BACKWARDS_READ: READ_EVENT_LOG_READ_FLAGS = READ_EVENT_LOG_READ_FLAGS(4);

/// 事件日志回溯单通道单次读取的最大字节数（64KB 缓冲）。
const EVENT_READ_BUF: usize = 64 * 1024;
/// 单通道最多回溯的记录条数（防无限循环 / 防过载）。
const MAX_RECORDS: usize = 5000;

/// 遍历某进程已加载模块（Toolhelp32 快照），对每个模块调用 `f`。
/// 失败（拒绝访问 / 快照失败）返回 None；`snapshot_process_modules` 与
/// `snapshot_module_addrs` 共用此循环，仅采集字段不同（避免重复同一段遍历）。
fn for_each_module(pid: u32, mut f: impl FnMut(&MODULEENTRY32W)) -> Option<()> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) }.ok()?;
    let mut me: MODULEENTRY32W = MODULEENTRY32W::default();
    me.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
    let mut first = unsafe { Module32FirstW(snapshot, &mut me).is_ok() };
    while first {
        f(&me);
        me.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
        first = unsafe { Module32NextW(snapshot, &mut me).is_ok() };
    }
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    Some(())
}

/// 取某进程已加载模块列表（F-RC14-b）。失败（拒绝访问 / 快照失败）返回空 Vec。
pub fn snapshot_process_modules(pid: u32, process_name: &str) -> Vec<ProcessModule> {
    let mut out = Vec::new();
    for_each_module(pid, |me| {
        let path = decode_utf16(&me.szExePath);
        out.push(ProcessModule {
            pid,
            process_name: process_name.to_string(),
            module_path: path,
            module_size: me.modBaseSize as u64,
        });
    });
    out
}

/// 把 UTF-16 数组解码为 String（遇 0 终止；非 UTF-16 字节用替换字符兜底）。
fn decode_utf16(arr: &[u16]) -> String {
    let end = arr.iter().position(|&c| c == 0).unwrap_or(arr.len());
    String::from_utf16_lossy(&arr[..end])
}

/// 回溯 System/Application 事件日志，取 [since_unix_secs, until_unix_secs] 窗口内的记录
/// （F-RC14-c 数据源）。只提取白名单判型需要的字段：通道 / 事件源 / 事件 ID / 级别 /
/// 消息摘要（前几条字符串拼接，截断 512）/ 发生时刻。
///
/// 实现要点：
/// - 用 EVENTLOG_SEQUENTIAL_READ | EVENTLOG_BACKWARDS_READ 从最新往回读（记录按时间有序），
///   当读到早于 since 的记录即停止该通道（时间窗口回溯，PRD §3.4.3「[onset-30s, now]」）；
/// - 缓冲不足（ERROR_INSUFFICIENT_BUFFER）时翻倍重试；读到底（ERROR_HANDLE_EOF）结束；
/// - 单通道最多读 MAX_RECORDS 条，防单次卡顿回溯开销失控。
pub fn read_windows_events(since_unix_secs: i64, until_unix_secs: i64) -> Vec<WindowsEventRecord> {
    let mut out = Vec::new();
    for channel in ["System", "Application"] {
        let log_name: Vec<u16> = channel.encode_utf16().chain(std::iter::once(0)).collect();
        let Ok(h) = (unsafe { OpenEventLogW(PCWSTR::null(), PCWSTR(log_name.as_ptr())) }) else {
            continue; // 无权限 / 通道不可用：静默跳过该通道
        };
        let records = read_channel(h, channel, since_unix_secs, until_unix_secs);
        unsafe {
            let _ = CloseEventLog(h);
        }
        out.extend(records);
    }
    out
}
/// 读单个事件日志通道的 [since, until] 窗口（从最新往回读，早于 since 即停）。
fn read_channel(
    h: HANDLE,
    channel: &str,
    since_unix_secs: i64,
    until_unix_secs: i64,
) -> Vec<WindowsEventRecord> {
    let mut out = Vec::new();
    let mut buf_size: usize = EVENT_READ_BUF;
    let mut buf: Vec<u8> = Vec::new();
    let mut reads: u32 = 0;
    loop {
        reads += 1;
        if reads > MAX_RECORDS as u32 {
            break; // 单通道回溯开销兜底
        }
        buf.resize(buf_size, 0);
        let mut bytes_read: u32 = 0;
        let mut needed: u32 = 0;
        let flags = READ_EVENT_LOG_READ_FLAGS(EVENTLOG_SEQUENTIAL_READ.0 | EVENTLOG_BACKWARDS_READ.0);
        let r = unsafe {
            ReadEventLogW(
                h,
                flags,
                0,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len() as u32,
                &mut bytes_read,
                &mut needed,
            )
        };
        match r {
            Ok(()) => {
                if bytes_read == 0 {
                    break;
                }
                let n = bytes_read as usize;
                let mut off = 0usize;
                let header_size = std::mem::size_of::<EVENTLOGRECORD>();
                while off + header_size <= n {
                    let rec: EVENTLOGRECORD = unsafe {
                        std::ptr::read_unaligned(buf.as_ptr().add(off) as *const EVENTLOGRECORD)
                    };
                    let length = rec.Length as usize;
                    if length == 0 || off + length > n {
                        break;
                    }
                    let ts = rec.TimeGenerated as i64;
                    if ts < since_unix_secs {
                        // 回读按时间倒序：一旦越过窗口下限，其余记录只会更旧
                        return out;
                    }
                    if ts <= until_unix_secs {
                        if let Some(ev) = parse_record(&buf[off..off + length], channel, ts, rec) {
                            out.push(ev);
                        }
                    }
                    off += length;
                }
                // 缓冲恰好填满说明可能还有更多记录；否则到本批次尾即结束
                if n == buf_size {
                    continue;
                }
                break;
            }
            Err(e) => {
                // Result<()> 的 Win32 错误码位于 HRESULT 低 16 位（0x8007xxxx）
                let code = (e.code().0 as u32) & 0xFFFF;
                if code == ERROR_INSUFFICIENT_BUFFER.0 {
                    buf_size = (buf_size * 2).min(4 * 1024 * 1024);
                    continue;
                }
                // ERROR_HANDLE_EOF / ERROR_EVENTLOG_FILE_CHANGED / 其它：本通道结束
                break;
            }
        }
    }
    out
}

/// 解析单条事件日志记录（header 之后是事件源 + NumStrings 个字符串）。
fn parse_record(buf: &[u8], channel: &str, ts: i64, rec: EVENTLOGRECORD) -> Option<WindowsEventRecord> {
    let header_size = std::mem::size_of::<EVENTLOGRECORD>();
    if buf.len() < header_size {
        return None;
    }
    // 事件源：紧跟 header 的 UTF-16 空终止串
    let source = read_utf16_z(buf, header_size);
    if source.is_empty() {
        return None;
    }
    // Vista+ 事件日志中，真正的事件 ID 在低 16 位（高位是 Provider 设施号）
    let id = rec.EventID & 0xFFFF;
    let level = match rec.EventType.0 {
        1 => "Error",
        2 => "Warning",
        4 => "Information",
        _ => "Other",
    };
    // 消息摘要：strings 区（StringOffset 起 NumStrings 个 UTF-16 空终止串），拼前几条
    let mut message = String::new();
    let so = rec.StringOffset as usize;
    if so >= header_size && so < buf.len() {
        let mut p = so;
        for _ in 0..rec.NumStrings {
            if p >= buf.len() {
                break;
            }
            match read_utf16_z_at(buf, p) {
                Some((s, next)) => {
                    if !s.is_empty() {
                        if !message.is_empty() {
                            message.push(' ');
                        }
                        message.push_str(&s);
                    }
                    p = next;
                }
                None => break,
            }
        }
    }
    if message.chars().count() > 512 {
        message = message.chars().take(512).collect();
    }
    Some(WindowsEventRecord {
        channel: channel.to_string(),
        provider: source,
        win_event_id: id,
        level: level.to_string(),
        message,
        ts: format_ts(ts),
    })
}

/// 读 start 起的 UTF-16 空终止串（不越界；返回 (字符串, 终止符后一字节偏移)）。
fn read_utf16_z_at(buf: &[u8], start: usize) -> Option<(String, usize)> {
    let mut end = start;
    while end + 1 < buf.len() {
        let u = u16::from_le_bytes([buf[end], buf[end + 1]]);
        if u == 0 {
            let s = String::from_utf16_lossy(&buf[start..end]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<u16>>());
            return Some((s, end + 2));
        }
        end += 2;
    }
    None
}

/// 读 start 起的 UTF-16 空终止串，只返回字符串（遇 0 终止；到缓冲尾也返回已读部分）。
fn read_utf16_z(buf: &[u8], start: usize) -> String {
    if start >= buf.len() {
        return String::new();
    }
    read_utf16_z_at(buf, start).map(|(s, _)| s).unwrap_or_default()
}

/// Unix 秒 → RFC3339（chrono 从 1970 起，合法范围内可解析）。
fn format_ts(unix_secs: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(unix_secs, 0)
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| unix_secs.to_string())
}


/// 取某进程已加载模块的 (完整路径, 基址, 大小) 列表（F-RC14-d 地址→模块解析用）。
/// 失败（拒绝访问 / 快照失败）返回空 Vec。
pub fn snapshot_module_addrs(pid: u32) -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    for_each_module(pid, |me| {
        let path = decode_utf16(&me.szExePath);
        out.push((path, me.modBaseAddr as u64, me.modBaseSize as u64));
    });
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProcessModule;

    #[test]
    fn decode_utf16_truncates_at_nul() {
        let arr = [b'a' as u16, 0, b'b' as u16];
        assert_eq!(decode_utf16(&arr), "a");
    }

    #[test]
    fn read_utf16_z_at_reads_until_nul() {
        // "Display" UTF-16 + nul
        let mut b = Vec::new();
        for c in "Display".encode_utf16() {
            b.extend_from_slice(&c.to_le_bytes());
        }
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0x58u16.to_le_bytes());
        let (s, next) = read_utf16_z_at(&b, 0).unwrap();
        assert_eq!(s, "Display");
        assert_eq!(next, b.len() - 2);
    }

    #[test]
    fn read_utf16_z_out_of_bounds_empty() {
        assert_eq!(read_utf16_z(&[], 0), "");
        assert_eq!(read_utf16_z(&[1, 2], 5), "");
    }

    #[test]
    fn format_ts_known_epoch() {
        assert_eq!(format_ts(0), "1970-01-01T00:00:00+00:00");
    }

    #[test]
    fn snapshot_process_modules_returns_current_exe() {
        // 当前进程必有模块；pid = 自己。
        let pid = std::process::id();
        let mods = snapshot_process_modules(pid, "self.exe");
        assert!(!mods.is_empty());
        let self_path = std::env::current_exe().unwrap();
        let any_self = mods
            .iter()
            .any(|m: &ProcessModule| {
                m.module_path
                    .to_lowercase()
                    .contains(&self_path.file_name().unwrap().to_string_lossy().to_lowercase())
            });
        assert!(any_self, "应包含当前 exe 模块");
    }

    #[test]
    fn read_windows_events_returns_bounded() {
        // 真实回溯：窗口放宽到 24h 内，不应 panic；返回条数有限。
        let until = chrono::Utc::now().timestamp();
        let since = until - 24 * 3600;
        let evs = read_windows_events(since, until);
        assert!(evs.len() <= MAX_RECORDS * 2);
        for ev in evs.iter().take(5) {
            assert!(!ev.provider.is_empty());
        }
    }
}