//! 临时验收工具：单独渲染「历史流量统计」窗口，供人工截图核对布局。
//!
//! 用法：`rtk cargo run -p find-stutter-ui --example traffic_snapshot -- 0`（列表视图）
//!       `rtk cargo run -p find-stutter-ui --example traffic_snapshot -- 1`（日历视图）
//!
//! 数据为手工构造的固定样本（真实数据链路已由 python 独立计算 + 单测覆盖），
//! 这里只验证渲染与布局。验收完成后删除，不入库。

use std::rc::Rc;

use find_stutter_ui::{CalCell, TrafficRow, TrafficStats};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

fn row(label: &str, up: &str, down: &str, total: &str, ratio: f32) -> TrafficRow {
    TrafficRow {
        label: SharedString::from(label),
        up: SharedString::from(up),
        down: SharedString::from(down),
        total: SharedString::from(total),
        ratio,
    }
}

fn cell(day: i32, level: i32, in_month: bool, is_today: bool, total: &str) -> CalCell {
    CalCell {
        day,
        level,
        in_month,
        is_today,
        total_text: SharedString::from(total),
    }
}

fn main() -> anyhow::Result<()> {
    let view: i32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let ui = TrafficStats::new()?;

    // 列表视图：15 行，ratio 覆盖 0 / 中等 / 满格，便于核对条形的画与不画。
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
    ui.set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
    ui.set_has_data(true);
    ui.set_summary_main(SharedString::from("共 15 条 · 累计 358.02 GB"));
    ui.set_summary_sub(SharedString::from("上传 91.60 GB · 下载 264.99 GB"));
    ui.set_stats_mode(1);
    ui.set_scale_mode(0);

    // 2026 年 11 月：1 号是周日，正好从日历第一行第一格开始，共 30 天，
    // 后面补 12 个「非本月」格（12 月 1~12 日）。
    let levels = [
        0, 1, 2, 1, 3, 4, 0, 2, 1, 1, 0, 0, 3, 2, 1, 0, 1, 2, 0, 1, 4, 2, 0, 1, 0, 1, 1, 2, 0, 1,
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
                day == 3, // 今天是 3 号，验证描边
                totals[idx],
            ));
        } else {
            cells.push(cell(day - 30, -1, false, false, ""));
        }
    }
    ui.set_cells(ModelRc::from(Rc::new(VecModel::from(cells))));
    ui.set_month_text(SharedString::from("2026 年 11 月"));
    ui.set_cal_summary_main(SharedString::from("本月总流量 356.59 GB"));
    ui.set_cal_summary_sub(SharedString::from("上传 91.60 GB · 下载 264.99 GB"));

    ui.set_view_index(view);
    ui.show()?;
    println!("窗口已显示，view={view}；按 Ctrl+C 或结束进程退出");
    slint::run_event_loop().unwrap();
    Ok(())
}
