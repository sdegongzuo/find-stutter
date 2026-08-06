use clap::Parser;
use find_stutter::{Cli, Commands};

fn main() -> anyhow::Result<()> {
    // 日志由 find_stutter_ui::run() 内部 init（用 try_init 容忍重复），
    // 这里不再 init，否则 lib.rs 二次 init 会 panic。
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
        _ => {
            // release 下启动 GUI 前解除控制台关联（不弹黑框）；debug 保留控制台便于看日志。
            // 只对 GUI 分支生效，export / stats 子命令仍需控制台输出，不受影响。
            find_stutter_ui::window::hide_console_for_gui();
            find_stutter_ui::run()?
        }
    }
    Ok(())
}
