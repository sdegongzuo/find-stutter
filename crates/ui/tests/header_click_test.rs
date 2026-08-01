//! ProcessList 列头点击（真实组件）
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
    slint::platform::set_platform(Box::new(TestPlatform)).ok();
    let win = WINDOW.with(|x| x.clone());
    win.set_size(slint::PhysicalSize::new(w, h));
    win
}

fn click(win: &Rc<MinimalSoftwareWindow>, x: f32, y: f32) {
    let pos = slint::LogicalPosition::new(x, y);
    let _ = win.dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position: pos,
        button: slint::platform::PointerEventButton::Left,
    });
    win.draw_if_needed(|_| {});
    let _ = win.dispatch_event(slint::platform::WindowEvent::PointerReleased {
        position: pos,
        button: slint::platform::PointerEventButton::Left,
    });
    win.draw_if_needed(|_| {});
}

#[test]
fn processlist_header_click() {
    let window = setup(700, 500);
    let ui = find_stutter_ui::ProcessList::new().unwrap();
    ui.show().unwrap();
    window.draw_if_needed(|_| {});

    let sorts = Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let s2 = sorts.clone();
    ui.on_sort_requested(move |col: slint::SharedString| {
        s2.borrow_mut().push(col.to_string());
    });

    // 点击 PID 列头（y≈45-50 列头行）
    for y in [35.0, 40.0, 45.0, 50.0, 55.0] {
        click(&window, 30.0, y);
    }
    let got = sorts.borrow().clone();
    eprintln!("触发排序列: {:?}", got);
    assert!(!got.is_empty(), "PID 列头点击应触发 sort-requested");
}
