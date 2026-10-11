//! Binary-owned startup and shutdown for the shared core, IPC and desktop services.
//! CLI parsing and command translation stay outside this lifecycle boundary.

use super::client_path;
use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{bounded, select};
use rivu::{
    core::{AppHandle, Runtime},
    gui, ipc,
    model::Command,
    mpris,
};
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::{Handle as SignalHandle, Signals},
};
use std::{path::PathBuf, thread};
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

pub(super) fn start(
    data_dir: PathBuf,
    config_path: PathBuf,
    paths: Vec<PathBuf>,
    desktop: bool,
) -> Result<()> {
    let signals = Signals::new([SIGINT, SIGTERM]).context("Registering shutdown signals")?;
    let socket = data_dir.join("rivu.sock");
    ipc::bind(&socket)?;
    let mut runtime = match Runtime::start(&data_dir, &config_path) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = std::fs::remove_file(&socket);
            return Err(error);
        }
    };
    let _shutdown_signals = ShutdownSignals::start(signals, runtime.handle.clone())?;
    let server = ipc::Server::start(socket, runtime.handle.clone())?;
    let media_handle = runtime.handle.clone();
    let media_changes = media_handle.subscribe();
    let (media_stop, media_stop_receiver) = bounded(1);
    let media_stop_for_cleanup = media_stop.clone();
    let media_data_dir = data_dir.clone();
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
                        match mpris::Mpris::start(media_handle.clone(), desktop, &media_data_dir) {
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
    if !paths.is_empty() {
        runtime.handle.send(Command::Scan {
            paths: paths.into_iter().map(client_path).collect::<Result<_>>()?,
            force: false,
        })?;
    }
    let interface_result = if desktop {
        gui::run(runtime.handle.clone(), data_dir.join("layout.json"))
    } else {
        println!("Rivu core ready. Data: {}", data_dir.display());
        Ok(())
    };
    if interface_result.is_err() {
        let _ = runtime.handle.send(Command::Shutdown);
    }
    let join_result = runtime.join();
    let _ = media_stop_for_cleanup.send(());
    let _ = media_worker.join();
    interface_result?;
    join_result.map_err(|_| anyhow!("Rivu core worker panicked"))?;
    drop(server);
    Ok(())
}
