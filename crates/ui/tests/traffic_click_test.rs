//! 悬浮窗「今日 ↑x ↓y」流量入口的点击热区回归测试。
//!
//! 写法照抄 `resource_click_test.rs`：`MinimalSoftwareWindow` + `RepaintBufferType::ReusedBuffer`
//! + 自建 `TestPlatform` + `dispatch_event` 模拟按下/抬起。之所以用离屏窗口而不是真实截图，
//! 是因为本机安全策略拦截屏幕抓取，而 Slint 的 software renderer 走的是同一条命中/布局管线。
//!
//! ## 坐标怎么来的
//!
//! slint 的 `Window` 没有 `absolute-position` 读取接口，`Overlay` 也没把四个热区的位置
//! 暴露成属性，所以在测试里**穷举扫描**：在候选矩形内按 3px 步长逐点点击，记录「这一次点击
//! 让某个计数器自增」的点，再取这些点的包围盒。扫描范围写在下面每个 `scan_points` 调用处。
//!
//! 这样断言不是「点空白也算通过」的软断言：一旦热区为空（入口不可点）或热区跑出自己那一列
//! （串台），断言会直接失败并打印实测包围盒。
//!
//! ## 覆盖的四个入口（互不串台）
//!
//! 1. 右列第三行「今日 ↑x ↓y」→ `traffic-clicked`（本窗口）
//! 2. 中列第三行「今日卡顿 N」→ `event-count-clicked`，不触发 traffic
//! 3. 右列第二行暂停按钮 → `toggle-pause`，不触发 traffic
//! 4. 左列 CPU 数值 → `resource-clicked("cpu")`，不触发 traffic
//!
//! 两组尺寸分别对应皮肤默认卡片（314×70）与 bubble（334×70）：两者右列宽度不同，
//! 热区换算容易在某一组里跑偏。

use std::cell::RefCell;
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::{ComponentHandle, SharedString};

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

