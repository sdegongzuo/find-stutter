use crate::types::Sample;
use chrono::Utc;
use log::warn;
use std::collections::HashMap;
use sysinfo::{Networks, System};
use wmi::{COMLibrary, Variant, WMIConnection};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE, PDH_FMT_LARGE,
};

/// Windows PDH-based disk I/O sampler.
///
/// Holds the PDH query + counter handles for the `_Total` physical-disk
/// read/write bytes-per-second counters. The query is opened once and the
/// counters are sampled every tick so the values are always fresh (the old
/// WMI approach only ran every 5 ticks AND matched the wrong variant type,
/// which is why disk always showed `0 B/s`). In the windows 0.58 crate PDH
/// handles are plain `isize`.
struct DiskPdh {
    query: isize,
    read_counter: isize,
    write_counter: isize,
}

impl DiskPdh {
    fn new() -> Option<Self> {
        unsafe {
            let mut query: isize = 0;
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != ERROR_SUCCESS.0 {
                warn!("PdhOpenQueryW failed");
                return None;
            }

            let mut read_counter: isize = 0;
            let mut write_counter: isize = 0;

            let read_path = w!(r"\PhysicalDisk(_Total)\Disk Read Bytes/sec");
            let write_path = w!(r"\PhysicalDisk(_Total)\Disk Write Bytes/sec");

            if PdhAddEnglishCounterW(query, read_path, 0, &mut read_counter) != ERROR_SUCCESS.0 {
                warn!("PdhAddEnglishCounterW (read) failed");
                PdhCloseQuery(query);
                return None;
            }
            if PdhAddEnglishCounterW(query, write_path, 0, &mut write_counter) != ERROR_SUCCESS.0 {
                warn!("PdhAddEnglishCounterW (write) failed");
                PdhCloseQuery(query);
                return None;
            }

            // Prime the query so the first real collect has a baseline for the
            // "bytes/sec" rate counter.
            PdhCollectQueryData(query);

            Some(Self {
                query,
                read_counter,
                write_counter,
            })
        }
    }

    /// Collect the current read/write bytes-per-second. Returns `(read_bps, write_bps)`.
    fn sample(&self) -> (u64, u64) {
        unsafe {
            if PdhCollectQueryData(self.query) != ERROR_SUCCESS.0 {
                return (0, 0);
            }

            let mut read_val: PDH_FMT_COUNTERVALUE = PDH_FMT_COUNTERVALUE::default();
            let mut write_val: PDH_FMT_COUNTERVALUE = PDH_FMT_COUNTERVALUE::default();

            if PdhGetFormattedCounterValue(self.read_counter, PDH_FMT_LARGE, None, &mut read_val)
                != ERROR_SUCCESS.0
            {
                return (0, 0);
            }
            if PdhGetFormattedCounterValue(self.write_counter, PDH_FMT_LARGE, None, &mut write_val)
                != ERROR_SUCCESS.0
            {
                return (0, 0);
            }

            // CStatus == 0 means valid data. The large value is an i64; clamp
            // negatives to 0 before casting to u64.
            let read = if read_val.CStatus == 0 {
                read_val.Anonymous.largeValue.max(0) as u64
            } else {
                0
            };
            let write = if write_val.CStatus == 0 {
                write_val.Anonymous.largeValue.max(0) as u64
            } else {
                0
            };

            (read, write)
        }
    }
}

impl Drop for DiskPdh {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

pub struct Collector {
    sys: System,
    networks: Networks,
    prev_net_sent: u64,
    prev_net_recv: u64,
    tick: u32,
    disk_pdh: Option<DiskPdh>,
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

        let disk_pdh = DiskPdh::new();

        Self {
            sys,
            networks,
            prev_net_sent,
            prev_net_recv,
            tick: 0,
            disk_pdh,
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

        // Disk I/O: sampled every tick via PDH (accurate, never 0 due to the
        // old every-5-tick WMI bug).
        let (disk_read_bps, disk_write_bps) = match &self.disk_pdh {
            Some(d) => d.sample(),
            None => (0, 0),
        };

        // Slow channel (every 5 ticks): CPU freq, GPU usage, temperature via WMI.
        // These are expensive/rarely-changing, so leaving them on the slow
        // channel is fine.
        let (cpu_freq, gpu_usage, cpu_temp) = if tick % 5 == 0 {
            self.collect_wmi_slow()
        } else {
            (None, None, None)
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

    fn collect_wmi_slow(&self) -> (Option<f32>, Option<f32>, Option<f32>) {
        let com = match COMLibrary::new() {
            Ok(c) => c,
            Err(e) => {
                warn!("COM library init failed: {}", e);
                return (None, None, None);
            }
        };
        let wmi_con = match WMIConnection::new(com) {
            Ok(c) => c,
            Err(e) => {
                warn!("WMI connection failed: {}", e);
                return (None, None, None);
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

        // Fixed: the correct class is Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine.
        // It has multiple rows (one per GPU engine), so we sum UtilizationPercentage
        // across all rows and cap at 100%.
        let gpu_usage = wmi_con
            .raw_query(
                "SELECT UtilizationPercentage \
                 FROM Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine",
            )
            .ok()
            .and_then(|r: Vec<HashMap<String, Variant>>| {
                let mut sum = 0u64;
                for row in &r {
                    let v = match row.get("UtilizationPercentage") {
                        Some(Variant::UI8(v)) => *v,
                        Some(Variant::UI4(v)) => *v as u64,
                        _ => 0,
                    };
                    sum = sum.saturating_add(v);
                }
                if r.is_empty() {
                    None
                } else {
                    Some((sum as f32).min(100.0))
                }
            });

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

        (cpu_freq, gpu_usage, cpu_temp)
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
