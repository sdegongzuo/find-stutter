use crate::types::Sample;
use chrono::Utc;
use log::warn;
use std::collections::HashMap;
use sysinfo::{Networks, System};
use wmi::{COMLibrary, Variant, WMIConnection};

pub struct Collector {
    sys: System,
    networks: Networks,
    prev_net_sent: u64,
    prev_net_recv: u64,
    tick: u32,
}

impl Collector {
    pub fn new() -> Self {
        let mut sys = System::new_all();
        sys.refresh_all();
        let networks = Networks::new_with_refreshed_list();

        let mut prev_net_sent = 0u64;
        let mut prev_net_recv = 0u64;
        for (_name, data) in networks.iter() {
            prev_net_sent += data.total_transmitted();
            prev_net_recv += data.total_received();
        }

        Self {
            sys,
            networks,
            prev_net_sent,
            prev_net_recv,
            tick: 0,
        }
    }

    pub fn collect(&mut self) -> Sample {
        self.sys.refresh_all();
        self.networks.refresh(true);

        let tick = self.tick;
        self.tick = self.tick.wrapping_add(1);

        let cpu_usage = self.sys.global_cpu_usage();
        let cpu_per_core: Vec<f32> = self.sys.cpus().iter().map(|c| c.cpu_usage()).collect();

        let mem_total = self.sys.total_memory();
        let mem_used = self.sys.used_memory();
        let mem_available = self.sys.available_memory();
        let mem_total_mb = mem_total / (1024 * 1024);
        let mem_used_mb = mem_used / (1024 * 1024);
        let mem_available_mb = mem_available / (1024 * 1024);
        let mem_usage_percent = if mem_total > 0 {
            (mem_used as f32 / mem_total as f32) * 100.0
        } else {
            0.0
        };

        let swap_total = self.sys.total_swap();
        let swap_used = self.sys.used_swap();
        let swap_usage_percent = if swap_total > 0 {
            (swap_used as f32 / swap_total as f32) * 100.0
        } else {
            0.0
        };

        let mut net_sent_total = 0u64;
        let mut net_recv_total = 0u64;
        for (_name, data) in self.networks.iter() {
            net_sent_total += data.total_transmitted();
            net_recv_total += data.total_received();
        }
        let net_sent_bps = net_sent_total.saturating_sub(self.prev_net_sent);
        let net_recv_bps = net_recv_total.saturating_sub(self.prev_net_recv);
        self.prev_net_sent = net_sent_total;
        self.prev_net_recv = net_recv_total;

        let process_count = self.sys.processes().len();

        // Slow channel (every 5 ticks): CPU freq, GPU usage, temperature, disk I/O via WMI.
        // BUG: disk is only sampled every 5 ticks (shows 0 for 4/5 of the time), and the
        // disk/GPU WMI queries below are broken (see collect_wmi_slow).
        let (cpu_freq, gpu_usage, cpu_temp, disk_read_bps, disk_write_bps) = if tick % 5 == 0 {
            self.collect_wmi_slow()
        } else {
            (None, None, None, 0, 0)
        };

        Sample {
            timestamp: Utc::now(),
            cpu_usage,
            cpu_per_core,
            cpu_freq_mhz: cpu_freq,
            mem_usage_percent,
            mem_used_mb,
            mem_total_mb,
            mem_available_mb,
            swap_usage_percent,
            disk_read_bps,
            disk_write_bps,
            net_sent_bps,
            net_recv_bps,
            net_sent_total,
            net_recv_total,
            gpu_usage,
            cpu_temp,
            gpu_temp: None,
            process_count,
            thread_count: 0,
        }
    }

    fn collect_wmi_slow(&self) -> (Option<f32>, Option<f32>, Option<f32>, u64, u64) {
        let com = match COMLibrary::new() {
            Ok(c) => c,
            Err(e) => {
                warn!("COM library init failed: {}", e);
                return (None, None, None, 0, 0);
            }
        };
        let wmi_con = match WMIConnection::new(com) {
            Ok(c) => c,
            Err(e) => {
                warn!("WMI connection failed: {}", e);
                return (None, None, None, 0, 0);
            }
        };

        let cpu_freq = wmi_con
            .raw_query("SELECT CurrentClockSpeed FROM Win32_Processor")
            .ok()
            .and_then(|r: Vec<HashMap<String, Variant>>| r.first().cloned())
            .and_then(|row| {
                if let Some(Variant::UI4(v)) = row.get("CurrentClockSpeed") {
                    Some(*v as f32)
                } else {
                    None
                }
            });

        // BUG: the class `Win32_PerfFormattedData_GPUPerformanceCounters` does not exist,
        // so this query returns no rows -> gpu_usage stays None (UI shows "--").
        let gpu_usage = wmi_con
            .raw_query("SELECT UtilizationPercentage FROM Win32_PerfFormattedData_GPUPerformanceCounters")
            .ok()
            .and_then(|r: Vec<HashMap<String, Variant>>| r.first().cloned())
            .and_then(|row| {
                if let Some(Variant::UI8(v)) = row.get("UtilizationPercentage") {
                    Some((*v as f32).min(100.0))
                } else {
                    None
                }
            });

        // BUG: DiskReadBytesPerSec / DiskWriteBytesPerSec are uint64 (UI8), but we only
        // match UI4, so the values always fall through to 0 (UI shows "0 B/s").
        let (disk_read_bps, disk_write_bps) = wmi_con
            .raw_query(
                "SELECT DiskReadBytesPerSec, DiskWriteBytesPerSec \
                 FROM Win32_PerfFormattedData_PerfDisk_PhysicalDisk WHERE Name='_Total'",
            )
            .ok()
            .and_then(|r: Vec<HashMap<String, Variant>>| r.first().cloned())
            .map(|row| {
                let read =
                    if let Some(Variant::UI4(v)) = row.get("DiskReadBytesPerSec") { *v as u64 } else { 0 };
                let write =
                    if let Some(Variant::UI4(v)) = row.get("DiskWriteBytesPerSec") { *v as u64 } else { 0 };
                (read, write)
            })
            .unwrap_or((0, 0));

        let cpu_temp = wmi_con
            .raw_query("SELECT CurrentTemperature FROM Win32_PerfFormattedData_ThermalZoneInformation")
            .ok()
            .and_then(|r: Vec<HashMap<String, Variant>>| r.first().cloned())
            .and_then(|row| {
                if let Some(Variant::UI4(v)) = row.get("CurrentTemperature") {
                    Some((*v as f32) / 10.0 - 273.15)
                } else {
                    None
                }
            });

        (cpu_freq, gpu_usage, cpu_temp, disk_read_bps, disk_write_bps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_new() {
        let collector = Collector::new();
        assert_eq!(collector.tick, 0);
    }

    #[test]
    fn collector_collect_returns_valid_sample() {
        let mut collector = Collector::new();
        let sample = collector.collect();

        assert!(sample.cpu_usage >= 0.0 && sample.cpu_usage <= 100.0);
        assert!(sample.mem_total_mb > 0);
        assert!(sample.process_count > 0);
    }

    #[test]
    fn collector_collect_increments_tick() {
        let mut collector = Collector::new();
        assert_eq!(collector.tick, 0);
        collector.collect();
        assert_eq!(collector.tick, 1);
        collector.collect();
        assert_eq!(collector.tick, 2);
    }

    #[test]
    fn collector_collect_covers_per_core() {
        let mut collector = Collector::new();
        let sample = collector.collect();
        assert!(!sample.cpu_per_core.is_empty());
        for usage in &sample.cpu_per_core {
            assert!(*usage >= 0.0 && *usage <= 100.0);
        }
    }
}