/// 模拟一次完整的左键点击（按下 → 重绘 → 抬起 → 重绘）。
fn click(win: &Rc<MinimalSoftwareWindow>, x: f32, y: f32) {
    let pos = slint::LogicalPosition::new(x, y);
    let _ = win.dispatch_event(slint::platform::WindowEvent::PointerMoved { position: pos });
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

/// 各入口的触发计数（由测试注册的回调填充）。
struct Counters {
    traffic: Rc<RefCell<u32>>,
    events: Rc<RefCell<u32>>,
    pauses: Rc<RefCell<u32>>,
    resources: Rc<RefCell<Vec<String>>>,
}

/// 造一个按真实文案撑开的悬浮窗（文案量级会影响列宽与 ClickableValue 宽度）。
fn make_overlay(w: f32, h: f32) -> (find_stutter_ui::Overlay, Counters) {
    let ui = find_stutter_ui::Overlay::new().unwrap();
    ui.set_skin_width(w);
    ui.set_skin_height(h);
    ui.set_cpu_value(SharedString::from("23%"));
    ui.set_mem_value(SharedString::from("68%"));
    ui.set_gpu_value(SharedString::from("41%"));
    ui.set_net_send_value(SharedString::from("1.2 MB/s"));
    ui.set_net_recv_value(SharedString::from("386 KB/s"));
    ui.set_net_today_up(SharedString::from("↑ 439M"));
    ui.set_net_today_down(SharedString::from("↓ 2.7G"));
    ui.set_service_status(SharedString::from("● 运行中"));
    ui.set_last_heartbeat(SharedString::from("12:49:57"));
    ui.set_event_count(SharedString::from("今日卡顿 1"));
    ui.show().unwrap();

    let traffic = Rc::new(RefCell::new(0u32));
    let t = traffic.clone();
    ui.on_traffic_clicked(move || *t.borrow_mut() += 1);

    let events = Rc::new(RefCell::new(0u32));
    let e = events.clone();
    ui.on_event_count_clicked(move || *e.borrow_mut() += 1);

    let pauses = Rc::new(RefCell::new(0u32));
    let p = pauses.clone();
    ui.on_toggle_pause(move || *p.borrow_mut() += 1);

    let resources = Rc::new(RefCell::new(Vec::<String>::new()));
    let r = resources.clone();
    ui.on_resource_clicked(move |kind: SharedString| r.borrow_mut().push(kind.to_string()));

    (
        ui,
        Counters {
            traffic,
            events,
            pauses,
            resources,
        },
    )
}

/// 在 `[x0, x1) × [y0, y1)` 内按 `step` 逐点点击，返回「本次点击让 `count` 自增」的点。
fn scan_points(
    win: &Rc<MinimalSoftwareWindow>,
    (x0, x1, y0, y1): (f32, f32, f32, f32),
    step: f32,
    count: &Rc<RefCell<u32>>,
) -> Vec<(f32, f32)> {
    let mut pts = Vec::new();
    let mut y = y0;
    while y < y1 {
        let mut x = x0;
        while x < x1 {
            let before = *count.borrow();
            click(win, x, y);
            if *count.borrow() > before {
                pts.push((x, y));
            }
            x += step;
        }
        y += step;
    }
    pts
}

/// 在矩形内扫描资源区命中的点，返回 (x, y, 资源维度)。
fn scan_resources(
    win: &Rc<MinimalSoftwareWindow>,
    (x0, x1, y0, y1): (f32, f32, f32, f32),
    step: f32,
    kinds: &Rc<RefCell<Vec<String>>>,
) -> Vec<(f32, f32, String)> {
    let mut pts = Vec::new();
    let mut y = y0;
    while y < y1 {
        let mut x = x0;
        while x < x1 {
            let before = kinds.borrow().len();
            click(win, x, y);
            let after = kinds.borrow().len();
            if after > before {
                let kind = kinds.borrow()[after - 1].clone();
                pts.push((x, y, kind));
            }
            x += step;
        }
        y += step;
    }
    pts
}

/// 命中点包围盒 (min_x, max_x, min_y, max_y)；空集合返回 None。
fn bbox(pts: &[(f32, f32)]) -> Option<(f32, f32, f32, f32)> {
    let first = *pts.first()?;
    let mut b = (first.0, first.0, first.1, first.1);
    for (x, y) in pts {
        b.0 = b.0.min(*x);
        b.1 = b.1.max(*x);
        b.2 = b.2.min(*y);
        b.3 = b.3.max(*y);
    }
    Some(b)
}

/// 包围盒中心（取整到 0.5，避免落在亚像素边界上）。
fn center(b: (f32, f32, f32, f32)) -> (f32, f32) {
    (((b.0 + b.1) / 2.0 * 2.0).round() / 2.0, ((b.2 + b.3) / 2.0 * 2.0).round() / 2.0)
}

/// 两个包围盒是否相交（用于「互不串台」断言）。
fn overlaps(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    a.0 <= b.1 && b.0 <= a.1 && a.2 <= b.3 && b.2 <= a.3
}

/// 两组尺寸共用的四条断言。
fn assert_all(w: f32, h: f32, tag: &str) {
    let win = setup();
    let (ui, c) = make_overlay(w, h);

    // 三行网格在 62px 内垂直居中：row1 = [top, top+18]、row2 = +22、row3 = +44
    // （spacing 4px）。这里只做粗校验，真正的落点由扫描得出。
    let row3_y = (h - 62.0) / 2.0 + 44.0;

    // ==== 断言 1：右列第三行「今日 ↑x ↓y」→ traffic-clicked ====
    // 扫描范围：右半窗（x ∈ [55% w, w)）× 整高，步长 3px。
    // 右列是 38% 宽、贴窗口右侧，55% 起点足以覆盖且不会扫到中列的「今日卡顿」。
    let pts = scan_points(&win, (w * 0.55, w, 0.0, h), 3.0, &c.traffic);
    let tb = bbox(&pts).unwrap_or_else(|| {
        panic!("[{tag}] 右列第三行未扫到任何 traffic 命中点：入口热区缺失或不可点")
    });
    eprintln!(
        "[{tag}] traffic 命中 {} 点，bbox x=[{:.1},{:.1}] y=[{:.1},{:.1}]（右列第三行 y≈{:.1}）",
        pts.len(),
        tb.0,
        tb.1,
        tb.2,
        tb.3,
        row3_y
    );
    assert!(
        tb.2 >= row3_y - 8.0 && tb.3 <= row3_y + 20.0,
        "[{tag}] traffic 热区应落在右列第三行，实测 y=[{:.1},{:.1}]，期望起点≈{:.1}",
        tb.2,
        tb.3,
        row3_y
    );
    assert!(
        tb.0 >= w * 0.5,
        "[{tag}] traffic 热区跑出右列：min_x={:.1}，右列起点应 > {:.1}",
        tb.0,
        w * 0.5
    );

    let (tx, ty) = center(tb);
    let before = *c.traffic.borrow();
    click(&win, tx, ty);
    assert_eq!(
        *c.traffic.borrow(),
        before + 1,
        "[{tag}] 点击 ({tx}, {ty})（traffic 热区中心）应触发一次 traffic-clicked"
    );

    // ==== 断言 2：中列第三行「今日卡顿 N」→ event-count-clicked，不串台 ====
    // 扫描范围：中列大致区间 x ∈ [22%, 55%) × 整高，步长 3px。
    let ev_pts = scan_points(&win, (w * 0.22, w * 0.55, 0.0, h), 3.0, &c.events);
    let eb = bbox(&ev_pts)
        .unwrap_or_else(|| panic!("[{tag}] 中列第三行未扫到任何 event-count 命中点"));
    eprintln!(
        "[{tag}] event 命中 {} 点，bbox x=[{:.1},{:.1}] y=[{:.1},{:.1}]",
        ev_pts.len(),
        eb.0,
        eb.1,
        eb.2,
        eb.3
    );
    assert!(
        !overlaps(tb, eb),
        "[{tag}] traffic 与 event-count 热区重叠：traffic={tb:?} event={eb:?}"
    );
    let (ex, ey) = center(eb);
    let (ev_before, tr_before) = (*c.events.borrow(), *c.traffic.borrow());
    click(&win, ex, ey);
    assert_eq!(
        *c.events.borrow(),
        ev_before + 1,
        "[{tag}] 点击 ({ex}, {ey})（今日卡顿热区中心）应触发一次 event-count-clicked"
    );
    assert_eq!(
        *c.traffic.borrow(),
        tr_before,
        "[{tag}] 点「今日卡顿」不应触发 traffic-clicked"
    );

    // ==== 断言 3：右列第二行暂停按钮 → toggle-pause，不触发 traffic ====
    // 扫描范围：右列 x ∈ [55% w, w) × 上半窗 y ∈ [0, 46)，步长 3px。
    // 只扫上半窗，天然避开第三行的流量入口（第三行起点 y≈48）。
    let pause_pts = scan_points(&win, (w * 0.55, w, 0.0, 46.0), 3.0, &c.pauses);
    let pb = bbox(&pause_pts)
        .unwrap_or_else(|| panic!("[{tag}] 右列第二行未扫到任何暂停命中点"));
    eprintln!(
        "[{tag}] pause 命中 {} 点，bbox x=[{:.1},{:.1}] y=[{:.1},{:.1}]",
        pause_pts.len(),
        pb.0,
        pb.1,
        pb.2,
        pb.3
    );
    assert!(
        !overlaps(tb, pb),
        "[{tag}] 暂停热区与 traffic 热区重叠：pause={pb:?} traffic={tb:?}"
    );
    let (px, py) = center(pb);
    let (p_before, tr_before) = (*c.pauses.borrow(), *c.traffic.borrow());
    click(&win, px, py);
    assert_eq!(
        *c.pauses.borrow(),
        p_before + 1,
        "[{tag}] 点击 ({px}, {py})（暂停按钮中心）应触发一次 toggle-pause"
    );
    assert_eq!(
        *c.traffic.borrow(),
        tr_before,
        "[{tag}] 点暂停按钮不应触发 traffic-clicked"
    );

    // ==== 断言 4：左列 CPU 数值 → resource-clicked("cpu")，不触发 traffic ====
    // 扫描范围：左列 x ∈ [0, 30% w) × 第一行 y ∈ [0, 22)，步长 3px。
    let res_pts = scan_resources(&win, (0.0, w * 0.3, 0.0, 22.0), 3.0, &c.resources);
    let cpu_pts: Vec<(f32, f32)> = res_pts
        .iter()
        .filter(|(_, _, k)| k == "cpu")
        .map(|(x, y, _)| (*x, *y))
        .collect();
    let cb = bbox(&cpu_pts)
        .unwrap_or_else(|| panic!("[{tag}] 左列第一行未扫到任何 cpu 命中点：{res_pts:?}"));
    eprintln!(
        "[{tag}] cpu 命中 {} 点，bbox x=[{:.1},{:.1}] y=[{:.1},{:.1}]",
        cpu_pts.len(),
        cb.0,
        cb.1,
        cb.2,
        cb.3
    );
    assert!(
        !overlaps(tb, cb),
        "[{tag}] CPU 热区与 traffic 热区重叠：cpu={cb:?} traffic={tb:?}"
    );
    let (cx, cy) = center(cb);
    let res_before = c.resources.borrow().len();
    let tr_before = *c.traffic.borrow();
    click(&win, cx, cy);
    assert_eq!(
        c.resources.borrow().len(),
        res_before + 1,
        "[{tag}] 点击 ({cx}, {cy})（CPU 数值中心）应触发一次 resource-clicked"
    );
    assert_eq!(
        c.resources.borrow().last().map(|s| s.as_str()),
        Some("cpu"),
        "[{tag}] 左列第一行应命中 cpu 而非其它维度"
    );
    assert_eq!(
        *c.traffic.borrow(),
        tr_before,
        "[{tag}] 点 CPU 数值不应触发 traffic-clicked"
    );

    drop(ui);
}

/// 默认卡片尺寸（skin.toml default：314×70）。
#[test]
fn traffic_entry_hit_default_card() {
    assert_all(314.0, 70.0, "card 314x70");
}

/// bubble 尺寸（skin.toml bubble：334×70）。
#[test]
fn traffic_entry_hit_bubble() {
    assert_all(334.0, 70.0, "bubble 334x70");
}

