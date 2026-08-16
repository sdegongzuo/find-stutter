//! UAC 自动提权（Windows ShellExecuteExW + "runas"）——公共实现。
//!
//! ADR-0001：与 analytics 同理，提权封装无界面依赖，下沉 core 供
//! ui（auto_start 自动装/启服务）与 cli（upgrade 停服 / install-start）共用，
//! 避免两份同构实现漂移。
//!
//! ## 用途
//!
//! GUI / CLI 都是普通用户身份启动的，但 `find-stutter-service install` / `start`
//! 需要管理员权限。首次部署时自动检测到「服务未注册」并弹 UAC 申请 admin，
//! 完成安装 + 启动，用户体验是「双击 → 一路点是 → 看到绿点」。
//!
//! ## 实现
//!
//! - Windows: `ShellExecuteExW` 传 `lpVerb = "runas"`，系统弹 UAC。
//!   - 拿到 `hProcess` 后 `WaitForSingleObject` 同步等子进程退出。
//!   - 进程退出码 = 子进程退出码；UAC 拒绝 = 1223 (`ERROR_CANCELLED`)。
//! - 非 Windows: 直接返回 [`ElevateOutcome::Unsupported`]。
//!
//! ## 失败模式
//!
//! - `UacDenied`：用户在 UAC 弹窗里点「否」→ 1223
//! - `ShellFailed(code)`：ShellExecuteExW 返回错误码（<=32 算成功；>32 失败）
//! - `Timeout`：子进程未在 `timeout` 内退出
//! - `Unsupported`：非 Windows 平台
//!
//! 参考：<https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shellexecuteexw>

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

/// 单次提权调用的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElevateOutcome {
    /// 提权成功 + 子进程退出，附带子进程退出码
    Ok(i32),
    /// UAC 弹窗被用户取消（点「否」）
    UacDenied,
    /// ShellExecuteExW 返回的代码 <= 32（关联未找到 / 路径不存在 / 资源不足等）
    ShellFailed(i32),
    /// 子进程未在 `timeout` 内退出（服务 hang 住；调用方不应无限等）
    Timeout,
    /// OS 不支持 UAC 提权（非 Windows 平台）
    Unsupported,
    /// IO 错误（OsString 转换等）
    Os(String),
}

impl ElevateOutcome {
    /// 人类可读的单行描述（用于日志）
    pub fn message(&self) -> String {
        match self {
            Self::Ok(0) => "提权进程成功完成（退出码 0）".to_string(),
            Self::Ok(c) => format!("提权进程完成但退出码非 0: {}", c),
            Self::UacDenied => "UAC 弹窗被用户拒绝（点「否」或 ESC）".to_string(),
            Self::ShellFailed(c) => format!("ShellExecuteExW 失败 (code={})", c),
            Self::Timeout => "提权进程超时未退出".to_string(),
            Self::Unsupported => "当前 OS 不支持 UAC 提权（非 Windows）".to_string(),
            Self::Os(e) => format!("OS 字符串转换失败: {}", e),
        }
    }

    /// 是否成功（子进程退出 0）
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(0))
    }
}

/// 提权运行 `exe args...` 并同步等待子进程退出。
///
/// - `exe`：目标 exe 的完整路径
/// - `args`：传给 exe 的命令行参数（不含 exe 自身；不需要手动引号）
/// - `timeout`：最长等待时间；超时返回 [`ElevateOutcome::Timeout`]，不杀进程
///   （避免误杀正在跑的服务）
pub fn spawn_elevated_and_wait(
    exe: &Path,
    args: &[&str],
    timeout: Duration,
) -> ElevateOutcome {
    if !cfg!(windows) {
        return ElevateOutcome::Unsupported;
    }
    if !exe.exists() {
        return ElevateOutcome::ShellFailed(0); // ERROR_FILE_NOT_FOUND 等价：路径不存在
    }

    #[cfg(windows)]
    {
        spawn_elevated_and_wait_impl(exe, args, timeout)
    }
}

