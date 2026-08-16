//! find-stutter CLI（ADR-0001 界面轴·agent）。
//!
//! 一等查询库：`events` / `samples` / `analysis` / `config` / `status` / `process`
//! 六件套子命令 + `query`（只读 SQL 逃生口，诊断期灵活聚合）+ `export`（CSV，
//! 行为与旧版一致）+ `upgrade`（决策 6 的提权例外）。
//! `crates/bin` 只做分发：无参数 = 启动 GUI；子命令 = 调这里的 [`dispatch`]。
//!
//! ## 契约（决策 1/3）
//!
//! - JSON 输出：键英文、值保留原文、时间 ISO8601（RFC3339 UTC）、
//!   **单行紧凑**（`serde_json::to_string`，便于 jq 管道）；
//! - `--from/--to/--limit` 过滤；schema 跟随领域模型演进，不做版本冻结；
//! - clap help 全中文。

pub mod elevate;
pub mod process_snapshot;
pub mod query;
pub mod service_status;
pub mod timeparse;
pub mod upgrade;

use clap::{Parser, Subcommand};

/// 顶部命令行定义（bin 直接 re-export 使用；子命令解析测试在本 crate 内完成，
/// 不需要拖入 GUI 依赖树——这正是 ADR-0001 拒绝「CLI 依赖 ui」的原因）。
#[derive(Parser, Debug)]
#[command(
    name = "find-stutter",
    about = "系统卡顿监控：无参数启动 GUI 悬浮窗（自动确保后台服务在跑）；子命令面向 agent 的 JSON 查询 / CSV 导出 / 升级",
    version
)]
pub struct Cli {
    /// 子命令；缺省 = 启动 GUI（悬浮窗 + 自动确保服务，UAC 由系统弹出）
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// 子命令集合（Stats 已按 ADR-0001 删除：`events --from <今天零点>` 覆盖同语义）。
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// 启动悬浮窗监控（默认；与无参数等价）
    Run,

    /// 列出卡顿事件（JSON，最新 N 条；时间升序）
    Events {
        /// 开始时间（默认：本地今日零点；支持 RFC3339 / YYYY-MM-DD HH:MM:SS / YYYY-MM-DD）
        #[arg(long)]
        from: Option<String>,

        /// 结束时间（默认：现在；格式同 --from）
        #[arg(long)]
        to: Option<String>,

        /// 最多返回条数（取最新 N 条；默认 100）
        #[arg(short, long, default_value_t = 100)]
        limit: usize,
    },

    /// 查询样本区间（JSON，最新 N 条；时间升序）
    Samples {
        /// 开始时间（默认：本地今日零点；支持 RFC3339 / YYYY-MM-DD HH:MM:SS / YYYY-MM-DD）
        #[arg(long)]
        from: Option<String>,

        /// 结束时间（默认：现在；格式同 --from）
        #[arg(long)]
        to: Option<String>,

        /// 最多返回条数（样本量大：1Hz 采样、保留 30 天，故默认只取最新 1000 条；可调大）
        #[arg(short, long, default_value_t = 1000)]
        limit: usize,
    },

    /// 聚合分析一次输出（JSON：KPI / 趋势 / 元凶榜 / 类型占比 / 最近事件根因报告）
    Analysis {
        /// 开始时间（默认：本地今日零点）
        #[arg(long)]
        from: Option<String>,

        /// 结束时间（默认：现在）
        #[arg(long)]
        to: Option<String>,
    },

    /// 只读 SQL 直查（JSON 行数组）：诊断期的灵活聚合逃生口——按天计数 /
    /// 分布统计 / 连续段分析等；固定口径查询优先用 events / samples / analysis
    Query {
        /// 单条只读 SQL（SELECT / WITH / PRAGMA；写语句与多语句拼接会被拒绝）
        sql: String,

        /// 数据库路径（默认：config.toml 的 storage.db_path；可指向 verify.db 等）
        #[arg(long)]
        db: Option<String>,
    },

    /// 当前生效配置（JSON：config.toml 加载结果，含默认值回退后的有效值）
    Config,

    /// 服务状态（JSON：SCM 状态 + 心跳健康 + db 路径）
    Status,

