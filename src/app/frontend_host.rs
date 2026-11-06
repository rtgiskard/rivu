//! Executable-owned frontend bootstrap; frontend libraries remain pure IPC clients.
use anyhow::{Context, Result};
use rivu_protocol::{
    response::StateSections,
    transport::{read_response_frame, timeout_at, unpack_hello, write_request_frame_buffered},
    wire::{WireRequest, connect},
};
use std::{
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::FileExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const DIAGNOSTIC_LIMIT: usize = 64 * 1024;
const BOOTSTRAP_ENV: &str = "RIVU_FRONTEND_BOOTSTRAP";

pub(super) fn absolute(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// One connection and one Hello share the caller's complete startup budget.
/// In particular, an already bound listener can keep this connection queued
/// while its owner initializes; it must not be replaced with short-lived probes.
async fn probe(data_dir: &Path, deadline: Instant) -> Result<u32> {
    timeout_at(deadline, async {
        let mut stream = connect(data_dir)
            .await
            .context("Connecting to Rivu service")?;
        write_request_frame_buffered(
            &mut stream,
            &WireRequest::Hello(StateSections::default()),
            &mut Vec::new(),
            &mut false,
        )
        .await?;
        let response = read_response_frame(&mut stream, &mut bytes::BytesMut::new()).await?;
        let hello = unpack_hello(response)?;
        Ok(u32::from_le_bytes(hello.instance_id))
    })
    .await
    .context("Waiting for service Hello readiness timed out")?
}

/// Anonymous, fixed-size stderr storage: no pipe can block the service or give
/// it SIGPIPE if the frontend times out or exits. Keep only startup diagnostics;
/// the child redirects stderr after successful initialization.
struct StartupDiagnostics(File);
impl StartupDiagnostics {
    fn new() -> Result<Self> {
        // SAFETY: the name is NUL-terminated and the returned descriptor is
        // immediately transferred to an owning File on success.
        let fd = unsafe {
            libc::memfd_create(
                c"rivu-startup".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd == -1 {
            return Err(std::io::Error::last_os_error()).context("Creating startup diagnostics");
        }
        // SAFETY: memfd_create returned a new, uniquely owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(DIAGNOSTIC_LIMIT as u64)?;
        // A full buffer makes subsequent writes fail rather than allocating
        // unbounded storage, without a pipe's SIGPIPE or blocking failure mode.
        // SAFETY: fcntl takes an integer seal mask for F_ADD_SEALS.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_GROW) } == -1 {
            return Err(std::io::Error::last_os_error()).context("Bounding startup diagnostics");
        }
        Ok(Self(file))
    }

    fn failure_context(&self) -> String {
        let mut bytes = vec![0; DIAGNOSTIC_LIMIT];
        match self.0.read_at(&mut bytes, 0) {
            Ok(count) => {
                bytes.truncate(count);
                while bytes.last() == Some(&0) {
                    bytes.pop();
                }
                let diagnostic = String::from_utf8_lossy(&bytes);
                let diagnostic = diagnostic.trim();
                if diagnostic.is_empty() {
                    "Starting independent Rivu service (no startup stderr)".into()
                } else {
                    format!("Starting independent Rivu service; startup stderr:\n{diagnostic}")
                }
            }
            Err(error) => {
                format!("Starting independent Rivu service; reading startup stderr: {error}")
            }
        }
    }
}

/// Only automatically started services detach this temporary diagnostic sink.
/// Manual `serve` keeps its ordinary stderr, including shutdown diagnostics.
pub(super) fn detach_startup_diagnostics() -> Result<()> {
    if std::env::var_os(BOOTSTRAP_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return Ok(());
    }
    let null = File::options().write(true).open("/dev/null")?;
    // SAFETY: dup2 atomically replaces process stderr with a valid owned fd;
    // no Rust object owns descriptor 2, and null remains alive for this call.
    if unsafe { libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO) } == -1 {
        return Err(std::io::Error::last_os_error()).context("Detaching service startup stderr");
    }
    Ok(())
}

/// Prefer the existing owner, and spawn only when connection proves it absent.
/// All probing, initialization and Hello I/O share one startup deadline.
/// Dropping this waiter closes its readiness connection, not the independent
/// service. The frontend owns waiting; the detached process owns Core lifetime.
pub(super) async fn ensure_service(data_dir: &Path, config_path: &Path) -> Result<u32> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    match probe(data_dir, deadline).await {
        Ok(instance) => return Ok(instance),
        Err(error) if crate::ipc::is_no_instance(&error) => {}
        Err(error) => return Err(error),
    }
    if Instant::now() >= deadline {
        return Err(rivu_protocol::transport::Elapsed).context("Rivu service startup timed out");
    }

    let diagnostics = StartupDiagnostics::new()?;
    let mut command = std::process::Command::new(
        std::env::current_exe().context("Locating Rivu service executable")?,
    );
    command
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--config")
        .arg(config_path)
        .arg("serve")
        .env(BOOTSTRAP_ENV, "1");
    // The service must survive both frontend and terminal-session exit.
    // SAFETY: setsid is async-signal-safe and captures no parent state.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut command = async_process::Command::from(command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(diagnostics.0.try_clone()?);
    let mut child = command
        .spawn()
        .context("Starting independent Rivu service")?;
    let ready = async {
        let mut backoff = Duration::from_millis(10);
        let ready = loop {
            match probe(data_dir, deadline).await {
                Ok(instance) => break Ok(instance),
                Err(error) if crate::ipc::is_no_instance(&error) => {
                    // A competing starter may have won ownership; its Hello,
                    // not the child PID or exit code, is authoritative.
                    if let Some(status) = child.try_status().context("Observing service startup")? {
                        break Err(error).with_context(|| {
                            format!("Rivu service exited before readiness ({status})")
                        });
                    }
                    if Instant::now() >= deadline {
                        break Err(error).context("Waiting for service Hello readiness timed out");
                    }
                }
                Err(error) => break Err(error),
            }
            async_io::Timer::at((Instant::now() + backoff).min(deadline)).await;
            backoff = (backoff * 2).min(Duration::from_millis(500));
        };
        if ready.is_err() {
            // Releasing the listener can precede the child's final error print.
            // Preserve that diagnostic without extending the startup budget or
            // killing a service which may still finish after our deadline.
            let _ = timeout_at(deadline, child.status()).await;
        }
        ready
    }
    .await;
    // async-process reaps dropped children without killing them or tying their
    // lifetime to a frontend-owned waiter thread.
    ready.with_context(|| diagnostics.failure_context())
}

#[cfg(test)]
mod tests;
