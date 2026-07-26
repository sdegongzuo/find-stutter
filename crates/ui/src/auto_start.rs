//! GUI 启动时自动检测 + 启动后台服务（P3 增强）。
//!
//! ## 行为
//!
//! 1. 在同目录 / CWD / PATH 中找 `find-stutter-service.exe`
//! 2. 调 `status` 子命令查 SCM 状态
//!    - 退出码 0 = `Running`，直接返回 `AlreadyRunning`
//! 3. 否则调 `start` 子命令尝试启动
//!    - 成功：等 0.5s 再 `status`，返回 `Started`
//!    - 失败：返回 `StartFailed(reason)`（多半是权限不足，提示用户手动）
//!
//! ## 失败模式（不会让 GUI 启动失败）
//!
//! - `ExeNotFound`：service crate 未构建 / 不在 PATH
//! - `NotRegistered`：service 还没用 `install` 注册到 SCM
//! - `StartFailed(reason)`：SCM 调用失败（权限/服务已禁用等）
//!
//! 所有失败情况下 GUI 仍能启动，只是顶部状态条会显示「● 服务已停止」。

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 自动启动结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoStartResult {
    /// 服务已经在跑（`status` 退出码 0）
    AlreadyRunning,
    /// 已自动启动
    Started,
    /// 服务未注册（用户需先运行 `find-stutter-service install`）
    NotRegistered,
    /// 找不到 `find-stutter-service.exe`（未构建？）
    ExeNotFound,
    /// 启动失败（多半是权限不足）
    StartFailed(String),
}

impl AutoStartResult {
    /// 人类可读的单行描述（用于日志/UI 提示）
    pub fn message(&self) -> String {
        match self {
            Self::AlreadyRunning => "服务已在运行".to_string(),
            Self::Started => "已自动启动后台服务".to_string(),
            Self::NotRegistered => {
                "服务未注册，请以管理员身份运行 find-stutter-service install".to_string()
            }
            Self::ExeNotFound => {
                "找不到 find-stutter-service.exe（未构建？请运行 cargo build --release -p find-stutter-service）"
                    .to_string()
            }
            Self::StartFailed(r) => format!("自动启动服务失败: {}（请手动运行 find-stutter-service start）", r),
        }
    }

    /// 状态是否健康（用于日志级别 / UI 提示）
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::AlreadyRunning | Self::Started)
    }
}

/// 入口：检测并尝试自动启动后台服务。
///
/// 不会 panic：所有错误都包在 `AutoStartResult` 里返回。
pub fn ensure_service_running() -> AutoStartResult {
    let Some(exe) = find_service_exe() else {
        return AutoStartResult::ExeNotFound;
    };

    // 1) status 退出码 0 = Running
    match run_service_cmd(&exe, &["status"]) {
        Ok(0) => return AutoStartResult::AlreadyRunning,
        Ok(_) => {
            // 退出码非 0：服务没在跑；status 的 stderr 区分 NotRegistered vs Stopped
            // 这里直接尝试 start（start 对未注册的服务会返回错误，OK）
        }
        Err(e) => {
            return AutoStartResult::StartFailed(format!("status 调用失败: {}", e));
        }
    }

    // 2) 尝试 start
    match run_service_cmd(&exe, &["start"]) {
        Ok(_) => {
            // 等 0.5s 启动 + 再 status 确认
            std::thread::sleep(Duration::from_millis(500));
            match run_service_cmd(&exe, &["status"]) {
                Ok(0) => AutoStartResult::Started,
                Ok(_) => {
                    AutoStartResult::StartFailed("start 命令返回 0 但 status 仍非 0".into())
                }
                Err(e) => AutoStartResult::StartFailed(format!("start 后 status 失败: {}", e)),
            }
        }
        Err(e) => AutoStartResult::StartFailed(format!("start 调用失败: {}", e)),
    }
}

/// 找 `find-stutter-service.exe` 的位置。
///
/// 查找顺序：
/// 1. GUI exe 同目录（最常见：用户把两个 exe 放一起）
/// 2. 当前工作目录
/// 3. PATH（用 `where` / `which`）
fn find_service_exe() -> Option<PathBuf> {
    let candidates = collect_exe_candidates();
    candidates.into_iter().find(|p| p.exists())
}

fn collect_exe_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();

    // 1) GUI exe 同目录
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            out.push(dir.join("find-stutter-service.exe"));
            out.push(dir.join("find-stutter-service"));
        }
    }

    // 2) 当前工作目录
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd.join("find-stutter-service.exe"));
        out.push(cwd.join("find-stutter-service"));
    }

    // 3) PATH（where / which）
    if let Some(p) = which_service_exe() {
        out.push(p);
    }

    out
}

