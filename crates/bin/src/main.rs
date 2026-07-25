use clap::Parser;
use find_stutter::{Cli, Commands};

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Export { from, to, output }) => {
            let config = find_stutter_core::Config::load("config.toml").unwrap_or_default();
            match find_stutter_core::Logger::new(&config.storage) {
                Ok(logger) => {
                    if let Err(e) = logger.export_csv(&from, &to, &output) {
                        eprintln!("Export failed: {}", e);
                    } else {
                        println!("Exported to {}", output);
                    }
                }
                Err(e) => eprintln!("Failed to open database: {}", e),
            }
        }
        Some(Commands::Stats) => {
            let config = find_stutter_core::Config::load("config.toml").unwrap_or_default();
            match find_stutter_core::Logger::new(&config.storage) {
                Ok(logger) => match logger.event_count_today() {
                    Ok(count) => println!("Stutter events today: {}", count),
                    Err(e) => eprintln!("Query failed: {}", e),
                },
                Err(e) => eprintln!("Failed to open database: {}", e),
            }
        }
        _ => run_overlay(),
    }
}

fn run_overlay() {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
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
        Box::new(|cc| Ok(Box::new(find_stutter_ui::app::MonitorApp::new(cc)))),
    )
    .ok();
}
