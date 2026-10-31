use clap::{Parser, Subcommand};
use rivu_migrate::{Mode, Options, Report, run};
use rivu_utils::terminal::{TerminalEscaped, cli_error};
use std::{path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(
    version,
    about = "Validate current Rivu configuration and database formats without starting the player"
)]
struct Args {
    /// Data directory to inspect; selects its config.toml unless --config is supplied.
    #[arg(
        long,
        global = true,
        value_name = "DIRECTORY",
        help_heading = "Global options"
    )]
    data_dir: Option<PathBuf>,
    /// Configuration file to inspect without starting or reconfiguring a player.
    #[arg(
        long,
        global = true,
        value_name = "FILE",
        help_heading = "Global options"
    )]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect existing formats and report required migrations without changes.
    Check,
    /// Back up and migrate supported databases under offline ownership.
    Apply,
}

fn main() -> ExitCode {
    let args = Args::try_parse().unwrap_or_else(|error| cli_error(error).exit());
    if let Err(error) = rivu_utils::logging::init(None) {
        eprintln!("Initializing logging failed: {:#}", TerminalEscaped(error));
        return ExitCode::from(2);
    }
    let mode = match args.command {
        Command::Check => Mode::Check,
        Command::Apply => Mode::Apply,
    };
    let options = Options::resolve(args.data_dir, args.config);
    let mut report = Report::default();
    let result = run(&options, mode, &mut report);
    for line in &report.resources {
        println!("{}", TerminalEscaped(line));
    }
    for backup in &report.backups {
        println!(
            "Backup: {} ({})",
            TerminalEscaped(backup.path.display()),
            backup.status.description()
        );
        if let Some(partial) = &backup.retained_partial {
            println!(
                "Retained complete backup alias: {}",
                TerminalEscaped(partial.display())
            );
        }
    }
    if let Err(error) = result {
        tracing::error!(
            error = %format_args!("{:#}", TerminalEscaped(error)),
            "Migration failed"
        );
        tracing::error!(
            configuration = %TerminalEscaped(report.config_status),
            database = %TerminalEscaped(report.database_status),
            "Resource status; no automatic rollback was performed"
        );
        return ExitCode::from(2);
    }
    if mode == Mode::Check && report.needs_changes {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::Args;
    use clap::Parser;
    use rivu_utils::terminal::cli_error;

    #[test]
    fn argument_diagnostics_escape_controls_without_flattening_layout() {
        for (argument, escaped) in [
            ("--bad\n\r\t\u{009b}", "--bad\\n\\r\\t\\u{009b}"),
            ("bad\n\r\u{009b}", "bad\\n\\r\\u{009b}"),
        ] {
            let error = Args::try_parse_from(["rivu-migrate", argument])
                .err()
                .expect("invalid argument must fail");
            let error = cli_error(error);
            assert_eq!(error.exit_code(), 2);
            let diagnostic = error.to_string();
            assert!(diagnostic.contains(escaped), "{diagnostic:?}");
            assert!(!diagnostic.contains(argument));
            assert!(
                diagnostic
                    .chars()
                    .all(|character| { !character.is_control() || character == '\n' })
            );
        }
    }
}
