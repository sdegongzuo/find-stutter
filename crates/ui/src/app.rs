use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{
    Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, FontTweak, Key, Pos2, Rounding,
    Sense, Stroke, Vec2,
};
use find_stutter_core::{Collector, Config, Detector, Logger, Sample, Severity};

use crate::overlay::{self, OverlayState};
use crate::skin::SkinConfig;
use crate::theme;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowLongPtrW, GWL_EXSTYLE, SetWindowLongPtrW, SetWindowPos, SWP_FRAMECHANGED,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_EX_TRANSPARENT,
};
use windows::core::w;

pub struct MonitorApp {
    pub sample: Arc<Mutex<Sample>>,
    pub overlay_state: Arc<Mutex<OverlayState>>,
    pub expanded: bool,
    pub skin: SkinConfig,
    pub show_window: bool,
    /// 暂停/恢复监控（右键菜单控制）
    paused: Arc<Mutex<bool>>,
    /// 点击穿透模式：开启后鼠标事件穿透窗口
    click_through: bool,
    /// 缓存的窗口句柄（首次获取后复用）
    hwnd: Option<HWND>,
    window_pos: Pos2,
    initialized: bool,
}

impl MonitorApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self::setup_fonts(&cc.egui_ctx);
        theme::apply_dark_style(&cc.egui_ctx);

        let sample = Arc::new(Mutex::new(Sample::default()));
        let overlay_state = Arc::new(Mutex::new(OverlayState::default()));
        let paused = Arc::new(Mutex::new(false));
        let sample_clone = sample.clone();
        let state_clone = overlay_state.clone();
        let paused_clone = paused.clone();

        thread::spawn(move || {
            let config = Config::load("config.toml").unwrap_or_default();
            let mut collector = Collector::new();
            let mut detector = Detector::new(&config.detection);
            let mut logger = Logger::new(&config.storage).unwrap_or_else(|e| {
                eprintln!("Logger init failed: {}", e);
                Logger::new(&find_stutter_core::StorageConfig::default()).unwrap()
            });
            let mut tick: u32 = 0;

            loop {
                // 暂停时跳过采集，仅短暂休眠，保持线程存活
                if let Ok(g) = paused_clone.lock() {
                    if *g {
                        thread::sleep(Duration::from_millis(400));
                        continue;
                    }
                }

                let s = collector.collect();
                tick += 1;

                // Write sample to database
                let _ = logger.write_sample(&s);

                // Check for stutter events
                if let Some(event) = detector.analyze(&s) {
                    let _ = logger.write_event(&event);
                    if let Ok(mut state) = state_clone.lock() {
                        state.stutter_count += 1;
                        // 仅在 Major/Critical 时触发悬浮窗闪烁提醒
                        if matches!(event.severity, Severity::Major | Severity::Critical) {
                            state.last_stutter_severity = Some(event.severity);
                            state.flash_until = Instant::now() + Duration::from_millis(900);
                        }
                    }
                }

                // Periodic flush and cleanup
                if tick % 10 == 0 {
                    let _ = logger.flush();
                }
                if tick % 3600 == 0 {
                    let _ = logger.cleanup();
                }

                // Update UI state
                if let Ok(mut guard) = sample_clone.lock() {
                    *guard = s.clone();
                }
                if let Ok(mut state) = state_clone.lock() {
                    state.sent_total = s.net_sent_total;
                    state.recv_total = s.net_recv_total;
                }

                thread::sleep(Duration::from_secs(1));
            }
        });

        Self {
            sample,
            overlay_state,
            expanded: false,
            skin: SkinConfig::default(),
            show_window: true,
            paused,
            click_through: false,
            hwnd: None,
            window_pos: Pos2::new(10.0, 10.0),
            initialized: false,
        }
    }

    fn setup_fonts(ctx: &egui::Context) {
        let mut fonts = FontDefinitions::default();
        let font_paths = [
            "C:\\Windows\\Fonts\\msyh.ttc",
            "C:\\Windows\\Fonts\\simhei.ttf",
            "C:\\Windows\\Fonts\\simsun.ttc",
        ];
        for path in &font_paths {
            if let Ok(data) = std::fs::read(path) {
                fonts.font_data.insert(
                    "cjk".to_owned(),
                    Arc::new(FontData::from_owned(data).tweak(FontTweak {
                        scale: 1.0,
                        ..Default::default()
                    })),
                );
                fonts
                    .families
                    .entry(FontFamily::Proportional)
                    .or_default()
                    .insert(0, "cjk".to_owned());
                fonts
                    .families
                    .entry(FontFamily::Monospace)
                    .or_default()
                    .insert(0, "cjk".to_owned());
                break;
            }
        }
        ctx.set_fonts(fonts);
    }

    /// 获取窗口句柄（按标题 "find-stutter" 查找，首次成功后缓存）
    fn get_hwnd(&mut self) -> Option<HWND> {
        if let Some(h) = self.hwnd {
            return Some(h);
        }
        unsafe {
            match FindWindowW(None, w!("find-stutter")) {
                Ok(hwnd) if !hwnd.is_invalid() => {
                    self.hwnd = Some(hwnd);
                    Some(hwnd)
                }
                _ => None,
            }
        }
    }

    /// 切换点击穿透：WS_EX_TRANSPARENT 使鼠标事件穿透窗口
    fn set_click_through(&mut self, enabled: bool) {
        if let Some(hwnd) = self.get_hwnd() {
            unsafe {
                let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
                let flag = WS_EX_TRANSPARENT.0;
                let new_style = if enabled { style | flag } else { style & !flag };
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style as isize);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
                );
            }
        }
        self.click_through = enabled;
    }
}

