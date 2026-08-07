use crate::types::{DetectionConfig, Sample, Severity, StutterEvent};
use chrono::Utc;
use std::time::SystemTime;

pub struct Detector {
    config: DetectionConfig,
    history: Vec<Sample>,
    stutter_start: Option<SystemTime>,
    current_causes: Vec<String>,
    /// CPU 滞回状态：进入后直到 < threshold - hysteresis 才解除
    /// （滞回带内维持激活，避免阈值附近反复 start/stop 反复记录）
    cpu_active: bool,
    /// Swap 滞回状态（同上）
    swap_active: bool,
}

impl Detector {
    pub fn new(config: &DetectionConfig) -> Self {
        Self {
            config: config.clone(),
            history: Vec::new(),
            stutter_start: None,
            current_causes: Vec::new(),
            cpu_active: false,
            swap_active: false,
        }
    }

    pub fn analyze(&mut self, sample: &Sample) -> Option<StutterEvent> {
        self.history.push(sample.clone());
        if self.history.len() > 120 {
            self.history.remove(0);
        }

        let mut causes = Vec::new();
        causes.extend(self.check_hard_thresholds(sample));
        causes.extend(self.check_spike());

        if !causes.is_empty() {
            if self.stutter_start.is_none() {
                self.stutter_start = Some(SystemTime::now());
                self.current_causes = causes;
            } else {
                for c in causes {
                    // 按 cause 类型去重：同类型（如 Swap usage）更新为最新文案，
                    // 避免滞回带内文案随数值变化导致字符串去重失效、cause 反复追加
                    // （一次卡顿中 current_causes 膨胀，还会虚高 severity）。
                    let key = cause_key(&c);
                    if let Some(pos) =
                        self.current_causes.iter().position(|x| cause_key(x) == key)
                    {
                        self.current_causes[pos] = c;
                    } else {
                        self.current_causes.push(c);
                    }
                }
            }
            None
        } else if let Some(start) = self.stutter_start {
            let duration_ms = start.elapsed().map(|d| d.as_millis() as u64).unwrap_or(0);
            self.stutter_start = None;

            if duration_ms >= self.config.sustained_seconds as u64 * 1000 {
                let event = StutterEvent {
                    timestamp: Utc::now(),
                    duration_ms,
                    severity: Self::determine_severity(&self.current_causes, duration_ms),
                    causes: self.current_causes.clone(),
                    snapshot: sample.clone(),
                };
                self.current_causes.clear();
                return Some(event);
            }
            self.current_causes.clear();
            None
        } else {
            None
        }
    }

    fn check_hard_thresholds(&mut self, sample: &Sample) -> Vec<String> {
        let mut causes = Vec::new();

        // CPU：滞回模型。进入 > cpu_threshold；退出 < cpu_threshold - cpu_hysteresis；
        // 滞回带内（threshold - hysteresis ~ threshold）维持 cpu_active 不变，
        // 防止 CPU 在阈值附近震荡时反复开始/结束卡顿记录。
        if sample.cpu_usage > self.config.cpu_threshold {
            self.cpu_active = true;
        } else if sample.cpu_usage <= self.config.cpu_threshold - self.config.cpu_hysteresis {
            self.cpu_active = false;
        }
        if self.cpu_active {
            if sample.cpu_usage > self.config.cpu_threshold {
                causes.push(format!(
                    "CPU usage {:.1}% > {}%",
                    sample.cpu_usage, self.config.cpu_threshold
                ));
            } else {
                // 滞回带内维持激活：数值已回落但未到退出线，
                // 文案明确"滞回保持"，避免出现 "85% > 90%" 的矛盾
                causes.push(format!(
                    "CPU usage {:.1}%（滞回保持，阈值 {}%）",
                    sample.cpu_usage, self.config.cpu_threshold
                ));
            }
        }

        if sample.mem_available_mb < self.config.mem_threshold_mb {
            causes.push(format!(
                "Available memory {}MB < {}MB",
                sample.mem_available_mb, self.config.mem_threshold_mb
            ));
        }

        // Swap：滞回模型（与 CPU 相同；进入 > swap_threshold，退出 < swap_threshold - hysteresis）
        if sample.swap_usage_percent > self.config.swap_threshold {
            self.swap_active = true;
        } else if sample.swap_usage_percent
            <= self.config.swap_threshold - self.config.swap_hysteresis
        {
            self.swap_active = false;
        }
        if self.swap_active {
            if sample.swap_usage_percent > self.config.swap_threshold {
                causes.push(format!(
                    "Swap usage {:.1}% > {}%",
                    sample.swap_usage_percent, self.config.swap_threshold
                ));
            } else {
                // 滞回带内维持激活（同上，文案避免 "45% > 50%" 的矛盾）
                causes.push(format!(
                    "Swap usage {:.1}%（滞回保持，阈值 {}%）",
                    sample.swap_usage_percent, self.config.swap_threshold
                ));
            }
        }

        causes
    }

