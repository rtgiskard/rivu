//! Binary-owned startup and shutdown for the shared core, IPC and desktop services.
//! CLI parsing and command translation stay outside this lifecycle boundary.

use super::client_path;
use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{bounded, select};
use rivu::{
    artwork::ArtworkManager,
    core::{AppHandle, Runtime},
    gui, ipc,
    model::Command,
    mpris,
};
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::{Handle as SignalHandle, Signals},
};
use std::{path::PathBuf, sync::Arc, thread};
pub(super) fn open_gui(data_dir: PathBuf, config_path: PathBuf, paths: Vec<PathBuf>) -> Result<()> {
    let socket = data_dir.join("rivu.sock");
    match ipc::request_ack(&socket, &Command::ShowWindow) {
        Ok(ack) if ack.ok => {
            if !paths.is_empty() {
                let scan = Command::Scan {
                    paths: paths.into_iter().map(client_path).collect::<Result<_>>()?,
                    force: false,
                };
                let ack = ipc::request_ack(&socket, &scan)?;
                if !ack.ok {
                    bail!("{}", ack.error.as_deref().unwrap_or("Command failed"));
                }
            }
            Ok(())
        }
        Ok(ack) => bail!("{}", ack.error.as_deref().unwrap_or("Command failed")),
        Err(error) if ipc::is_no_instance(&error) => start(data_dir, config_path, paths, true),
        Err(error) => Err(error),
    }
}

struct ShutdownSignals {
    handle: SignalHandle,
    worker: Option<thread::JoinHandle<()>>,
}

impl ShutdownSignals {
    fn start(mut signals: Signals, app: AppHandle) -> Result<Self> {
        let handle = signals.handle();
        let worker = thread::Builder::new()
            .name("rivu-signals".into())
            .spawn(move || {
                // Coalesce repeated signals into one request. Waiting for queue
                // capacity is safe here, never in the low-level signal handler.
                if let Some(signal) = signals.forever().next() {
                    tracing::info!(signal, "shutdown_signal_received");
                    let _ = app.request_shutdown();
                }
            })?;
        Ok(Self {
            handle,
            worker: Some(worker),
        })
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct MediaWorker {
    stop: crossbeam_channel::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Drop for MediaWorker {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(super) fn start(
    data_dir: PathBuf,
    config_path: PathBuf,
    paths: Vec<PathBuf>,
    desktop: bool,
) -> Result<()> {
    let signals = Signals::new([SIGINT, SIGTERM]).context("Registering shutdown signals")?;
    let socket = data_dir.join("rivu.sock");
    let socket_owner = ipc::SocketOwner::acquire(&socket)?;
    let mut runtime = Runtime::start(&data_dir, &config_path)?;
    let _shutdown_signals = ShutdownSignals::start(signals, runtime.handle.clone())?;
    let server = ipc::Server::start(&socket_owner, runtime.handle.clone())?;
    let artwork = Arc::new(ArtworkManager::new(&data_dir));
    let media_handle = runtime.handle.clone();
    let media_changes = media_handle.subscribe();
    let (media_stop, media_stop_receiver) = bounded(1);
    let media_artwork = Arc::clone(&artwork);
    let media_worker = thread::Builder::new()
        .name("rivu-mpris-manager".into())
        .spawn(move || {
            let mut service = None;
            let mut enabled = false;
            loop {
                let snapshot = media_handle.gui_snapshot();
                if snapshot.system.shutting_down {
                    break;
                }
                if snapshot.system.config.mpris_enabled != enabled {
                    enabled = snapshot.system.config.mpris_enabled;
                    let status = if enabled {
                        match mpris::Mpris::start(
                            media_handle.clone(),
                            desktop,
                            Arc::clone(&media_artwork),
                        ) {
                            Ok(started) => {
                                service = Some(started);
                                "Connected to desktop media controls".to_owned()
                            }
                            Err(error) => format!("MPRIS unavailable: {error:#}"),
                        }
                    } else {
                        service = None;
                        "Desktop media controls disabled".to_owned()
                    };
                    let _ = media_handle.send(Command::MprisStatus { status });
                }
                select! {
                    recv(media_stop_receiver) -> _ => break,
                    recv(media_changes) -> changed => {
                        if changed.is_err() {
                            break;
                        }
                    }
                }
            }
            drop(service);
        })?;
    let media_worker = MediaWorker {
        stop: media_stop,
        worker: Some(media_worker),
    };
    if !paths.is_empty() {
        runtime.handle.send(Command::Scan {
            paths: paths.into_iter().map(client_path).collect::<Result<_>>()?,
            force: false,
        })?;
    }
    let interface_result = if desktop {
        gui::run(
            runtime.handle.clone(),
            data_dir.join("layout.json"),
            artwork,
        )
    } else {
        println!("Rivu core ready. Data: {}", data_dir.display());
        Ok(())
    };
    if interface_result.is_err() {
        let _ = runtime.handle.send(Command::Shutdown);
    }
    let join_result = runtime.join();
    drop(media_worker);
    interface_result?;
    join_result.map_err(|_| anyhow!("Rivu core worker panicked"))?;
    drop(server);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_start_does_not_touch_runtime_storage_or_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("rivu.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let config = directory.path().join("config.toml");
        std::fs::write(&config, b"invalid config intentionally").unwrap();
        assert!(
            start(
                directory.path().to_owned(),
                config.clone(),
                Vec::new(),
                false
            )
            .is_err()
        );
        assert!(!directory.path().join("library.db").exists());
        assert_eq!(
            std::fs::read(config).unwrap(),
            b"invalid config intentionally"
        );
        assert!(std::os::unix::net::UnixStream::connect(socket).is_ok());
        drop(listener);
    }

    #[test]
    fn failed_runtime_start_retires_its_bound_socket() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        std::fs::write(&config, b"invalid config intentionally").unwrap();
        assert!(start(directory.path().to_owned(), config, Vec::new(), false).is_err());
        let socket = directory.path().join("rivu.sock");
        assert!(!socket.exists());
        assert!(ipc::SocketOwner::acquire(&socket).is_ok());
    }
}
