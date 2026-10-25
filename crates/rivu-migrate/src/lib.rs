//! Offline operations only: no player, UI, audio, or runtime dependency.
use anyhow::{Context, Result, bail, ensure};
use rivu_format::{config, database, ownership::data_directory_address};
use rusqlite::{
    Connection, OpenFlags,
    backup::{Backup, StepResult},
};
use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read},
    os::unix::{fs::OpenOptionsExt, net::UnixListener},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Check,
    Apply,
}

pub struct Options {
    pub data_dir: PathBuf,
    pub config: PathBuf,
    pub explicit_data_dir: bool,
}

impl Options {
    /// Match the host's --config/--data-dir precedence, including its fallback.
    pub fn resolve(data_dir: Option<PathBuf>, config: Option<PathBuf>) -> Self {
        let explicit_data_dir = data_dir.is_some();
        let paths = config::paths::Paths::resolve(data_dir, config);
        Self {
            data_dir: paths.data_dir,
            config: paths.config,
            explicit_data_dir,
        }
    }
}

pub struct Report {
    pub resources: Vec<String>,
    pub backups: Vec<PathBuf>,
    pub needs_changes: bool,
    pub config_status: &'static str,
    pub database_status: &'static str,
}

impl Default for Report {
    fn default() -> Self {
        Self {
            resources: Vec::new(),
            backups: Vec::new(),
            needs_changes: false,
            config_status: "unchanged",
            database_status: "unchanged",
        }
    }
}

struct DatabasePlan {
    path: PathBuf,
    connection: Connection,
    maintenance: bool,
}

fn validate_config(path: &Path) -> Result<Option<PathBuf>> {
    let mut file = match rivu_format::input::open_regular_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Reading configuration {}", path.display()));
        }
    };
    let mut original = Vec::new();
    file.read_to_end(&mut original)
        .with_context(|| format!("Reading configuration {}", path.display()))?;
    let target = fs::canonicalize(path).context("Resolving existing configuration target")?;
    let text = std::str::from_utf8(&original).context("Configuration is not UTF-8")?;
    config::validate_toml(text).context("Validating current configuration")?;
    Ok(Some(target))
}

fn database_plan(options: &Options) -> Result<Option<DatabasePlan>> {
    let path = options.data_dir.join("library.db");
    match fs::metadata(&path) {
        Err(error) if error.kind() == ErrorKind::NotFound && !options.explicit_data_dir => {
            return Ok(None);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Reading designated database {}; no database will be created",
                    path.display()
                )
            });
        }
        Ok(metadata) => ensure!(
            metadata.is_file(),
            "Database path is not a regular file: {}",
            path.display()
        ),
    }
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Opening database read-only {}", path.display()))?;
    database::validate_structure(&connection).context("Validating database schema structure")?;
    let maintenance = match database::inspect(&connection)? {
        database::DatabaseState::Versioned {
            album_trigger_revision,
            ..
        } => album_trigger_revision != Some(database::ALBUM_TRIGGER_REVISION),
        _ => bail!("Database is not a supported versioned schema"),
    };
    Ok(Some(DatabasePlan {
        path,
        connection,
        maintenance,
    }))
}

