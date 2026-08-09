//! 趋势图渲染（PRD §6.2 方案 A：plotters → slint::Image）。
//!
//! 在后台线程把趋势图（次数堆叠柱状 + severity 配色）画到内存 RGBA 缓冲，
//! 转 `slint::SharedImageBuffer` 后经回调推回 UI 线程，由 UI 线程转
//! `slint::Image` 设置。整个渲染不触碰 slint 窗口，可安全在后台线程执行
//! （PRD §6.3：趋势图渲染较重，放后台避免 UI 冻结）。

use slint::{Rgba8Pixel, SharedPixelBuffer};

use crate::analytics::{CauseTypeCount, ResourceData, ResourcePoint, ResourceView, TrendPoint};

/// 把 plotters 的 RGB（3 字节/像素）缓冲转成 slint 可用的 RGBA8 缓冲（alpha=255 不透明）。
///
/// plotters 的 `BitMapBackend::with_buffer` 按 RGB 解释缓冲，渲染完再补 alpha=255
/// 交给 slint。趋势图/饼图/资源图三处都需要这一步，统一收敛到本函数避免重复。
fn rgb_to_rgba8(rgb: &[u8], w: u32, h: u32) -> SharedPixelBuffer<Rgba8Pixel> {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    {
        let slice = buffer.make_mut_slice();
        for i in 0..(w * h) as usize {
            slice[i] = Rgba8Pixel {
                r: rgb[i * 3],
                g: rgb[i * 3 + 1],
                b: rgb[i * 3 + 2],
                a: 0xFF,
            };
        }
    }
    buffer
}

/// 渲染趋势图。
///
/// - 成功：回调收到 `Some(SharedPixelBuffer<Rgba8Pixel>)`（已渲染的位图）。
/// - 失败/无数据：回调收到 `None`（UI 端保持占位文本）。
///
/// `on_done` 在后台线程调用（调用方负责 `invoke_from_event_loop` 推回 UI），
/// 因此本函数本身不触碰 slint 窗口；`SharedPixelBuffer` 是 `Send`，
/// 可安全跨线程传回 UI 线程后转 `slint::Image::from_rgba8`。
pub(crate) fn render_trend_chart<F>(trend: &[TrendPoint], w: u32, h: u32, on_done: F)
where
    F: FnOnce(Option<SharedPixelBuffer<Rgba8Pixel>>) + Send + 'static,
{
    // 空数据 → 无图（UI 端显示「暂无趋势数据」占位）
    if trend.is_empty() {
        on_done(None);
        return;
    }

    let width = w.max(1);
    let height = h.max(1);
    // plotters 的 BitMapBackend 缓冲按 RGB（3 字节/像素）解释，
    // 渲染完再转 RGBA8（alpha=255）交给 slint。
    let mut rgb: Vec<u8> = vec![0xFF; (width * height * 3) as usize];

    {
        use plotters::prelude::*;
        let root = BitMapBackend::with_buffer(&mut rgb, (width, height)).into_drawing_area();
        root.fill(&WHITE).ok();

        let n = trend.len();
        let max_count = trend
            .iter()
            .map(|p| p.count as f64)
            .fold(0.0, f64::max)
            .max(1.0);

        let mut chart = ChartBuilder::on(&root)
            .caption("卡顿次数趋势（按本地时区）", ("sans-serif", 16))
            .margin(8)
            .x_label_area_size(38)
            .y_label_area_size(46)
            .build_cartesian_2d(0f64..n as f64, 0f64..(max_count * 1.1))
            .unwrap();

        chart
            .configure_mesh()
            .disable_x_mesh()
            .x_labels(n.min(12) as usize)
            .x_label_formatter(&|x| {
                let i = x.round() as usize;
                trend
                    .get(i)
                    .map(|p| short_label(&p.bucket))
                    .unwrap_or_default()
                    .to_string()
            })
            .y_label_formatter(&|y| format!("{:.0}", y))
            .draw()
            .ok();

        // severity 配色：minor 浅蓝 / major 橙 / critical 红（堆叠）
        for (i, p) in trend.iter().enumerate() {
            let x0 = i as f64;
            let x1 = x0 + 0.7;
            let y_minor = p.minor as f64;
            let y_major = y_minor + p.major as f64;
            let y_crit = y_major + p.critical as f64;

            let _ = chart.draw_series(std::iter::once(Rectangle::new(
                [(x0, 0.0), (x1, y_minor)],
                RGBColor(0x9e, 0xc9, 0xe8).filled(),
            )));
            let _ = chart.draw_series(std::iter::once(Rectangle::new(
                [(x0, y_minor), (x1, y_major)],
                RGBColor(0xf0, 0xa8, 0x30).filled(),
            )));
            let _ = chart.draw_series(std::iter::once(Rectangle::new(
                [(x0, y_major), (x1, y_crit)],
                RGBColor(0xc4, 0x4c, 0x4c).filled(),
            )));
        }

        root.present().ok();
    }

    // RGB → RGBA8（alpha=255 不透明）
    let buffer = rgb_to_rgba8(&rgb, width, height);

    on_done(Some(buffer));
}

