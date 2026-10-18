//! Process-wide diagnostics with weekly rolling file output.
//!
//! The permanent subscriber keeps stderr warnings/errors visible. A session owns
//! the optional non-blocking file worker and leases the process-wide file route;
//! shutting it down withdraws that route before flushing the worker.

use crate::config::{Config, LogLevel};
use anyhow::{Context, Result, ensure};
use chrono::{SecondsFormat, Utc};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};
use tracing::{Event, Level, Subscriber};
use tracing_appender::{
    non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard},
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{
    Layer,
    filter::{LevelFilter, dynamic_filter_fn},
    fmt::{self, FmtContext, FormatEvent, FormatFields, format::Writer},
    layer::SubscriberExt,
    registry::LookupSpan,
};

// Serialize the entire installation, including set_global_default. A failed
// installation leaves this empty so the next caller receives a real retry.
static INSTALLATION: Mutex<Option<Arc<RoutingState>>> = Mutex::new(None);

const WEEK_SECONDS: i64 = 7 * 24 * 60 * 60;
const SUNDAY_EPOCH_OFFSET: i64 = 3 * 24 * 60 * 60;
const LOG_PREFIX: &str = "rivu.log.";
const FILE_QUEUE_MESSAGES: usize = 4096;

struct RoutingState {
    level: AtomicU8,
    file_enabled: AtomicBool,
    sink: Arc<Mutex<LogSink>>,
    // Configuration/ownership serialization is separate from the writer lock:
    // preparing a worker and persisting config never block log producers.
    lease: Mutex<bool>,
}

impl RoutingState {
    fn new() -> Self {
        Self {
            level: AtomicU8::new(level_code(LogLevel::Warning)),
            file_enabled: AtomicBool::new(false),
            sink: Arc::new(Mutex::new(LogSink::Disabled)),
            lease: Mutex::new(false),
        }
    }
}

pub(crate) struct Session {
    state: Arc<SessionState>,
}

#[derive(Clone)]
pub(crate) struct Control {
    state: Arc<SessionState>,
}

struct SessionState {
    data_dir: PathBuf,
    routing: Arc<RoutingState>,
    settings: Mutex<SessionSettings>,
}

struct SessionSettings {
    active: bool,
    worker: Option<FileWorker>,
}

struct FileWorker {
    guard: WorkerGuard,
    dropped: ErrorCounter,
    retention_weeks: Arc<AtomicU64>,
}

impl FileWorker {
    fn retire(self) {
        // The route has already been withdrawn, and no lifecycle/sink lock is
        // held while WorkerGuard drains the queue and flushes the appender.
        drop(self.guard);
        let dropped = self.dropped.dropped_lines();
        if dropped != 0 {
            tracing::warn!(dropped_messages = dropped, "file logging queue overflow");
        }
    }
}

enum LogSink {
    Disabled,
    File(NonBlocking),
}

impl Write for LogSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Disabled => Ok(bytes.len()),
            Self::File(writer) => writer.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Disabled => Ok(()),
            Self::File(writer) => writer.flush(),
        }
    }
}

#[derive(Clone)]
struct SharedSink(Arc<Mutex<LogSink>>);

impl Write for SharedSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("logging sink mutex poisoned")
            .write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().expect("logging sink mutex poisoned").flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedSink {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

struct RetainingWriter {
    inner: RollingFileAppender,
    directory: PathBuf,
    retention_weeks: Arc<AtomicU64>,
    last_week: Option<i64>,
}

impl Write for RetainingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let result = self.inner.write(bytes);
        let week = current_week();
        if self.last_week != Some(week) {
            self.last_week = Some(week);
            cleanup_old_logs(
                &self.directory,
                self.retention_weeks.load(Ordering::Relaxed),
            );
        }
        result
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Clone, Copy)]
struct LineFormatter {
    ansi: bool,
}

impl<S, N> FormatEvent<S, N> for LineFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let metadata = event.metadata();
        let level = level_char(metadata.level());
        let level = if self.ansi {
            color_level(metadata.level(), level)
        } else {
            level.to_owned()
        };
        write!(
            writer,
            "{} {} {}: ",
            Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            level,
            metadata.target()
        )?;
        context.format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

