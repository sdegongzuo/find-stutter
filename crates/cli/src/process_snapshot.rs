//! `process` 子命令：用 core collector 现场采一次 top 进程快照（不写库）。
//!
//! 与服务采集共用同一 `Collector`（同一次完整采集，含进程指纹），
//! 输出单行 JSON：采集时刻 + 系统指标摘要 + top 进程列表（按 CPU / 内存
//! 双维度合并，字段与卡顿事件 culprits 同构，便于 agent 对照）。

use find_stutter_core::{Collector, Sample};
use serde_json::{json, Value};

/// 现场采集一帧并转 JSON（不写库；`limit` 截取 top 进程数）。
pub fn snapshot_json(limit: usize) -> anyhow::Result<Value> {
    let mut collector = Collector::new();
    let sample: Sample = collector.collect();
    Ok(sample_to_json(&sample, limit))
}

/// Sample 快照 → JSON（拆出来便于单测：可以用构造的 Sample 验证输出结构）。
///
/// `limit` 语义与 help 一致：「最多返回进程数」——`limit=0` 返回空 top 列表
/// （只要系统摘要，不列进程）。
pub fn sample_to_json(s: &Sample, limit: usize) -> Value {
    let mut procs: Vec<find_stutter_core::ProcessBrief> = s.top_processes.clone();
    // top_processes 次序（采集器 merge_top(8,8,12)）：CPU 维度 top 8 在前、
    // 内存维度去重补充在后；limit ≤ 8 时等价于「按 CPU 截取」，
    // 超过 8 后混入内存维度进程（help 语义只承诺「最多返回进程数」）
    procs.truncate(limit);
    let top: Vec<Value> = procs
        .iter()
        .map(|p| {
            json!({
                "pid": p.pid,
                "name": p.name,
                "cpu_usage": p.cpu_usage,
                "mem_used_mb": p.mem_used_mb,
                "exe_path": p.exe_path,
                "handle_count": p.handle_count,
                "gdi_objects": p.gdi_objects,
                "user_objects": p.user_objects,
                "io_read_bps": p.io_read_bps,
                "io_write_bps": p.io_write_bps,
            })
        })
        .collect();
    json!({
        "timestamp": s.timestamp.to_rfc3339(),
        "summary": {
            "cpu_usage": s.cpu_usage,
            "cpu_freq_mhz": s.cpu_freq_mhz,
            "mem_usage_percent": s.mem_usage_percent,
            "mem_available_mb": s.mem_available_mb,
            "disk_read_bps": s.disk_read_bps,
            "disk_write_bps": s.disk_write_bps,
            "net_sent_bps": s.net_sent_bps,
            "net_recv_bps": s.net_recv_bps,
            "gpu_usage": s.gpu_usage,
            "cpu_temp": s.cpu_temp,
            "process_count": s.process_count,
            "thread_count": s.thread_count,
        },
        "top_processes": top,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use find_stutter_core::ProcessBrief;

    #[test]
    fn sample_to_json_shape_and_limit() {
        let mut s = Sample::default();
        s.cpu_usage = 55.5;
        s.process_count = 100;
        for i in 0..5 {
            s.top_processes.push(ProcessBrief {
                pid: 1000 + i,
                name: format!("proc{}.exe", i),
                cpu_usage: 90.0 - i as f32,
                mem_used_mb: 500,
                ..Default::default()
            });
        }
        let v = sample_to_json(&s, 3);
        assert_eq!(v["summary"]["cpu_usage"].as_f64().unwrap(), 55.5);
        assert_eq!(v["summary"]["process_count"].as_u64().unwrap(), 100);
        let top = v["top_processes"].as_array().unwrap();
        assert_eq!(top.len(), 3, "limit=3 应截取 3 个进程");
        assert_eq!(top[0]["name"].as_str().unwrap(), "proc0.exe");
        assert_eq!(top[0]["pid"].as_u64().unwrap(), 1000);
    }

    #[test]
    fn limit_zero_returns_empty_list() {
        // 「最多返回进程数」：limit=0 = 不列进程，只要系统摘要
        let mut s = Sample::default();
        s.process_count = 100;
        s.top_processes.push(ProcessBrief {
            pid: 1,
            name: "a.exe".into(),
            cpu_usage: 1.0,
            mem_used_mb: 1,
            ..Default::default()
        });
        let v = sample_to_json(&s, 0);
        assert!(v["top_processes"].as_array().unwrap().is_empty());
        // 摘要仍在
        assert_eq!(v["summary"]["process_count"].as_u64().unwrap(), 100);
    }

    /// 真机采集冒烟：一次 collect + JSON 化（不写库、不依赖服务）。
    /// 在 CI / 无 wmi 权限环境可能失败，失败仅跳过断言。
    #[test]
    fn snapshot_json_smoke() {
        if let Ok(v) = snapshot_json(5) {
            assert!(v["timestamp"].is_string());
            assert!(v["summary"]["process_count"].is_u64());
        }
    }
}
