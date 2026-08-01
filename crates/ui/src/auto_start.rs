//! GUI 启动时自动检测 + 启动后台服务（P3 增强 + UAC 提权）。
//!
//! ## 行为
//!
//! 1. 在同目录 / CWD / PATH 中找 `find-stutter-service.exe`
//! 2. 调 `status` 子命令查 SCM 状态（退出码：0=Running, 1=Stopped/Pending, 2=NotFound, 3=Error）
//! 3. 按状态走不同路径：
//!    - `0` (Running)        → `AlreadyRunning`
//!    - `2` (NotFound)       → 提权 `install-start`（UAC 弹窗一次完成 install + start）
//!    - `1` (Stopped/Pending)→ 提权 `start`
//!    - `3` (Error)          → `StartFailed`
//! 4. 提权操作调用 [`crate::elevate::spawn_elevated_and_wait`]
//!    - 失败模式：`UacDenied`（用户拒绝） / `ShellFailed` / `Timeout` / `Os`
//!
//! ## 失败模式（不会让 GUI 启动失败）
//!
//! - `ExeNotFound`：service crate 未构建 / 不在 PATH
//! - `NotRegistered`：用户拒绝 UAC，service 仍未注册
//! - `StartFailed(reason)`：其他 SCM 错误 / 提权异常
//!
//! 所有失败情况下 GUI 仍能启动，只是顶部状态条会显示「● 服务已停止」。
//!
//! ## 与 P3 base 的区别
//!
//! 旧版只在普通权限下调 `start`（多半因权限不足失败）。
//! 新版遇到 `NotFound`/`Stopped` 时主动通过 `runas` 提权，
//! 真正做到「双击 GUI → 一路点是 → 看到绿点」。

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::elevate::{self, ElevateOutcome};

/// service `status` 退出码协议
pub mod status_code {
    /// 服务在跑
    pub const RUNNING: i32 = 0;
    /// 已注册但未跑 / 启动中 / 暂停等「存在但不可用」
    pub const STOPPED: i32 = 1;
    /// 未注册（需要 install + 提权）
    pub const NOT_FOUND: i32 = 2;
    /// 其他 SCM 错误
    pub const ERROR: i32 = 3;
}

/// 自动启动结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoStartResult {
    /// 服务已经在跑（`status` 退出码 0）
    AlreadyRunning,
    /// 已通过提权 start 启动成功
    Started,
    /// 已通过提权 install-start 一次性完成注册 + 启动
    InstalledAndStarted,
    /// 服务仍未注册（UAC 被拒绝 或 install 失败）
    NotRegistered,
    /// 自动启动被跳过（环境变量 `FIND_STUTTER_SKIP_SERVICE` 或
    /// `config.toml [ui] auto_start_service = false`）
    Skipped,
    /// 找不到 `find-stutter-service.exe`（未构建？）
    ExeNotFound,
    /// 启动失败（其他原因：SCM 错误 / 提权异常 / 进程非 0 退出）
    StartFailed(String),
}

impl AutoStartResult {
    /// 人类可读的单行描述（用于日志/UI 提示）
    pub fn message(&self) -> String {
        match self {
            Self::AlreadyRunning => "服务已在运行".to_string(),
            Self::Started => "已自动启动后台服务".to_string(),
            Self::InstalledAndStarted => "已自动注册并启动后台服务".to_string(),
            Self::NotRegistered => {
                "服务未注册且 UAC 提权被拒绝（请手动以管理员身份运行 find-stutter-service install）"
                    .to_string()
            }
            Self::Skipped => "服务自动启动已关闭（跳过）".to_string(),
            Self::ExeNotFound => {
                "找不到 find-stutter-service.exe（未构建？请运行 cargo build --release -p find-stutter-service）"
                    .to_string()
            }
            Self::StartFailed(r) => format!("自动启动服务失败: {}（请手动运行 find-stutter-service start）", r),
        }
    }

    /// 状态是否健康
    pub fn is_ok(&self) -> bool {
        matches!(
            self,
            Self::AlreadyRunning | Self::Started | Self::InstalledAndStarted
        )
    }
}
/// 提权命令的最长等待时间（install-start 通常 < 3s，start < 1s，30s 留足裕度）
const ELEVATE_TIMEOUT: Duration = Duration::from_secs(30);

/// `start` 之后等服务真正进入 `Running` 的轮询窗口
const POST_START_POLL: Duration = Duration::from_millis(1500);

