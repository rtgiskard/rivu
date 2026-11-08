//! Process-wide diagnostics with weekly rolling file output.
//!
//! The permanent subscriber keeps stderr warnings/errors visible, using text or
//! JSON diagnostics selected at initialization. One session owns the optional
//! bounded, non-blocking file worker and reconfigures its reloadable
//! layer. The appender prunes matching logs to its file-count limit at construction
//! and rotation; count changes apply on the next enable or session start, without
//! rebuilding an active appender. Shutting down withdraws output and waits for the
//! writer to flush and close.

use anyhow::{Context, Result, ensure};
use chrono::Utc;
use serde::ser::SerializeMap;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
};
use tracing::{Event, Level, Subscriber};
use tracing_appender::{
    non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard},
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{
    Layer,
    filter::{Filtered, LevelFilter},
    fmt::{self, FmtContext, FormatEvent, FormatFields, format::Writer},
    layer::SubscriberExt,
    registry::{LookupSpan, Registry},
    reload,
};

/// Stderr diagnostics are independent of the permanent text file-log contract.
#[derive(Clone, Copy)]
pub enum StderrFormat {
    Text,
    Json,
}

// Serialize the entire installation, including set_global_default. A failed
// installation leaves this empty so the next caller receives a real retry.
static INSTALLATION: Mutex<Option<Arc<RoutingState>>> = Mutex::new(None);

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
    pub max_log_files: u32,
}

type FileLayer = Filtered<
    fmt::Layer<Registry, fmt::format::DefaultFields, LineFormatter, LogSink>,
    LevelFilter,
    Registry,
>;

struct RoutingState {
    file: reload::Handle<FileLayer, Registry>,
    level_override: Option<LevelFilter>,
    session_active: AtomicBool,
}

impl RoutingState {
    fn set_output(&self, level: LevelFilter, sink: Option<LogSink>) {
        // Avoid rebuilding tracing's callsite cache for unrelated settings.
        if sink.is_none()
            && self
                .file
                .with_current(|layer| *layer.filter() == level)
                .expect("logging subscriber unavailable")
        {
            return;
        }
        self.file
            .modify(|layer| {
                *layer.filter_mut() = level;
                if let Some(sink) = sink {
                    *layer.inner_mut().writer_mut() = sink;
                }
            })
            .expect("logging subscriber unavailable");
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

struct FileWorker {
    guard: WorkerGuard,
    dropped: ErrorCounter,
    finished: Receiver<()>,
}

impl FileWorker {
    fn retire(self) {
        // Withdraw the route and drop every producer before retiring. The
        // guard's bounded shutdown wait can expire while the writer flushes;
        // wait for the writer itself to close, without holding lifecycle locks.
        drop(self.guard);
        let _ = self.finished.recv();
        let dropped = self.dropped.dropped_lines();
        if dropped != 0 {
            tracing::warn!(dropped_messages = dropped, "file logging queue overflow");
        }
    }
}

#[derive(Clone)]
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

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

struct ClosingWriter {
    inner: RollingFileAppender,
    // Last field: disconnect only after the appender/file has been dropped.
    _finished: SyncSender<()>,
}

impl Write for ClosingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Clone, Copy)]
struct LineFormatter {
    ansi: bool,
    stderr: Option<StderrFormat>,
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
        let fields = EventFields { context, event };
        let metadata = event.metadata();
        if matches!(self.stderr, Some(StderrFormat::Json)) {
            let result = (|| -> serde_json::Result<()> {
                let mut serializer = serde_json::Serializer::new(FormatWriter(&mut writer));
                let mut object = serde::Serializer::serialize_map(&mut serializer, Some(4))?;
                object.serialize_entry(
                    "timestamp",
                    &DisplayValue(Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ")),
                )?;
                object.serialize_entry("level", metadata.level().as_str())?;
                object.serialize_entry("target", metadata.target())?;
                object.serialize_entry("error", &DisplayValue(fields))?;
                object.end()
            })();
            result.map_err(|_| std::fmt::Error)?;
            return writeln!(writer);
        }
        if self.ansi {
            write!(writer, "\x1b[{}m", level_color(metadata.level()))?;
        }
        Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .write_to(&mut writer)?;
        write!(writer, " {}", level_char(metadata.level()))?;
        if self.ansi {
            write!(writer, "\x1b[0m")?;
        }
        write!(writer, " {}: ", metadata.target())?;
        if matches!(self.stderr, Some(StderrFormat::Text)) {
            write!(writer, "{}", crate::terminal::TerminalEscaped(fields))?;
        } else {
            write!(writer, "{fields}")?;
        }
        writeln!(writer)
    }
}

