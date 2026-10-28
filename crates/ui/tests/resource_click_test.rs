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

fn make_overlay(w: f32, h: f32) -> (find_stutter_ui::Overlay, Hit, Rc<RefCell<u32>>) {
    let ui = find_stutter_ui::Overlay::new().unwrap();
    ui.set_skin_width(w);
    ui.set_skin_height(h);
    // 用**最宽的取值**扫描：默认占位符 "--" 永远放得下，测不出中列溢出
    // （P1-2 就是这么漏过既有用例的）。1000 KB/s 级别的速率是日常真实值。
    ui.set_cpu_value(slint::SharedString::from("100%"));
    ui.set_mem_value(slint::SharedString::from("100%"));
    ui.set_gpu_value(slint::SharedString::from("100.0%"));
    ui.set_net_send_value(slint::SharedString::from("1023.9 KB/s"));
    ui.set_net_recv_value(slint::SharedString::from("1023.9 MB/s"));
    ui.set_event_count(slint::SharedString::from("今日卡顿 12"));
    ui.set_last_heartbeat(slint::SharedString::from("最近心跳 12:49:57"));
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

/// 某一类命中区的包围盒（物理像素）。
#[derive(Debug, Clone, Copy)]
struct Bounds {
    min_x: f32,
    max_x: f32,
    min_y: f32,
    max_y: f32,
}

impl Bounds {
    fn new(x: f32, y: f32) -> Self {
        Self {
            min_x: x,
            max_x: x,
            min_y: y,
            max_y: y,
        }
    }
    fn add(&mut self, x: f32, y: f32) {
        self.min_x = self.min_x.min(x);
        self.max_x = self.max_x.max(x);
        self.min_y = self.min_y.min(y);
        self.max_y = self.max_y.max(y);
    }
}

// 布局常量，必须与 crates/ui/ui/overlay.slint 的 HorizontalLayout 一致：
// padding-left 12 / padding-right 46（暂停按钮），左列 24%，右列 38%。
// 改了 slint 的列宽就要同步这里，否则下面的越界断言会给出错误的通过/失败。
const PAD_LEFT: f32 = 12.0;
const PAD_RIGHT: f32 = 46.0;
const LEFT_COL_PCT: f32 = 0.24;
const RIGHT_COL_PCT: f32 = 0.38;

fn content_width(w: f32) -> f32 {
    w - PAD_LEFT - PAD_RIGHT
}
/// 左列右边界（左列以右的 x 都不该命中 cpu/mem）
fn left_col_end(w: f32) -> f32 {
    PAD_LEFT + LEFT_COL_PCT * content_width(w)
}
/// 右列左边界（该 x 以左的命中都不得越过，否则会压住服务状态/心跳文字）
fn right_col_start(w: f32) -> f32 {
    w - PAD_RIGHT - RIGHT_COL_PCT * content_width(w)
}

/// 全窗扫描（步进 3px），返回每类命中的包围盒。
///
/// 只断言「出现过哪些类型」是抓不住命中区膨胀的——把整行都做成可点，
/// 集合依然是 {cpu,mem,net}。所以这里连矩形一起记录，交给调用方断言不越列。
fn sweep(
    win: &Rc<MinimalSoftwareWindow>,
    w: i32,
    h: i32,
    hits: &Hit,
) -> std::collections::BTreeMap<String, Bounds> {
    let mut map: std::collections::BTreeMap<String, Bounds> = std::collections::BTreeMap::new();
    let mut x = 0;
    while x < w {
        let mut y = 0;
        while y < h {
            if let Some(k) = click_kind(win, x as f32, y as f32, hits) {
                match map.get_mut(&k) {
                    Some(b) => b.add(x as f32, y as f32),
                    None => {
                        map.insert(k, Bounds::new(x as f32, y as f32));
                    }
                }
            }
            y += 3;
        }
        x += 3;
    }
    map
}

/// 断言命中区没有跑出自己那一列（P1-2 的回归锁）。
fn assert_hits_stay_in_columns(map: &std::collections::BTreeMap<String, Bounds>, w: f32) {
    let kinds: Vec<&String> = map.keys().collect();
    assert_eq!(
        kinds,
        vec!["cpu", "mem", "net"],
        "命中类型集合不符：{:?}",
        map
    );
    let left_end = left_col_end(w);
    let right_start = right_col_start(w);
    for kind in ["cpu", "mem"] {
        let b = map[kind];
        assert!(
            b.max_x <= left_end + 1.0,
            "{} 命中区越出左列：max_x={} 左列止于 {}（列宽回退到 30%？）",
            kind,
            b.max_x,
            left_end
        );
    }
    let net = map["net"];
    assert!(
        net.max_x < right_start - 4.0,
        "net 命中区压到右列：max_x={} 右列起点={}（中列宽度不足，数值溢出到列外）",
        net.max_x,
        right_start
    );
}

#[test]
fn overlay_card_only_number_clickable() {
    let window = setup();
    let (ui, hits, pauses) = make_overlay(360.0, 78.0);

    // 点标签（左列行首，CPU / 内存 文字）不应命中
    let n0 = hits.borrow().len();
    click(&window, 10.0, 18.0);
    click(&window, 10.0, 39.0);
    assert_eq!(hits.borrow().len(), n0, "标签区不应触发 resource-clicked");
    assert_eq!(*pauses.borrow(), 0, "标签点击不应触发暂停");

    // 全窗扫描：三个数字区都能命中，且命中矩形不越出自己的列
    let got = sweep(&window, 360, 78, &hits);
    eprintln!("card 命中包围盒: {:?}", got);
    assert_hits_stay_in_columns(&got, 360.0);

    // 暂停按钮（右侧垂直居中）→ toggle-pause，且不触发资源区
    let before_hits = hits.borrow().len();
    let before_pauses = *pauses.borrow();
    let before_paused = ui.get_paused();
    click(&window, 340.0, 39.0);
    assert_eq!(
        *pauses.borrow(),
        before_pauses + 1,
        "card 暂停区应触发一次 toggle-pause"
    );
    assert_eq!(
        ui.get_paused(),
        !before_paused,
        "暂停点击后 slint 侧 paused 属性应翻转（只在渲染层翻转而没同步属性是 P1-1 的成因）"
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
    let (ui, hits, pauses) = make_overlay(380.0, 78.0);
    let event_clicks = Rc::new(RefCell::new(0u32));
    let event_clicks_cb = event_clicks.clone();
    ui.on_event_count_clicked(move || {
        *event_clicks_cb.borrow_mut() += 1;
    });

    // 点标签（左列行首，CPU / 内存）不应命中
    let n0 = hits.borrow().len();
    click(&window, 16.0, 18.0);
    click(&window, 16.0, 39.0);
    assert_eq!(hits.borrow().len(), n0, "标签区不应触发 resource-clicked");

    let got = sweep(&window, 380, 78, &hits);
    eprintln!("bubble 命中包围盒: {:?}", got);
    assert_hits_stay_in_columns(&got, 380.0);

    // 暂停按钮在 bubble 下同样可点，且不与资源区混淆。
    // 全窗扫描本身会扫过暂停热区，故用点击前后的增量断言。
    let before_hits = hits.borrow().len();
    let before_pauses = *pauses.borrow();
    click(&window, 360.0, 39.0);
    assert_eq!(
        *pauses.borrow(),
        before_pauses + 1,
        "bubble 暂停区应触发一次 toggle-pause"
    );
    assert_eq!(
        hits.borrow().len(),
        before_hits,
        "bubble 暂停区点击不应触发 resource-clicked"
    );

    // 今日卡顿文字 → 打开分析，且不误触资源弹窗
    let before_events = *event_clicks.borrow();
    let before_hits = hits.borrow().len();
    click(&window, 140.0, 60.0);
    assert_eq!(
        *event_clicks.borrow(),
        before_events + 1,
        "今日卡顿区应触发一次 event-count-clicked"
    );
    assert_eq!(
        hits.borrow().len(),
        before_hits,
        "今日卡顿区点击不应触发 resource-clicked"
    );

    drop(ui);
}