#[cfg(windows)]
fn spawn_elevated_and_wait_impl(
    exe: &Path,
    args: &[&str],
    timeout: Duration,
) -> ElevateOutcome {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HWND, WAIT_EVENT, WAIT_TIMEOUT};
    use windows::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, WaitForSingleObject,
    };

    // 1) 拼 lpParameters：纯参数（不含 exe 路径），空格分隔，含空格的 token 加引号
    let mut cmd_line: Vec<u16> = vec![];
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            cmd_line.push(' ' as u16);
        }
        if a.contains(' ') || a.contains('\t') {
            cmd_line.push('"' as u16);
            for c in a.encode_utf16() {
                cmd_line.push(c);
            }
            cmd_line.push('"' as u16);
        } else {
            for c in a.encode_utf16() {
                cmd_line.push(c);
            }
        }
    }
    // 末尾 null terminator
    cmd_line.push(0);

    // 2) 三个 wide string 都提升为 binding，确保活到 ShellExecuteExW 返回
    //    （PCWSTR 只是裸指针，原始 Vec 不能在表达式中临时构造）
    let lp_verb = wide_string(OsStr::new("runas"));
    let lp_file = wide_string(exe.as_os_str());

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI,
        hwnd: HWND(std::ptr::null_mut()),
        lpVerb: PCWSTR(lp_verb.as_ptr()),
        lpFile: PCWSTR(lp_file.as_ptr()),
        lpParameters: PCWSTR(cmd_line.as_ptr()),
        lpDirectory: PCWSTR::null(),
        nShow: SW_SHOWNORMAL.0,
        lpClass: PCWSTR::null(),
        ..Default::default()
    };

    // 3) 调 ShellExecuteExW
    let result = unsafe { ShellExecuteExW(&mut info) };
    if let Err(e) = result {
        // UAC 拒绝通常以 HRESULT 形式返回：0x800704C7 = ERROR_CANCELLED (1223)
        let code = e.code().0;
        if code == 1223 || code == 0x800704C7u32 as i32 {
            return ElevateOutcome::UacDenied;
        }
        return ElevateOutcome::Os(format!("ShellExecuteExW HRESULT 0x{:08X}", code as u32));
    }

    // ShellExecuteExW 成功 → info.hProcess 是子进程 HANDLE
    let h_process = info.hProcess;
    if h_process.is_invalid() {
        return ElevateOutcome::ShellFailed(0);
    }

    // 4) 同步等待
    let wait_result = unsafe { WaitForSingleObject(h_process, timeout.as_millis() as u32) };
    match wait_result {
        WAIT_EVENT(0) => {
            // 已退出：取退出码
            let mut exit_code: u32 = 0;
            let ok = unsafe { GetExitCodeProcess(h_process, &mut exit_code) };
            unsafe {
                let _ = CloseHandle(h_process);
            }
            if ok.is_ok() {
                ElevateOutcome::Ok(exit_code as i32)
            } else {
                ElevateOutcome::Os("GetExitCodeProcess failed".into())
            }
        }
        WAIT_TIMEOUT => {
            // 不杀进程（服务可能刚启动），让调用方决定
            unsafe {
                let _ = CloseHandle(h_process);
            }
            ElevateOutcome::Timeout
        }
        _ => {
            unsafe {
                let _ = CloseHandle(h_process);
            }
            ElevateOutcome::Os(format!("WaitForSingleObject 异常: {:?}", wait_result))
        }
    }
}

