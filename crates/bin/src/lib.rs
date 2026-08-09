use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "find-stutter", about = "系统卡顿监控悬浮窗")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// 启动悬浮窗监控（默认）
    Run,
    /// 导出采样数据为 CSV
    Export {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(short, long, default_value = "export.csv")]
        output: String,
    },
    /// 打印今日卡顿统计
    Stats,
}
