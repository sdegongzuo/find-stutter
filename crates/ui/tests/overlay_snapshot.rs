//! 悬浮窗「离屏渲染快照」：用 Slint 的 software renderer 把 Overlay 真实渲染成 BMP。
//!
//! ## 为什么需要
//!
//! 本机安全策略拦截了 `Add-Type`（动态编译 C# 调 GDI `CopyFromScreen`）与所有屏幕抓取
//! API，`ffmpeg` / ImageMagick 也没装 —— 也就是说**无法对真实桌面截图**。
//! 但 UI 验证不必依赖桌面：Slint 的 `MinimalSoftwareWindow` 走的是同一条渲染管线，
//! 渲染结果即用户看到的像素（差别只在窗口合成/透明边缘）。
//!
//! 产物：`target/overlay-snapshot.bmp`（RGBA 快照合成到浅灰底，便于看清圆角与边框）。
//!
//! ## 注意
//! 这里只注入「指标文本」，皮肤走 .slint 里的默认值（浅色 card）。
//! 真实皮肤配色由 `skin.toml` 经 `apply_metrics` 注入，不在本测试覆盖范围内。

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize};
use std::rc::Rc;

thread_local! {
    static WINDOW: Rc<MinimalSoftwareWindow> =
        MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
}

struct TestPlatform;

impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(WINDOW.with(|x| x.clone()))
    }
}

/// RGBA（预乘 alpha）→ 24 位 BMP，先按给定底色合成，避免透明区域全黑。
fn write_bmp(path: &str, w: u32, h: u32, px: &[slint::Rgba8Pixel], bg: (u8, u8, u8)) {
    let row_bytes = (w * 4) as usize;
    let pixel_bytes = row_bytes * h as usize;
    let file_size = 54 + pixel_bytes;
    let mut out = Vec::with_capacity(file_size);

    // BITMAPFILEHEADER
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    // BITMAPINFOHEADER
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes()); // 正数 = 自底向上
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(pixel_bytes as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    for y in (0..h).rev() {
        for x in 0..w {
            let p = &px[(y * w + x) as usize];
            let a = p.a as u32;
            // 预乘 alpha 合成：out = src + bg * (1 - a)
            let blend = |s: u8, b: u8| -> u8 { (s as u32 + b as u32 * (255 - a) / 255).min(255) as u8 };
            out.push(blend(p.b, bg.2)); // B
            out.push(blend(p.g, bg.1)); // G
            out.push(blend(p.r, bg.0)); // R
            out.push(0xff);
        }
    }
    std::fs::write(path, out).expect("写入 BMP 失败");
}

#[test]
fn render_overlay_snapshot() {
    slint::platform::set_platform(Box::new(TestPlatform)).ok();
    let window = WINDOW.with(|x| x.clone());

    let ui = find_stutter_ui::Overlay::new().unwrap();
    // 造一组接近真实的数据，方便看清列宽与对齐是否正常
    ui.set_cpu_value("23%".into());
    ui.set_mem_value("68%".into());
    ui.set_gpu_value("41%".into());
    ui.set_net_send_value("1.2 MB/s".into());
    ui.set_net_recv_value("386 KB/s".into());
    ui.set_net_today_up("↑ 4.1 GB".into());
    ui.set_net_today_down("↓ 12.6 GB".into());
    ui.set_event_count("今日卡顿 2".into());
    ui.set_last_heartbeat("最近心跳 14:52:31".into());
    ui.set_service_status("● 服务运行中".into());

    let (w, h) = (380u32, 100u32);
    window.set_size(PhysicalSize::new(w, h));
    ui.show().unwrap();
    window.request_redraw();
    window.draw_if_needed(|_| {});

    let snap = ui.window().take_snapshot().expect("take_snapshot 返回 None");
    assert_eq!(snap.width(), w, "快照宽度应与窗口一致");
    assert_eq!(snap.height(), h, "快照高度应与窗口一致");

    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/overlay-snapshot.bmp");
    write_bmp(
        out.to_str().unwrap(),
        snap.width(),
        snap.height(),
        snap.as_slice(),
        (0xf0, 0xf0, 0xf0),
    );
    assert!(out.exists(), "快照未写出: {}", out.display());
    println!("快照已生成: {}", out.display());
}
