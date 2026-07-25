use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use eframe::egui;
use egui::{FontData, FontFamily, FontDefinitions, FontTweak, Pos2, Rounding, Stroke, Vec2};
use find_stutter_core::{Collector, Config, Detector, Logger, Sample};

use crate::overlay::{self, OverlayState};
use crate::skin::SkinConfig;
use crate::theme;

pub struct MonitorApp {
    pub sample: Arc<Mutex<Sample>>,
    pub overlay_state: Arc<Mutex<OverlayState>>,
    pub expanded: bool,
    pub skin: SkinConfig,
    pub show_window: bool,
    window_pos: Pos2,
    dragging: bool,
    drag_offset: Vec2,
    initialized: bool,
    hwnd_configured: bool,
}

impl MonitorApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self::setup_fonts(&cc.egui_ctx);
        theme::apply_dark_style(&cc.egui_ctx);

        let sample = Arc::new(Mutex::new(Sample::default()));
        let overlay_state = Arc::new(Mutex::new(OverlayState::default()));
        let sample_clone = sample.clone();
        let state_clone = overlay_state.clone();

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
                let s = collector.collect();
                tick += 1;

                // Write sample to database
                let _ = logger.write_sample(&s);

                // Check for stutter events
                if let Some(event) = detector.analyze(&s) {
                    let _ = logger.write_event(&event);
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
            window_pos: Pos2::new(10.0, 10.0),
            dragging: false,
            drag_offset: Vec2::ZERO,
            initialized: false,
            hwnd_configured: false,
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

    /// Configure the Win32 window for proper transparency (eliminates drag ghosting)
    fn configure_layered_window(&self, _ctx: &egui::Context) {
        #[cfg(target_os = "windows")]
        {
            use windows::Win32::UI::WindowsAndMessaging::{
                FindWindowW, GetWindowLongW, SetWindowLongW, GWL_EXSTYLE, WS_EX_LAYERED,
                WS_EX_TOPMOST,
            };

            // Find our window by title
            let title: Vec<u16> = "find-stutter\0".encode_utf16().collect();
            unsafe {
                if let Ok(hwnd) = FindWindowW(None, windows::core::PCWSTR(title.as_ptr())) {
                    // Set WS_EX_LAYERED | WS_EX_TOPMOST
                    let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
                    let new_ex_style =
                        ex_style | WS_EX_LAYERED.0 as i32 | WS_EX_TOPMOST.0 as i32;
                    SetWindowLongW(hwnd, GWL_EXSTYLE, new_ex_style);
                }
            }
        }
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

        if !self.hwnd_configured {
            self.configure_layered_window(ctx);
            self.hwnd_configured = true;
        }

        // Force full-window repaint during drag
        if self.dragging {
            ctx.request_repaint();
        }

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

                let response = ui.interact(ui.max_rect(), egui::Id::new("drag_zone"), egui::Sense::click_and_drag());

                if response.drag_started() {
                    self.dragging = true;
                    self.drag_offset = response.interact_pointer_pos().unwrap_or_default() - self.window_pos;
                }
                if self.dragging {
                    if let Some(pos) = response.interact_pointer_pos() {
                        self.window_pos = pos - self.drag_offset;
                        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(self.window_pos));
                    }
                    if response.drag_stopped() {
                        self.dragging = false;
                    }
                }

                if response.clicked() {
                    self.expanded = !self.expanded;
                }
            });
        });

        ctx.request_repaint();
    }
}