/// 是否应跳过「自动启动服务」流程。
///
/// 自动测试 / CI / 沙箱环境不希望每次启动 GUI 都弹 UAC，
/// 提供两种关闭方式（任一命中即跳过）：
/// 1. 环境变量 `FIND_STUTTER_SKIP_SERVICE`（任意非空值）
/// 2. `config.toml [ui] auto_start_service = false`
///
/// 注意：这里**只**检查环境变量；config 开关由调用方（lib.rs）读取。
pub fn auto_start_disabled() -> bool {
    std::env::var_os("FIND_STUTTER_SKIP_SERVICE").is_some()
}

/// 入口：检测并尝试自动启动后台服务。
///
/// `db_path`：stutter.db 的路径（用于校验 service 是否真的在跑——
///     SCM 标记 Running 但 db 里的 service_heartbeat 表没行，说明 service
///     是用旧 binary 跑的（不写心跳），需触发 stop + start 升级）。
///
/// 若 [`auto_start_disabled`] 返回 true（环境变量或配置关闭），
/// 直接返回 [`AutoStartResult::Skipped`]，不执行任何提权操作。
pub fn ensure_service_running(db_path: &Path) -> AutoStartResult {
    if auto_start_disabled() {
        log::info!("服务自动启动已关闭（环境变量/配置），跳过");
        return AutoStartResult::Skipped;
    }

    ensure_service_running_with_exe(find_service_exe(), db_path)
}

/// 核心状态机：给定 service exe（可能为 None）+ db 路径，决定走哪条路径。
///
/// 拆出来是为了可测试性：`exe = None` 直接返回 `ExeNotFound`，
/// 不依赖真实环境（开发机 / CI 上服务是否注册、是否在跑）。
/// [`ensure_service_running`] 只负责「找 exe + 跳过开关」，然后委托到这里。
pub fn ensure_service_running_with_exe(
    exe: Option<PathBuf>,
    db_path: &Path,
) -> AutoStartResult {
    let Some(exe) = exe else {
        return AutoStartResult::ExeNotFound;
    };

    // 1) status 退出码
    let initial_code = match run_service_cmd(&exe, &["status"]) {
        Ok(c) => c,
        Err(e) => return AutoStartResult::StartFailed(format!("status 调用失败: {}", e)),
    };

    match initial_code {
        status_code::RUNNING => {
            // 进一步检查 db 是否有 heartbeat 记录
            // 没有 → service 是用旧 binary 跑的，触发升级（stop + start）
            match read_heartbeat_status(db_path) {
                HeartbeatStatus::Present => AutoStartResult::AlreadyRunning,
                HeartbeatStatus::Missing => {
                    log::warn!(
                        "SCM 标记 Running 但 db {} 缺 service_heartbeat，疑似旧版本 service，主动 stop + start 升级",
                        db_path.display()
                    );
                    restart_via_elevation(&exe)
                }
                HeartbeatStatus::NoDb | HeartbeatStatus::DbError => {
                    // db 不存在 / 读不动：可能 GUI 启动前 db 还没建，
                    // 不强制重启（避免误杀正在写 db 的 service）
                    AutoStartResult::AlreadyRunning
                }
            }
        }
        status_code::NOT_FOUND => install_and_start_via_elevation(&exe),
        status_code::STOPPED => start_via_elevation(&exe),
        _ => AutoStartResult::StartFailed(format!(
            "service status 退出码 {} (非预期状态)",
            initial_code
        )),
    }
}

/// db 里的 heartbeat 状态（用于「SCM Running + db 无 heartbeat = 老 binary」判定）
#[derive(Debug)]
enum HeartbeatStatus {
    /// service_heartbeat 表有行（service 是新 binary 正常在跑）
    Present,
    /// 表存在但 0 行（旧 binary 在跑 / 刚启动还没 tick）
    Missing,
    /// db 文件不存在（GUI 第一次启动 / service 从未跑过）
    NoDb,
    /// 打开 / 查询失败
    DbError,
}