#[cfg(windows)]
fn wide_string(s: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(std::iter::once(0)).collect()
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    /// 验证：消息文案稳定（依赖字符串包含断言）
    #[test]
    fn message_contains_keywords() {
        assert!(ElevateOutcome::Ok(0).message().contains("成功"));
        assert!(ElevateOutcome::Ok(1).message().contains("非 0"));
        assert!(ElevateOutcome::UacDenied.message().contains("UAC"));
        assert!(ElevateOutcome::ShellFailed(31).message().contains("ShellExecuteExW"));
        assert!(ElevateOutcome::Timeout.message().contains("超时"));
        assert!(ElevateOutcome::Unsupported.message().contains("UAC"));
    }

    /// 验证：is_ok 只对 Ok(0) 为真
    #[test]
    fn is_ok_predicate() {
        assert!(ElevateOutcome::Ok(0).is_ok());
        assert!(!ElevateOutcome::Ok(1).is_ok());
        assert!(!ElevateOutcome::UacDenied.is_ok());
        assert!(!ElevateOutcome::ShellFailed(0).is_ok());
        assert!(!ElevateOutcome::Timeout.is_ok());
        assert!(!ElevateOutcome::Unsupported.is_ok());
        assert!(!ElevateOutcome::Os("x".into()).is_ok());
    }

    /// 验证：PartialEq 可用
    #[test]
    fn partial_eq() {
        assert_eq!(ElevateOutcome::Ok(0), ElevateOutcome::Ok(0));
        assert_ne!(ElevateOutcome::Ok(0), ElevateOutcome::Ok(1));
        assert_eq!(ElevateOutcome::UacDenied, ElevateOutcome::UacDenied);
    }

    /// 验证：Debug 输出稳定
    #[test]
    fn debug_impl_works() {
        let s = format!("{:?}", ElevateOutcome::Timeout);
        assert!(s.contains("Timeout"));
    }

    /// 验证：wide_string 以 null 结尾
    #[test]
    fn wide_string_ends_with_null() {
        let s = wide_string(OsStr::new("runas"));
        assert_eq!(s.last(), Some(&0));
        let s2 = wide_string(OsStr::new(""));
        assert_eq!(s2, vec![0]);
    }

    /// 验证：wide_string 对中文也正确（LPWSTR 必须是 UTF-16）
    #[test]
    fn wide_string_handles_chinese() {
        let s = wide_string(OsStr::new("安装"));
        // 0x5B89 0x88C5 0x0000
        assert!(s.contains(&0x5B89));
        assert!(s.contains(&0x88C5));
        assert_eq!(s.last(), Some(&0));
    }

    /// 验证：路径不存在时返回 ShellFailed
    #[test]
    #[cfg(windows)]
    fn spawn_with_nonexistent_exe_returns_shell_failed() {
        let result = spawn_elevated_and_wait(
            Path::new("D:/__definitely_not_exists__/missing.exe"),
            &["status"],
            Duration::from_secs(5),
        );
        // 不存在路径在 `exe.exists()` 检查时就 return ShellFailed(0)
        assert!(
            matches!(result, ElevateOutcome::ShellFailed(0)),
            "got: {:?}",
            result
        );
    }

    /// 验证：非 Windows 平台路径不存在时直接返回 Unsupported（cfg!(windows) 为 false）
    #[test]
    #[cfg(not(windows))]
    fn spawn_on_non_windows_returns_unsupported() {
        let result = spawn_elevated_and_wait(
            Path::new("/nonexistent"),
            &["status"],
            Duration::from_secs(1),
        );
        assert_eq!(result, ElevateOutcome::Unsupported);
    }

    /// 验证：OsString 转换不会因为空 args 数组 panic
    #[test]
    #[cfg(windows)]
    fn spawn_with_empty_args_on_nonexistent_exe() {
        let result = spawn_elevated_and_wait(
            Path::new("D:/__missing__/x.exe"),
            &[],
            Duration::from_secs(1),
        );
        assert!(matches!(result, ElevateOutcome::ShellFailed(0)));
    }

    /// 辅助：确保 wide_string 的反操作正确（用于 debug，不直接测）
    #[test]
    fn roundtrip_wide_string() {
        let s = "hello world";
        let w = wide_string(OsStr::new(s));
        // 去掉末尾 null
        let trimmed = &w[..w.len() - 1];
        let back = OsString::from_wide(trimmed);
        assert_eq!(back.to_string_lossy(), s);
    }
}