struct EventFields<'a, 'context, 'event, S, N> {
    context: &'a FmtContext<'context, S, N>,
    event: &'a Event<'event>,
}

impl<S, N> std::fmt::Display for EventFields<'_, '_, '_, S, N>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.context
            .format_fields(Writer::new(formatter), self.event)
    }
}

struct DisplayValue<T>(T);

impl<T: std::fmt::Display> serde::Serialize for DisplayValue<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

// Bridge the existing formatting sink without buffering a second diagnostic.
struct FormatWriter<'a, 'writer>(&'a mut Writer<'writer>);

impl Write for FormatWriter<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
        std::fmt::Write::write_str(self.0, text).map_err(io::Error::other)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Install logging with an optional process-lifetime override for both outputs.
/// The first initialization wins, including stderr format; configuration changes
/// cannot replace the override or change the file log's text format.
pub fn init(level_override: Option<LogLevel>, stderr: StderrFormat) -> Result<()> {
    install_once(level_override, stderr).map(|_| ())
}

fn install_once(
    level_override: Option<LogLevel>,
    stderr: StderrFormat,
) -> Result<Arc<RoutingState>> {
    let mut installed = INSTALLATION
        .lock()
        .expect("logging installation mutex poisoned");
    if let Some(routing) = installed.as_ref() {
        return Ok(routing.clone());
    }
    let (subscriber, routing) = subscriber(level_override, stderr);
    tracing::subscriber::set_global_default(subscriber)
        .context("installing Rivu logging subscriber")?;
    *installed = Some(routing.clone());
    Ok(routing)
}

fn subscriber(
    level_override: Option<LogLevel>,
    stderr: StderrFormat,
) -> (impl Subscriber + Send + Sync, Arc<RoutingState>) {
    let level_override = level_override.map(level_filter);
    let stderr_ansi = matches!(stderr, StderrFormat::Text) && stderr_ansi();
    let stderr_layer = fmt::layer()
        .event_format(LineFormatter {
            ansi: stderr_ansi,
            stderr: Some(stderr),
        })
        .with_ansi(false)
        .with_writer(io::stderr)
        .with_filter(level_override.unwrap_or(LevelFilter::WARN));
    let file_layer = fmt::layer()
        .event_format(LineFormatter {
            ansi: false,
            stderr: None,
        })
        .with_ansi(false)
        .with_writer(LogSink::Disabled)
        .with_filter(LevelFilter::OFF);
    let (file_layer, file) = reload::Layer::new(file_layer);
    let routing = Arc::new(RoutingState {
        file,
        level_override,
        session_active: AtomicBool::new(false),
    });
    let subscriber = tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer);
    (subscriber, routing)
}

impl Session {
    /// Start the process's only active session, even when file output is disabled.
    /// A new session may start after the previous one finishes shutting down.
    pub fn start(data_dir: &Path, options: &Options) -> Result<Self> {
        let routing = install_once(None, StderrFormat::Text)?;
        Self::start_with_routing(data_dir, options, routing)
    }

