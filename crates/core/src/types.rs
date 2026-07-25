use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 一次系统指标采样
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub timestamp: DateTime<Utc>,

    // CPU
    pub cpu_usage: f32,
    pub cpu_per_core: Vec<f32>,
    pub cpu_freq_mhz: Option<f32>,

    // 内存
    pub mem_usage_percent: f32,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    pub mem_available_mb: u64,
    pub swap_usage_percent: f32,

    // 磁盘 I/O (bytes/sec)
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,

    // 网络 I/O (bytes/sec)
    pub net_sent_bps: u64,
    pub net_recv_bps: u64,
    pub net_sent_total: u64,
    pub net_recv_total: u64,

    // GPU
    pub gpu_usage: Option<f32>,

    // 温度
    pub cpu_temp: Option<f32>,
    pub gpu_temp: Option<f32>,

    // 进程
    pub process_count: usize,
    pub thread_count: usize,
}

impl Default for Sample {
    fn default() -> Self {
        Self {
            timestamp: Utc::now(),
            cpu_usage: 0.0,
            cpu_per_core: Vec::new(),
            cpu_freq_mhz: None,
            mem_usage_percent: 0.0,
            mem_used_mb: 0,
            mem_total_mb: 0,
            mem_available_mb: 0,
            swap_usage_percent: 0.0,
            disk_read_bps: 0,
            disk_write_bps: 0,
            net_sent_bps: 0,
            net_recv_bps: 0,
            net_sent_total: 0,
            net_recv_total: 0,
            gpu_usage: None,
            cpu_temp: None,
            gpu_temp: None,
            process_count: 0,
            thread_count: 0,
        }
    }
}

/// 卡顿严重程度
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Minor,
    Major,
    Critical,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Minor => write!(f, "minor"),
            Severity::Major => write!(f, "major"),
            Severity::Critical => write!(f, "critical"),
        }
    }
}

/// 卡顿事件
#[derive(Debug, Clone)]
pub struct StutterEvent {
    pub timestamp: DateTime<Utc>,
    pub duration_ms: u64,
    pub severity: Severity,
    pub causes: Vec<String>,
    pub snapshot: Sample,
}

/// 检测器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionConfig {
    pub cpu_threshold: f32,
    pub mem_threshold_percent: f32,
    pub mem_threshold_mb: u64,
    pub swap_threshold: f32,
    pub disk_rate_spike_ratio: f32,
    pub spike_ratio: f32,
    pub sustained_seconds: u32,
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self {
            cpu_threshold: 90.0,
            mem_threshold_percent: 90.0,
            mem_threshold_mb: 500,
            swap_threshold: 50.0,
            disk_rate_spike_ratio: 5.0,
            spike_ratio: 2.0,
            sustained_seconds: 3,
        }
    }
}

/// 采样配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamplingConfig {
    pub interval_ms: u64,
    pub slow_interval_factor: u32,
}

impl Default for SamplingConfig {
    fn default() -> Self {
        Self {
            interval_ms: 1000,
            slow_interval_factor: 5,
        }
    }
}

/// 存储配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub db_path: String,
    pub retention_days: u32,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            db_path: "stutter.db".to_string(),
            retention_days: 30,
        }
    }
}

/// UI 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub skin: String,
    pub always_on_top: bool,
    pub show_upload: bool,
    pub show_download: bool,
    pub show_cpu: bool,
    pub show_memory: bool,
    pub show_gpu: bool,
    pub show_disk: bool,
    pub show_cpu_freq: bool,
    pub show_temperature: bool,
    pub mouse_transparent: bool,
    pub click_through: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            skin: "default".to_string(),
            always_on_top: true,
            show_upload: true,
            show_download: true,
            show_cpu: true,
            show_memory: true,
            show_gpu: true,
            show_disk: true,
            show_cpu_freq: false,
            show_temperature: false,
            mouse_transparent: false,
            click_through: false,
        }
    }
}

/// 通知配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationConfig {
    pub stutter_alert: bool,
    pub min_severity: String,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            stutter_alert: true,
            min_severity: "major".to_string(),
        }
    }
}

/// 日志配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

