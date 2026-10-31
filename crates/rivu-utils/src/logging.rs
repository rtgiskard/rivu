//! Process-wide diagnostics with weekly rolling file output.
//!
//! The permanent subscriber keeps stderr warnings/errors visible. A session owns
//! the optional non-blocking file worker and leases the process-wide file route;
//! shutting it down withdraws that route before flushing the worker.

use anyhow::{Context, Result, ensure};
use chrono::{SecondsFormat, Utc};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
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

#[derive(Clone, Copy)]
pub enum LogLevel {
    Debug,
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy)]
pub struct Options {
    pub level: LogLevel,
    pub to_file: bool,
    pub retention_weeks: u32,
}

struct RoutingState {
    level: AtomicU8,
    level_override: Option<u8>,
    file_enabled: AtomicBool,
    sink: Arc<Mutex<LogSink>>,
    // Configuration/ownership serialization is separate from the writer lock:
    // preparing a worker and persisting config never block log producers.
    lease: Mutex<bool>,
}

impl RoutingState {
    fn new(level_override: Option<LogLevel>) -> Self {
        Self {
            level: AtomicU8::new(level_code(LogLevel::Warning)),
            level_override: level_override.map(level_code),
            file_enabled: AtomicBool::new(false),
            sink: Arc::new(Mutex::new(LogSink::Disabled)),
            lease: Mutex::new(false),
        }
    }
}

pub struct Session {
    state: Arc<SessionState>,
}

#[derive(Clone)]
pub struct Control {
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

struct RetentionState {
    weeks: AtomicU64,
    committed: AtomicBool,
}

struct FileWorker {
    guard: WorkerGuard,
    dropped: ErrorCounter,
    retention: Arc<RetentionState>,
    wake: NonBlocking,
    finished: Receiver<()>,
}

impl FileWorker {
    fn set_retention(&mut self, weeks: u64) {
        self.retention.weeks.store(weeks, Ordering::Relaxed);
        self.retention.committed.store(true, Ordering::Release);
        // NonBlocking enqueues even an empty write_all. The appender worker
        // flushes that batch, so idle sessions need no first business event.
        // If its bounded queue is full, an existing batch supplies the wakeup.
        let _ = self.wake.write_all(&[]);
    }

    fn retire(self) {
        // Withdraw the route and drop every producer before retiring. The
        // guard's bounded shutdown wait can expire during filesystem cleanup;
        // wait for the writer itself to close, without holding lifecycle locks.
        drop(self.wake);
        drop(self.guard);
        let _ = self.finished.recv();
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
    retention: Arc<RetentionState>,
    last_cleanup: Option<(i64, u64)>,
    // Last field: disconnect only after the appender/file has been dropped.
    _finished: SyncSender<()>,
}

impl RetainingWriter {
    fn cleanup_if_needed(&mut self) {
        // Preparation must not delete files before persistence succeeds.
        if !self.retention.committed.load(Ordering::Acquire) {
            return;
        }
        let retention = self.retention.weeks.load(Ordering::Relaxed);
        let cleanup = (current_week(), retention);
        if self.last_cleanup != Some(cleanup) {
            cleanup_old_logs(&self.directory, retention);
            self.last_cleanup = Some(cleanup);
        }
    }
}

impl Write for RetainingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let result = self.inner.write(bytes);
        self.cleanup_if_needed();
        result
    }

