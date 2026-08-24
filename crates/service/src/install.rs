//! Windows SCM（Service Control Manager）操作。
//!
//! 实现：使用 windows-service crate 调 SCM API。
//!
//! 提供：
//! - [`install`]   注册服务（需管理员权限）
//! - [`uninstall`] 卸载服务
//! - [`start`]     启动已注册的服务
//! - [`stop`]      停止已运行的服务
//! - [`status`]    查询服务状态

use crate::service::{SERVICE_DISPLAY_NAME, SERVICE_NAME};

/// SCM 操作统一错误类型
#[derive(Debug, thiserror::Error)]
pub enum ScmError {
    #[error("SCM 操作失败: {0}")]
    Win(#[from] windows_service::Error),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("服务不存在")]
    NotFound,

    #[error("非 Windows 平台")]
    NotWindows,
}

pub type ScmResult<T> = Result<T, ScmError>;

/// 配置 SCM 失败恢复：服务进程异常退出（崩溃 / 未处理 panic）时自动重启。
///
/// 三段退避：5s → 10s → 30s，24h 无失败则重置计数。用 `sc.exe` 而非原生
/// ChangeServiceConfig2：windows-service crate 未暴露失败动作 API，而 install
/// 本就在已提权上下文中执行，sc.exe 等价可靠。失败仅告警不阻塞安装。
fn ensure_failure_recovery() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("sc")
        .args([
            "failure",
            SERVICE_NAME,
            "reset=",
            "86400",
            "actions=",
            "restart/5000/restart/10000/restart/30000",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            log::info!("service 失败恢复策略已配置（崩溃后 5s/10s/30s 自动重启）");
        }
        Ok(o) => {
            log::warn!(
                "sc failure 配置未成功（忽略，不影响安装）: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
        Err(e) => {
            log::warn!("sc failure 调用失败（忽略，不影响安装）: {}", e);
        }
    }
}

/// 注册服务：把当前 exe 注册为名为 `SERVICE_NAME` 的 Windows 服务。
///
/// 启动类型：自动（开机自启）
/// 失败码：normal
///
/// ## 升级行为（关键：避免无谓 stop+start）
///
/// 1. **首次注册**（`create_service` 成功）→ 立即 `start` 启动
/// 2. **服务已注册 + binary 一致** → 啥都不做（service 已经在用最新 binary）
///    - 如果 service 是 Stopped，启动它
/// 3. **服务已注册 + binary 不一致**（升级路径）
///    - `change_config` 更新 binary path
///    - 如果 service 在 Running → `stop + start` 让 SCM 用新 binary 重启
///
/// 关键：`change_config` 单独**不会**让正在跑的进程重读代码，必须 stop+start。
/// 但我们只在 binary 真的不一致时升级，避免每次 GUI 启动都中断 service。
pub fn install() -> ScmResult<()> {
    #[cfg(windows)]
    {
        use std::time::Duration;
        use windows_service::service::{
            ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState,
        };
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

        let exe_path = std::env::current_exe()?;

        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CREATE_SERVICE | ServiceManagerAccess::CONNECT,
        )?;

        let info = ServiceInfo {
            name: SERVICE_NAME.into(),
            display_name: SERVICE_DISPLAY_NAME.into(),
            service_type: windows_service::service::ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: exe_path.clone(),
            launch_arguments: vec![],
            dependencies: vec![],
            account_name: None,
            account_password: None,
        };

        match manager.create_service(&info, ServiceAccess::CHANGE_CONFIG) {
            Ok(_s) => {
                log::info!(
                    "service 已首次注册: {} (path={})",
                    SERVICE_NAME,
                    exe_path.display()
                );
                // 崩溃自恢复（幂等，可随时重设）；失败不影响安装主流程
                ensure_failure_recovery();
                // 首次注册后立即 start
                let start_handle =
                    manager.open_service(SERVICE_NAME, ServiceAccess::START)?;
                start_handle.start(&[] as &[&str])?;
                log::info!("service 已启动");
                Ok(())
            }
            Err(e) => {
                if !is_service_exists_error(&e) {
                    return Err(e.into());
                }

                // 服务已存在：query 当前 binary path 决定是否升级
                let existing = manager.open_service(
                    SERVICE_NAME,
                    ServiceAccess::CHANGE_CONFIG
                        | ServiceAccess::QUERY_CONFIG
                        | ServiceAccess::QUERY_STATUS
                        | ServiceAccess::START
                        | ServiceAccess::STOP,
                )?;

                let current_cfg = existing.query_config()?;
                let current_path = current_cfg.executable_path.clone();
                let need_upgrade = !paths_equal(&current_path, &exe_path);

                if need_upgrade {
                    log::info!(
                        "service binary 需升级: {} → {}",
                        current_path.display(),
                        exe_path.display()
                    );
                    existing.change_config(&info)?;

                    let status = existing.query_status()?;
                    if matches!(status.current_state, ServiceState::Running) {
                        log::info!("service 在跑，重启以应用新 binary");
                        let _ = existing.stop();
                        // 等待 stop 完成（最多 ~5s）
                        for _ in 0..10 {
                            std::thread::sleep(Duration::from_millis(500));
                            let s = existing.query_status()?;
                            if matches!(s.current_state, ServiceState::Stopped) {
                                break;
                            }
                        }
                        existing.start(&[] as &[&str])?;
                        log::info!("service 已用新 binary 重启");
                    } else {
                        existing.start(&[] as &[&str])?;
                    }
                } else {
                    log::info!(
                        "service binary 已是最新: {} — 跳过升级",
                        current_path.display()
                    );
                    // binary 一致：确保在跑
                    let status = existing.query_status()?;
                    if !matches!(status.current_state, ServiceState::Running) {
                        log::info!("service 没在跑，start");
                        existing.start(&[] as &[&str])?;
                    }
                }
                Ok(())
            }
        }
    }
    #[cfg(not(windows))]
    {
        Err(ScmError::NotWindows)
    }
}

/// 比较两条路径是否指向同一文件（Windows 上 case-insensitive、UNC 标准化）
#[cfg(windows)]
fn paths_equal(a: &std::path::Path, b: &std::path::Path) -> bool {
    let a_norm = a.to_string_lossy().to_lowercase().replace('/', "\\");
    let b_norm = b.to_string_lossy().to_lowercase().replace('/', "\\");
    a_norm == b_norm
}

#[cfg(windows)]
fn is_service_exists_error(e: &windows_service::Error) -> bool {
    use windows_service::Error;
    if let Error::Winapi(io_err) = e {
        io_err.raw_os_error() == Some(1073) // ERROR_SERVICE_EXISTS
    } else {
        false
    }
}

/// 卸载服务。
pub fn uninstall() -> ScmResult<()> {
    #[cfg(windows)]
    {
        use windows_service::service::ServiceAccess;
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

        let manager =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager
            .open_service(SERVICE_NAME, ServiceAccess::DELETE)
            .map_err(|_| ScmError::NotFound)?;
        service.delete()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(ScmError::NotWindows)
    }
}

/// 启动已注册的服务。
pub fn start() -> ScmResult<()> {
    #[cfg(windows)]
    {
        use windows_service::service::ServiceAccess;
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

        let manager =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager
            .open_service(SERVICE_NAME, ServiceAccess::START)
            .map_err(|_| ScmError::NotFound)?;
        service.start(&[] as &[&str])?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(ScmError::NotWindows)
    }
}

/// 停止已运行的服务。
pub fn stop() -> ScmResult<()> {
    #[cfg(windows)]
    {
        use windows_service::service::ServiceAccess;
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

        let manager =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager
            .open_service(SERVICE_NAME, ServiceAccess::STOP)
            .map_err(|_| ScmError::NotFound)?;
        service.stop()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(ScmError::NotWindows)
    }
}

/// 服务状态（简化版：人类可读）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceStatusInfo {
    Stopped,
    StartPending,
    StopPending,
    Running,
    ContinuePending,
    PausePending,
    Paused,
    NotFound,
}

