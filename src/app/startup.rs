//! Startup orchestration only; historical recognition and conversion stay in rivu-migrate.
use crate::ipc::SocketOwner;
use anyhow::{Context, Result, ensure};
use rivu_format::database::{self, DatabaseState};
use rusqlite::{Connection, OpenFlags};
use std::{path::Path, process::Command, time::Instant};

fn inspect_database(path: &Path) -> Result<DatabaseState> {
    if !path
        .try_exists()
        .with_context(|| format!("Inspecting database {}", path.display()))?
    {
        return Ok(DatabaseState::Empty);
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Opening database read-only {}", path.display()))?;
    database::inspect(&connection)
}

pub(super) fn acquire_storage(data_dir: &Path, config_path: &Path) -> Result<SocketOwner> {
    let owner = SocketOwner::acquire(data_dir)?;
    let path = data_dir.join("library.db");
    let state = inspect_database(&path)?;
    if matches!(state, DatabaseState::Empty)
        || state
            == (DatabaseState::Versioned {
                schema_version: database::SCHEMA_VERSION,
            })
    {
        return Ok(owner);
    }

    let executable = std::env::current_exe()
        .context("Locating Rivu executable for automatic migration")?
        .with_file_name("rivu-migrate");
    let executable = if executable.try_exists()? {
        executable
    } else {
        "rivu-migrate".into()
    };
    tracing::info!(
        database = ?path,
        ?state,
        schema_version = database::SCHEMA_VERSION,
        ?executable,
        ?data_dir,
        config = ?config_path,
        "Invoking database migration tool"
    );
    // No writable connection or services exist yet. The child acquires its own
    // exclusive ownership; afterwards we reacquire, failing if another host won.
    drop(owner);
    let started = Instant::now();
    let status = Command::new(&executable)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--config")
        .arg(config_path)
        .arg("apply")
        .status()
        .with_context(|| format!("Starting {executable:?} for automatic migration; install rivu-migrate beside rivu or on PATH"))?;
    tracing::info!(
        %status,
        elapsed = ?started.elapsed(),
        "Migration tool exited"
    );
    ensure!(
        status.success(),
        "Automatic database migration failed ({status}); startup stopped; inspect the migration output and retained backups"
    );

    let owner = SocketOwner::acquire(data_dir)
        .context("Reacquiring data-directory ownership after migration")?;
    let state = inspect_database(&path)?;
    ensure!(
        state
            == (DatabaseState::Versioned {
                schema_version: database::SCHEMA_VERSION
            }),
        "Migration tool succeeded but database {path:?} has {state:?}, expected format {}; startup stopped",
        database::SCHEMA_VERSION,
    );
    tracing::info!(
        schema_version = database::SCHEMA_VERSION,
        "Database format verified; resuming startup"
    );
    Ok(owner)
}
