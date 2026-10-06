//! Process-wide diagnostics with weekly rolling file output.
//!
//! The subscriber keeps stderr warnings/errors visible while file logging is
//! controlled by [`Config`]. File writes are non-blocking; retention cleanup is
//! performed by the rolling writer once per week and when configuration changes.

use crate::config::{Config, LogLevel};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};
use tracing::{Event, Level, Subscriber};
use tracing_appender::{
    non_blocking::{NonBlocking, WorkerGuard},
    rolling::RollingFileAppender,
};
use tracing_subscriber::{
    Layer,
    filter::{LevelFilter, filter_fn},
    fmt::{self, FmtContext, FormatEvent, FormatFields, format::Writer},
    layer::SubscriberExt,
    registry::LookupSpan,
};

static STATE: OnceLock<Arc<LoggingState>> = OnceLock::new();

const WEEK_SECONDS: i64 = 7 * 24 * 60 * 60;
const SUNDAY_EPOCH_OFFSET: i64 = 3 * 24 * 60 * 60;
const LOG_PREFIX: &str = "rivu.log.";

struct LoggingState {
    data_dir: PathBuf,
    level: AtomicU8,
    file_enabled: AtomicBool,
    retention_weeks: Arc<AtomicU64>,
    sink: Arc<Mutex<LogSink>>,
    guard: Mutex<Option<WorkerGuard>>,
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

pub fn init(data_dir: &Path, config: &Config) -> Result<()> {
    if let Some(state) = STATE.get() {
        configure_state(state, config)?;
        return Ok(());
    }

    let level = level_code(config.log_level);
    let retention_weeks = Arc::new(AtomicU64::new(u64::from(config.log_retention_weeks)));
    let sink = Arc::new(Mutex::new(LogSink::Disabled));
    let state = Arc::new(LoggingState {
        data_dir: data_dir.to_path_buf(),
        level: AtomicU8::new(level),
        file_enabled: AtomicBool::new(false),
        retention_weeks: retention_weeks.clone(),
        sink: sink.clone(),
        guard: Mutex::new(None),
    });

    if config.log_to_file {
        let (writer, guard) = make_file_writer(data_dir, retention_weeks)?;
        *sink.lock().expect("logging sink mutex poisoned") = LogSink::File(writer);
        *state.guard.lock().expect("logging guard mutex poisoned") = Some(guard);
        state.file_enabled.store(true, Ordering::Release);
    }

    let stderr_layer = fmt::layer()
        .event_format(LineFormatter {
            ansi: stderr_ansi(),
        })
        .with_writer(io::stderr)
        .with_filter(LevelFilter::WARN);
    let filter_state = state.clone();
    let file_layer = fmt::layer()
        .event_format(LineFormatter { ansi: false })
        .with_writer(SharedSink(sink))
        .with_filter(filter_fn(move |metadata| {
            filter_state.file_enabled.load(Ordering::Relaxed)
                && level_enabled(filter_state.level.load(Ordering::Relaxed), metadata.level())
        }));

    tracing::subscriber::set_global_default(
        tracing_subscriber::registry()
            .with(stderr_layer)
            .with(file_layer),
    )
    .context("installing Rivu logging subscriber")?;
    let _ = STATE.set(state);
    Ok(())
}

pub fn reconfigure(config: &Config) -> Result<()> {
    let Some(state) = STATE.get() else {
        return Ok(());
    };
    configure_state(state, config)
}

fn configure_state(state: &LoggingState, config: &Config) -> Result<()> {
    let desired_retention = u64::from(config.log_retention_weeks);
    let previous_retention = state.retention_weeks.load(Ordering::Relaxed);
    state
        .retention_weeks
        .store(desired_retention, Ordering::Relaxed);

    let was_enabled = state.file_enabled.load(Ordering::Acquire);
    if was_enabled != config.log_to_file {
        let (sink, guard) = if config.log_to_file {
            let (writer, guard) =
                match make_file_writer(&state.data_dir, state.retention_weeks.clone()) {
                    Ok(value) => value,
                    Err(error) => {
                        state
                            .retention_weeks
                            .store(previous_retention, Ordering::Relaxed);
                        return Err(error);
                    }
                };
            (LogSink::File(writer), Some(guard))
        } else {
            (LogSink::Disabled, None)
        };
        *state.sink.lock().expect("logging sink mutex poisoned") = sink;
        *state.guard.lock().expect("logging guard mutex poisoned") = guard;
        state
            .file_enabled
            .store(config.log_to_file, Ordering::Release);
    } else if config.log_to_file {
        cleanup_old_logs(&state.data_dir.join("logs"), desired_retention);
    }
    state
        .level
        .store(level_code(config.log_level), Ordering::Relaxed);
    Ok(())
}

fn make_file_writer(
    data_dir: &Path,
    retention_weeks: Arc<AtomicU64>,
) -> Result<(NonBlocking, WorkerGuard)> {
    let directory = data_dir.join("logs");
    fs::create_dir_all(&directory)
        .with_context(|| format!("creating log directory {}", directory.display()))?;
    cleanup_old_logs(&directory, retention_weeks.load(Ordering::Relaxed));
    let appender = tracing_appender::rolling::weekly(&directory, "rivu.log");
    let writer = RetainingWriter {
        inner: appender,
        directory,
        retention_weeks,
        last_week: None,
    };
    Ok(tracing_appender::non_blocking(writer))
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

    #[test]
    fn levels_use_threshold_order() {
        assert!(level_enabled(0, &Level::DEBUG));
        assert!(level_enabled(1, &Level::INFO));
        assert!(!level_enabled(1, &Level::DEBUG));
        assert!(level_enabled(2, &Level::ERROR));
        assert!(!level_enabled(3, &Level::WARN));
    }

    #[test]
    fn week_boundary_is_sunday_utc() {
        assert_eq!((3 * 24 * 60 * 60_i64).div_euclid(WEEK_SECONDS), 0);
        assert_eq!((4 * 24 * 60 * 60_i64).div_euclid(WEEK_SECONDS), 0);
        assert_eq!((10 * 24 * 60 * 60_i64).div_euclid(WEEK_SECONDS), 1);
    }
}