/// On failure the report retains database backup paths and any completed or
/// uncertain maintenance. Configuration is only read and validated.
pub fn run(options: &Options, mode: Mode, report: &mut Report) -> Result<()> {
    // Apply inspects existing resources under the same ownership as the host;
    // acquiring only after preflight would leave the plan open to startup races.
    let _owner = if mode == Mode::Apply && options.data_dir.is_dir() {
        Some(
            UnixListener::bind_addr(&data_directory_address(&options.data_dir)?).context(
                "Cannot take data-directory ownership; stop the running Rivu instance first",
            )?,
        )
    } else {
        None
    };
    let config = validate_config(&options.config)
        .with_context(|| format!("Preflighting configuration {}", options.config.display()))?;
    let database = database_plan(options).with_context(|| {
        format!(
            "Preflighting database {}",
            options.data_dir.join("library.db").display()
        )
    })?;

    match &config {
        Some(target) => report.resources.push(format!(
            "Configuration {}: current (schema 1), untouched",
            target.display()
        )),
        None => report.resources.push(format!(
            "Configuration {}: absent, defaults apply; not created",
            options.config.display()
        )),
    }
    match &database {
        Some(plan) => report.resources.push(format!(
            "Database {}: schema 1; {} (supported derived trigger revision {})",
            plan.path.display(),
            if plan.maintenance {
                "derived trigger maintenance required"
            } else {
                "current, untouched"
            },
            database::ALBUM_TRIGGER_REVISION
        )),
        None => report.resources.push(format!(
            "Database {}: absent at default location; not created",
            options.data_dir.join("library.db").display()
        )),
    }
    report.needs_changes = database.as_ref().is_some_and(|p| p.maintenance);
    // Advisory only: concurrent disk use can still make later writes fail.
    // Probe even in check mode, but only budget resources that need changes.
    storage_preflight(database.as_ref())?;
    if mode == Mode::Check || !report.needs_changes {
        return Ok(());
    }

    // Back up the complete database before attempting transactional maintenance.
    if let Some(plan) = database.as_ref().filter(|p| p.maintenance) {
        let (path, file) = reserve_backup(&plan.path)?;
        report.backups.push(path.clone());
        drop(file);
        let mut destination =
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        {
            let backup = Backup::new(&plan.connection, &mut destination)?;
            loop {
                match backup
                    .step(256)
                    .context("Copying SQLite backup (may be incomplete)")?
                {
                    StepResult::Done => break,
                    StepResult::More => {}
                    StepResult::Busy | StepResult::Locked => bail!(
                        "SQLite backup is busy; backup {} may be incomplete",
                        path.display()
                    ),
                    _ => bail!(
                        "Unexpected SQLite backup state; backup {} may be incomplete",
                        path.display()
                    ),
                }
            }
        }
        destination
            .close()
            .map_err(|(_, error)| error)
            .context("Closing SQLite backup")?;
        File::open(&path)?
            .sync_all()
            .context("Syncing SQLite backup")?;
        sync_parent(&path)?;
    }

    if let Some(plan) = &database
        && plan.maintenance
    {
        let writer = Connection::open_with_flags(&plan.path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        // FULL makes SQLite's maintenance transaction durable, including WAL.
        writer.pragma_update(None, "synchronous", "FULL")?;
        report.database_status = "maintenance attempted; inspect database on failure";
        database::ensure_summary_triggers(&writer)
            .context("Applying transactional derived trigger maintenance")?;
        report.database_status = "current derived triggers installed";
    }
    if let Some(plan) = &database {
        database::validate_current(&plan.connection)?;
        ensure!(
            matches!(
                database::inspect(&plan.connection)?,
                database::DatabaseState::Versioned {
                    album_trigger_revision: Some(database::ALBUM_TRIGGER_REVISION),
                    ..
                }
            ),
            "Database derived maintenance is not current after apply"
        );
    }
    report
        .resources
        .push("Apply complete: designated existing resources are current.".to_owned());
    Ok(())
}

fn database_storage_bytes(page_count: u64, page_size: u64) -> Result<u64> {
    let backup = page_count
        .checked_mul(page_size)
        .context("SQLite backup size overflows storage estimate")?;
    // Current migration only replaces a fixed set of triggers and its marker;
    // it does not rebuild tables. Allow two full-image equivalents including
    // per-page journal/WAL framing, plus 16 MiB for schema growth and headers.
    // This intentionally overestimates the small maintenance transaction.
    let journal = page_size
        .checked_add(32)
        .and_then(|size| page_count.checked_mul(size))
        .and_then(|size| size.checked_mul(2))
        .context("SQLite journal/WAL size overflows storage estimate")?;
    backup
        .checked_add(journal)
        .and_then(|size| size.checked_add(16 * 1024 * 1024))
        .context("SQLite storage requirement overflows")
}

fn storage_preflight(database: Option<&DatabasePlan>) -> Result<()> {
    let Some(plan) = database.filter(|plan| plan.maintenance) else {
        return Ok(());
    };
    let estimate = || -> Result<()> {
        let pages = plan
            .connection
            .pragma_query_value(None, "page_count", |row| row.get::<_, i64>(0))?;
        let page_size = plan
            .connection
            .pragma_query_value(None, "page_size", |row| row.get::<_, i64>(0))?;
        let required = database_storage_bytes(
            u64::try_from(pages).context("Negative SQLite page count")?,
            u64::try_from(page_size).context("Negative SQLite page size")?,
        )?;
        let parent = plan
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        ensure_storage_capacity(&plan.path, required, fs2::available_space(parent)?)
    };
    estimate().with_context(|| format!("Estimating storage for database {}", plan.path.display()))
}

fn ensure_storage_capacity(target: &Path, required: u64, available: u64) -> Result<()> {
    ensure!(
        available >= required,
        "Insufficient available storage for {}: need {required} bytes, available {available} bytes (advisory estimate)",
        target.display()
    );
    Ok(())
}

fn reserve_backup(target: &Path) -> Result<(PathBuf, File)> {
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    for attempt in 0..100 {
        let mut name = target
            .file_name()
            .context("Backup target must name a file")?
            .to_os_string();
        name.push(".");
        name.push(&timestamp);
        if attempt != 0 {
            name.push(format!(".{attempt}"));
        }
        name.push(".bak");
        let path = target.with_file_name(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("Creating backup {}", path.display()));
            }
        }
    }
    bail!(
        "Could not reserve a unique timestamped backup for {}",
        target.display()
    )
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?
        .sync_all()
        .with_context(|| format!("Syncing directory {}", parent.display()))
}

#[cfg(test)]
mod tests;
