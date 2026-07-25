use crate::types::{DetectionConfig, Sample, Severity, StutterEvent};
use chrono::Utc;
use std::time::SystemTime;

pub struct Detector {
    config: DetectionConfig,
    history: Vec<Sample>,
    stutter_start: Option<SystemTime>,
    current_causes: Vec<String>,
}

impl Detector {
    pub fn new(config: &DetectionConfig) -> Self {
        Self {
            config: config.clone(),
            history: Vec::new(),
            stutter_start: None,
            current_causes: Vec::new(),
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
                    if !self.current_causes.contains(&c) {
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

    fn check_hard_thresholds(&self, sample: &Sample) -> Vec<String> {
        let mut causes = Vec::new();

        if sample.cpu_usage > self.config.cpu_threshold {
            causes.push(format!(
                "CPU usage {:.1}% > {}%",
                sample.cpu_usage, self.config.cpu_threshold
            ));
        }

        if sample.mem_available_mb < self.config.mem_threshold_mb {
            causes.push(format!(
                "Available memory {}MB < {}MB",
                sample.mem_available_mb, self.config.mem_threshold_mb
            ));
        }

        if sample.swap_usage_percent > self.config.swap_threshold {
            causes.push(format!(
                "Swap usage {:.1}% > {}%",
                sample.swap_usage_percent, self.config.swap_threshold
            ));
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
        );

        Self::spike_check(
            &mut causes,
            "Disk write",
            "B/s",
            recent.iter().map(|s| s.disk_write_bps as f32).collect(),
            baseline.iter().map(|s| s.disk_write_bps as f32).collect(),
            self.config.disk_rate_spike_ratio,
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
    ) {
        let r_avg = recent.iter().sum::<f32>() / recent.len() as f32;
        let b_avg = baseline.iter().sum::<f32>() / baseline.len() as f32;
        if b_avg > 1.0 {
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
}
