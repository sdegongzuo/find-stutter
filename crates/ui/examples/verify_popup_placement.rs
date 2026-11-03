//! 开发用验证工具：真实 Win32 路径验证「弹窗首帧即终态」。
//!
//! ## 背景
//!
//! 早期实现是「先 show 到 winit 默认位置，再夹进工作区 / 贴到悬浮窗下方」——
//! 用户看到的是**先露一截、再跳到完整位置**的闪动。现在改为在 `show()` **之前**
//! 用 `plan_popup_rect_near` 算好落点并交给 `Window::set_position`，窗口创建时
//! 就在终态，不存在中途移动。
//!
//! 本工具验证三件事（单测只覆盖纯函数，这里跑真实窗口链路）：
//! 1. 弹窗实际落点 == 显示前算出的计划矩形（set_position 被窗口创建采用）；
//! 2. 整窗在所在显示器工作区内；
//! 3. 锚点贴屏幕底边时，弹窗翻到锚点**上方**（不遮挡锚点）。
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
        monitor_work_area_for_window, native_rect, plan_popup_rect_near, set_window_rect,
    };
    use std::sync::Arc;
    use std::time::Duration;

    // 锚点窗口（模拟悬浮窗）：无锚点开局 → 光标所在显示器居中
    let anchor_win = match ProcessListWindow::show(70.0, 2000, false, true, "cpu", None, None) {
        Ok(w) => Arc::new(w),
        Err(e) => {
            eprintln!("锚点窗口创建失败: {}", e);
            std::process::exit(2);
        }
    };

    // 等 show 后的 500ms 定时器（窗口样式）跑完
    let a1 = anchor_win.clone();
    slint::Timer::single_shot(Duration::from_millis(900), move || {
        let window = a1.window();
        let rect = native_rect(window).expect("锚点窗口矩形");
        let work = monitor_work_area_for_window(window).expect("工作区");
        println!("工作区 rcWork = {:?}", work);
        println!("锚点窗口原始矩形 = {:?}", rect);

        // 把锚点缩成悬浮窗的真实比例（380×78 逻辑 = 760×156 @200%）再挪到
        // 屏幕右下角——真实场景里锚点就是这个小悬浮窗，贴边才是会抛锚的位置
        let aw = 760;
        let ah = 156;
        let ax = work.2 - aw;
        let ay = work.3 - ah;
        set_window_rect(window, ax, ay, aw, ah);

        let a2 = a1.clone();
        slint::Timer::single_shot(Duration::from_millis(300), move || {
            let anchor_rect = native_rect(a2.window()).expect("锚点窗口矩形");
            let work = monitor_work_area_for_window(a2.window()).expect("工作区");
            println!("锚点贴右下角后 = {:?}", anchor_rect);

            // 关键：计划矩形在「弹窗显示之前」算好，并作为 show 的锚点参数传入
            let plan =
                plan_popup_rect_near(a2.window(), 900.0, 506.0, 8.0).expect("显示前应能算出落点");
            println!("显示前计划矩形 = {:?}", plan);

            let b = match ProcessListWindow::show(
                70.0,
                2000,
                false,
                true,
                "cpu",
                None,
                Some(&a2.window()),
            ) {
                Ok(w) => Arc::new(w),
                Err(e) => {
                    eprintln!("FAIL: 弹窗创建失败: {}", e);
                    std::process::exit(1);
                }
            };

            slint::Timer::single_shot(Duration::from_millis(400), move || {
                let got = native_rect(b.window()).expect("弹窗矩形");
                println!("弹窗实际矩形 = {:?}", got);

                let same_as_plan = (got.0, got.1) == (plan.0, plan.1);
                let inside = got.0 >= work.0
                    && got.1 >= work.1
                    && got.0 + got.2 <= work.2
                    && got.1 + got.3 <= work.3;
                // 锚点贴底 → 弹窗应翻到锚点上方（不遮挡）
                let above_anchor = got.1 + got.3 <= anchor_rect.1;

                println!("落点 == 计划（首帧即终态）= {}", same_as_plan);
                println!("整窗在工作区内 = {}", inside);
                println!("翻到锚点上方 = {}", above_anchor);
                if same_as_plan && inside && above_anchor {
                    println!("PASS");
                } else {
                    println!("FAIL");
                    std::process::exit(1);
                }
                let _ = slint::quit_event_loop();
            });
        });
    });

    if let Err(e) = slint::run_event_loop() {
        eprintln!("事件循环异常: {}", e);
        std::process::exit(2);
    }
}