    fn check_spike(&self) -> Vec<String> {
        let mut causes = Vec::new();
        let len = self.history.len();
        if len < 70 {
            return causes;
        }

        let recent = &self.history[len - 10..];
        let baseline = &self.history[len - 70..len - 10];

        Self::spike_check(
            &mut causes,
            "CPU",
            "%",
            recent.iter().map(|s| s.cpu_usage).collect(),
            baseline.iter().map(|s| s.cpu_usage).collect(),
            self.config.spike_ratio,
            0.0, // CPU 为百分比，无绝对下限
        );

        Self::spike_check(
            &mut causes,
            "Disk write",
            "B/s",
            recent.iter().map(|s| s.disk_write_bps as f32).collect(),
            baseline.iter().map(|s| s.disk_write_bps as f32).collect(),
            self.config.disk_rate_spike_ratio,
            self.config.spike_min_bps as f32,
        );

        Self::spike_check(
            &mut causes,
            "Network",
            "B/s",
            recent
                .iter()
                .map(|s| (s.net_sent_bps + s.net_recv_bps) as f32)
                .collect(),
            baseline
                .iter()
                .map(|s| (s.net_sent_bps + s.net_recv_bps) as f32)
                .collect(),
            self.config.spike_ratio,
            self.config.spike_min_bps as f32,
        );

        let recent_mem: Vec<f32> = recent.iter().map(|s| s.mem_available_mb as f32).collect();
        let baseline_mem: Vec<f32> = baseline.iter().map(|s| s.mem_available_mb as f32).collect();
        let r_avg = recent_mem.iter().sum::<f32>() / recent_mem.len() as f32;
        let b_avg = baseline_mem.iter().sum::<f32>() / baseline_mem.len() as f32;
        if b_avg > 1.0 {
            let ratio = (b_avg - r_avg).abs() / b_avg;
            if ratio > self.config.spike_ratio {
                causes.push(format!(
                    "Memory available spike: {:.0}MB → {:.0}MB",
                    b_avg, r_avg
                ));
            }
        }

        causes
    }

    fn spike_check(
        causes: &mut Vec<String>,
        name: &str,
        unit: &str,
        recent: Vec<f32>,
        baseline: Vec<f32>,
        threshold: f32,
        min_abs: f32,
    ) {
        let r_avg = recent.iter().sum::<f32>() / recent.len() as f32;
        let b_avg = baseline.iter().sum::<f32>() / baseline.len() as f32;
        // 绝对下限：当前速率必须达到 min_abs 才判定 spike（网络/磁盘用，
        // 避免空闲零头 B/s 被倍数放大误报）。CPU 传 0 表示不设下限。
        if r_avg >= min_abs && b_avg > 1.0 {
            let ratio = (r_avg - b_avg).abs() / b_avg;
            if ratio > threshold {
                causes.push(format!(
                    "{} spike: {:.1}{} → {:.1}{}",
                    name, b_avg, unit, r_avg, unit
                ));
            }
        }
    }

    fn determine_severity(causes: &[String], duration_ms: u64) -> Severity {
        let count = causes.len();
        if count >= 3 || duration_ms > 30_000 {
            Severity::Critical
        } else if count >= 2 || duration_ms > 10_000 {
            Severity::Major
        } else {
            Severity::Minor
        }
    }
}