/// 升级路径：service 在跑但用的是旧 binary，主动 stop + start 让 SCM 重启用新 binary
fn restart_via_elevation(exe: &Path) -> AutoStartResult {
    // install 命令会检测到服务已存在 → change_config 更新 binary + （如果 Running）stop + start
    let outcome = elevate::spawn_elevated_and_wait(exe, &["install"], ELEVATE_TIMEOUT);
    log::info!("install (升级) 提权结果: {}", outcome.message());

    match outcome {
        ElevateOutcome::Ok(0) => {
            std::thread::sleep(POST_START_POLL);
            match run_service_cmd(exe, &["status"]) {
                Ok(0) => {
                    // 升级成功 + 已经在跑 → 算 InstalledAndStarted
                    // （虽然其实是 upgrade，但用户体验一样：service 跑起来了）
                    AutoStartResult::InstalledAndStarted
                }
                Ok(c) => AutoStartResult::StartFailed(format!(
                    "install 升级后 status 非 0 (code={})",
                    c
                )),
                Err(e) => AutoStartResult::StartFailed(format!("install 升级后 status 失败: {}", e)),
            }
        }
        ElevateOutcome::UacDenied => AutoStartResult::StartFailed(
            "UAC 提权被拒绝（无法升级 service binary）".into(),
        ),
        ElevateOutcome::Ok(code) => AutoStartResult::StartFailed(format!(
            "install 退出码非 0: {}",
            code
        )),
        ElevateOutcome::ShellFailed(c) => AutoStartResult::StartFailed(format!(
            "提权 ShellExecuteExW 失败 (code={})",
            c
        )),
        ElevateOutcome::Timeout => AutoStartResult::StartFailed(
            "install 30s 内未退出".into(),
        ),
        ElevateOutcome::Unsupported => AutoStartResult::StartFailed(
            "当前 OS 不支持 UAC 提权".into(),
        ),
        ElevateOutcome::Os(e) => AutoStartResult::StartFailed(format!("提权 OS 错误: {}", e)),
    }
}

/// 读 db 查 service_heartbeat 是否有行
fn read_heartbeat_status(db_path: &Path) -> HeartbeatStatus {
    use rusqlite::OpenFlags;

    if !db_path.exists() {
        return HeartbeatStatus::NoDb;
    }

    let conn = match rusqlite::Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(_e) => return HeartbeatStatus::DbError,
    };

    // 表不存在（极旧的 db schema）→ 等同 Missing
    let count: Result<i64, _> = conn.query_row(
        "SELECT COUNT(*) FROM service_heartbeat",
        [],
        |r| r.get(0),
    );

    match count {
        Ok(n) if n > 0 => HeartbeatStatus::Present,
        Ok(_) => HeartbeatStatus::Missing,
        Err(e) => {
            // "no such table" → Missing
            if e.to_string().contains("no such table") {
                HeartbeatStatus::Missing
            } else {
                HeartbeatStatus::DbError
            }
        }
    }
}

/// NotFound 路径：提权跑 `install-start`（一次完成 install + start）
fn install_and_start_via_elevation(exe: &Path) -> AutoStartResult {
    log::info!("service 未注册，请求 UAC 提权以执行 install-start");
    let outcome = elevate::spawn_elevated_and_wait(exe, &["install-start"], ELEVATE_TIMEOUT);
    log::info!("install-start 提权结果: {}", outcome.message());

    match outcome {
        ElevateOutcome::Ok(0) => {
            std::thread::sleep(POST_START_POLL);
            match run_service_cmd(exe, &["status"]) {
                Ok(0) => AutoStartResult::InstalledAndStarted,
                Ok(c) => AutoStartResult::StartFailed(format!(
                    "install-start 退出 0 但 status 仍非 0 (code={})",
                    c
                )),
                Err(e) => AutoStartResult::StartFailed(format!("install-start 后 status 失败: {}", e)),
            }
        }
        ElevateOutcome::UacDenied => AutoStartResult::NotRegistered,
        ElevateOutcome::Ok(code) => AutoStartResult::StartFailed(format!(
            "install-start 退出码非 0: {}",
            code
        )),
        ElevateOutcome::ShellFailed(c) => AutoStartResult::StartFailed(format!(
            "提权 ShellExecuteExW 失败 (code={})",
            c
        )),
        ElevateOutcome::Timeout => AutoStartResult::StartFailed(
            "install-start 30s 内未退出（SCM 可能卡住）".into(),
        ),
        ElevateOutcome::Unsupported => AutoStartResult::StartFailed(
            "当前 OS 不支持 UAC 提权（请手动管理员运行 install）".into(),
        ),
        ElevateOutcome::Os(e) => AutoStartResult::StartFailed(format!("提权 OS 错误: {}", e)),
    }
}