impl eframe::App for MonitorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.show_window {
            ctx.request_repaint_after(Duration::from_secs(1));
            return;
        }

        let sample = self.sample.lock().map(|s| s.clone()).unwrap_or_default();
        let overlay_state = self.overlay_state.lock().map(|s| s.clone()).unwrap_or_default();

        if !self.initialized {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(self.window_pos));
            self.initialized = true;
        }

        // 卡顿闪烁提醒：Major/Critical 触发时边框脉冲闪烁
        let time_secs = ctx.input(|i| i.time);
        let flash = if Instant::now() < overlay_state.flash_until {
            let color = match overlay_state.last_stutter_severity {
                Some(Severity::Critical) => Color32::from_rgb(255, 70, 70),
                Some(Severity::Major) => Color32::from_rgb(255, 170, 40),
                _ => Color32::from_rgb(255, 200, 60),
            };
            let pulse = 0.55 + 0.45 * (time_secs * 6.0).sin();
            Some((color, pulse as f32))
        } else {
            None
        };

        // T 键切换点击穿透（穿透时鼠标无效，用键盘兜底关闭）
        if ctx.input(|i| i.key_pressed(Key::T)) {
            self.set_click_through(!self.click_through);
        }

        let paused_now = self.paused.lock().map(|g| *g).unwrap_or(false);

        let panel_frame = egui::Frame::none()
            .fill(self.skin.bg_color())
            .stroke(Stroke::new(1.0_f32, self.skin.border_color()))
            .rounding(Rounding::same(self.skin.border_radius))
            .inner_margin(egui::Margin::symmetric(12.0, 8.0));

        egui::CentralPanel::default().frame(panel_frame).show(ctx, |ui| {
            let desired_size = if self.expanded {
                Vec2::new(self.skin.width, self.skin.height * 2.0)
            } else {
                Vec2::new(self.skin.width, self.skin.height)
            };
            ui.allocate_ui(desired_size, |ui| {
                overlay::render_compact(ui, &sample, &self.skin);

                if self.expanded {
                    overlay::render_detail(ui, &sample, &overlay_state, &self.skin);
                }

                // 右键菜单的动作先用局部变量收集，避免在闭包内直接可变借用 self
                let mut toggle_pause = false;
                let mut toggle_expand = false;
                let mut toggle_click_through = false;
                let mut do_quit = false;

                let response = ui.interact(
                    ui.max_rect(),
                    egui::Id::new("drag_zone"),
                    Sense::click_and_drag(),
                );

                // 原生拖拽：按下并拖动时交给 winit/Windows 用 SC_DRAGMOVE 移动窗口，
                // 由系统负责重绘，彻底避免透明窗逐帧位移导致的重影/闪烁。
                if response.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }

                // 单击展开/收起详情
                if response.clicked() {
                    self.expanded = !self.expanded;
                }

                // 右键菜单
                response.context_menu(|ui| {
                    if ui
                        .button(if paused_now { "恢复监控" } else { "暂停监控" })
                        .clicked()
                    {
                        toggle_pause = true;
                    }
                    if ui.button("展开/收起详情").clicked() {
                        toggle_expand = true;
                    }
                    if ui
                        .button(if self.click_through {
                            "退出点击穿透"
                        } else {
                            "点击穿透模式"
                        })
                        .clicked()
                    {
                        toggle_click_through = true;
                    }
                    if ui.button("退出").clicked() {
                        do_quit = true;
                    }
                });

                if toggle_pause {
                    if let Ok(mut g) = self.paused.lock() {
                        *g = !*g;
                    }
                }
                if toggle_expand {
                    self.expanded = !self.expanded;
                }
                if toggle_click_through {
                    self.set_click_through(!self.click_through);
                }
                if do_quit {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }

                // 闪烁边框
                if let Some((color, alpha)) = flash {
                    let rect = ui.max_rect();
                    ui.painter().rect_stroke(
                        rect,
                        Rounding::same(self.skin.border_radius),
                        Stroke::new(3.0_f32, color.gamma_multiply(alpha)),
                    );
                }

                // 点击穿透提示
                if self.click_through {
                    let rect = ui.max_rect();
                    ui.painter().text(
                        rect.min + Vec2::new(6.0, 3.0),
                        Align2::LEFT_TOP,
                        "穿透模式 · 按 T 退出",
                        FontId::proportional((self.skin.font_size - 4.0).max(9.0)),
                        Color32::from_rgb(255, 220, 120),
                    );
                }
            });
        });

        ctx.request_repaint();
    }
}
