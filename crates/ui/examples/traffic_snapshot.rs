//! 临时验收工具：用 Slint 软件渲染器**离屏**渲染「历史流量统计」窗口并写出 BMP。
//!
//! 为什么不用真实窗口 + snap 截图：no-frame 透明窗口在部分合成状态下 PrintWindow
//! 会拿到全黑，回退 BitBlt 又会抓到遮挡物（实测踩坑）；离屏渲染走同一条渲染管线，
//! 结果即用户看到的像素，且不受桌面遮挡影响。
//!
//! 用法：`rtk cargo run -p find-stutter-ui --example traffic_snapshot -- 0|1|2`
//!       0=列表视图 1=日历视图 2=空状态
//! 产物：`target/traffic-snapshot-<view>.bmp`
//!
//! 数据为手工构造的固定样本（真实数据链路已由 python 独立计算 + 单测覆盖）。
//! 验收完成后可删除，不入库。

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
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(pixel_bytes as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..h).rev() {
        for x in 0..w {
            let p = &px[(y * w + x) as usize];
            let a = p.a as u32;
            let blend =
                |s: u8, b: u8| -> u8 { (s as u32 + b as u32 * (255 - a) / 255).min(255) as u8 };
            out.push(blend(p.b, bg.2));
            out.push(blend(p.g, bg.1));
            out.push(blend(p.r, bg.0));
            out.push(0xff);
        }
    }
    std::fs::write(path, out).expect("写入 BMP 失败");
}

fn row(label: &str, up: &str, down: &str, total: &str, ratio: f32) -> find_stutter_ui::TrafficRow {
    find_stutter_ui::TrafficRow {
        label: label.into(),
        up: up.into(),
        down: down.into(),
        total: total.into(),
        ratio,
    }
}

fn cell(day: i32, level: i32, in_month: bool, is_today: bool, total: &str) -> find_stutter_ui::CalCell {
    find_stutter_ui::CalCell {
        day,
        level,
        in_month,
        is_today,
        total_text: total.into(),
    }
}

fn main() -> anyhow::Result<()> {
    let view: i32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    slint::platform::set_platform(Box::new(TestPlatform)).ok();
    let window = WINDOW.with(|x| x.clone());

    let ui = find_stutter_ui::TrafficStats::new()?;

    if view == 2 {
        ui.set_has_data(false);
        ui.set_view_index(0);
    } else {
        // 列表视图：15 行，ratio 覆盖 0 / 中等 / 满格，便于核对条形的画与不画
        let rows = vec![
            row("2026/第 45 周", "590 KB", "14.01 MB", "14.58 MB", 0.05),
            row("2026/第 44 周", "7.38 GB", "17.16 GB", "24.54 GB", 0.51),
            row("2026/第 43 周", "0 B", "0 B", "0 B", 0.0),
            row("2026/第 42 周", "5.12 GB", "20.18 GB", "25.30 GB", 0.53),
            row("2026/第 41 周", "7.09 GB", "20.67 GB", "27.76 GB", 0.58),
            row("2026/第 40 周", "11.72 GB", "22.22 GB", "33.94 GB", 0.71),
            row("2026/第 39 周", "4.58 GB", "21.15 GB", "25.73 GB", 0.54),
            row("2026/第 38 周", "5.52 GB", "34.55 GB", "40.06 GB", 0.84),
            row("2026/第 37 周", "5.19 GB", "24.06 GB", "29.25 GB", 0.61),
            row("2026/第 36 周", "4.77 GB", "30.51 GB", "35.28 GB", 0.74),
            row("2026/第 35 周", "12.15 GB", "35.67 GB", "47.82 GB", 1.0),
            row("2026/第 34 周", "17.65 GB", "18.94 GB", "36.59 GB", 0.77),
            row("2026/第 33 周", "6.02 GB", "19.33 GB", "25.35 GB", 0.53),
            row("2026/第 32 周", "9.84 GB", "23.47 GB", "33.31 GB", 0.70),
            row("2026/第 31 周", "13.21 GB", "26.82 GB", "40.03 GB", 0.84),
        ];
        ui.set_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
        ui.set_has_data(true);
        ui.set_summary_main("共 15 条 · 累计 358.02 GB".into());
        ui.set_summary_sub("上传 91.60 GB · 下载 264.99 GB".into());
        ui.set_stats_mode(1);
        ui.set_scale_mode(0);

        // 2026 年 11 月：1 号是周日，正好从日历第一行第一格开始，共 30 天
        let levels = [
            0, 1, 2, 1, 3, 4, 0, 2, 1, 1, 0, 0, 3, 2, 1, 0, 1, 2, 0, 1, 4, 2, 0, 1, 0, 1, 1, 2, 0,
            1,
        ];
        let totals = [
            "", "312M", "1.4G", "268M", "2.6G", "3.2G", "84M", "1.1G", "590M", "470M", "128M", "",
            "2.2G", "1.7G", "780M", "", "640M", "1.3G", "96M", "520M", "3.0G", "1.9G", "", "760M",
            "", "880M", "1.6G", "2.4G", "", "1.2G",
        ];
        let mut cells = Vec::with_capacity(42);
        for i in 0..42 {
            let day = i + 1;
            if day <= 30 {
                let idx = day as usize - 1;
                cells.push(cell(
                    day,
                    levels[idx],
                    true,
                    day == 3,
                    totals[idx],
                ));
            } else {
                cells.push(cell(day - 30, -1, false, false, ""));
            }
        }
        ui.set_cells(slint::ModelRc::from(Rc::new(slint::VecModel::from(cells))));
        ui.set_month_text("2026 年 11 月".into());
        ui.set_cal_summary_main("本月总流量 356.59 GB".into());
        ui.set_cal_summary_sub("上传 91.60 GB · 下载 264.99 GB".into());
        ui.set_view_index(view);
    }

    let (w, h) = (880u32, 620u32);
    window.set_size(PhysicalSize::new(w, h));
    ui.show().unwrap();
    window.request_redraw();
    window.draw_if_needed(|_| {});

    let snap = ui.window().take_snapshot().expect("take_snapshot 返回 None");
    let out = format!("target/traffic-snapshot-{view}.bmp");
    write_bmp(&out, snap.width(), snap.height(), snap.as_slice(), (0xf0, 0xf0, 0xf0));
    println!("已写出: {out} ({}x{})", snap.width(), snap.height());
    Ok(())
}