    fn flush(&mut self) -> io::Result<()> {
        let result = self.inner.flush();
        self.cleanup_if_needed();
        result
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

/// Install logging with an optional process-lifetime override for both outputs.
/// The first initialization wins; configuration changes cannot replace the override.
pub fn init(level_override: Option<LogLevel>) -> Result<()> {
    install_once(level_override).map(|_| ())
}

fn install_once(level_override: Option<LogLevel>) -> Result<Arc<RoutingState>> {
    let mut installed = INSTALLATION
        .lock()
        .expect("logging installation mutex poisoned");
    if let Some(routing) = installed.as_ref() {
        return Ok(routing.clone());
    }
    let routing = Arc::new(RoutingState::new(level_override));
    tracing::subscriber::set_global_default(subscriber(routing.clone()))
        .context("installing Rivu logging subscriber")?;
    *installed = Some(routing.clone());
    Ok(routing)
}

fn subscriber(routing: Arc<RoutingState>) -> impl Subscriber + Send + Sync {
    let stderr_level = routing
        .level_override
        .unwrap_or(level_code(LogLevel::Warning));
    let stderr_ansi = stderr_ansi();
    let stderr_layer = fmt::layer()
        .event_format(LineFormatter { ansi: stderr_ansi })
        .with_ansi(stderr_ansi)
        .with_writer(io::stderr)
        .with_filter(
            dynamic_filter_fn(move |metadata, _| level_enabled(stderr_level, metadata.level()))
                .with_max_level_hint(LevelFilter::DEBUG),
        );
    let sink = routing.sink.clone();
    let file_layer = fmt::layer()
        .event_format(LineFormatter { ansi: false })
        .with_ansi(false)
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
    pub fn start(data_dir: &Path, options: &Options) -> Result<Self> {
        let routing = install_once(None)?;
        Self::start_with_routing(data_dir, options, routing)
    }

    fn start_with_routing(
        data_dir: &Path,
        options: &Options,
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
        session.control().reconfigure_with(options, || Ok(()))?;
        Ok(session)
    }

    pub fn control(&self) -> Control {
        Control {
            state: self.state.clone(),
        }
    }

    pub fn shutdown(&mut self) {
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
    // Prepare resources before persistence, then commit without a fallible step.
    // Neither persistence failure nor a competing file owner changes the route.
    pub fn reconfigure_with(
        &self,
        options: &Options,
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
            let retention = u64::from(options.retention_weeks);
            if options.to_file && settings.worker.is_none() {
                ensure!(
                    !*lease,
                    "file logging is already owned by another active session"
                );
                prepared = Some(make_file_writer(&self.state.data_dir, retention)?);
            }

            persist()?;

            if options.to_file {
                if let Some((writer, mut worker)) = prepared.take() {
                    worker.set_retention(retention);
                    *routing.sink.lock().expect("logging sink mutex poisoned") =
                        LogSink::File(writer);
                    *lease = true;
                    settings.worker = Some(worker);
                } else if let Some(worker) = settings.worker.as_mut()
                    && worker.retention.weeks.load(Ordering::Relaxed) != retention
                {
                    worker.set_retention(retention);
                }
                routing.level.store(
                    routing
                        .level_override
                        .unwrap_or_else(|| level_code(options.level)),
                    Ordering::Relaxed,
                );
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
        if let Some((writer, worker)) = prepared {
            drop(writer);
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
    let retention = Arc::new(RetentionState {
        weeks: AtomicU64::new(retention),
        committed: AtomicBool::new(false),
    });
    let (finished, completion) = mpsc::sync_channel(0);
    let writer = RetainingWriter {
        inner: appender,
        directory,
        retention: retention.clone(),
        last_cleanup: None,
        _finished: finished,
    };
    let (writer, guard) = NonBlockingBuilder::default()
        .buffered_lines_limit(FILE_QUEUE_MESSAGES)
        .lossy(true)
        .finish(writer);
    let worker = FileWorker {
        guard,
        dropped: writer.error_counter(),
        retention,
        wake: writer.clone(),
        finished: completion,
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
        let routing = Arc::new(RoutingState::new(None));
        let dispatch = tracing::Dispatch::new(subscriber(routing.clone()));
        (routing, dispatch)
    }

    fn disabled_options() -> Options {
        Options {
            level: LogLevel::Warning,
            to_file: false,
            retention_weeks: 4,
        }
    }

    fn file_options(level: LogLevel) -> Options {
        Options {
            level,
            to_file: true,
            retention_weeks: 4,
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
    fn file_logs_keep_structured_fields_free_of_ansi() {
        let (routing, dispatch) = harness();
        let directory = tempfile::tempdir().unwrap();
        let mut session =
            Session::start_with_routing(directory.path(), &file_options(LogLevel::Error), routing)
                .unwrap();
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::error!(target: "logging_test", operation = "scan", "operation failed");
        });
        session.shutdown();
        let contents = log_contents(directory.path());
        assert!(
            contents.contains("operation failed operation=\"scan\""),
            "{contents:?}"
        );
        assert!(
            contents
                .chars()
                .all(|character| !character.is_control() || character == '\n'),
            "{contents:?}"
        );
    }

    #[test]
    fn process_override_survives_session_start_reconfiguration_and_file_toggle() {
        for override_level in [LogLevel::Debug, LogLevel::Error] {
            let routing = Arc::new(RoutingState::new(Some(override_level)));
            let dispatch = tracing::Dispatch::new(subscriber(routing.clone()));
            let directory = tempfile::tempdir().unwrap();
            let mut session = Session::start_with_routing(
                directory.path(),
                &file_options(LogLevel::Warning),
                routing,
            )
            .unwrap();
            let control = session.control();
            emit(&dispatch, "startup");
            for (level, message) in [
                (LogLevel::Error, "configured error"),
                (LogLevel::Debug, "configured debug"),
            ] {
                control
                    .reconfigure_with(&file_options(level), || Ok(()))
                    .unwrap();
                emit(&dispatch, message);
            }
            control
                .reconfigure_with(&disabled_options(), || Ok(()))
                .unwrap();
            emit(&dispatch, "disabled");
            control
                .reconfigure_with(&file_options(LogLevel::Info), || Ok(()))
                .unwrap();
            emit(&dispatch, "reenabled");
            session.shutdown();
            let contents = log_contents(directory.path());
            for message in [
                "startup",
                "configured error",
                "configured debug",
                "reenabled",
            ] {
                assert!(contents.contains(&format!("error {message}")));
                assert_eq!(
                    contents.contains(&format!("debug {message}")),
                    matches!(override_level, LogLevel::Debug),
                );
            }
            assert!(!contents.contains("disabled"));
        }
    }

    fn aged_log(data_dir: &Path, name: &str, weeks: u64) -> PathBuf {
        let directory = data_dir.join("logs");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(name);
        let file = fs::File::create(&path).unwrap();
        let modified = SystemTime::now()
            .checked_sub(Duration::from_secs(weeks * 7 * 24 * 60 * 60))
            .unwrap();
        file.set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        path
    }

    #[test]
    fn empty_batches_trigger_startup_and_retention_cleanup_on_idle_worker() {
        // Observe real appender flushes, not business log writes or polling.
        struct ObservedWriter {
            inner: RetainingWriter,
            flushed: SyncSender<()>,
        }

        impl Write for ObservedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.inner.write(bytes)
            }

            fn flush(&mut self) -> io::Result<()> {
                let result = self.inner.flush();
                let _ = self.flushed.try_send(());
                result
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let oldest = aged_log(directory.path(), "rivu.log.oldest", 12);
        let older = aged_log(directory.path(), "rivu.log.older", 6);
        let unrelated = aged_log(directory.path(), "unrelated", 12);
        let retention = Arc::new(RetentionState {
            weeks: AtomicU64::new(8),
            committed: AtomicBool::new(false),
        });
        let (finished, completion) = mpsc::sync_channel(0);
        let (flushed, flushes) = mpsc::sync_channel(1);
        let writer = ObservedWriter {
            inner: RetainingWriter {
                inner: RollingFileAppender::builder()
                    .rotation(Rotation::WEEKLY)
                    .filename_prefix("rivu.log")
                    .build(directory.path().join("logs"))
                    .unwrap(),
                directory: directory.path().join("logs"),
                retention: retention.clone(),
                last_cleanup: None,
                _finished: finished,
            },
            flushed,
        };
        let (writer, guard) = NonBlockingBuilder::default()
            .buffered_lines_limit(FILE_QUEUE_MESSAGES)
            .finish(writer);
        let mut worker = FileWorker {
            guard,
            dropped: writer.error_counter(),
            retention,
            wake: writer.clone(),
            finished: completion,
        };
        worker.set_retention(8);
        flushes.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!oldest.exists());
        assert!(older.exists());
        worker.set_retention(4);
        flushes.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!older.exists());
        assert!(unrelated.exists());
        assert_eq!(worker.dropped.dropped_lines(), 0);
        drop(writer);
        worker.retire();
    }

    #[test]
    fn shutdown_finishes_startup_cleanup_without_business_logs() {
        let (routing, _) = harness();
        let directory = tempfile::tempdir().unwrap();
        let old = aged_log(directory.path(), "rivu.log.old", 6);
        let mut session = Session::start_with_routing(
            directory.path(),
            &file_options(LogLevel::Warning),
            routing,
        )
        .unwrap();
        session.shutdown();
        assert!(!old.exists());
        assert!(log_contents(directory.path()).is_empty());
    }

    #[test]
    fn rejected_enable_does_not_clean_up_prepared_writer() {
        let (routing, _) = harness();
        let directory = tempfile::tempdir().unwrap();
        let old = aged_log(directory.path(), "rivu.log.old", 6);
        let session =
            Session::start_with_routing(directory.path(), &disabled_options(), routing).unwrap();
        assert!(
            session
                .control()
                .reconfigure_with(&file_options(LogLevel::Warning), || {
                    anyhow::bail!("configuration save rejected")
                })
                .is_err()
        );
        assert!(old.exists());
    }

    #[test]
    fn disabled_startup_does_not_create_log_directory_or_worker() {
        let (routing, _) = harness();
        let directory = tempfile::tempdir().unwrap();
        let session =
            Session::start_with_routing(directory.path(), &disabled_options(), routing).unwrap();
        assert!(!directory.path().join("logs").exists());
        assert!(session.state.settings.lock().unwrap().worker.is_none());
    }

    #[test]
    fn retention_reconfiguration_keeps_rejected_policy_and_applies_committed_policy() {
        let (routing, _) = harness();
        let directory = tempfile::tempdir().unwrap();
        let old = aged_log(directory.path(), "rivu.log.old", 6);
        let initial = Options {
            retention_weeks: 8,
            ..file_options(LogLevel::Warning)
        };
        let mut session =
            Session::start_with_routing(directory.path(), &initial, routing.clone()).unwrap();
        assert!(
            session
                .control()
                .reconfigure_with(&file_options(LogLevel::Warning), || {
                    anyhow::bail!("configuration save rejected")
                })
                .is_err()
        );
        session.shutdown();
        assert!(old.exists());

        let mut session = Session::start_with_routing(directory.path(), &initial, routing).unwrap();
        session
            .control()
            .reconfigure_with(&file_options(LogLevel::Warning), || Ok(()))
            .unwrap();
        session.shutdown();
        assert!(!old.exists());
    }

    #[test]
    fn retirement_waits_for_physical_writer_shutdown_after_guard_timeout() {
        struct BlockedWriter {
            entered: Option<SyncSender<()>>,
            release: Receiver<()>,
            _finished: SyncSender<()>,
        }

        impl Write for BlockedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                if let Some(entered) = self.entered.take() {
                    entered.send(()).unwrap();
                    self.release.recv().unwrap();
                }
                Ok(())
            }
        }

        let (entered, started) = mpsc::sync_channel(1);
        let (release, blocked) = mpsc::sync_channel(1);
        let (finished, completion) = mpsc::sync_channel(0);
        let (writer, guard) = NonBlockingBuilder::default().finish(BlockedWriter {
            entered: Some(entered),
            release: blocked,
            _finished: finished,
        });
        let mut worker = FileWorker {
            guard,
            dropped: writer.error_counter(),
            retention: Arc::new(RetentionState {
                weeks: AtomicU64::new(4),
                committed: AtomicBool::new(false),
            }),
            wake: writer.clone(),
            finished: completion,
        };
        worker.set_retention(4);
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(writer);
        let (retired, retired_receiver) = mpsc::sync_channel(1);
        let retiring = std::thread::spawn(move || {
            worker.retire();
            retired.send(()).unwrap();
        });
        // tracing-appender's guard waits at most one second after enqueueing
        // shutdown. Physical retirement must still wait for our blocked flush.
        let result = retired_receiver.recv_timeout(Duration::from_millis(1200));
        release.send(()).unwrap();
        retiring.join().unwrap();
        assert!(matches!(result, Err(mpsc::RecvTimeoutError::Timeout)));
        retired_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
    }

    #[test]
    fn existing_callsites_follow_live_level_and_enable_changes() {
        let (routing, dispatch) = harness();
        let directory = tempfile::tempdir().unwrap();
        let mut session =
            Session::start_with_routing(directory.path(), &disabled_options(), routing).unwrap();
        let control = session.control();
        emit(&dispatch, "disabled initially");
        control
            .reconfigure_with(&file_options(LogLevel::Error), || Ok(()))
            .unwrap();
        emit(&dispatch, "error only");
        control
            .reconfigure_with(&file_options(LogLevel::Debug), || Ok(()))
            .unwrap();
        emit(&dispatch, "debug enabled");
        control
            .reconfigure_with(&file_options(LogLevel::Error), || Ok(()))
            .unwrap();
        emit(&dispatch, "debug disabled again");
        control
            .reconfigure_with(&disabled_options(), || Ok(()))
            .unwrap();
        emit(&dispatch, "file disabled");
        control
            .reconfigure_with(&file_options(LogLevel::Debug), || Ok(()))
            .unwrap();
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
            &file_options(LogLevel::Error),
            routing.clone(),
        )
        .unwrap();
        let _second =
            Session::start_with_routing(second_dir.path(), &disabled_options(), routing).unwrap();
        let persisted = AtomicBool::new(false);
        let error = _second
            .control()
            .reconfigure_with(&file_options(LogLevel::Debug), || {
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
            Session::start_with_routing(directory.path(), &file_options(LogLevel::Error), routing)
                .unwrap();
        let rejected = Options {
            retention_weeks: 1,
            ..file_options(LogLevel::Debug)
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
                .reconfigure_with(&disabled_options(), || {
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
            Session::start_with_routing(directory.path(), &disabled_options(), routing).unwrap();
        let persisted = AtomicBool::new(false);
        let enabled = file_options(LogLevel::Debug);
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
        session
            .control()
            .reconfigure_with(&enabled, || Ok(()))
            .unwrap();
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
            Session::start_with_routing(first_dir.path(), &disabled_options(), routing.clone())
                .unwrap();
        let config = file_options(LogLevel::Debug);
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
        let config = file_options(LogLevel::Debug);
        let mut first =
            Session::start_with_routing(first_dir.path(), &config, routing.clone()).unwrap();
        let control = first.control();
        let stale = control.clone();
        let mut second =
            Session::start_with_routing(second_dir.path(), &disabled_options(), routing).unwrap();
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
        let error = stale.reconfigure_with(&config, || Ok(())).unwrap_err();
        assert!(error.to_string().contains("shut down"));
        assert!(
            control
                .reconfigure_with(&config, || panic!("must not persist after shutdown"))
                .is_err()
        );
        second
            .control()
            .reconfigure_with(&config, || Ok(()))
            .unwrap();
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
        let config = file_options(LogLevel::Debug);
        let mut first =
            Session::start_with_routing(first_dir.path(), &config, routing.clone()).unwrap();
        emit(&dispatch, "before disable");
        first
            .control()
            .reconfigure_with(&disabled_options(), || Ok(()))
            .unwrap();
        assert!(log_contents(first_dir.path()).contains("debug before disable"));
        let second = Session::start_with_routing(second_dir.path(), &config, routing).unwrap();
        let stale = second.control();
        emit(&dispatch, "before owner drop");
        drop(second);
        assert!(log_contents(second_dir.path()).contains("debug before owner drop"));
        assert!(stale.reconfigure_with(&config, || Ok(())).is_err());
        first
            .control()
            .reconfigure_with(&config, || Ok(()))
            .unwrap();
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
