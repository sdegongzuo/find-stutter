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

    // 进程快照：top N by CPU 与 top N by 内存的并集（去重），用于卡顿 culprit 归因
    // serde(default)：旧库 snapshot JSON 无此字段时回退为空列表，避免反序列化失败
    #[serde(default)]
    pub top_processes: Vec<ProcessBrief>,
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
            top_processes: Vec::new(),
        }
    }
}

/// 单个进程的资源占用快照（用于卡顿 culprit 归因）。
///
/// 采集器每次采样本地按 CPU / 内存排序取 top 进程，检测器在卡顿持续期间
/// 累积这些快照（按 pid 取最大用量），卡顿结束时提取 top 进程作为 culprits。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessBrief {
    pub pid: u32,
    pub name: String,
    /// 该进程 CPU 占用（%，sysinfo 全局口径）
    pub cpu_usage: f32,
    /// 该进程内存占用（MB）
    pub mem_used_mb: u64,
}

impl ProcessBrief {
    /// 从给定进程快照集合中，按 CPU / 内存两个维度各取 top 并去重合并（最多 `max` 个）。
    ///
    /// 供两处复用（避免重复实现同一套「双维度 top + 去重」逻辑）：
    /// - 采集器每 tick 取全局 top（CPU top8 + 内存 top8，≤12）；
    /// - 检测器卡顿结束时提取元凶（CPU top3 + 内存 top3，≤6）。
    pub fn merge_top(
        mut all: Vec<ProcessBrief>,
        cpu_take: usize,
        mem_take: usize,
        max: usize,
    ) -> Vec<ProcessBrief> {
        // CPU 维度降序截取
        all.sort_by(|a, b| {
            b.cpu_usage
                .partial_cmp(&a.cpu_usage)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let cpu_top: Vec<ProcessBrief> = all.iter().take(cpu_take).cloned().collect();
        // 内存维度降序截取
        all.sort_by(|a, b| b.mem_used_mb.cmp(&a.mem_used_mb));
        let mem_top: Vec<ProcessBrief> = all.into_iter().take(mem_take).collect();

        // 按 pid 去重合并（CPU 维度优先），到上限即停
        let mut result: Vec<ProcessBrief> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for p in cpu_top.into_iter().chain(mem_top) {
            if seen.insert(p.pid) {
                result.push(p);
            }
            if result.len() >= max {
                break;
            }
        }
        result
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
    /// 造成本次卡顿的进程（CPU / 内存维度 top 进程，去重最多 ~6 个）
    pub culprits: Vec<ProcessBrief>,
}

/// 检测器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionConfig {
    pub cpu_threshold: f32,
    /// CPU 滞回：进入卡顿需 > cpu_threshold；退出需 < cpu_threshold - cpu_hysteresis。
    /// 滞回带内维持当前状态，避免阈值附近反复横跳反复记录。
    #[serde(default = "default_cpu_hysteresis")]
    pub cpu_hysteresis: f32,
    pub mem_threshold_percent: f32,
    pub mem_threshold_mb: u64,
    pub swap_threshold: f32,
    /// Swap 滞回：进入需 > swap_threshold；退出需 < swap_threshold - swap_hysteresis。
    /// 滞回带内维持当前状态。
    #[serde(default = "default_swap_hysteresis")]
    pub swap_hysteresis: f32,
    pub disk_rate_spike_ratio: f32,
    pub spike_ratio: f32,
    /// 网络/磁盘 spike 的绝对下限（B/s）：当前速率低于该值不判定 spike，
    /// 避免空闲时几 B/s ~ 几十 KB/s 的零头波动按倍数误报。
    #[serde(default = "default_spike_min_bps")]
    pub spike_min_bps: u64,
    pub sustained_seconds: u32,
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self {
            cpu_threshold: 90.0,
            cpu_hysteresis: 10.0,
            mem_threshold_percent: 90.0,
            mem_threshold_mb: 500,
            swap_threshold: 50.0,
            swap_hysteresis: 10.0,
            disk_rate_spike_ratio: 10.0,
            spike_ratio: 3.0,
            spike_min_bps: 2_000_000,
            sustained_seconds: 3,
        }
    }
}

fn default_cpu_hysteresis() -> f32 {
    10.0
}

fn default_swap_hysteresis() -> f32 {
    10.0
}

fn default_spike_min_bps() -> u64 {
    2_000_000
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
    /// 启动 GUI 时是否自动检测 + 启动后台服务（含 UAC 提权）。
    /// 自动测试 / CI 环境建议关掉（或设环境变量 FIND_STUTTER_SKIP_SERVICE=1），
    /// 避免每次启动都弹 UAC。
    #[serde(default = "default_true")]
    pub auto_start_service: bool,
    /// P2：任务栏嵌入模式（伪任务栏窗口，显示在屏幕底部，可拖动到任务栏位置）
    #[serde(default)]
    pub taskbar: bool,
    /// 进程详情页：CPU/内存使用率超过该百分比（%）的行高亮标红（默认 30）
    #[serde(default = "default_highlight_pct")]
    pub process_highlight_pct: f32,
    /// 进程详情页：自动刷新间隔（毫秒）。默认 30000 = 30 秒
    #[serde(default = "default_process_refresh_ms")]
    pub process_refresh_ms: u64,
}

fn default_true() -> bool { true }

fn default_highlight_pct() -> f32 { 30.0 }

fn default_process_refresh_ms() -> u64 { 30_000 }

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
            auto_start_service: true,
            taskbar: false,
            process_highlight_pct: default_highlight_pct(),
            process_refresh_ms: default_process_refresh_ms(),
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
    /// 加载配置文件。
    ///
    /// 查找顺序：
    /// 1. 用户指定的 `path`（通常是 `config.toml`）
    /// 2. **当前可执行文件所在目录**下的 `path`（关键！SCM 启动 service 时
    ///    CWD 是 `C:\Windows\System32`，那里没 config.toml；fallback 到
    ///    binary 同目录 `target\release\config.toml`）
    /// 3. 从 binary 目录**逐级向上**查找 `path`（开发布局：binary 在
    ///    `target/release/`，config.toml 在项目根；SCM 服务需要这个回退）
    /// 4. 最后再尝试原路径返回原始错误
    ///
    /// 同时把 `db_path` 相对路径**解析为绝对路径**（基于 config 所在目录），
    /// 避免 SCM service 写到 `C:\Windows\System32\stutter.db`。
    pub fn load(path: &str) -> anyhow::Result<Self> {
        // 1) 尝试给定路径
        if let Ok(content) = std::fs::read_to_string(path) {
            let base = std::path::Path::new(path)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            return Self::parse_with_base(&content, &base);
        }
        // 2) fallback 到 binary 同目录 + 3) 逐级向上
        if let Ok(me) = std::env::current_exe() {
            if let Some(dir) = me.parent() {
                // binary 同目录（如 target/release/config.toml）
                let alt = dir.join(path);
                if let Ok(content) = std::fs::read_to_string(&alt) {
                    log::info!("config 加载自 binary 同目录: {}", alt.display());
                    return Self::parse_with_base(&content, dir);
                }
                // 从 binary 目录逐级向上找（target/release → target → 项目根）
                for ancestor in dir.ancestors().skip(1) {
                    let candidate = ancestor.join(path);
                    if let Ok(content) = std::fs::read_to_string(&candidate) {
                        log::info!(
                            "config 加载自 binary 上级目录: {}",
                            candidate.display()
                        );
                        return Self::parse_with_base(&content, ancestor);
                    }
                }
            }
        }
        // 4) 原路径再试一次让调用方看到原始错误
        let content = std::fs::read_to_string(path)?;
        Self::parse_with_base(
            &content,
            std::path::Path::new(path)
                .parent()
                .unwrap_or_else(|| std::path::Path::new(".")),
        )
    }

    /// 解析 TOML 字符串并把 `db_path` 相对路径转为绝对路径。
    fn parse_with_base(content: &str, base: &std::path::Path) -> anyhow::Result<Self> {
        let mut config: Config = toml::from_str(content)?;
        let p = std::path::Path::new(&config.storage.db_path);
        if p.is_relative() {
            // base 本身是相对路径（如 CWD 下的 "."）时，先转成绝对路径，
            // 否则 base.join("stutter.db") 仍是相对的（日志里出现
            // "db_path 解析为绝对路径: stutter.db" 就是这种情况）。
            let base_abs = if base.is_absolute() {
                base.to_path_buf()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| std::path::PathBuf::from("."))
                    .join(base)
            };
            let abs = base_abs.join(p);
            config.storage.db_path = abs.to_string_lossy().to_string();
            log::info!("db_path 解析为绝对路径: {}", config.storage.db_path);
        }
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
        assert!(s.top_processes.is_empty());
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
        assert_eq!(c.disk_rate_spike_ratio, 10.0);
        assert_eq!(c.spike_ratio, 3.0);
        assert_eq!(c.spike_min_bps, 2_000_000);
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
