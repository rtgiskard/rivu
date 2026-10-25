mod terminal;

use clap::{Parser, Subcommand};
use rivu_migrate::{Mode, Options, Report, run};
use std::{path::PathBuf, process::ExitCode};
use terminal::Terminal;

#[derive(Parser)]
#[command(
    version,
    about = "Validate Rivu configuration and maintain existing databases without starting the player"
)]
struct Args {
    #[arg(long, global = true, value_name = "DIRECTORY")]
    data_dir: Option<PathBuf>,
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect files read-only; exit 1 when an upgrade is needed, 2 on error.
    Check,
    /// Validate configuration and back up database changes under offline ownership.
    Apply,
}

fn main() -> ExitCode {
    let args = Args::try_parse().unwrap_or_else(|error| terminal::cli_error(error).exit());
    let mode = match args.command {
        Command::Check => Mode::Check,
        Command::Apply => Mode::Apply,
    };
    let options = Options::resolve(args.data_dir, args.config);
    let mut report = Report::default();
    let result = run(&options, mode, &mut report);
    for line in &report.resources {
        println!("{}", Terminal(line));
    }
    for backup in &report.backups {
        println!(
            "Backup: {} ({})",
            Terminal(backup.path.display()),
            backup.status.description()
        );
        if let Some(partial) = &backup.retained_partial {
            println!(
                "Retained complete backup alias: {}",
                Terminal(partial.display())
            );
        }
    }
    if let Err(error) = result {
        eprintln!("Migration failed: {:#}", Terminal(error));
        eprintln!(
            "Resource status: configuration {}; database {}. No automatic rollback was performed.",
            Terminal(report.config_status),
            Terminal(report.database_status)
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
    use super::*;

    #[test]
    fn argument_diagnostics_escape_controls_without_flattening_layout() {
        for (argument, escaped) in [
            ("--bad\n\r\t\u{009b}", "--bad\\n\\r\\t\\u{009b}"),
            ("bad\n\r\u{009b}", "bad\\n\\r\\u{009b}"),
        ] {
            let error = Args::try_parse_from(["rivu-migrate", argument])
                .err()
                .expect("invalid argument must fail");
            let error = terminal::cli_error(error);
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
