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

/// 注册服务：把当前 exe 注册为名为 `SERVICE_NAME` 的 Windows 服务。
///
/// 启动类型：自动（开机自启）
/// 失败码：normal
pub fn install() -> ScmResult<()> {
    #[cfg(windows)]
    {
        use windows_service::service::{
            ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType,
        };
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

        let exe_path = std::env::current_exe()?;

        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CREATE_SERVICE,
        )?;

        let info = ServiceInfo {
            name: SERVICE_NAME.into(),
            display_name: SERVICE_DISPLAY_NAME.into(),
            service_type: windows_service::service::ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: exe_path,
            launch_arguments: vec![],
            dependencies: vec![],
            account_name: None,
            account_password: None,
        };

        let _service = manager.create_service(&info, ServiceAccess::CHANGE_CONFIG)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(ScmError::NotWindows)
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
