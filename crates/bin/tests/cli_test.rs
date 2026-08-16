//! find-stutter 入口 crate 分发行为测试（ADR-0001）。
//!
//! clap 解析细节的测试在 `find-stutter-cli`（bin 只做分发）；
//! 这里验证「bin 暴露的转发接口 + GUI 意图判定」。
//! 经 `find_stutter_cli::try_parse_args_from` 构造 Cli，bin 自身不依赖 clap。

use find_stutter::{dispatch, Commands, DispatchOutcome};
use find_stutter::Cli;

#[test]
fn cli_default_no_args() {
    // 无参数 → command 为 None（GUI 分支）
    let cli = parse_args_from(&["find-stutter"]);
    assert!(cli.command.is_none());
}

#[test]
fn cli_run_subcommand() {
    let cli = parse_args_from(&["find-stutter", "run"]);
    assert!(matches!(cli.command, Some(Commands::Run)));
}

#[test]
fn cli_export_subcommand() {
    let cli = parse_args_from(&[
        "find-stutter",
        "export",
        "--from",
        "2026-01-01",
        "--to",
        "2026-12-31",
    ]);
    match cli.command {
        Some(Commands::Export { from, to, output }) => {
            assert_eq!(from, "2026-01-01");
            assert_eq!(to, "2026-12-31");
            assert_eq!(output, "export.csv"); // 默认输出名
        }
        other => panic!("应解析为 Export，实际 {:?}", other),
    }
}

#[test]
fn cli_export_with_custom_output() {
    let cli = parse_args_from(&[
        "find-stutter",
        "export",
        "--from",
        "2026-01-01",
        "--to",
        "2026-12-31",
        "-o",
        "my_report.csv",
    ]);
    match cli.command {
        Some(Commands::Export { output, .. }) => assert_eq!(output, "my_report.csv"),
        other => panic!("应解析为 Export，实际 {:?}", other),
    }
}

/// ADR-0001：Stats 子命令已删除（被 events --from <今天零点> 覆盖）。
#[test]
fn cli_stats_subcommand_is_removed() {
    let result = find_stutter::try_parse_args_from(&["find-stutter", "stats"]);
    assert!(result.is_err(), "stats 不应再是合法子命令");
}

/// GUI 意图：无参数与 run 都返回 LaunchGui。
#[test]
fn dispatch_gui_intent() {
    let cli = parse_args_from(&["find-stutter"]);
    assert_eq!(dispatch(&cli).unwrap(), DispatchOutcome::LaunchGui);
    let cli = parse_args_from(&["find-stutter", "run"]);
    assert_eq!(dispatch(&cli).unwrap(), DispatchOutcome::LaunchGui);
}

/// upgrade 子命令解析（ADR-0001 决策 6：替代 upgrade-service.ps1）。
#[test]
fn cli_upgrade_subcommand_flags() {
    assert!(matches!(
        find_stutter::try_parse_args_from(["find-stutter", "upgrade"]).unwrap().command,
        Some(Commands::Upgrade { no_build: false })
    ));
    assert!(matches!(
        find_stutter::try_parse_args_from(["find-stutter", "upgrade", "--no-build"])
            .unwrap()
            .command,
        Some(Commands::Upgrade { no_build: true })
    ));
}

/// 六件套子命令解析冒烟（详细断言在 cli crate）。
#[test]
fn dispatch_query_subcommands_parse() {
    for args in [
        vec!["find-stutter", "events"],
        vec!["find-stutter", "samples", "--limit", "10"],
        vec!["find-stutter", "analysis"],
        vec!["find-stutter", "config"],
        vec!["find-stutter", "status"],
        vec!["find-stutter", "process"],
    ] {
        let cli = parse_args_from(&args);
        assert!(
            matches!(dispatch(&cli), Ok(DispatchOutcome::Done) | Err(_)),
            "{:?} 应解析并分发",
            args
        );
    }
}

/// 测试内构造 Cli 的统一入口（转发到 find_stutter_cli::try_parse_args_from）。
fn parse_args_from(args: &[&str]) -> Cli {
    find_stutter::try_parse_args_from(args).unwrap()
}
