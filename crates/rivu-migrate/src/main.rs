use clap::{Parser, Subcommand};
use rivu_migrate::{Mode, Options, Report, run};
use std::{path::PathBuf, process::ExitCode};

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
    let args = Args::parse();
    let mode = match args.command {
        Command::Check => Mode::Check,
        Command::Apply => Mode::Apply,
    };
    let options = Options::resolve(args.data_dir, args.config);
    let mut report = Report::default();
    let result = run(&options, mode, &mut report);
    for line in &report.resources {
        println!("{line}");
    }
    for path in &report.backups {
        println!("Backup: {}", path.display());
    }
    if let Err(error) = result {
        eprintln!("Migration failed: {error:#}");
        eprintln!(
            "Resource status: configuration {}; database {}. No automatic rollback was performed.",
            report.config_status, report.database_status
        );
        return ExitCode::from(2);
    }
    if mode == Mode::Check && report.needs_changes {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
