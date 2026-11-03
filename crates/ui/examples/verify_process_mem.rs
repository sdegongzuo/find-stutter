//! 开发用验证工具：独立拉起「进程详情」窗口，验证 SYSTEM/受保护进程的
//! 内存兜底显示（2026-11-03 修复：DACL 拒绝 OpenProcess 的进程此前错误
//! 显示 0 MB，现走 NtQuerySystemInformation 免句柄快照兜底）。
//!
//! ## 用法
//!
//! ```text
//! # 终端 A：拉起窗口（10 秒后自动退出）
//! cargo run -p find-stutter-ui --example verify_process_mem
//!
//! # 终端 B：窗口出现后截图（snap 不受遮挡影响）
//! snap win 进程详情 target/verify_process_mem.png
//! ```
//!
//! 验证点：HnCloudService 一类 SYSTEM 服务进程的「内存」「专用工作集」
//! 列应为非 0 数值（可用 Get-Process 对照实际值）。

#[cfg(not(windows))]
fn main() {
    eprintln!("本工具仅支持 Windows");
}

#[cfg(windows)]
fn main() {
    // 先单独跑一轮采样（sample() 含内存兜底链路），打印 DACL 拒绝场景的
    // 代表性进程（HONOR SYSTEM 服务），stdout 直接给数值证据
    let mut sampler = find_stutter_ui::process_list::ProcessSampler::new(None);
    let rows = sampler.sample();
    println!("=== 采样 {} 个进程，Hn*/SYSTEM 代表行: ===", rows.len());
    for r in rows.iter().filter(|r| {
        r.name.starts_with("Hn") || r.name.starts_with("HN") || r.name == "dwm.exe"
    }) {
        println!(
            "  pid={:<6} {:<28} 内存={:<8} 专用工作集={}",
            r.pid,
            r.name,
            find_stutter_ui::process_list::format_mem(r.memory_bytes),
            find_stutter_ui::process_list::format_mem(r.physical_mem_bytes),
        );
    }

    let _win = find_stutter_ui::process_list::ProcessListWindow::show(
        80.0,   // highlight_pct：高亮阈值（与默认 config 相近）
        1000,   // refresh_ms：1s 刷新，加快出数据
        false,  // 完整列模式（含专用工作集列）
        true,   // 临时弹窗：工具窗口样式（不占任务栏）
        "mem",  // 按内存排序，0 的进程若仍异常会沉底，正常值一目了然
        None,   // 不连 stutter.db（网络列显示「—」即可）
    )
    .expect("打开进程详情窗口失败");

    // 10 秒后自动退出：足够采样线程出首批数据 + 外部 snap 截图
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::SingleShot,
        std::time::Duration::from_secs(10),
        || {
            let _ = slint::quit_event_loop();
        },
    );
    slint::run_event_loop().expect("事件循环异常退出");
}
