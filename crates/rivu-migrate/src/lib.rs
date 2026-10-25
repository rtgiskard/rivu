//! Offline operations only: no player, UI, audio, or runtime dependency.
use anyhow::{Context, Result, bail, ensure};
use rivu_format::{config, database, ownership::data_directory_address};
use rusqlite::{
    Connection, OpenFlags,
    backup::{Backup, StepResult},
};
use std::{
    fs::{self, File, OpenOptions},
    io::ErrorKind,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupStatus {
    /// Private partial artifact; copying, closing, syncing, or publication failed.
    Unpublished,
    /// Published without replacing another file; directory durability is uncertain.
    PublishedSyncUncertain,
    /// SQLite closed and both file data and the published directory entry synced.
    Complete,
}

impl BackupStatus {
    pub fn description(self) -> &'static str {
        match self {
            Self::Unpublished => "unpublished; may be incomplete",
            Self::PublishedSyncUncertain => "published; directory sync uncertain",
            Self::Complete => "complete; file and directory synced",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupReport {
    /// Actual artifact location, updated immediately after successful publication.
    pub path: PathBuf,
    pub status: BackupStatus,
    /// Existing complete-content alias left at the original partial path.
    pub retained_partial: Option<PathBuf>,
}

pub struct Report {
    pub resources: Vec<String>,
    pub backups: Vec<BackupReport>,
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
}

fn validate_config(path: &Path) -> Result<Option<PathBuf>> {
    let text = match config::read_toml(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Reading configuration {}", path.display()));
        }
    };
    let target = fs::canonicalize(path).context("Resolving existing configuration target")?;
    config::validate_toml(&text).context("Validating current configuration")?;
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
    Ok(Some(DatabasePlan { path, connection }))
}

/// Validate existing resources without rewriting them. Apply additionally takes
/// offline ownership; no released database migration is currently available.
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
            "Database {}: schema {}; current, untouched",
            plan.path.display(),
            database::SCHEMA_VERSION
        )),
        None => report.resources.push(format!(
            "Database {}: absent at default location; not created",
            options.data_dir.join("library.db").display()
        )),
    }
    report.needs_changes = false;
    if mode == Mode::Apply {
        report.resources.push(
            "No database migration is required for the current baseline; resources are unchanged."
                .to_owned(),
        );
    }
    Ok(())
}

/// Execute a supplied database migration under offline ownership after validating
/// configuration and the current database structure. Publish a durable SQLite
/// backup before invoking `change` in a FULL-synchronous transaction. The operation
/// must include its own postcondition validation; any error rolls it back. Reports
/// retain backup artifacts and attempted/committed status even on failure.
/// This is execution infrastructure, not a registered baseline migration.
pub fn apply_database_change(
    options: &Options,
    report: &mut Report,
    change: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<()>,
) -> Result<()> {
    let _owner = UnixListener::bind_addr(&data_directory_address(&options.data_dir)?)
        .context("Cannot take data-directory ownership; stop the running Rivu instance first")?;
    validate_config(&options.config)
        .with_context(|| format!("Preflighting configuration {}", options.config.display()))?;
    let plan = database_plan(options)?.context("No existing database to migrate")?;
    storage_preflight(&plan)?;
    // Back up the complete database before attempting a transactional change.
    let (path, published, file) = reserve_backup(&plan.path)?;
    report.backups.push(BackupReport {
        path,
        status: BackupStatus::Unpublished,
        retained_partial: None,
    });
    let artifact = report.backups.last_mut().expect("just recorded backup");
    let mut destination =
        Connection::open_with_flags(&artifact.path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
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
                    artifact.path.display()
                ),
                _ => bail!(
                    "Unexpected SQLite backup state; backup {} may be incomplete",
                    artifact.path.display()
                ),
            }
        }
    }
    destination
        .close()
        .map_err(|(_, error)| error)
        .context("Closing SQLite backup")?;
    file.sync_all().context("Syncing SQLite backup")?;
    let mut partial = tempfile::TempPath::try_from_path(&artifact.path)?;
    // Every failure artifact is evidence, not a disposable temporary file.
    partial.disable_cleanup(true);
    partial
        .persist_noclobber(&published)
        .map_err(|error| error.error)
        .context("Publishing SQLite backup without replacement")?;
    // Publication already happened even if the following directory sync
    // fails. Keep the real path and never misreport this as a partial file.
    let partial_path = std::mem::replace(&mut artifact.path, published);
    artifact.status = BackupStatus::PublishedSyncUncertain;
    // The no-replace link/unlink fallback can leave a complete-content
    // alias behind. Report only aliases that actually remain.
    match fs::symlink_metadata(&partial_path) {
        Ok(_) => artifact.retained_partial = Some(partial_path),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Inspecting retained backup alias {}",
                    partial_path.display()
                )
            });
        }
    }
    sync_parent(&artifact.path)?;
    artifact.status = BackupStatus::Complete;
    let writer = Connection::open_with_flags(&plan.path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    // FULL makes the migration transaction durable, including WAL.
    writer.pragma_update(None, "synchronous", "FULL")?;
    report.database_status = "migration attempted; inspect database on failure";
    let transaction = writer.unchecked_transaction()?;
    change(&transaction).context("Applying transactional database migration")?;
    transaction
        .commit()
        .context("Committing database migration")?;
    report.database_status = "database migration committed";
    Ok(())
}

fn database_storage_bytes(page_count: u64, page_size: u64) -> Result<u64> {
    let backup = page_count
        .checked_mul(page_size)
        .context("SQLite backup size overflows storage estimate")?;
    // Budget a backup plus two full-image journal/WAL equivalents and 16 MiB
    // for schema growth and headers. Future migrations must review this bound.
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

fn storage_preflight(plan: &DatabasePlan) -> Result<()> {
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

fn reserve_backup(target: &Path) -> Result<(PathBuf, PathBuf, File)> {
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
        let published = target.with_file_name(&name);
        match fs::symlink_metadata(&published) {
            Ok(_) => continue,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Inspecting backup publication path"),
        }
        name.push(".partial");
        let path = target.with_file_name(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((path, published, file)),
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
