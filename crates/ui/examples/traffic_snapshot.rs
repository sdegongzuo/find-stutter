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

fn main() -> anyhow::Result<()> {
    let view: i32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let ui = TrafficStats::new()?;

    let rows = vec![
        row("2026/第 45 周", "590 KB", "14.01 MB", "14.58 MB", 0.02),
        row("2026/第 44 周", "7.38 GB", "17.16 GB", "24.54 GB", 0.62),
        row("2026/第 43 周", "6.87 GB", "17.87 GB", "24.74 GB", 0.63),
        row("2026/第 42 周", "5.12 GB", "20.18 GB", "25.30 GB", 0.64),
        row("2026/第 41 周", "7.09 GB", "20.67 GB", "27.76 GB", 0.67),
        row("2026/第 40 周", "11.72 GB", "22.22 GB", "33.94 GB", 0.73),
        row("2026/第 39 周", "4.58 GB", "21.15 GB", "25.73 GB", 0.65),
        row("2026/第 38 周", "5.52 GB", "34.55 GB", "40.06 GB", 0.79),
        row("2026/第 37 周", "5.19 GB", "24.06 GB", "29.25 GB", 0.70),
        row("2026/第 36 周", "4.77 GB", "30.51 GB", "35.28 GB", 0.75),
        row("2026/第 35 周", "12.15 GB", "35.67 GB", "47.82 GB", 0.86),
        row("2026/第 34 周", "17.65 GB", "18.94 GB", "36.59 GB", 0.77),
    ];
    ui.set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
    ui.set_has_data(true);
    ui.set_summary_text(SharedString::from(
        "共 12 条 · 累计 356.59 GB（上传 91.60 GB / 下载 264.99 GB）",
    ));
    ui.set_stats_mode(1);
    ui.set_scale_mode(0);

    // 2026 年 11 月：1 号是周日，往前补 0 格，共 30 天
    let levels = [
        0, 1, 2, 1, 3, 4, 0, 2, 1, 1, 0, 0, 3, 2, 1, 0, 1, 2, 0, 1, 4, 2, 0, 1, 0, 1, 1, 2, 0, 1,
    ];
    let mut cells = Vec::with_capacity(42);
    for i in 0..42 {
        let day = i as i32 - 0 + 1;
        if day <= 30 {
            cells.push(CalCell {
                day,
                level: levels[(day - 1) as usize],
                in_month: true,
            });
        } else {
            cells.push(CalCell {
                day: day - 30,
                level: -1,
                in_month: false,
            });
        }
    }
    ui.set_cells(ModelRc::from(Rc::new(VecModel::from(cells))));
    ui.set_month_text(SharedString::from("2026 年 11 月"));
    ui.set_cal_summary_text(SharedString::from(
        "当前月总流量：14.58 MB（上传 590 KB，下载 14.01 MB）",
    ));

    ui.set_view_index(view);
    ui.show()?;
    println!("窗口已显示，view={view}；按 Ctrl+C 或结束进程退出");
    slint::run_event_loop().unwrap();
    Ok(())
}