/// 渲染卡顿类型占比饼图（PRD §6.4 / M3 F4）。
///
/// 与 `render_trend_chart` 同范式：后台线程把 plotters 饼图（含图例）画到内存
/// RGBA 缓冲 → `SharedPixelBuffer` → 经回调推回 UI 线程转 `slint::Image`。
/// 整个渲染不触碰 slint 窗口，可安全在后台线程执行。
///
/// - 成功：回调收到 `Some(SharedPixelBuffer<Rgba8Pixel>)`。
/// - 失败/无数据（types 为空）：回调收到 `None`（UI 端保持占位文本）。
pub(crate) fn render_cause_pie<F>(types: &[CauseTypeCount], w: u32, h: u32, on_done: F)
where
    F: FnOnce(Option<SharedPixelBuffer<Rgba8Pixel>>) + Send + 'static,
{
    // 空数据 → 无图（UI 端显示「暂无类型数据」占位）
    if types.is_empty() {
        on_done(None);
        return;
    }

    let width = w.max(1);
    let height = h.max(1);
    let mut rgb: Vec<u8> = vec![0xFF; (width * height * 3) as usize];

    {
        use plotters::prelude::*;

        let root = BitMapBackend::with_buffer(&mut rgb, (width, height)).into_drawing_area();
        root.fill(&WHITE).ok();

        // 调色板：固定 8 色，与 detector 文案种类无关（最多约 8 类）
        let palette: [RGBColor; 8] = [
            RGBColor(0xc4, 0x4c, 0x4c), // 红
            RGBColor(0xf0, 0xa8, 0x30), // 橙
            RGBColor(0x4c, 0x9a, 0xc4), // 蓝
            RGBColor(0x6a, 0xb1, 0x4c), // 绿
            RGBColor(0x9e, 0xc9, 0xe8), // 浅蓝
            RGBColor(0xb0, 0x7c, 0xc4), // 紫
            RGBColor(0xc4, 0x8a, 0x4c), // 棕
            RGBColor(0x88, 0x88, 0x88), // 灰（其他）
        ];

        // 饼图居左，图例居右
        let cx = (width as i32) * 3 / 8;
        let cy = (height as i32) / 2;
        let radius = ((height / 3).min(width / 6)).max(10) as f64;
        let center = (cx, cy);

        let sizes: Vec<f64> = types.iter().map(|t| t.count as f64).collect();
        let colors: Vec<RGBColor> = types
            .iter()
            .enumerate()
            .map(|(i, _)| palette[i % palette.len()])
            .collect();
        // 饼块内只画百分比，类型名 + 次数放到右侧图例（避免饼外标签拥挤重叠）
        let labels: Vec<String> = vec![String::new(); types.len()];

        let mut pie = Pie::new(&center, &radius, &sizes, &colors, &labels);
        pie.percentages(("sans-serif", 12).into_font().color(&BLACK));
        root.draw(&pie).ok();

        // 图例：右侧逐行（色块 + 文案 + 次数）
        let lx = (width as i32) * 5 / 8;
        let line_h = 22i32;
        let mut ly = (height as i32) / 2 - (types.len() as i32) * line_h / 2;
        for (i, t) in types.iter().enumerate() {
            let color = palette[i % palette.len()];
            root.draw(&Rectangle::new(
                [(lx, ly), (lx + 12, ly + 12)],
                color.filled(),
            ))
            .ok();
            root.draw_text(
                &format!("{}  {}次", t.cause_type, t.count),
                &("sans-serif", 13).into_text_style(&root).color(&BLACK),
                (lx + 18, ly),
            )
            .ok();
            ly += line_h;
        }

        root.present().ok();
    }

    // RGB → RGBA8（alpha=255 不透明）
    let buffer = rgb_to_rgba8(&rgb, width, height);

    on_done(Some(buffer));
}

