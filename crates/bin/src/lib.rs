use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "find-stutter", about = "System stutter monitor with floating overlay")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Run with floating overlay (default)
    Run,
    /// Export data to CSV
    Export {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(short, long, default_value = "export.csv")]
        output: String,
    },
    /// Show statistics
    Stats,
}