fn install_once() -> Result<Arc<RoutingState>> {
    let mut installed = INSTALLATION
        .lock()
        .expect("logging installation mutex poisoned");
    if let Some(routing) = installed.as_ref() {
        return Ok(routing.clone());
    }
    let routing = Arc::new(RoutingState::new());
    tracing::subscriber::set_global_default(subscriber(routing.clone()))
        .context("installing Rivu logging subscriber")?;
    *installed = Some(routing.clone());
    Ok(routing)
}

fn subscriber(routing: Arc<RoutingState>) -> impl Subscriber + Send + Sync {
    let stderr_layer = fmt::layer()
        .event_format(LineFormatter {
            ansi: stderr_ansi(),
        })
        .with_writer(io::stderr)
        .with_filter(LevelFilter::WARN);
    let sink = routing.sink.clone();
    let file_layer = fmt::layer()
        .event_format(LineFormatter { ansi: false })
        .with_writer(SharedSink(sink))
        .with_filter(
            dynamic_filter_fn(move |metadata, _| {
                routing.file_enabled.load(Ordering::Relaxed)
                    && level_enabled(routing.level.load(Ordering::Relaxed), metadata.level())
            })
            .with_max_level_hint(LevelFilter::DEBUG),
        );

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
}

impl Session {
    pub(crate) fn start(data_dir: &Path, config: &Config) -> Result<Self> {
        let routing = install_once()?;
        Self::start_with_routing(data_dir, config, routing)
    }

    fn start_with_routing(
        data_dir: &Path,
        config: &Config,
        routing: Arc<RoutingState>,
    ) -> Result<Self> {
        let session = Self {
            state: Arc::new(SessionState {
                data_dir: data_dir.to_path_buf(),
                routing,
                settings: Mutex::new(SessionSettings {
                    active: true,
                    worker: None,
                }),
            }),
        };
        session.control().reconfigure(config)?;
        Ok(session)
    }

    pub(crate) fn control(&self) -> Control {
        Control {
            state: self.state.clone(),
        }
    }