/// 把分桶键缩成坐标轴短标签（兼容三种粒度，PRD §4 F1 / E11）：
/// - 天粒度键 `YYYY-MM-DD`       → `MM-DD`
/// - 小时粒度键 `YYYY-MM-DD HH:00` → `HH:00`
/// - 15 分钟粒度键 `YYYY-MM-DD HH:30`（或 `HH:15`/`HH:45`）→ `HH:MM`
fn short_label(bucket: &str) -> String {
    let parts: Vec<&str> = bucket.split(' ').collect();
    match parts.as_slice() {
        // 仅日期（天粒度）：YYYY-MM-DD → MM-DD
        [date] => {
            let d: Vec<&str> = date.split('-').collect();
            if d.len() == 3 {
                format!("{}-{}", d[1], d[2])
            } else {
                date.to_string()
            }
        }
        // 日期+时间（小时 / 15 分钟粒度）：取 `HH:MM` 部分
        [_, hm] => hm.to_string(),
        // 其它（异常格式）原样返回，便于排查
        _ => bucket.to_string(),
    }
}

/// 渲染系统资源关联图（PRD F3 / M4）。
///
/// 与趋势图/饼图同范式：后台线程把 plotters 图（双 Y 轴）画到内存 RGBA 缓冲 →
/// `SharedPixelBuffer` → 经回调推回 UI 线程转 `slint::Image`。
///
/// 内容：
/// - 左轴（0-100）：CPU% / 内存% 绘 min–max 浅色带（Polygon，同色调低 alpha）+ avg 实线，
///   GPU%（可选，仅 avg 实线，Nullable 时跳过）；min–max 带保留尖峰，避免只画 avg 被抹平
///   （PRD §6.3/§8 要求看出与卡顿尖峰的对应）。
/// - 右轴（磁盘 B/s 自适应量程）：磁盘读 / 写 B/s 同样绘 min–max 浅色带 + avg 实线
///   （`ResourcePoint` 含 disk_read/write 的 min/max/avg，PRD §6.3「每像素桶取 min/max/avg」）；
///   量纲差异用双轴处理（详见 `ResourceData` 注释；不再归一到左轴以免 % 曲线被淹没）。
/// - 卡顿事件竖线：在事件桶位置画浅红竖线，直观看卡顿是否对齐资源尖峰。
/// - X 轴域用「完整桶数」（0..bucket_count），与 `data.points.x` / `data.event_x` 真实桶序号对齐。
/// - `view`：高级模式可选指标 + 对数轴（PRD §4 F3）：仅绘制 `view` 中启用的系列；
///   磁盘读/写 B/s 在 `view.log_disk` 为真时改用对数归一（仍落在左轴 0-100 区，
///   右轴 formatter 还原 B/s 不变），使数量级悬殊的磁盘尖峰可见。
///
/// - 成功：回调收到 `Some(...)`；无采样点（points 为空）→ `None`（UI 占位）。
pub(crate) fn render_resource_chart<F>(
    data: &ResourceData,
    w: u32,
    h: u32,
    view: &ResourceView,
    on_done: F,
) where
    F: FnOnce(Option<SharedPixelBuffer<Rgba8Pixel>>) + Send + 'static,
{
    if data.points.is_empty() {
        on_done(None);
        return;
    }

    let width = w.max(1);
    let height = h.max(1);
    let mut rgb: Vec<u8> = vec![0xFF; (width * height * 3) as usize];

    {
        use plotters::prelude::*;
        let root = BitMapBackend::with_buffer(&mut rgb, (width, height)).into_drawing_area();
        root.fill(&WHITE).ok();

        // X 轴域必须用「完整桶数」而非「非空桶数」：data.points.x 与 data.event_x
        // 都是真实桶序号（0..bucket_count-1），只有用完整桶数做域，曲线与卡顿竖线才对齐，
        // 否则（旧实现用非空桶数 n）曲线会被压缩、竖线错位到错误位置。
        // 桶数 = 覆盖整段需 floor(span/bucket_secs)+1（含起止桶，故 +1）
        let bucket_count: f64 =
            (((data.span_secs + data.bucket_secs - 1) / data.bucket_secs) as f64) + 1.0;
        let max_disk = data.max_disk();
        let has_gpu = view.gpu && data.points.iter().any(|p| p.gpu_avg.is_some());

        // 双轴处理量纲差异（PRD F3）：左轴 0-100 画 CPU%/内存%/GPU%；
        // 磁盘 B/s 与 % 差几个数量级，单独放右轴。为兼容 plotters 0.3.7 的 API，
        // 两条轴坐标都取 0-100，磁盘值按 max_disk 归一到 0-100 后画在左轴坐标区，
        // 右轴标签由 formatter 把 0-100 还原为 B/s（见 fmt_bytes）。这样既不淹没
        // % 曲线，又能在同一像素区叠出磁盘曲线。
        let mut chart = ChartBuilder::on(&root)
            .margin(8)
            .x_label_area_size(34)
            .y_label_area_size(44)
            .right_y_label_area_size(64)
            .build_cartesian_2d(0f64..bucket_count, 0f64..100f64)
            .unwrap()
            .set_secondary_coord(0f64..bucket_count, 0f64..100f64);

        chart
            .configure_mesh()
            .disable_x_mesh()
            .x_labels(8)
            .x_label_formatter(&|x| data.x_label(*x as i64))
            .y_desc("CPU / 内存 %")
            .y_label_formatter(&|y| format!("{:.0}", y))
            .draw()
            .ok();

        chart
            .configure_secondary_axes()
            .y_desc("磁盘 B/s")
            .y_label_formatter(&|v| fmt_bytes(*v / 100.0 * max_disk))
            .draw()
            .ok();

        // 卡顿事件竖线（浅红，置于最底层）
        for ex in &data.event_x {
            let x = *ex as f64;
            chart
                .draw_series(std::iter::once(PathElement::new(
                    [(x, 0.0), (x, 100.0)],
                    RGBColor(0xe0, 0xa0, 0xa0),
                )))
                .ok();
        }

        // CPU% / 内存%：min–max 浅色带（Polygon 填充，比实线更浅的同色调、低 alpha）
        // + avg 实线。只画 avg 会把尖峰抹平（PRD §6.3/§8 要求看出与卡顿尖峰的对应）。
        // 仅当 view 对应开关开启时绘制（F3 高级可选指标）。
        if view.cpu {
            chart
                .draw_series(std::iter::once(Polygon::new(
                    band_polygon(&data.points, |p| p.cpu_min as f64, |p| p.cpu_max as f64),
                    RGBColor(0xc4, 0x4c, 0x4c).mix(0.15).filled(),
                )))
                .ok();
            // 左轴：CPU% avg（红）
            chart
                .draw_series(LineSeries::new(
                    data.points.iter().map(|p| (p.x as f64, p.cpu_avg as f64)),
                    RGBColor(0xc4, 0x4c, 0x4c),
                ))
                .ok();
        }
        if view.mem {
            chart
                .draw_series(std::iter::once(Polygon::new(
                    band_polygon(&data.points, |p| p.mem_min as f64, |p| p.mem_max as f64),
                    RGBColor(0x4c, 0x9a, 0xc4).mix(0.15).filled(),
                )))
                .ok();
            // 左轴：内存% avg（蓝）
            chart
                .draw_series(LineSeries::new(
                    data.points.iter().map(|p| (p.x as f64, p.mem_avg as f64)),
                    RGBColor(0x4c, 0x9a, 0xc4),
                ))
                .ok();
        }
        // 左轴：GPU%（可选，橙）—仅 avg 实线（GPU 仅有 avg，无 min/max）
        if has_gpu {
            chart
                .draw_series(LineSeries::new(
                    data.points
                        .iter()
                        .filter_map(|p| p.gpu_avg.map(|g| (p.x as f64, g as f64))),
                    RGBColor(0xe0, 0xa8, 0x30),
                ))
                .ok();
        }

        // 磁盘读/写 B/s：归一到 0-100 后画在左轴坐标区（右轴标签已还原为 B/s）。
        // view.log_disk 为真时改用对数归一：norm(b)=log10(b+1)/log10(max_disk+1)*100，
        // 仍落在 0-100 左轴坐标区，使数量级悬殊的磁盘尖峰可见。
        // 磁盘同样含 min/max/avg：先画 min–max 浅色带（同色调低 alpha），再画 avg 实线，
        // 保留尖峰（PRD §6.3/§8）。norm 闭包统一处理线性/对数归一，min/max/avg 共用之。
        let denom = if view.log_disk {
            (max_disk + 1.0).log10().max(f64::MIN_POSITIVE)
        } else {
            1.0
        };
        let norm = |b: f64| {
            if view.log_disk {
                ((b + 1.0).log10() / denom * 100.0).clamp(0.0, 100.0)
            } else {
                (b / max_disk * 100.0).clamp(0.0, 100.0)
            }
        };
        if view.disk_read {
            chart
                .draw_series(std::iter::once(Polygon::new(
                    band_polygon(
                        &data.points,
                        |p| norm(p.disk_read_min),
                        |p| norm(p.disk_read_max),
                    ),
                    RGBColor(0x6a, 0xb1, 0x4c).mix(0.15).filled(),
                )))
                .ok();
            chart
                .draw_series(LineSeries::new(
                    data.points.iter().map(|p| (p.x as f64, norm(p.disk_read_avg))),
                    RGBColor(0x6a, 0xb1, 0x4c),
                ))
                .ok();
        }
        if view.disk_write {
            chart
                .draw_series(std::iter::once(Polygon::new(
                    band_polygon(
                        &data.points,
                        |p| norm(p.disk_write_min),
                        |p| norm(p.disk_write_max),
                    ),
                    RGBColor(0xb0, 0x7c, 0xc4).mix(0.15).filled(),
                )))
                .ok();
            chart
                .draw_series(LineSeries::new(
                    data.points.iter().map(|p| (p.x as f64, norm(p.disk_write_avg))),
                    RGBColor(0xb0, 0x7c, 0xc4),
                ))
                .ok();
        }

        // 图例（右上角）：仅列已启用的系列
        let mut legend: Vec<(RGBColor, &str)> = Vec::new();
        if view.cpu {
            legend.push((RGBColor(0xc4, 0x4c, 0x4c), "CPU%"));
        }
        if view.mem {
            legend.push((RGBColor(0x4c, 0x9a, 0xc4), "内存%"));
        }
        if view.disk_read {
            legend.push((RGBColor(0x6a, 0xb1, 0x4c), "磁盘读"));
        }
        if view.disk_write {
            legend.push((RGBColor(0xb0, 0x7c, 0xc4), "磁盘写"));
        }
        if has_gpu {
            legend.push((RGBColor(0xe0, 0xa8, 0x30), "GPU%"));
        }
        legend.push((RGBColor(0xe0, 0xa0, 0xa0), "卡顿"));
        let lx = (width as i32) - 150;
        let mut ly = 6i32;
        for (c, t) in legend {
            root.draw(&Rectangle::new([(lx, ly), (lx + 10, ly + 10)], c.filled()))
                .ok();
            root.draw_text(
                t,
                &("sans-serif", 12).into_text_style(&root).color(&BLACK),
                (lx + 14, ly),
            )
            .ok();
            ly += 16;
        }

        root.present().ok();
    }

    // RGB → RGBA8（alpha=255 不透明）
    let buffer = rgb_to_rgba8(&rgb, width, height);

    on_done(Some(buffer));
}

/// 字节数（B/s）转人类可读短标签（供资源图右轴使用）。
fn fmt_bytes(b: f64) -> String {
    if b >= 1e9 {
        format!("{:.1}G", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1}M", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.1}K", b / 1e3)
    } else {
        format!("{:.0}", b)
    }
}

/// 构造 min–max 带的闭合坐标（正向 (x,min) 序列 + 反向 (x,max) 序列）。
///
/// 供资源图 `Polygon` 填充：CPU%/内存% 的 min–max 浅色带叠在 avg 实线之下，
/// 把整段区间的波动（尖峰）显示出来（PRD §6.3/§8）。`data.points.x` 是真实桶序号，
/// 与 X 轴域（完整桶数）对齐。
fn band_polygon(
    points: &[ResourcePoint],
    min: impl Fn(&ResourcePoint) -> f64,
    max: impl Fn(&ResourcePoint) -> f64,
) -> Vec<(f64, f64)> {
    let fwd: Vec<(f64, f64)> = points.iter().map(|p| (p.x as f64, min(p))).collect();
    let rev: Vec<(f64, f64)> = points.iter().rev().map(|p| (p.x as f64, max(p))).collect();
    fwd.into_iter().chain(rev).collect()
}