/// Stopped 路径：提权跑 `start`
fn start_via_elevation(exe: &Path) -> AutoStartResult {
    log::info!("service 已注册但未运行，请求 UAC 提权以执行 start");
    let outcome = elevate::spawn_elevated_and_wait(exe, &["start"], ELEVATE_TIMEOUT);
    log::info!("start 提权结果: {}", outcome.message());

    match outcome {
        ElevateOutcome::Ok(0) => {
            std::thread::sleep(POST_START_POLL);
            match run_service_cmd(exe, &["status"]) {
                Ok(0) => AutoStartResult::Started,
                Ok(c) => AutoStartResult::StartFailed(format!(
                    "start 退出 0 但 status 仍非 0 (code={})",
                    c
                )),
                Err(e) => AutoStartResult::StartFailed(format!("start 后 status 失败: {}", e)),
            }
        }
        ElevateOutcome::UacDenied => AutoStartResult::StartFailed(
            "UAC 提权被拒绝（服务已注册但未运行）".into(),
        ),
        ElevateOutcome::Ok(code) => AutoStartResult::StartFailed(format!(
            "start 退出码非 0: {}",
            code
        )),
        ElevateOutcome::ShellFailed(c) => AutoStartResult::StartFailed(format!(
            "提权 ShellExecuteExW 失败 (code={})",
            c
        )),
        ElevateOutcome::Timeout => AutoStartResult::StartFailed("start 30s 内未退出".into()),
        ElevateOutcome::Unsupported => AutoStartResult::StartFailed(
            "当前 OS 不支持 UAC 提权".into(),
        ),
        ElevateOutcome::Os(e) => AutoStartResult::StartFailed(format!("提权 OS 错误: {}", e)),
    }
}

/// 找 `find-stutter-service.exe` 的位置。
///
/// 查找顺序：
/// 1. GUI exe 同目录
/// 2. 当前工作目录
/// 3. PATH（用 `where` / `which`）
fn find_service_exe() -> Option<PathBuf> {
    let candidates = collect_exe_candidates();
    candidates.into_iter().find(|p| p.exists())
}