    pub(crate) fn shutdown(&mut self) {
        let worker = {
            let mut settings = self
                .state
                .settings
                .lock()
                .expect("logging session mutex poisoned");
            if !settings.active {
                return;
            }
            settings.active = false;
            let worker = settings.worker.take();
            if worker.is_some() {
                let routing = &self.state.routing;
                let mut lease = routing.lease.lock().expect("logging lease mutex poisoned");
                debug_assert!(*lease);
                routing.file_enabled.store(false, Ordering::Release);
                *routing.sink.lock().expect("logging sink mutex poisoned") = LogSink::Disabled;
                *lease = false;
            }
            worker
        };
        if let Some(worker) = worker {
            worker.retire();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Control {
    pub(crate) fn reconfigure(&self, config: &Config) -> Result<()> {
        self.reconfigure_with(config, || Ok(()))
    }

    // Prepare resources before persistence, then commit without a fallible step.
    // Neither persistence failure nor a competing file owner changes the route.
    pub(crate) fn reconfigure_with(
        &self,
        config: &Config,
        persist: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let routing = &self.state.routing;
        let mut prepared = None;
        let mut retired = None;
        let result = (|| {
            let mut settings = self
                .state
                .settings
                .lock()
                .expect("logging session mutex poisoned");
            ensure!(settings.active, "logging session has shut down");
            let mut lease = routing.lease.lock().expect("logging lease mutex poisoned");
            let retention = u64::from(config.log_retention_weeks);
            if config.log_to_file && settings.worker.is_none() {
                ensure!(
                    !*lease,
                    "file logging is already owned by another active session"
                );
                prepared = Some(make_file_writer(&self.state.data_dir, retention)?);
            }

            persist()?;

            if config.log_to_file {
                if let Some((writer, worker)) = prepared.take() {
                    cleanup_old_logs(&self.state.data_dir.join("logs"), retention);
                    *routing.sink.lock().expect("logging sink mutex poisoned") =
                        LogSink::File(writer);
                    *lease = true;
                    settings.worker = Some(worker);
                } else if let Some(worker) = settings.worker.as_ref()
                    && worker.retention_weeks.load(Ordering::Relaxed) != retention
                {
                    worker.retention_weeks.store(retention, Ordering::Relaxed);
                    cleanup_old_logs(&self.state.data_dir.join("logs"), retention);
                }
                routing
                    .level
                    .store(level_code(config.log_level), Ordering::Relaxed);
                routing.file_enabled.store(true, Ordering::Release);
            } else if settings.worker.is_some() {
                routing.file_enabled.store(false, Ordering::Release);
                *routing.sink.lock().expect("logging sink mutex poisoned") = LogSink::Disabled;
                *lease = false;
                retired = settings.worker.take();
            }
            Ok(())
        })();
        // Also retire a prepared worker after failed persistence, outside locks.
        if let Some((_, worker)) = prepared {
            worker.retire();
        }
        if let Some(worker) = retired {
            worker.retire();
        }
        result
    }
}

fn make_file_writer(data_dir: &Path, retention: u64) -> Result<(NonBlocking, FileWorker)> {
    let directory = data_dir.join("logs");
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating log directory {}", directory.display()))?;
    // The convenience weekly() constructor panics on open failure. Preparation
    // must instead return the real error without touching the previous route.
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::WEEKLY)
        .filename_prefix("rivu.log")
        .build(&directory)
        .with_context(|| format!("opening log file in {}", directory.display()))?;
    let retention_weeks = Arc::new(AtomicU64::new(retention));
    let writer = RetainingWriter {
        inner: appender,
        directory,
        retention_weeks: retention_weeks.clone(),
        last_week: Some(current_week()),
    };
    let (writer, guard) = NonBlockingBuilder::default()
        .buffered_lines_limit(FILE_QUEUE_MESSAGES)
        .lossy(true)
        .finish(writer);
    let worker = FileWorker {
        guard,
        dropped: writer.error_counter(),
        retention_weeks,
    };
    Ok((writer, worker))
}

fn cleanup_old_logs(directory: &Path, retention_weeks: u64) {
    let cutoff = SystemTime::now().checked_sub(Duration::from_secs(
        retention_weeks.saturating_mul(7 * 24 * 60 * 60),
    ));
    let Some(cutoff) = cutoff else {
        return;
    };
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_log = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(LOG_PREFIX));
        if !is_log || !path.is_file() {
            continue;
        }
        let old = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified < cutoff);
        if old {
            let _ = fs::remove_file(path);
        }
    }
}

fn current_week() -> i64 {
    (Utc::now().timestamp() - SUNDAY_EPOCH_OFFSET).div_euclid(WEEK_SECONDS)
}

fn level_code(level: LogLevel) -> u8 {
    match level {
        LogLevel::Debug => 0,
        LogLevel::Info => 1,
        LogLevel::Warning => 2,
        LogLevel::Error => 3,
    }
}

fn level_enabled(configured: u8, level: &Level) -> bool {
    match *level {
        Level::ERROR => configured <= 3,
        Level::WARN => configured <= 2,
        Level::INFO => configured <= 1,
        Level::DEBUG => configured == 0,
        Level::TRACE => false,
    }
}

fn level_char(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "E",
        Level::WARN => "W",
        Level::INFO => "I",
        Level::DEBUG | Level::TRACE => "D",
    }
}

fn color_level(level: &Level, value: &str) -> String {
    let color = match *level {
        Level::ERROR => "1;31",
        Level::WARN => "33",
        Level::INFO => "34",
        Level::DEBUG | Level::TRACE => "2;36",
    };
    format!("\x1b[{color}m{value}\x1b[0m")
}

