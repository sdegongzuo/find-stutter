//! 开发用验证工具：真实 Win32 路径验证「进程详情窗口靠近屏幕边界时被夹回工作区」。
//!
//! ## 背景
//!
//! 资源区点击（CPU/内存/网络）打开的进程详情窗口位置固定为「悬浮窗左下方
//! 8px」，屏幕右下角打开时窗口会有一截跑到工作区外看不见。修复后：
//! 下方放不下 → 翻到锚点上方；横向/纵向越界 → 夹进所在显示器工作区。
//!
//! 单测覆盖了纯函数（clamp_rect_to_work / place_popup_rect），本工具补的是
//! **真实窗口链路**：SetWindowPos 写回 → GetWindowRect 读回 → 是否真的落在
//! rcWork 内。
//!
//! **必须在 Slint 事件循环里跑**：`ui.show()` 只是登记，原生 HWND 要等事件
//! 循环启动后才真正创建，`window_handle()` 在此之前恒为 Err。
//!
//! ## 用法
//!
//! ```text
//! cargo run -p find-stutter-ui --example verify_popup_placement
//! ```
//!
//! 输出末尾 `PASS` / `FAIL` 便于脚本断言。

#[cfg(not(windows))]
fn main() {
    eprintln!("本工具仅支持 Windows");
}

#[cfg(windows)]
fn main() {
    use find_stutter_ui::process_list::ProcessListWindow;
    use find_stutter_ui::window::{
        monitor_work_area_for_window, native_rect, place_popup_rect, set_window_rect,
    };
    use std::sync::Arc;
    use std::time::Duration;

    let win = match ProcessListWindow::show(70.0, 1000, false, true, "cpu", None) {
        Ok(w) => Arc::new(w),
        Err(e) => {
            eprintln!("窗口创建失败: {}", e);
            std::process::exit(2);
        }
    };

    // 等 show 后的 500ms 定时器（工具窗口样式 / 打开时夹取）跑完再验
    let timer_win = win.clone();
    slint::Timer::single_shot(Duration::from_millis(900), move || {
        let window = timer_win.window();
        let Some(rect0) = native_rect(window) else {
            eprintln!("FAIL: 取不到窗口矩形（HWND 未就绪）");
            std::process::exit(1);
        };
        println!("打开时原生矩形 = {:?}", rect0);
        let Some(work) = monitor_work_area_for_window(window) else {
            eprintln!("FAIL: 取不到工作区（rcWork）");
            std::process::exit(1);
        };
        println!("工作区 rcWork = {:?}", work);

        // 锚点模拟「贴着屏幕右下角的悬浮窗」：右下角留 420×64 的框
        let anchor = (work.2 - 420, work.3 - 64 - 8, 420, 64);
        let (_, _, w, h) = rect0;
        let target = place_popup_rect(anchor, w, h, work, 8);
        println!("计算目标矩形 = {:?}（锚点 {:?}）", target, anchor);
        set_window_rect(window, target.0, target.1, target.2, target.3);

        // 写回是同步的，读回前给 winit 一拍处理 WM_WINDOWPOSCHANGED
        let check_win = timer_win.clone();
        let anchor_check = anchor;
        let work_check = work;
        slint::Timer::single_shot(Duration::from_millis(250), move || {
            let window = check_win.window();
            let got = native_rect(window).expect("读回窗口矩形");
            println!("实际窗口矩形 = {:?}", got);
            let inside = got.0 >= work_check.0
                && got.1 >= work_check.1
                && got.0 + got.2 <= work_check.2
                && got.1 + got.3 <= work_check.3;
            // 锚点贴底 → 应翻到锚点上方（不遮挡锚点）
            let above = got.1 + got.3 <= anchor_check.1;
            println!("整窗在工作区内 = {}", inside);
            println!("翻到锚点上方 = {}", above);
            if inside && above {
                println!("PASS");
            } else {
                println!("FAIL");
                std::process::exit(1);
            }
            // 保持窗口 2 秒便于肉眼 / 截图核对，然后退出
            let quit_win = check_win.clone();
            slint::Timer::single_shot(Duration::from_millis(2000), move || {
                drop(quit_win);
                let _ = slint::quit_event_loop();
            });
        });
    });

    if let Err(e) = slint::run_event_loop() {
        eprintln!("事件循环异常: {}", e);
        std::process::exit(2);
    }
}
