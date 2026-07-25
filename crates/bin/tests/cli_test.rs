use clap::Parser;

// Import from the bin crate's main module
// Note: Cli and Commands must be pub(crate) in main.rs

#[test]
fn cli_default_no_args() {
    // When no args, command should be None (overlay mode)
    let result = find_stutter::Cli::try_parse_from(["find-stutter"]);
    assert!(result.is_ok());
    assert!(result.unwrap().command.is_none());
}

#[test]
fn cli_run_subcommand() {
    let result = find_stutter::Cli::try_parse_from(["find-stutter", "run"]);
    assert!(result.is_ok());
    match result.unwrap().command {
        Some(find_stutter::Commands::Run) => {}
        _ => panic!("Expected Run command"),
    }
}

#[test]
fn cli_export_subcommand() {
    let result = find_stutter::Cli::try_parse_from([
        "find-stutter",
        "export",
        "--from",
        "2026-01-01",
        "--to",
        "2026-12-31",
    ]);
    assert!(result.is_ok());
    match result.unwrap().command {
        Some(find_stutter::Commands::Export { from, to, output }) => {
            assert_eq!(from, "2026-01-01");
            assert_eq!(to, "2026-12-31");
            assert_eq!(output, "export.csv"); // default
        }
        _ => panic!("Expected Export command"),
    }
}

#[test]
fn cli_export_with_custom_output() {
    let result = find_stutter::Cli::try_parse_from([
        "find-stutter",
        "export",
        "--from",
        "2026-01-01",
        "--to",
        "2026-12-31",
        "-o",
        "my_report.csv",
    ]);
    assert!(result.is_ok());
    match result.unwrap().command {
        Some(find_stutter::Commands::Export { output, .. }) => {
            assert_eq!(output, "my_report.csv");
        }
        _ => panic!("Expected Export command"),
    }
}

#[test]
fn cli_stats_subcommand() {
    let result = find_stutter::Cli::try_parse_from(["find-stutter", "stats"]);
    assert!(result.is_ok());
    match result.unwrap().command {
        Some(find_stutter::Commands::Stats) => {}
        _ => panic!("Expected Stats command"),
    }
}