/// cause 的稳定类型 key：按已知前缀匹配，用于同类型去重/更新。
/// 滞回带内文案数值会变化（"CPU usage 85%（滞回保持…）" vs "CPU usage 95% > 90%"），
/// 但类型 key 不变；CPU 硬阈值与 CPU spike 是不同的 cause（key 不同）。
fn cause_key(cause: &str) -> &str {
    const PREFIXES: [&str; 7] = [
        "CPU usage",
        "CPU spike",
        "Disk write",
        "Network",
        "Memory available",
        "Available memory",
        "Swap usage",
    ];
    for p in PREFIXES {
        if cause.starts_with(p) {
            return p;
        }
    }
    cause
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sample(cpu: f32, mem_avail_mb: u64, swap: f32) -> Sample {
        let mut s = Sample::default();
        s.cpu_usage = cpu;
        s.mem_available_mb = mem_avail_mb;
        s.swap_usage_percent = swap;
        s
    }

    // --- Detector::new ---

    #[test]
    fn detector_new_initial_state() {
        let config = DetectionConfig::default();
        let d = Detector::new(&config);
        assert!(d.history.is_empty());
        assert!(d.stutter_start.is_none());
        assert!(d.current_causes.is_empty());
        assert_eq!(d.config.cpu_threshold, 90.0);
    }

    // --- analyze: normal sample ---

    #[test]
    fn analyze_normal_sample_returns_none() {
        let config = DetectionConfig::default();
        let mut d = Detector::new(&config);
        let sample = make_sample(30.0, 2000, 10.0);
        assert!(d.analyze(&sample).is_none());
    }

    #[test]
    fn analyze_normal_sample_no_causes() {
        let config = DetectionConfig::default();
        let mut d = Detector::new(&config);
        let sample = make_sample(30.0, 2000, 10.0);
        d.analyze(&sample);
        assert!(d.current_causes.is_empty());
        assert!(d.stutter_start.is_none());
    }

    // --- analyze: CPU threshold triggers causes ---

    #[test]
    fn analyze_high_cpu_starts_stutter_tracking() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        let high = make_sample(95.0, 2000, 10.0);
        let result = d.analyze(&high);
        assert!(result.is_none()); // No event yet, stutter just started
        assert!(!d.current_causes.is_empty());
        assert!(d.stutter_start.is_some());
        assert!(d.current_causes[0].contains("CPU usage"));
    }

    #[test]
    fn analyze_low_memory_starts_stutter_tracking() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        let low_mem = make_sample(30.0, 100, 10.0);
        d.analyze(&low_mem);
        assert!(!d.current_causes.is_empty());
        assert!(d.current_causes[0].contains("Available memory"));
    }

    #[test]
    fn analyze_high_swap_starts_stutter_tracking() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        let high_swap = make_sample(30.0, 2000, 80.0);
        d.analyze(&high_swap);
        assert!(!d.current_causes.is_empty());
        assert!(d.current_causes[0].contains("Swap usage"));
    }

    #[test]
    fn analyze_multiple_thresholds_collects_all_causes() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // High CPU + low memory + high swap = 3 causes
        let bad = make_sample(95.0, 100, 80.0);
        d.analyze(&bad);
        assert_eq!(d.current_causes.len(), 3);
    }

    // --- analyze: event generation after sustained period ---

    #[test]
    fn analyze_event_generated_after_sustained_period() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // Push high-CPU samples to start stutter
        let high = make_sample(95.0, 2000, 10.0);
        for _ in 0..3 {
            d.analyze(&high);
        }

        // Wait for sustained period
        std::thread::sleep(std::time::Duration::from_millis(1200));

        // Push normal sample to end stutter → should generate event
        let normal = make_sample(20.0, 2000, 10.0);
        let event = d.analyze(&normal);
        assert!(event.is_some());

        let event = event.unwrap();
        assert!(!event.causes.is_empty());
        assert!(event.duration_ms >= 1000);
        assert_eq!(event.severity, Severity::Minor); // 1 cause
    }

    #[test]
    fn analyze_no_event_if_stutter_too_short() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 3;
        let mut d = Detector::new(&config);

        // Push high-CPU samples
        let high = make_sample(95.0, 2000, 10.0);
        d.analyze(&high);

        // Wait less than sustained_seconds
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Push normal sample — stutter too short, no event
        let normal = make_sample(20.0, 2000, 10.0);
        let event = d.analyze(&normal);
        assert!(event.is_none());
        // Causes should be cleared since stutter was too short
        assert!(d.current_causes.is_empty());
        assert!(d.stutter_start.is_none());
    }

    // --- analyze: severity via cause count ---

    #[test]
    fn analyze_severity_minor_single_cause() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // Only high CPU → 1 cause
        let high = make_sample(95.0, 2000, 10.0);
        for _ in 0..3 {
            d.analyze(&high);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));

        let normal = make_sample(20.0, 2000, 10.0);
        let event = d.analyze(&normal).unwrap();
        assert_eq!(event.severity, Severity::Minor);
    }

    #[test]
    fn analyze_severity_major_two_causes() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // High CPU + high swap → 2 causes
        let bad = make_sample(95.0, 2000, 80.0);
        for _ in 0..3 {
            d.analyze(&bad);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));

        let normal = make_sample(20.0, 2000, 10.0);
        let event = d.analyze(&normal).unwrap();
        assert_eq!(event.severity, Severity::Major);
    }

    #[test]
    fn analyze_severity_critical_three_causes() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // High CPU + low memory + high swap → 3 causes
        let bad = make_sample(95.0, 100, 80.0);
        for _ in 0..3 {
            d.analyze(&bad);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));

        let normal = make_sample(20.0, 2000, 10.0);
        let event = d.analyze(&normal).unwrap();
        assert_eq!(event.severity, Severity::Critical);
    }

    // --- check_hard_thresholds boundary tests ---

    #[test]
    fn analyze_cpu_at_threshold_no_trigger() {
        let config = DetectionConfig::default(); // threshold 90.0
        let mut d = Detector::new(&config);

        // CPU exactly at threshold → no trigger (uses >)
        let sample = make_sample(90.0, 2000, 10.0);
        d.analyze(&sample);
        assert!(d.current_causes.is_empty());
    }

    #[test]
    fn analyze_cpu_above_threshold_triggers() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // CPU just above threshold
        let sample = make_sample(90.1, 2000, 10.0);
        d.analyze(&sample);
        assert!(!d.current_causes.is_empty());
    }

    #[test]
    fn analyze_mem_at_threshold_no_trigger() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // Available memory exactly at threshold → no trigger (uses <)
        let sample = make_sample(30.0, 500, 10.0);
        d.analyze(&sample);
        assert!(d.current_causes.is_empty());
    }

    #[test]
    fn analyze_mem_below_threshold_triggers() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // Available memory just below threshold
        let sample = make_sample(30.0, 499, 10.0);
        d.analyze(&sample);
        assert!(!d.current_causes.is_empty());
    }

    #[test]
    fn analyze_swap_at_threshold_no_trigger() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // Swap exactly at threshold → no trigger (uses >)
        let sample = make_sample(30.0, 2000, 50.0);
        d.analyze(&sample);
        assert!(d.current_causes.is_empty());
    }

    #[test]
    fn analyze_swap_above_threshold_triggers() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // Swap just above threshold
        let sample = make_sample(30.0, 2000, 50.1);
        d.analyze(&sample);
        assert!(!d.current_causes.is_empty());
    }

    // --- history management ---

    #[test]
    fn analyze_history_capped_at_120() {
        let config = DetectionConfig::default();
        let mut d = Detector::new(&config);

        let sample = make_sample(30.0, 2000, 10.0);
        for _ in 0..130 {
            d.analyze(&sample);
        }
        assert!(d.history.len() <= 120);
    }

    // --- stutter causes merge ---

    #[test]
    fn analyze_merges_new_causes_during_stutter() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 5; // long enough to not finish
        let mut d = Detector::new(&config);

        // Start with high CPU only
        let cpu_only = make_sample(95.0, 2000, 10.0);
        d.analyze(&cpu_only);
        assert_eq!(d.current_causes.len(), 1);

        // Now also breach swap
        let cpu_and_swap = make_sample(95.0, 2000, 80.0);
        d.analyze(&cpu_and_swap);
        assert_eq!(d.current_causes.len(), 2);
    }

    // --- swap / cpu 滞回（hysteresis）---

    #[test]
    fn swap_hysteresis_keeps_active_within_band() {
        let config = DetectionConfig::default(); // swap_threshold=50, hysteresis=10 → 退出线 40
        let mut d = Detector::new(&config);

        d.analyze(&make_sample(30.0, 2000, 55.0)); // >50 进入
        assert!(!d.current_causes.is_empty());
        assert!(d.current_causes[0].contains("Swap usage"));

        // 滞回带内（45：< 50 但 > 40）→ 维持激活，不解除；同类型 cause 更新而非追加
        d.analyze(&make_sample(30.0, 2000, 45.0));
        assert!(
            !d.current_causes.is_empty(),
            "滞回带内应维持 Swap 激活状态"
        );
        assert_eq!(
            d.current_causes.len(),
            1,
            "滞回带内同类型 cause 应更新而非追加，got: {:?}",
            d.current_causes
        );
        assert!(d.current_causes[0].contains("Swap usage"));
        assert!(
            d.current_causes[0].contains("滞回保持"),
            "滞回带内文案应标注滞回保持，got: {}",
            d.current_causes[0]
        );
    }

    #[test]
    fn swap_hysteresis_releases_below_exit_line() {
        let config = DetectionConfig::default(); // 退出线 40
        let mut d = Detector::new(&config);

        d.analyze(&make_sample(30.0, 2000, 55.0)); // 进入
        assert!(!d.current_causes.is_empty());

        d.analyze(&make_sample(30.0, 2000, 35.0)); // <40 退出
        assert!(d.current_causes.is_empty());
    }

    #[test]
    fn cpu_hysteresis_keeps_active_within_band() {
        let config = DetectionConfig::default(); // cpu_threshold=90, hysteresis=10 → 退出线 80
        let mut d = Detector::new(&config);

        d.analyze(&make_sample(95.0, 2000, 10.0)); // >90 进入
        assert!(!d.current_causes.is_empty());
        assert!(d.current_causes[0].contains("CPU usage"));

        // 滞回带内（85：< 90 但 > 80）→ 维持激活；同类型 cause 更新而非追加
        d.analyze(&make_sample(85.0, 2000, 10.0));
        assert!(
            !d.current_causes.is_empty(),
            "滞回带内应维持 CPU 激活状态"
        );
        assert_eq!(
            d.current_causes.len(),
            1,
            "滞回带内同类型 cause 应更新而非追加，got: {:?}",
            d.current_causes
        );
        assert!(d.current_causes[0].contains("CPU usage"));
        assert!(
            d.current_causes[0].contains("滞回保持"),
            "滞回带内文案应标注滞回保持，got: {}",
            d.current_causes[0]
        );
    }

    #[test]
    fn cpu_hysteresis_releases_below_exit_line() {
        let config = DetectionConfig::default(); // 退出线 80
        let mut d = Detector::new(&config);

        d.analyze(&make_sample(95.0, 2000, 10.0)); // 进入
        assert!(!d.current_causes.is_empty());

        d.analyze(&make_sample(75.0, 2000, 10.0)); // <80 退出
        assert!(d.current_causes.is_empty());
    }

    // --- spike 绝对下限 ---

    fn make_sample_net(cpu: f32, mem_avail_mb: u64, swap: f32, net_bps: u64) -> Sample {
        let mut s = make_sample(cpu, mem_avail_mb, swap);
        s.net_sent_bps = net_bps;
        s
    }

    /// 空闲零头（KB 级波动）即使倍数很大也不应触发 spike（绝对下限拦截）。
    #[test]
    fn spike_min_floor_ignores_small_rates() {
        let config = DetectionConfig::default(); // spike_ratio=2.0, spike_min_bps=1MB
        let mut d = Detector::new(&config);

        // 60 个基线样本（1 KB/s）+ 10 个 recent（10 KB/s）：ratio=9 > 2，
        // 但 r_avg=10KB << 1MB → 绝对下限拦截
        for _ in 0..60 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 1_000));
        }
        for _ in 0..10 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 10_000));
        }
        assert!(
            d.current_causes.iter().all(|c| !c.contains("spike")),
            "KB 级零头不应触发 spike，got: {:?}",
            d.current_causes
        );
    }

    /// 真实大流量（≥ 绝对下限）时 spike 正常触发。
    #[test]
    fn spike_min_floor_allows_large_rates() {
        let config = DetectionConfig::default();
        let mut d = Detector::new(&config);

        // 60 个基线（1 MB/s）+ 10 个 recent（5 MB/s）：ratio=4 > 2 且 ≥ 1MB
        for _ in 0..60 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 1_000_000));
        }
        for _ in 0..10 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 5_000_000));
        }
        assert!(
            d.current_causes.iter().any(|c| c.contains("Network spike")),
            "真实大流量 spike 应触发，got: {:?}",
            d.current_causes
        );
    }

    // --- cause 类型去重 key ---

    #[test]
    fn cause_key_groups_hysteresis_variants() {
        // 滞回带内/外文案不同，但类型 key 一致 → 同一条 cause 更新而非追加
        assert_eq!(
            cause_key("CPU usage 95.0% > 90%"),
            cause_key("CPU usage 85.0%（滞回保持，阈值 90%）")
        );
        assert_eq!(
            cause_key("Swap usage 55.0% > 50%"),
            cause_key("Swap usage 45.0%（滞回保持，阈值 50%）")
        );
        // 硬阈值与 spike 是不同 cause
        assert_ne!(cause_key("CPU usage 95.0% > 90%"), cause_key("CPU spike: 1.0% → 3.0%"));
        // spike 各类型互不混淆
        assert_ne!(cause_key("Disk write spike: 1B/s → 3B/s"), cause_key("Network spike: 1B/s → 3B/s"));
        assert_ne!(cause_key("Memory available spike: 1MB → 3MB"), cause_key("Available memory 100MB < 500MB"));
    }
}