/// 全局配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub sampling: SamplingConfig,
    pub detection: DetectionConfig,
    pub ui: UiConfig,
    pub storage: StorageConfig,
    pub notifications: NotificationConfig,
    pub logging: LoggingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sampling: SamplingConfig::default(),
            detection: DetectionConfig::default(),
            ui: UiConfig::default(),
            storage: StorageConfig::default(),
            notifications: NotificationConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    pub fn save(&self, path: &str) -> anyhow::Result<()> {
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_default_values() {
        let s = Sample::default();
        assert_eq!(s.cpu_usage, 0.0);
        assert!(s.cpu_per_core.is_empty());
        assert!(s.cpu_freq_mhz.is_none());
        assert_eq!(s.mem_usage_percent, 0.0);
        assert_eq!(s.mem_used_mb, 0);
        assert_eq!(s.mem_total_mb, 0);
        assert_eq!(s.mem_available_mb, 0);
        assert_eq!(s.swap_usage_percent, 0.0);
        assert_eq!(s.disk_read_bps, 0);
        assert_eq!(s.disk_write_bps, 0);
        assert_eq!(s.net_sent_bps, 0);
        assert_eq!(s.net_recv_bps, 0);
        assert_eq!(s.net_sent_total, 0);
        assert_eq!(s.net_recv_total, 0);
        assert!(s.gpu_usage.is_none());
        assert!(s.cpu_temp.is_none());
        assert!(s.gpu_temp.is_none());
        assert_eq!(s.process_count, 0);
        assert_eq!(s.thread_count, 0);
    }

    #[test]
    fn severity_display_minor() {
        assert_eq!(Severity::Minor.to_string(), "minor");
    }

    #[test]
    fn severity_display_major() {
        assert_eq!(Severity::Major.to_string(), "major");
    }

    #[test]
    fn severity_display_critical() {
        assert_eq!(Severity::Critical.to_string(), "critical");
    }

    #[test]
    fn detection_config_defaults() {
        let c = DetectionConfig::default();
        assert_eq!(c.cpu_threshold, 90.0);
        assert_eq!(c.mem_threshold_percent, 90.0);
        assert_eq!(c.mem_threshold_mb, 500);
        assert_eq!(c.swap_threshold, 50.0);
        assert_eq!(c.disk_rate_spike_ratio, 5.0);
        assert_eq!(c.spike_ratio, 2.0);
        assert_eq!(c.sustained_seconds, 3);
    }

    #[test]
    fn sampling_config_defaults() {
        let c = SamplingConfig::default();
        assert_eq!(c.interval_ms, 1000);
        assert_eq!(c.slow_interval_factor, 5);
    }

    #[test]
    fn storage_config_defaults() {
        let c = StorageConfig::default();
        assert_eq!(c.db_path, "stutter.db");
        assert_eq!(c.retention_days, 30);
    }

    #[test]
    fn config_defaults() {
        let c = Config::default();
        assert_eq!(c.sampling.interval_ms, 1000);
        assert_eq!(c.detection.cpu_threshold, 90.0);
        assert_eq!(c.ui.skin, "default");
        assert!(c.ui.always_on_top);
        assert_eq!(c.storage.retention_days, 30);
        assert!(c.notifications.stutter_alert);
        assert_eq!(c.notifications.min_severity, "major");
        assert_eq!(c.logging.level, "info");
    }

    fn temp_path(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "find_stutter_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{}.toml", name))
            .to_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn config_save_and_load_roundtrip() {
        let path = temp_path("config_roundtrip");
        let config = Config::default();
        config.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.sampling.interval_ms, config.sampling.interval_ms);
        assert_eq!(
            loaded.detection.cpu_threshold,
            config.detection.cpu_threshold
        );
        assert_eq!(loaded.storage.retention_days, config.storage.retention_days);
        assert_eq!(loaded.ui.skin, config.ui.skin);
        assert_eq!(
            loaded.notifications.min_severity,
            config.notifications.min_severity
        );
        assert_eq!(loaded.logging.level, config.logging.level);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn config_save_and_load_custom_values() {
        let path = temp_path("config_custom");
        let mut config = Config::default();
        config.sampling.interval_ms = 500;
        config.detection.cpu_threshold = 80.0;
        config.storage.retention_days = 7;
        config.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.sampling.interval_ms, 500);
        assert_eq!(loaded.detection.cpu_threshold, 80.0);
        assert_eq!(loaded.storage.retention_days, 7);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn config_load_nonexistent_file_fails() {
        let result = Config::load("/nonexistent/path/config.toml");
        assert!(result.is_err());
    }

    #[test]
    fn severity_equality() {
        assert_eq!(Severity::Minor, Severity::Minor);
        assert_ne!(Severity::Minor, Severity::Major);
        assert_ne!(Severity::Major, Severity::Critical);
    }

    #[test]
    fn sample_clone() {
        let mut s = Sample::default();
        s.cpu_usage = 75.5;
        s.cpu_per_core = vec![50.0, 60.0];
        let cloned = s.clone();
        assert_eq!(cloned.cpu_usage, 75.5);
        assert_eq!(cloned.cpu_per_core, vec![50.0, 60.0]);
    }
}