    fn start_with_routing(
        data_dir: &Path,
        options: &Options,
        routing: Arc<RoutingState>,
    ) -> Result<Self> {
        ensure!(
            routing
                .session_active
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "another logging session is already active"
        );
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
            if settings.worker.is_some() {
                self.state
                    .routing
                    .set_output(LevelFilter::OFF, Some(LogSink::Disabled));
            }
            settings.worker.take()
        };
        if let Some(worker) = worker {
            worker.retire();
        }
        self.state
            .routing
            .session_active
            .store(false, Ordering::Release);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Control {
    // Prepare resources before persistence, then commit without a fallible step.
    // Persistence failure leaves the current output and filter intact. Appender
    // preparation may already prune files; those filesystem changes are not undone.
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
            if options.to_file && settings.worker.is_none() {
                prepared = Some(make_file_writer(
                    &self.state.data_dir,
                    options.max_log_files,
                )?);
            }

            persist()?;

            if options.to_file {
                let sink = if let Some((writer, worker)) = prepared.take() {
                    settings.worker = Some(worker);
                    Some(LogSink::File(writer))
                } else {
                    None
                };
                routing.set_output(
                    routing
                        .level_override
                        .unwrap_or_else(|| level_filter(options.level)),
                    sink,
                );
            } else if settings.worker.is_some() {
                routing.set_output(LevelFilter::OFF, Some(LogSink::Disabled));
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

fn make_file_writer(data_dir: &Path, max_log_files: u32) -> Result<(NonBlocking, FileWorker)> {
    let directory = data_dir.join("logs");
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating log directory {}", directory.display()))?;
    // The convenience weekly() constructor panics on open failure. Preparation
    // must instead return the real error without touching the previous route.
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::WEEKLY)
        .filename_prefix("rivu.log")
        .max_log_files(max_log_files as usize)
        .build(&directory)
        .with_context(|| format!("opening log file in {}", directory.display()))?;
    let (finished, completion) = mpsc::sync_channel(0);
    let writer = ClosingWriter {
        inner: appender,
        _finished: finished,
    };
    let (writer, guard) = NonBlockingBuilder::default()
        .buffered_lines_limit(FILE_QUEUE_MESSAGES)
        .lossy(true)
        .finish(writer);
    let worker = FileWorker {
        guard,
        dropped: writer.error_counter(),
        finished: completion,
    };
    Ok((writer, worker))
}

fn level_filter(level: LogLevel) -> LevelFilter {
    match level {
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Warning => LevelFilter::WARN,
        LogLevel::Error => LevelFilter::ERROR,
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

fn level_color(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "1;31",
        Level::WARN => "33",
        Level::INFO => "34",
        Level::DEBUG | Level::TRACE => "36",
    }
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
    use std::time::Duration;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn diagnostic(stderr: Option<StderrFormat>) -> String {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .event_format(LineFormatter {
                    ansi: false,
                    stderr,
                })
                .with_ansi(false)
                .with_writer(captured.clone()),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(
                target: "logging_test",
                path = %"音楽/\x1b[2J\nfile\t\u{009b}",
                "failed {}", "\r\0detail"
            );
        });
        let bytes = captured.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn stderr_text_escapes_untrusted_fields_but_file_logs_remain_unchanged() {
        let text = diagnostic(Some(StderrFormat::Text));
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("failed \\r\\0detail"), "{text}");
        assert!(text.contains("音楽/\\x1b[2J\\nfile\\t\\u{009b}"), "{text}");
        assert!(!text.trim_end_matches('\n').chars().any(char::is_control));

        let file = diagnostic(None);
        assert!(file.contains("failed \r\0detail"));
        assert!(file.contains("音楽/\x1b[2J\nfile\t\u{009b}"));
    }

    #[test]
    fn stderr_json_retains_diagnostics_and_control_characters_as_json_data() {
        let json = diagnostic(Some(StderrFormat::Json));
        assert_eq!(json.lines().count(), 1);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["level"], "ERROR");
        assert_eq!(value["target"], "logging_test");
        assert!(value["timestamp"].as_str().unwrap().ends_with('Z'));
        let error = value["error"].as_str().unwrap();
        assert!(error.contains("failed \r\0detail"));
        assert!(error.contains("音楽/\x1b[2J\nfile\t\u{009b}"));
    }