fn stderr_ansi() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    match std::env::var("RIVU_COLOR").ok().as_deref() {
        Some("always") => true,
        Some("never") => false,
        _ => std::io::IsTerminal::is_terminal(&io::stderr()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn harness() -> (Arc<RoutingState>, tracing::Dispatch) {
        let routing = Arc::new(RoutingState::new());
        let dispatch = tracing::Dispatch::new(subscriber(routing.clone()));
        (routing, dispatch)
    }

    fn file_config(level: LogLevel) -> Config {
        Config {
            log_to_file: true,
            log_level: level,
            ..Config::default()
        }
    }

    fn log_contents(data_dir: &Path) -> String {
        let mut paths: Vec<_> = fs::read_dir(data_dir.join("logs"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        paths.sort();
        paths
            .into_iter()
            .map(|path| fs::read_to_string(path).unwrap())
            .collect()
    }

    fn emit(dispatch: &tracing::Dispatch, message: &str) {
        tracing::dispatcher::with_default(dispatch, || {
            tracing::debug!(target: "logging_test", "debug {message}");
            tracing::error!(target: "logging_test", "error {message}");
        });
    }

    #[test]
    fn existing_callsites_follow_live_level_and_enable_changes() {
        let (routing, dispatch) = harness();
        let directory = tempfile::tempdir().unwrap();
        let mut session =
            Session::start_with_routing(directory.path(), &Config::default(), routing).unwrap();
        let control = session.control();
        emit(&dispatch, "disabled initially");
        control.reconfigure(&file_config(LogLevel::Error)).unwrap();
        emit(&dispatch, "error only");
        control.reconfigure(&file_config(LogLevel::Debug)).unwrap();
        emit(&dispatch, "debug enabled");
        control.reconfigure(&file_config(LogLevel::Error)).unwrap();
        emit(&dispatch, "debug disabled again");
        control.reconfigure(&Config::default()).unwrap();
        emit(&dispatch, "file disabled");
        control.reconfigure(&file_config(LogLevel::Debug)).unwrap();
        emit(&dispatch, "file reenabled");
        session.shutdown();
        let contents = log_contents(directory.path());
        assert!(contents.contains("error error only"));
        assert!(!contents.contains("debug error only"));
        assert!(contents.contains("debug debug enabled"));
        assert!(!contents.contains("debug debug disabled again"));
        assert!(!contents.contains("file disabled"));
        assert!(!contents.contains("disabled initially"));
        assert!(contents.contains("debug file reenabled"));
    }

    #[test]
    fn failed_enable_preserves_active_writer_and_filter() {
        let (routing, dispatch) = harness();
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let mut first = Session::start_with_routing(
            first_dir.path(),
            &file_config(LogLevel::Error),
            routing.clone(),
        )
        .unwrap();
        let _second =
            Session::start_with_routing(second_dir.path(), &Config::default(), routing).unwrap();
        let persisted = AtomicBool::new(false);
        let error = _second
            .control()
            .reconfigure_with(&file_config(LogLevel::Debug), || {
                persisted.store(true, Ordering::Relaxed);
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("another active session"));
        assert!(!persisted.load(Ordering::Relaxed));
        emit(&dispatch, "after rejected enable");
        first.shutdown();
        let contents = log_contents(first_dir.path());
        assert!(contents.contains("error after rejected enable"));
        assert!(!contents.contains("debug after rejected enable"));
        assert!(!second_dir.path().join("logs").exists());
    }

    #[test]
    fn failed_persistence_keeps_writer_and_config() {
        let (routing, dispatch) = harness();
        let directory = tempfile::tempdir().unwrap();
        let mut session =
            Session::start_with_routing(directory.path(), &file_config(LogLevel::Error), routing)
                .unwrap();
        let rejected = Config {
            log_retention_weeks: 1,
            ..file_config(LogLevel::Debug)
        };
        let error = session
            .control()
            .reconfigure_with(&rejected, || anyhow::bail!("configuration save rejected"))
            .unwrap_err();
        assert_eq!(error.to_string(), "configuration save rejected");
        emit(&dispatch, "after rejected save");
        // A rejected disable must leave the same writer available as well.
        assert!(
            session
                .control()
                .reconfigure_with(&Config::default(), || {
                    anyhow::bail!("configuration save rejected")
                })
                .is_err()
        );
        emit(&dispatch, "after rejected disable");
        session.shutdown();
        let contents = log_contents(directory.path());
        assert!(contents.contains("error after rejected save"));
        assert!(contents.contains("error after rejected disable"));
        assert!(!contents.contains("debug after rejected"));
    }

    #[test]
    fn failed_preparation_does_not_persist_and_can_retry() {
        let (routing, dispatch) = harness();
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("logs"), "not a directory").unwrap();
        let mut session =
            Session::start_with_routing(directory.path(), &Config::default(), routing).unwrap();
        let persisted = AtomicBool::new(false);
        let enabled = file_config(LogLevel::Debug);
        let error = session
            .control()
            .reconfigure_with(&enabled, || {
                persisted.store(true, Ordering::Relaxed);
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("creating log directory"));
        assert!(!persisted.load(Ordering::Relaxed));
        fs::remove_file(directory.path().join("logs")).unwrap();
        session.control().reconfigure(&enabled).unwrap();
        emit(&dispatch, "after retry");
        session.shutdown();
        assert!(log_contents(directory.path()).contains("debug after retry"));
    }

    #[test]
    fn failed_enable_persistence_leaves_lease_available() {
        let (routing, dispatch) = harness();
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let first =
            Session::start_with_routing(first_dir.path(), &Config::default(), routing.clone())
                .unwrap();
        let config = file_config(LogLevel::Debug);
        assert!(
            first
                .control()
                .reconfigure_with(&config, || { anyhow::bail!("configuration save rejected") })
                .is_err()
        );
        let mut second = Session::start_with_routing(second_dir.path(), &config, routing).unwrap();
        emit(&dispatch, "new owner after rejected save");
        second.shutdown();
        assert!(log_contents(first_dir.path()).is_empty());
        assert!(log_contents(second_dir.path()).contains("debug new owner after rejected save"));
    }

    #[test]
    fn shutdown_flushes_releases_lease_and_rejects_stale_control() {
        let (routing, dispatch) = harness();
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let config = file_config(LogLevel::Debug);
        let mut first =
            Session::start_with_routing(first_dir.path(), &config, routing.clone()).unwrap();
        let control = first.control();
        let stale = control.clone();
        let mut second =
            Session::start_with_routing(second_dir.path(), &Config::default(), routing).unwrap();
        tracing::dispatcher::with_default(&dispatch, || {
            for index in 0..64 {
                tracing::debug!(target: "logging_test", "queued-before-shutdown-{index}");
            }
        });
        first.shutdown();
        let contents = log_contents(first_dir.path());
        for index in 0..64 {
            assert!(contents.contains(&format!("queued-before-shutdown-{index}\n")));
        }
        let error = stale.reconfigure(&config).unwrap_err();
        assert!(error.to_string().contains("shut down"));
        assert!(
            control
                .reconfigure_with(&config, || panic!("must not persist after shutdown"))
                .is_err()
        );
        second.control().reconfigure(&config).unwrap();
        first.shutdown();
        emit(&dispatch, "second owner");
        second.shutdown();
        assert!(log_contents(second_dir.path()).contains("debug second owner"));
        assert!(!log_contents(first_dir.path()).contains("second owner"));
    }

    #[test]
    fn disabling_and_dropping_owners_release_the_file_lease() {
        let (routing, dispatch) = harness();
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let config = file_config(LogLevel::Debug);
        let mut first =
            Session::start_with_routing(first_dir.path(), &config, routing.clone()).unwrap();
        emit(&dispatch, "before disable");
        first.control().reconfigure(&Config::default()).unwrap();
        assert!(log_contents(first_dir.path()).contains("debug before disable"));
        let second = Session::start_with_routing(second_dir.path(), &config, routing).unwrap();
        let stale = second.control();
        emit(&dispatch, "before owner drop");
        drop(second);
        assert!(log_contents(second_dir.path()).contains("debug before owner drop"));
        assert!(stale.reconfigure(&config).is_err());
        first.control().reconfigure(&config).unwrap();
        emit(&dispatch, "after owner drop");
        first.shutdown();
        assert!(log_contents(first_dir.path()).contains("debug after owner drop"));
    }

    #[test]
    fn levels_use_threshold_order() {
        assert!(level_enabled(0, &Level::DEBUG));
        assert!(level_enabled(1, &Level::INFO));
        assert!(!level_enabled(1, &Level::DEBUG));
        assert!(level_enabled(2, &Level::ERROR));
        assert!(!level_enabled(3, &Level::WARN));
    }
}