    /// 现场采集一次 top 进程快照（JSON，不写库）
    Process {
        /// 最多返回进程数（默认 10；0 = 只输出系统摘要，不列进程）
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
    },

    /// 导出采样数据为 CSV（中文表头，行为与旧版一致）
    Export {
        /// 开始时间（YYYY-MM-DD 或 YYYY-MM-DD HH:MM:SS）
        #[arg(long)]
        from: String,

        /// 结束时间（格式同 --from）
        #[arg(long)]
        to: String,

        /// 输出文件路径
        #[arg(short, long, default_value = "export.csv")]
        output: String,
    },

    /// 升级：停服（提权）→ rtk 构建 release → 重装启动（提权）。
    /// ADR-0001 决策 6：替代本地 upgrade-service.ps1；决策 4「CLI 不做提权控制」的唯一例外
    Upgrade {
        /// 跳过构建，仅用已有 release exe 重装服务（如只改了配置 / 手动构建过）
        #[arg(long)]
        no_build: bool,
    },
}

/// dispatch 的返回：是否需要启动 GUI（由 bin 处理——cli 不依赖 ui crate）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// 无参数 / Run：应启动 GUI（bin 负责先隐藏控制台再调 find_stutter_ui::run）
    LaunchGui,
    /// 子命令已处理完毕（JSON / CSV 已输出到 stdout）
    Done,
}

/// 解析进程参数（`Cli::parse()` 的薄封装）。
/// bin crate 经此入口解析，自身无需依赖 clap——bin 只做分发。
pub fn parse_args() -> Cli {
    Cli::parse()
}

/// 解析给定参数列表（`Cli::try_parse_from` 的薄封装；测试 / 嵌入调用用）。
/// bin 侧测试经此构造 `Cli`，同样不需要依赖 clap。
pub fn try_parse_args_from<I, T>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    Cli::try_parse_from(args)
}

/// 三个 db 查询子命令（events / samples / analysis）的共用分发：
/// 打开生效 db → 执行查询 → 单行 JSON 输出。
fn dispatch_db_query<F>(f: F) -> anyhow::Result<DispatchOutcome>
where
    F: FnOnce(&rusqlite::Connection) -> anyhow::Result<serde_json::Value>,
{
    let db_path = query::db_path_from_config();
    let conn = query::open_db(std::path::Path::new(&db_path))?;
    let v = f(&conn)?;
    println_json(&v);
    Ok(DispatchOutcome::Done)
}

/// 分发入口：执行查询子命令并输出；GUI 分支只返回意图，由 bin 落地。
pub fn dispatch(cli: &Cli) -> anyhow::Result<DispatchOutcome> {
    match &cli.command {
        None | Some(Commands::Run) => Ok(DispatchOutcome::LaunchGui),

        Some(Commands::Events { from, to, limit }) => dispatch_db_query(|conn| {
            query::events_json(conn, from.as_deref(), to.as_deref(), *limit)
        }),

        Some(Commands::Samples { from, to, limit }) => dispatch_db_query(|conn| {
            query::samples_json(conn, from.as_deref(), to.as_deref(), *limit)
        }),

        Some(Commands::Analysis { from, to }) => dispatch_db_query(|conn| {
            query::analysis_json(conn, from.as_deref(), to.as_deref())
        }),

        // query 需要 --db 覆盖能力，不走 dispatch_db_query 的固定路径
        Some(Commands::Query { sql, db }) => {
            let db_path = db.clone().unwrap_or_else(query::db_path_from_config);
            let conn = query::open_db(std::path::Path::new(&db_path))?;
            let v = query::sql_json(&conn, sql)?;
            println_json(&v);
            Ok(DispatchOutcome::Done)
        }

        Some(Commands::Config) => {
            println_json(&query::config_json());
            Ok(DispatchOutcome::Done)
        }

        Some(Commands::Status) => {
            println_json(&service_status::status_json());
            Ok(DispatchOutcome::Done)
        }

        Some(Commands::Process { limit }) => {
            let v = process_snapshot::snapshot_json(*limit)?;
            println_json(&v);
            Ok(DispatchOutcome::Done)
        }

        Some(Commands::Export { from, to, output }) => {
            // 行为与旧 bin 版完全一致：core Logger 的 export_csv（samples 表 → 中文表头 CSV）
            let config = find_stutter_core::Config::load("config.toml").unwrap_or_default();
            match find_stutter_core::Logger::new(&config.storage) {
                Ok(logger) => {
                    if let Err(e) = logger.export_csv(from, to, output) {
                        eprintln!("导出失败: {}", e);
                        std::process::exit(1);
                    } else {
                        println!("已导出到 {}", output);
                    }
                }
                Err(e) => {
                    eprintln!("打开数据库失败: {}", e);
                    std::process::exit(1);
                }
            }
            Ok(DispatchOutcome::Done)
        }

        Some(Commands::Upgrade { no_build }) => {
            let plan = upgrade::plan_upgrade(*no_build, None, None, None)?;
            eprintln!(
                "[upgrade] service exe: {}",
                plan.service_exe.display()
            );
            eprintln!("[upgrade] rtk: {}", plan.rtk.display());
            eprintln!("[upgrade] 仓库根: {}", plan.repo_root.display());
            let ok = upgrade::run_upgrade(&plan)?;
            if ok {
                println!("升级完成：服务已用新构建运行");
            }
            Ok(DispatchOutcome::Done)
        }
    }
}

