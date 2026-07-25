use eframe::egui;

mod app;
mod overlay;
mod skin;
mod theme;

fn main() -> eframe::Result<()> {
    env_logger::init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_decorations(false)
            .with_always_on_top()
            .with_transparent(true)
            .with_inner_size([280.0, 90.0])
            .with_min_inner_size([200.0, 60.0])
            .with_position([10.0, 10.0]),
        ..Default::default()
    };

    eframe::run_native(
        "find-stutter",
        options,
        Box::new(|cc| Ok(Box::new(app::MonitorApp::new(cc)))),
    )
}