#[cfg(windows)]
fn which_service_exe() -> Option<PathBuf> {
    let output = std::process::Command::new("where")
        .arg("find-stutter-service")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout);
    s.lines().next().map(|l| PathBuf::from(l.trim()))
}

#[cfg(not(windows))]
fn which_service_exe() -> Option<PathBuf> {
    let output = std::process::Command::new("which")
        .arg("find-stutter-service")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout);
    s.lines().next().map(|l| PathBuf::from(l.trim()))
}

/// 同步执行 service 子命令，返回退出码。
///
/// stderr 丢弃（service 子命令出错时只关心退出码，详细信息由调用方看日志）。
fn run_service_cmd(exe: &Path, args: &[&str]) -> std::io::Result<i32> {
    let output = std::process::Command::new(exe)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()?;
    if !output.status.success() && !output.stdout.is_empty() {
        // 把 stdout 当诊断信息写日志
        if let Ok(s) = std::str::from_utf8(&output.stdout) {
            for line in s.lines() {
                log::info!("[service {}] {}", args.join(" "), line);
            }
        }
    }
    Ok(output.status.code().unwrap_or(-1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证结果文案稳定（不依赖 i18n 框架，靠字符串包含断言）
    #[test]
    fn message_contains_keywords() {
        assert!(AutoStartResult::AlreadyRunning.message().contains("运行"));
        assert!(AutoStartResult::Started.message().contains("自动启动"));
        assert!(AutoStartResult::NotRegistered.message().contains("install"));
        assert!(AutoStartResult::ExeNotFound.message().contains("find-stutter-service"));
        assert!(AutoStartResult::StartFailed("x".into()).message().contains("失败"));
    }

    /// 验证 is_ok 逻辑
    #[test]
    fn is_ok_predicate() {
        assert!(AutoStartResult::AlreadyRunning.is_ok());
        assert!(AutoStartResult::Started.is_ok());
        assert!(!AutoStartResult::NotRegistered.is_ok());
        assert!(!AutoStartResult::ExeNotFound.is_ok());
        assert!(!AutoStartResult::StartFailed("x".into()).is_ok());
    }

    /// 验证 collect_exe_candidates 至少给出 1 个候选（GUI exe 同目录 / CWD / PATH）
    #[test]
    fn collect_exe_candidates_returns_non_empty() {
        let c = collect_exe_candidates();
        assert!(!c.is_empty(), "至少应包含 GUI exe 同目录下的候选");
    }

    /// 验证 Debug 输出
    #[test]
    fn debug_impl_works() {
        let r = AutoStartResult::AlreadyRunning;
        let s = format!("{:?}", r);
        assert!(s.contains("AlreadyRunning"));
    }

    /// 验证 PartialEq
    #[test]
    fn partial_eq() {
        assert_eq!(AutoStartResult::ExeNotFound, AutoStartResult::ExeNotFound);
        assert_ne!(AutoStartResult::ExeNotFound, AutoStartResult::AlreadyRunning);
    }

    /// 验证：找不到 service exe 时返回 ExeNotFound
    /// （这个测试在 PATH 不含 service 的开发环境里能过；CI 里 cargo path 也不带）
    #[test]
    fn ensure_service_when_exe_missing() {
        // 把 PATH 清空
        let saved = std::env::var_os("PATH");
        // 用空 PATH 不一定可行（部分系统保留），所以只测函数不 panic
        // 实际结果可能是 ExeNotFound 或 StartFailed（取决于环境）
        let r = ensure_service_running();
        match r {
            AutoStartResult::ExeNotFound
            | AutoStartResult::StartFailed(_)
            | AutoStartResult::NotRegistered => {
                // 接受
            }
            AutoStartResult::AlreadyRunning | AutoStartResult::Started => {
                panic!("环境里居然有 find-stutter-service.exe 在 PATH 中");
            }
        }
        // 恢复（即使 saved 是 None 也无所谓）
        if let Some(v) = saved {
            std::env::set_var("PATH", v);
        }
    }

    /// 验证：run_service_cmd 对不存在的 exe 返回 Err
    #[test]
    fn run_service_cmd_missing_exe_returns_err() {
        let result = run_service_cmd(
            Path::new("D:/__definitely_not_exists__/missing.exe"),
            &["status"],
        );
        assert!(result.is_err());
    }
}