fn collect_exe_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();

    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            out.push(dir.join("find-stutter-service.exe"));
            out.push(dir.join("find-stutter-service"));
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd.join("find-stutter-service.exe"));
        out.push(cwd.join("find-stutter-service"));
    }

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
fn run_service_cmd(exe: &Path, args: &[&str]) -> std::io::Result<i32> {
    let output = std::process::Command::new(exe)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()?;
    if !output.status.success() && !output.stdout.is_empty() {
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

    /// 验证：所有 AutoStartResult 变体的文案稳定
    #[test]
    fn message_contains_keywords() {
        assert!(AutoStartResult::AlreadyRunning.message().contains("运行"));
        assert!(AutoStartResult::Started.message().contains("自动启动"));
        assert!(AutoStartResult::InstalledAndStarted.message().contains("注册"));
        assert!(AutoStartResult::NotRegistered.message().contains("install"));
        assert!(AutoStartResult::ExeNotFound.message().contains("find-stutter-service"));
        assert!(AutoStartResult::StartFailed("x".into()).message().contains("失败"));
    }

    /// 验证：is_ok 逻辑
    #[test]
    fn is_ok_predicate() {
        assert!(AutoStartResult::AlreadyRunning.is_ok());
        assert!(AutoStartResult::Started.is_ok());
        assert!(AutoStartResult::InstalledAndStarted.is_ok());
        assert!(!AutoStartResult::NotRegistered.is_ok());
        assert!(!AutoStartResult::ExeNotFound.is_ok());
        assert!(!AutoStartResult::StartFailed("x".into()).is_ok());
    }

    /// 验证：collect_exe_candidates 至少给出 1 个候选
    #[test]
    fn collect_exe_candidates_returns_non_empty() {
        let c = collect_exe_candidates();
        assert!(!c.is_empty(), "至少应包含 GUI exe 同目录下的候选");
    }

    /// 验证：Debug 输出
    #[test]
    fn debug_impl_works() {
        let r = AutoStartResult::InstalledAndStarted;
        let s = format!("{:?}", r);
        assert!(s.contains("InstalledAndStarted"));
    }

    /// 验证：PartialEq
    #[test]
    fn partial_eq() {
        assert_eq!(AutoStartResult::ExeNotFound, AutoStartResult::ExeNotFound);
        assert_ne!(
            AutoStartResult::ExeNotFound,
            AutoStartResult::AlreadyRunning
        );
        assert_eq!(
            AutoStartResult::InstalledAndStarted,
            AutoStartResult::InstalledAndStarted
        );
    }

    /// 验证：status_code 常量值稳定（service main.rs + GUI 端协议）
    #[test]
    fn status_code_protocol_is_stable() {
        assert_eq!(status_code::RUNNING, 0);
        assert_eq!(status_code::STOPPED, 1);
        assert_eq!(status_code::NOT_FOUND, 2);
        assert_eq!(status_code::ERROR, 3);
    }

    /// 串行锁：env 变量是进程级的，并发测试会互相干扰，
    /// 所有读/写 env 的测试共享此锁串行执行。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 验证：找不到 service exe 时返回 ExeNotFound。
    /// 用可注入版本传 None，不依赖真实环境（开发机上服务可能已注册/在跑）。
    #[test]
    fn ensure_service_when_exe_missing() {
        let r = ensure_service_running_with_exe(None, Path::new("D:/__missing__/stutter.db"));
        assert_eq!(r, AutoStartResult::ExeNotFound);
    }

    /// 验证：设置了 FIND_STUTTER_SKIP_SERVICE 时直接返回 Skipped，
    /// 不执行任何 status / 提权操作（自动测试环境不弹 UAC）。
    #[test]
    fn ensure_service_skipped_when_env_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("FIND_STUTTER_SKIP_SERVICE", "1");
        let r = ensure_service_running(Path::new("D:/__missing__/stutter.db"));
        assert_eq!(r, AutoStartResult::Skipped);
        std::env::remove_var("FIND_STUTTER_SKIP_SERVICE");
    }

    /// 验证：auto_start_disabled 读环境变量
    #[test]
    fn auto_start_disabled_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("FIND_STUTTER_SKIP_SERVICE");
        assert!(!auto_start_disabled());
        std::env::set_var("FIND_STUTTER_SKIP_SERVICE", "1");
        assert!(auto_start_disabled());
        std::env::remove_var("FIND_STUTTER_SKIP_SERVICE");
    }

    /// 验证：Skipped 变体文案与 is_ok 语义
    #[test]
    fn skipped_variant_contract() {
        assert!(!AutoStartResult::Skipped.is_ok());
        assert!(AutoStartResult::Skipped.message().contains("跳过"));
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

    // ========== read_heartbeat_status 测试 ==========

    /// 验证：db 不存在 → NoDb
    #[test]
    fn heartbeat_status_no_db() {
        let s = read_heartbeat_status(Path::new("D:/__missing_for_test__/stutter.db"));
        assert!(matches!(s, HeartbeatStatus::NoDb));
    }

    /// 验证：临时 db 空表 → Missing
    #[test]
    fn heartbeat_status_empty_db() {
        // 在 temp dir 创建一个空 sqlite db
        let tmp = std::env::temp_dir().join("find_stutter_test_empty.db");
        let _ = std::fs::remove_file(&tmp);
        rusqlite::Connection::open(&tmp).unwrap().close().unwrap();
        let s = read_heartbeat_status(&tmp);
        assert!(
            matches!(s, HeartbeatStatus::Missing),
            "空 db 应该返回 Missing，得到: {:?}",
            s
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// 验证：临时 db 有 service_heartbeat 表 + 1 行 → Present
    #[test]
    fn heartbeat_status_present() {
        let tmp = std::env::temp_dir().join("find_stutter_test_present.db");
        let _ = std::fs::remove_file(&tmp);
        {
            let conn = rusqlite::Connection::open(&tmp).unwrap();
            conn.execute_batch(
                "CREATE TABLE service_heartbeat (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    timestamp TEXT NOT NULL,
                    pid INTEGER NOT NULL
                );
                INSERT INTO service_heartbeat VALUES (1, '2026-07-26T00:00:00+00:00', 1234);",
            )
            .unwrap();
        }
        let s = read_heartbeat_status(&tmp);
        assert!(
            matches!(s, HeartbeatStatus::Present),
            "有行的 db 应该返回 Present，得到: {:?}",
            s
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// 验证：临时 db 有 service_heartbeat 表但 0 行 → Missing
    #[test]
    fn heartbeat_status_table_empty() {
        let tmp = std::env::temp_dir().join("find_stutter_test_table_empty.db");
        let _ = std::fs::remove_file(&tmp);
        {
            let conn = rusqlite::Connection::open(&tmp).unwrap();
            conn.execute_batch(
                "CREATE TABLE service_heartbeat (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    timestamp TEXT NOT NULL,
                    pid INTEGER NOT NULL
                );",
            )
            .unwrap();
        }
        let s = read_heartbeat_status(&tmp);
        assert!(
            matches!(s, HeartbeatStatus::Missing),
            "表存在但 0 行的 db 应该返回 Missing，得到: {:?}",
            s
        );
        let _ = std::fs::remove_file(&tmp);
    }
}
