use clap::Parser;
use find_stutter::{Cli, Commands};

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Export { from, to, output }) => {
            let config = find_stutter_core::Config::load("config.toml").unwrap_or_default();
            match find_stutter_core::Logger::new(&config.storage) {
                Ok(logger) => {
                    if let Err(e) = logger.export_csv(&from, &to, &output) {
                        eprintln!("Export failed: {}", e);
                        std::process::exit(1);
                    } else {
                        println!("Exported to {}", output);
                    }
                }
                Err(e) => {
                    eprintln!("Failed to open database: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Some(Commands::Stats) => {
            let config = find_stutter_core::Config::load("config.toml").unwrap_or_default();
            match find_stutter_core::Logger::new(&config.storage) {
                Ok(logger) => match logger.event_count_today() {
                    Ok(count) => println!("Stutter events today: {}", count),
                    Err(e) => {
                        eprintln!("Query failed: {}", e);
                        std::process::exit(1);
                    }
                },
                Err(e) => {
                    eprintln!("Failed to open database: {}", e);
                    std::process::exit(1);
                }
            }
        }
        _ => find_stutter_ui::run()?,
    }
    Ok(())
}