    // Keep each session's dispatch as the thread default through reconfiguration
    // and shutdown: tracing's single-dispatch fast path rebuilds callsite interests
    // against the current default, not merely every live Dispatch.
    fn harness() -> (Arc<RoutingState>, tracing::Dispatch) {
        let (subscriber, routing) = subscriber(None, StderrFormat::Text);
        let dispatch = tracing::Dispatch::new(subscriber);
        (routing, dispatch)
    }

    fn disabled_options() -> Options {
        Options {
            level: LogLevel::Warning,
            to_file: false,
            max_log_files: 4,
        }
    }

    fn file_options(level: LogLevel) -> Options {
        Options {
            level,
            to_file: true,
            max_log_files: 4,
        }
    }

    fn log_contents(data_dir: &Path) -> String {
        fs::read_dir(data_dir.join("logs"))
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect()
    }

    fn emit(dispatch: &tracing::Dispatch, message: &str) {
        tracing::dispatcher::with_default(dispatch, || {
            tracing::debug!(target: "logging_test", operation = "scan", "debug {message}");
            tracing::error!(target: "logging_test", operation = "scan", "error {message}");
        });
    }

    #[test]
    fn process_override_survives_session_start_reconfiguration_and_file_toggle() {
        for override_level in [LogLevel::Debug, LogLevel::Error] {
            let (subscriber, routing) = subscriber(Some(override_level), StderrFormat::Text);
            let dispatch = tracing::Dispatch::new(subscriber);
            let _default = tracing::dispatcher::set_default(&dispatch);
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
        let worker = FileWorker {
            guard,
            dropped: writer.error_counter(),
            finished: completion,
        };
        let mut writer = writer;
        writer.write_all(b"queued before retirement").unwrap();
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
    fn live_reconfiguration_preserves_filtering_and_defers_count_changes() {
        let (routing, dispatch) = harness();
        let _default = tracing::dispatcher::set_default(&dispatch);
        let directory = tempfile::tempdir().unwrap();
        let logs = directory.path().join("logs");
        let mut session =
            Session::start_with_routing(directory.path(), &disabled_options(), routing).unwrap();
        assert!(!logs.exists());
        let control = session.control();
        emit(&dispatch, "initially disabled");
        control
            .reconfigure_with(&file_options(LogLevel::Error), || Ok(()))
            .unwrap();
        emit(&dispatch, "error only");
        for year in 2000..2003 {
            fs::write(logs.join(format!("rivu.log.{year}-01-02")), "historical").unwrap();
        }
        let unrelated = logs.join("unrelated");
        fs::write(&unrelated, "preserve").unwrap();
        let changed = Options {
            max_log_files: 1,
            ..file_options(LogLevel::Debug)
        };
        control.reconfigure_with(&changed, || Ok(())).unwrap();
        emit(&dispatch, "debug enabled");
        control
            .reconfigure_with(
                &Options {
                    level: LogLevel::Error,
                    ..changed
                },
                || Ok(()),
            )
            .unwrap();
        emit(&dispatch, "debug disabled again");
        control
            .reconfigure_with(&disabled_options(), || Ok(()))
            .unwrap();
        emit(&dispatch, "file disabled");
        assert_eq!(fs::read_dir(&logs).unwrap().count(), 5);
        let contents = log_contents(directory.path());
        assert!(contents.contains("error error only"));
        assert!(!contents.contains("debug error only"));
        assert!(contents.contains("debug debug enabled"));
        assert!(!contents.contains("debug debug disabled again"));
        assert!(!contents.contains("file disabled"));
        assert!(!contents.contains("initially disabled"));
        assert!(contents.contains("operation=\"scan\""));
        assert!(!contents.contains('\x1b'));
        control.reconfigure_with(&changed, || Ok(())).unwrap();
        emit(&dispatch, "reenabled");
        session.shutdown();
        assert_eq!(fs::read_dir(&logs).unwrap().count(), 2);
        assert_eq!(fs::read_to_string(unrelated).unwrap(), "preserve");
        assert!(log_contents(directory.path()).contains("debug reenabled"));
    }

    #[test]
    fn session_ownership_survives_failures_and_releases_after_shutdown_or_drop() {
        let (routing, dispatch) = harness();
        let _default = tracing::dispatcher::set_default(&dispatch);
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let config = file_options(LogLevel::Debug);
        let mut first =
            Session::start_with_routing(first_dir.path(), &disabled_options(), routing.clone())
                .unwrap();
        let control = first.control();
        assert!(
            control
                .reconfigure_with(&config, || anyhow::bail!("save failed"))
                .is_err()
        );
        assert!(Session::start_with_routing(second_dir.path(), &config, routing.clone()).is_err());
        assert!(!second_dir.path().join("logs").exists());
        emit(&dispatch, "rejected enable");
        control
            .reconfigure_with(&file_options(LogLevel::Error), || Ok(()))
            .unwrap();
        assert!(Session::start_with_routing(second_dir.path(), &config, routing.clone()).is_err());
        emit(&dispatch, "active owner");
        for (options, phase) in [
            (config, "rejected level"),
            (disabled_options(), "rejected disable"),
        ] {
            assert!(
                control
                    .reconfigure_with(&options, || anyhow::bail!("save failed"))
                    .is_err()
            );
            emit(&dispatch, phase);
        }
        control
            .reconfigure_with(&disabled_options(), || Ok(()))
            .unwrap();
        let contents = log_contents(first_dir.path());
        assert!(!contents.contains("rejected enable"));
        assert!(contents.contains("error active owner"));
        assert!(!contents.contains("debug active owner"));
        assert!(contents.contains("error rejected level"));
        assert!(contents.contains("error rejected disable"));
        assert!(!contents.contains("debug rejected"));
        assert!(Session::start_with_routing(second_dir.path(), &config, routing.clone()).is_err());
        control.reconfigure_with(&config, || Ok(())).unwrap();
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
        assert!(
            control
                .reconfigure_with(&config, || panic!("must not persist after shutdown"))
                .is_err()
        );
        let second =
            Session::start_with_routing(second_dir.path(), &config, routing.clone()).unwrap();
        let stale = second.control();
        first.shutdown();
        emit(&dispatch, "second owner");
        drop(second);
        assert!(log_contents(second_dir.path()).contains("debug second owner"));
        assert!(!log_contents(first_dir.path()).contains("second owner"));
        assert!(
            stale
                .reconfigure_with(&config, || panic!("must not persist after drop"))
                .is_err()
        );
        let mut third = Session::start_with_routing(first_dir.path(), &config, routing).unwrap();
        emit(&dispatch, "after owner drop");
        third.shutdown();
        assert!(log_contents(first_dir.path()).contains("debug after owner drop"));
    }

    #[test]
    fn failed_preparation_does_not_persist_and_can_retry() {
        let (routing, dispatch) = harness();
        let _default = tracing::dispatcher::set_default(&dispatch);
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("logs"), "not a directory").unwrap();
        let enabled = file_options(LogLevel::Debug);
        assert!(Session::start_with_routing(directory.path(), &enabled, routing.clone()).is_err());
        let mut session =
            Session::start_with_routing(directory.path(), &disabled_options(), routing).unwrap();
        assert!(
            session
                .control()
                .reconfigure_with(&enabled, || panic!(
                    "must not persist after preparation failure"
                ))
                .is_err()
        );
        fs::remove_file(directory.path().join("logs")).unwrap();
        session
            .control()
            .reconfigure_with(&enabled, || Ok(()))
            .unwrap();
        emit(&dispatch, "after retry");
        session.shutdown();
        assert!(log_contents(directory.path()).contains("debug after retry"));
    }
}
