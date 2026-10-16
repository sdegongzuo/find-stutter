//! Overlay 资源区左键点击（真实组件）：card / bubble 两种布局下，
//! 只有「数字」（ClickableValue）区域命中并触发 resource-clicked("cpu"/"mem"/"net")，
//! 点标签不触发；点暂停按钮区触发 toggle-pause 且不误触资源区。
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::ComponentHandle;
use std::cell::RefCell;
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

fn setup() -> Rc<MinimalSoftwareWindow> {
    slint::platform::set_platform(Box::new(TestPlatform)).ok();
    WINDOW.with(|x| x.clone())
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

/// 点一下，返回本次命中的资源类型（未命中返回 None）。
fn click_kind(
    win: &Rc<MinimalSoftwareWindow>,
    x: f32,
    y: f32,
    hits: &Rc<RefCell<Vec<String>>>,
) -> Option<String> {
    let before = hits.borrow().len();
    click(win, x, y);
    let b = hits.borrow();
    if b.len() > before {
        Some(b[b.len() - 1].clone())
    } else {
        None
    }
}

type Hit = Rc<RefCell<Vec<String>>>;

fn make_overlay(w: f32, h: f32, layout: &str) -> (find_stutter_ui::Overlay, Hit, Rc<RefCell<u32>>) {
    let ui = find_stutter_ui::Overlay::new().unwrap();
    ui.set_skin_width(w);
    ui.set_skin_height(h);
    ui.set_layout_mode(slint::SharedString::from(layout));
    ui.show().unwrap();

    let hits: Hit = Rc::new(RefCell::new(Vec::new()));
    let h2 = hits.clone();
    ui.on_resource_clicked(move |kind: slint::SharedString| {
        h2.borrow_mut().push(kind.to_string());
    });
    let pauses = Rc::new(RefCell::new(0u32));
    let p2 = pauses.clone();
    ui.on_toggle_pause(move || {
        *p2.borrow_mut() += 1;
    });
    (ui, hits, pauses)
}

/// 全窗扫描（步进 3px），返回出现过的资源类型集合。
fn sweep(win: &Rc<MinimalSoftwareWindow>, w: i32, h: i32, hits: &Hit) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    let mut x = 0;
    while x < w {
        let mut y = 0;
        while y < h {
            if let Some(k) = click_kind(win, x as f32, y as f32, hits) {
                set.insert(k);
            }
            y += 3;
        }
        x += 3;
    }
    set.into_iter().collect()
}

#[test]
fn overlay_card_only_number_clickable() {
    let window = setup();
    let (ui, hits, pauses) = make_overlay(360.0, 78.0, "card");

    // 点标签（左列行首，CPU / 内存 文字）不应命中
    let n0 = hits.borrow().len();
    click(&window, 10.0, 18.0);
    click(&window, 10.0, 39.0);
    assert_eq!(hits.borrow().len(), n0, "标签区不应触发 resource-clicked");
    assert_eq!(*pauses.borrow(), 0, "标签点击不应触发暂停");

    // 全窗扫描：三个数字区都能命中，且只出现 cpu/mem/net
    let got = sweep(&window, 360, 78, &hits);
    eprintln!("card 命中类型: {:?}", got);
    assert_eq!(got, vec!["cpu", "mem", "net"], "card 布局命中类型不符");

    // 暂停按钮（右列第三行）→ toggle-pause，且不触发资源区
    let before_hits = hits.borrow().len();
    let before_pauses = *pauses.borrow();
    click(&window, 300.0, 60.0);
    assert_eq!(
        *pauses.borrow(),
        before_pauses + 1,
        "card 暂停区应触发一次 toggle-pause"
    );
    assert_eq!(
        hits.borrow().len(),
        before_hits,
        "暂停区点击不应触发 resource-clicked"
    );

    drop(ui);
}

#[test]
fn overlay_bubble_only_number_clickable() {
    let window = setup();
    let (ui, hits, _pauses) = make_overlay(380.0, 78.0, "bubble");

    // 点标签（左列行首，CPU: / 内存:）不应命中
    let n0 = hits.borrow().len();
    click(&window, 16.0, 18.0);
    click(&window, 16.0, 39.0);
    assert_eq!(hits.borrow().len(), n0, "标签区不应触发 resource-clicked");

    let got = sweep(&window, 380, 78, &hits);
    eprintln!("bubble 命中类型: {:?}", got);
    assert_eq!(got, vec!["cpu", "mem", "net"], "bubble 布局命中类型不符");

    drop(ui);
}