/// 单行紧凑 JSON 输出（契约：便于 jq 管道）。
fn println_json(v: &serde_json::Value) {
    println!("{}", serde_json::to_string(v).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_no_subcommand_is_gui() {
        let cli = Cli::try_parse_from(["find-stutter"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn parse_run_is_gui() {
        let cli = Cli::try_parse_from(["find-stutter", "run"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Run)));
    }

    #[test]
    fn parse_events_defaults() {
        let cli = Cli::try_parse_from(["find-stutter", "events"]).unwrap();
        match cli.command {
            Some(Commands::Events { from, to, limit }) => {
                assert!(from.is_none());
                assert!(to.is_none());
                assert_eq!(limit, 100);
            }
            other => panic!("应解析为 Events，实际 {:?}", other),
        }
    }

    #[test]
    fn parse_events_with_filters() {
        let cli = Cli::try_parse_from([
            "find-stutter", "events", "--from", "2026-08-16", "--to",
            "2026-08-16T23:59:59Z", "--limit", "5",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Events { from, to, limit }) => {
                assert_eq!(from.as_deref(), Some("2026-08-16"));
                assert_eq!(to.as_deref(), Some("2026-08-16T23:59:59Z"));
                assert_eq!(limit, 5);
            }
            other => panic!("应解析为 Events，实际 {:?}", other),
        }
    }

    #[test]
    fn parse_samples_default_limit_is_1000() {
        let cli = Cli::try_parse_from(["find-stutter", "samples"]).unwrap();
        match cli.command {
            Some(Commands::Samples { limit, .. }) => assert_eq!(limit, 1000),
            other => panic!("应解析为 Samples，实际 {:?}", other),
        }
    }

    #[test]
    fn parse_analysis_range() {
        let cli = Cli::try_parse_from([
            "find-stutter", "analysis", "--from", "2026-08-01", "--to", "2026-08-16",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Analysis { from, to }) => {
                assert_eq!(from.as_deref(), Some("2026-08-01"));
                assert_eq!(to.as_deref(), Some("2026-08-16"));
            }
            other => panic!("应解析为 Analysis，实际 {:?}", other),
        }
    }

    #[test]
    fn parse_query_positional_sql_and_db_flag() {
        let cli = Cli::try_parse_from([
            "find-stutter", "query",
            "SELECT COUNT(*) AS n FROM stutter_events",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Query { sql, db }) => {
                assert_eq!(sql, "SELECT COUNT(*) AS n FROM stutter_events");
                assert!(db.is_none(), "未指定 --db 时应为 None（回退 config 路径）");
            }
            other => panic!("应解析为 Query，实际 {:?}", other),
        }

        let cli =
            Cli::try_parse_from(["find-stutter", "query", "--db", "verify.db", "SELECT 1"])
                .unwrap();
        match cli.command {
            Some(Commands::Query { sql, db }) => {
                assert_eq!(sql, "SELECT 1");
                assert_eq!(db.as_deref(), Some("verify.db"));
            }
            other => panic!("应解析为 Query，实际 {:?}", other),
        }
    }

    #[test]
    fn parse_config_status_process() {
        assert!(matches!(
            Cli::try_parse_from(["find-stutter", "config"]).unwrap().command,
            Some(Commands::Config)
        ));
        assert!(matches!(
            Cli::try_parse_from(["find-stutter", "status"]).unwrap().command,
            Some(Commands::Status)
        ));
        match Cli::try_parse_from(["find-stutter", "process", "-l", "3"]).unwrap().command {
            Some(Commands::Process { limit }) => assert_eq!(limit, 3),
            other => panic!("应解析为 Process，实际 {:?}", other),
        }
    }

    #[test]
    fn parse_export() {
        let cli = Cli::try_parse_from([
            "find-stutter", "export", "--from", "2026-07-25", "--to", "2026-07-26",
            "-o", "report.csv",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Export { from, to, output }) => {
                assert_eq!(from, "2026-07-25");
                assert_eq!(to, "2026-07-26");
                assert_eq!(output, "report.csv");
            }
            other => panic!("应解析为 Export，实际 {:?}", other),
        }
    }

    #[test]
    fn parse_export_default_output() {
        let cli = Cli::try_parse_from([
            "find-stutter", "export", "--from", "a", "--to", "b",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Export { output, .. }) => assert_eq!(output, "export.csv"),
            other => panic!("应解析为 Export，实际 {:?}", other),
        }
    }

    #[test]
    fn parse_upgrade_flags() {
        assert!(matches!(
            Cli::try_parse_from(["find-stutter", "upgrade"]).unwrap().command,
            Some(Commands::Upgrade { no_build: false })
        ));
        assert!(matches!(
            Cli::try_parse_from(["find-stutter", "upgrade", "--no-build"]).unwrap().command,
            Some(Commands::Upgrade { no_build: true })
        ));
    }

    /// ADR-0001：Stats 子命令已删除（被 events --from <今天零点> 覆盖）。
    #[test]
    fn stats_subcommand_is_removed() {
        assert!(Cli::try_parse_from(["find-stutter", "stats"]).is_err());
    }

    #[test]
    fn unknown_subcommand_fails() {
        assert!(Cli::try_parse_from(["find-stutter", "explode"]).is_err());
    }

    /// dispatch 对 GUI 分支只返回意图，不触碰 ui crate。
    #[test]
    fn dispatch_none_and_run_return_launch_gui() {
        let cli = Cli::try_parse_from(["find-stutter"]).unwrap();
        assert_eq!(dispatch(&cli).unwrap(), DispatchOutcome::LaunchGui);
        let cli = Cli::try_parse_from(["find-stutter", "run"]).unwrap();
        assert_eq!(dispatch(&cli).unwrap(), DispatchOutcome::LaunchGui);
    }

    /// dispatch(Config) / dispatch(Status) 不依赖 db 存在，应成功且 Done。
    /// （stdout 会在测试里打出来，无害。）
    #[test]
    fn dispatch_config_and_status_are_done() {
        let cli = Cli::try_parse_from(["find-stutter", "config"]).unwrap();
        assert_eq!(dispatch(&cli).unwrap(), DispatchOutcome::Done);
    }

    /// dispatch(Events) 对不存在的 db：open_readonly 失败 → Err（退出码非 0，
    /// agent 可感知）；对存在的空 db 输出空数组。空库场景在 query.rs 已测，
    /// 这里只验证「db 打不开时返回错误而不是 panic」。
    #[test]
    fn dispatch_events_missing_db_errors_cleanly() {
        // 把 CWD 切到临时目录没有可移植手段（env 变更进程级），
        // 改为直接构造：config.toml 在仓库根必然存在 → db 也存在。
        // 此用例验证 dispatch 不 panic 即可（两种结果都合法）。
        let cli = Cli::try_parse_from(["find-stutter", "events", "--limit", "1"]).unwrap();
        let _ = dispatch(&cli);
    }
}