/// 查询服务状态。
pub fn status() -> ScmResult<ServiceStatusInfo> {
    #[cfg(windows)]
    {
        use windows_service::service::{ServiceAccess, ServiceState};
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

        let manager =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
            Ok(s) => s,
            Err(_) => return Ok(ServiceStatusInfo::NotFound),
        };
        let status = service.query_status()?;
        Ok(match status.current_state {
            ServiceState::Stopped => ServiceStatusInfo::Stopped,
            ServiceState::StartPending => ServiceStatusInfo::StartPending,
            ServiceState::StopPending => ServiceStatusInfo::StopPending,
            ServiceState::Running => ServiceStatusInfo::Running,
            ServiceState::ContinuePending => ServiceStatusInfo::ContinuePending,
            ServiceState::PausePending => ServiceStatusInfo::PausePending,
            ServiceState::Paused => ServiceStatusInfo::Paused,
        })
    }
    #[cfg(not(windows))]
    {
        Err(ScmError::NotWindows)
    }
}

/// 把状态格式化成可打印字符串
pub fn status_to_string(s: &ServiceStatusInfo) -> String {
    match s {
        ServiceStatusInfo::Stopped => "Stopped (未运行)".to_string(),
        ServiceStatusInfo::StartPending => "Start Pending (正在启动)".to_string(),
        ServiceStatusInfo::StopPending => "Stop Pending (正在停止)".to_string(),
        ServiceStatusInfo::Running => "Running (运行中)".to_string(),
        ServiceStatusInfo::ContinuePending => "Continue Pending".to_string(),
        ServiceStatusInfo::PausePending => "Pause Pending".to_string(),
        ServiceStatusInfo::Paused => "Paused".to_string(),
        ServiceStatusInfo::NotFound => "Not Found (服务未注册)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_name_constants() {
        // 名称必须稳定（README/文档/UI 都引用）
        assert_eq!(SERVICE_NAME, "FindStutter");
        assert_eq!(SERVICE_DISPLAY_NAME, "Find Stutter Monitor");
    }

    #[test]
    fn status_to_string_known_states() {
        assert_eq!(
            status_to_string(&ServiceStatusInfo::Running),
            "Running (运行中)"
        );
        assert_eq!(
            status_to_string(&ServiceStatusInfo::Stopped),
            "Stopped (未运行)"
        );
        assert_eq!(
            status_to_string(&ServiceStatusInfo::NotFound),
            "Not Found (服务未注册)"
        );
    }

    /// 验证 ScmError::NotFound 的错误消息
    #[test]
    fn scm_error_not_found_message() {
        let e = ScmError::NotFound;
        assert_eq!(e.to_string(), "服务不存在");
    }
}
