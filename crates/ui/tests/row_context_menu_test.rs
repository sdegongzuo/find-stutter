//! ProcessList 行右键回调参数验证（真实组件 + ListView）
//!
//! 用户反馈：连续右键不同进程，菜单顶部标题的名称不跟着变。
//! 本测试验证 slint 端 `root.data.pid/name` 通过 row-context-menu 回调
//! 是否正确跟随行变化（ListView 实例复用/绑定是否拿到正确数据）。
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::ComponentHandle;
use std::rc::Rc;

thread_local! {
    static WINDOW: Rc<MinimalSoftwareWindow> =
        MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
}
struct TestPlatform;
impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(WINDOW.with(|x| x.clone()))
    }
}

fn setup(w: u32, h: u32) -> Rc<MinimalSoftwareWindow> {
    // set_platform 进程内全局唯一：首个测试设置成功，后续测试返回
    // "platform already set"（忽略该错误，继续复用；其他错误才 panic）。
    if let Err(e) = slint::platform::set_platform(Box::new(TestPlatform)) {
        if !e.to_string().contains("already") {
            panic!("set_platform 失败: {}", e);
        }
    }
    let win = WINDOW.with(|x| x.clone());
    win.set_size(slint::PhysicalSize::new(w, h));
    win
}

fn right_click(win: &Rc<MinimalSoftwareWindow>, x: f32, y: f32) {
    let pos = slint::LogicalPosition::new(x, y);
    let _ = win.dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position: pos,
        button: slint::platform::PointerEventButton::Right,
    });
    win.draw_if_needed(|_| {});
    let _ = win.dispatch_event(slint::platform::WindowEvent::PointerReleased {
        position: pos,
        button: slint::platform::PointerEventButton::Right,
    });
    win.draw_if_needed(|_| {});
}

fn row(pid: i32, name: &str) -> find_stutter_ui::ProcessRowData {
    find_stutter_ui::ProcessRowData {
        pid,
        name: name.into(),
        name_full: name.into(),
        group_key: String::new().into(),
        user: String::new().into(),
        cpu: "0.0%".into(),
        cpu_high: false,
        mem: "0 B".into(),
        mem_high: false,
        disk: "R 0 B/s W 0 B/s".into(),
        disk_full: String::new().into(),
        net: "0 B/s".into(),
        net_full: String::new().into(),
        net_total: "0 B".into(),
        net_total_full: String::new().into(),
        status: "运行中".into(),
        is_group: false,
        child_count: 0,
    }
}

#[test]
fn processlist_row_right_click_name_follows_row() {
    let window = setup(700, 500);
    let ui = find_stutter_ui::ProcessList::new().unwrap();
    ui.show().unwrap();

    // 两行不同进程
    let model = slint::VecModel::<find_stutter_ui::ProcessRowData>::default();
    model.push(row(100, "notepad.exe"));
    model.push(row(200, "wps.exe"));
    ui.set_process_model(Rc::new(model).into());
    window.draw_if_needed(|_| {});

    let got = Rc::new(std::cell::RefCell::new(Vec::<(i32, String)>::new()));
    let g2 = got.clone();
    ui.on_row_context_menu(move |pid: i32, name: slint::SharedString| {
        g2.borrow_mut().push((pid, name.to_string()));
    });

    // 列表区从标题栏(30) + 列头(22) + spacing/padding 之后开始 ≈ y=70，行高 26
    // 第一行中心 ~83，第二行中心 ~109（列宽左半 x=30 保证落在名称列）
    right_click(&window, 30.0, 83.0);
    right_click(&window, 30.0, 109.0);

    let got = got.borrow().clone();
    eprintln!("右键回调收到: {:?}", got);
    assert_eq!(got.len(), 2, "两次右键应触发两次回调");
    assert_eq!(got[0].0, 100, "第一行 PID 应为 100");
    assert_eq!(got[0].1, "notepad.exe", "第一行名称应为 notepad.exe");
    assert_eq!(got[1].0, 200, "第二行 PID 应为 200");
    assert_eq!(got[1].1, "wps.exe", "第二行名称应为 wps.exe");
}

/// 模拟 1Hz 刷新：model 整体替换（同长度）后再右键，
/// 验证 ListView 行实例复用时 data 绑定是否取到新数据（旧值缓存 bug 检测）。
#[test]
fn processlist_right_click_after_model_refresh_gets_new_data() {
    let window = setup(700, 500);
    let ui = find_stutter_ui::ProcessList::new().unwrap();
    ui.show().unwrap();

    // 第一帧：notepad.exe / wps.exe
    let model = slint::VecModel::<find_stutter_ui::ProcessRowData>::default();
    model.push(row(100, "notepad.exe"));
    model.push(row(200, "wps.exe"));
    ui.set_process_model(Rc::new(model).into());
    window.draw_if_needed(|_| {});

    // 模拟刷新：整体替换为 chrome.exe / qq.exe
    let model2 = slint::VecModel::<find_stutter_ui::ProcessRowData>::default();
    model2.push(row(300, "chrome.exe"));
    model2.push(row(400, "qq.exe"));
    ui.set_process_model(Rc::new(model2).into());
    window.draw_if_needed(|_| {});

    let got = Rc::new(std::cell::RefCell::new(Vec::<(i32, String)>::new()));
    let g2 = got.clone();
    ui.on_row_context_menu(move |pid: i32, name: slint::SharedString| {
        g2.borrow_mut().push((pid, name.to_string()));
    });

    // 右键刷新后的第一、二行
    right_click(&window, 30.0, 83.0);
    right_click(&window, 30.0, 109.0);

    let got = got.borrow().clone();
    eprintln!("model 刷新后右键收到: {:?}", got);
    assert_eq!(got.len(), 2, "两次右键应触发两次回调");
    assert_eq!(got[0].0, 300, "刷新后第一行 PID 应为 300（不得是旧值 100）");
    assert_eq!(got[0].1, "chrome.exe", "刷新后第一行名称应为 chrome.exe（不得是旧值 notepad.exe）");
    assert_eq!(got[1].0, 400, "刷新后第二行 PID 应为 400");
    assert_eq!(got[1].1, "qq.exe", "刷新后第二行名称应为 qq.exe");
}
