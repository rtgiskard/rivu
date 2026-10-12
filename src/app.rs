//! Application lifecycle and services shared by graphical and headless instances.
//! CLI parsing and client-relative path translation remain at the binary boundary.

use crate::{
    artwork::ArtworkManager,
    core::{CoreHandle, Runtime},
    gui::{self, session::UiSession},
    ipc,
    model::Command,
    mpris,
    tray::TrayController,
};
use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{bounded, select};
use futures::{StreamExt, channel::mpsc};
use gpui::{App as GpuiApp, QuitMode, Subscription, Task};
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::{Handle as SignalHandle, Signals},
};
use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc, thread};

/// Owns one instance's core runtime, external services, and optional GUI session.
/// Closing a window releases the session's view, never the application services.
/// App retains socket acquisition until core, media, and transport have stopped.
pub struct App {
    runtime: Runtime,
    _signals: ShutdownSignals,
    media: Option<MediaWorker>,
    artwork: Arc<ArtworkManager>,
    tray: Option<TrayController>,
    gui: Option<GuiState>,
    _server: ipc::Server,
    // Retire the socket last, including after partial startup and core join.
    _socket_owner: ipc::SocketOwner,
}

/// GPUI integration state, not a second application or services owner.
struct GuiState {
    session: Rc<RefCell<UiSession>>,
    window_closed: Option<Subscription>,
    app_quit: Option<Subscription>,
    wake_task: Option<Task<()>>,
    error: Option<anyhow::Error>,
    quitting: bool,
}

impl App {
    /// Show an existing instance, or start a graphical application if none exists.
    /// Import paths must already be resolved against the calling client's cwd.
    pub fn open_gui(data_dir: PathBuf, config_path: PathBuf, paths: Vec<PathBuf>) -> Result<()> {
        let socket = data_dir.join("rivu.sock");
        match ipc::request_ack(&socket, &Command::ShowWindow) {
            Ok(ack) if ack.ok => {
                if !paths.is_empty() {
                    let ack = ipc::request_ack(
                        &socket,
                        &Command::Scan {
                            paths,
                            force: false,
                        },
                    )?;
                    if !ack.ok {
                        bail!("{}", ack.error.as_deref().unwrap_or("Command failed"));
                    }
                }
                Ok(())
            }
            Ok(ack) => bail!("{}", ack.error.as_deref().unwrap_or("Command failed")),
            Err(error) if ipc::is_no_instance(&error) => {
                Self::start(data_dir, config_path, paths, true)?.run()
            }
            Err(error) => Err(error),
        }
    }

    /// Acquire the instance before touching core storage or configuration.
    /// Import paths must already be resolved against the calling client's cwd.
    pub fn start(
        data_dir: PathBuf,
        config_path: PathBuf,
        paths: Vec<PathBuf>,
        graphical: bool,
    ) -> Result<Self> {
        let signals = Signals::new([SIGINT, SIGTERM]).context("Registering shutdown signals")?;
        let socket_owner = ipc::SocketOwner::acquire(&data_dir.join("rivu.sock"))?;
        let runtime = Runtime::start(&data_dir, &config_path)?;
        let signals = ShutdownSignals::start(signals, runtime.handle.clone())?;
        let server = ipc::Server::start(&socket_owner, runtime.handle.clone())?;
        let artwork = Arc::new(ArtworkManager::new(&data_dir));
        let mut app = Self {
            runtime,
            _signals: signals,
            media: None,
            artwork,
            tray: None,
            gui: None,
            _server: server,
            _socket_owner: socket_owner,
        };
        app.media = Some(MediaWorker::start(
            app.runtime.handle.clone(),
            graphical,
            Arc::clone(&app.artwork),
        )?);
        if !paths.is_empty() {
            app.runtime.handle.send(Command::Scan {
                paths,
                force: false,
            })?;
        }
        if graphical {
            app.gui = Some(GuiState {
                session: UiSession::new(
                    app.runtime.handle.clone(),
                    data_dir.join("layout.json"),
                    Arc::clone(&app.artwork),
                ),
                window_closed: None,
                app_quit: None,
                wake_task: None,
                error: None,
                quitting: false,
            });
        } else {
            println!("Rivu core ready. Data: {}", data_dir.display());
        }
        Ok(app)
    }

    /// Run until application shutdown; a headless instance waits for its core.
    pub fn run(mut self) -> Result<()> {
        if self.gui.is_some() {
            self.run_gui()
        } else {
            self.finish(Ok(()))
        }
    }

    fn show_window(&mut self, cx: &mut GpuiApp) {
        let gui = self.gui.as_mut().expect("graphical application");
        match gui.session.borrow_mut().show_window(cx) {
            Ok(()) => gui.error = None,
            Err(error) => {
                tracing::error!(%error, "Could not open Rivu window; use Show window to retry");
                gui.error = Some(error.context("Could not open Rivu window"));
            }
        }
    }

    fn handle_requests(&mut self, cx: &mut GpuiApp) {
        if self.gui.as_ref().is_none_or(|gui| gui.quitting) {
            return;
        }
        if self.runtime.handle.is_shutting_down() {
            self.quit_gui(cx);
        } else if self.runtime.handle.take_raise_request() {
            self.show_window(cx);
        }
    }

    fn shutdown_gui(&mut self) {
        let Some(gui) = self.gui.as_mut() else {
            return;
        };
        if gui.quitting {
            return;
        }
        gui.quitting = true;
        // Withdraw CanRaise and stop producers before releasing the UI receiver.
        self.runtime.handle.clear_gui_opener();
        gui.wake_task.take();
        gui.window_closed.take();
        gui.app_quit.take();
        gui.session.borrow_mut().release_window();
        // Tray/service teardown may join workers and must stay outside GPUI's
        // deadline-limited quit observer.
    }

    fn quit_gui(&mut self, cx: &mut GpuiApp) {
        self.shutdown_gui();
        cx.quit();
    }

    fn run_gui(mut self) -> Result<()> {
        if self.runtime.handle.config_snapshot().tray_enabled {
            self.tray = TrayController::start(self.runtime.handle.clone())?;
        }
        let app = Rc::new(RefCell::new(self));
        let gpui_owner = Rc::clone(&app);
        gpui_platform::application()
            .with_quit_mode(QuitMode::Explicit)
            .run(move |cx: &mut GpuiApp| {
                if let Err(error) = gui::init(cx) {
                    let mut app = gpui_owner.borrow_mut();
                    app.gui.as_mut().expect("graphical application").error = Some(error);
                    app.quit_gui(cx);
                    return;
                }

                // App owns these listeners across window close; weak captures
                // cannot retain the application after the event loop returns.
                let closed_owner = Rc::downgrade(&gpui_owner);
                let window_closed = cx.on_window_closed(move |_, id| {
                    if let Some(app) = closed_owner.upgrade() {
                        if let Some(gui) = app.borrow().gui.as_ref() {
                            gui.session.borrow_mut().window_closed(id);
                        }
                    }
                });
                let quit_owner = Rc::downgrade(&gpui_owner);
                let app_quit = cx.on_app_quit(move |_| {
                    if let Some(app) = quit_owner.upgrade() {
                        app.borrow_mut().shutdown_gui();
                    }
                    futures::future::ready(())
                });

                // Playback wakes the view separately; this persistent receiver
                // handles Raise and shutdown even when no window exists.
                let (wake_sender, mut wake_receiver) = mpsc::channel::<()>(1);
                let wake_sender = parking_lot::Mutex::new(wake_sender);
                gpui_owner.borrow().runtime.handle.set_gui_opener(move || {
                    let _ = wake_sender.lock().try_send(());
                });
                let request_owner = Rc::downgrade(&gpui_owner);
                let wake_task = cx.spawn(async move |cx| {
                    while wake_receiver.next().await.is_some() {
                        let Some(app) = request_owner.upgrade() else {
                            break;
                        };
                        cx.update(|cx| app.borrow_mut().handle_requests(cx));
                    }
                });
                let mut app = gpui_owner.borrow_mut();
                let gui = app.gui.as_mut().expect("graphical application");
                gui.window_closed = Some(window_closed);
                gui.app_quit = Some(app_quit);
                gui.wake_task = Some(wake_task);

                // Reconcile signals and Raise accepted before registration.
                if app.runtime.handle.is_shutting_down() {
                    app.quit_gui(cx);
                } else {
                    app.runtime.handle.take_raise_request();
                    app.show_window(cx);
                }
            });

        let mut app = app.borrow_mut();
        app.shutdown_gui();
        drop(app.tray.take());
        // Blocking core acknowledgment and worker joins are outside GPUI.
        let shutdown = if !app.runtime.handle.is_shutting_down() {
            let response = app.runtime.handle.request_ack(Command::Shutdown);
            if response.ok {
                Ok(())
            } else {
                Err(anyhow!(
                    "{}",
                    response
                        .error
                        .as_deref()
                        .unwrap_or("Could not shut down Rivu")
                ))
            }
        } else {
            Ok(())
        };
        let error = app
            .gui
            .as_mut()
            .expect("graphical application")
            .error
            .take();
        app.finish(error.map_or(shutdown, Err))
    }

    fn finish(&mut self, interface_result: Result<()>) -> Result<()> {
        if interface_result.is_err() {
            let _ = self.runtime.handle.request_shutdown();
        }
        let join_result = self.runtime.join();
        // Keep IPC/socket acquisition alive through core join and MPRIS stop.
        drop(self.media.take());
        interface_result?;
        join_result.map_err(|_| anyhow!("Rivu core worker panicked"))
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.shutdown_gui();
        drop(self.tray.take());
        if !self.runtime.is_finished() {
            let _ = self.runtime.handle.request_shutdown();
        }
        let _ = self.runtime.join();
        drop(self.media.take());
    }
}

struct ShutdownSignals {
    handle: SignalHandle,
    worker: Option<thread::JoinHandle<()>>,
}

impl ShutdownSignals {
    fn start(mut signals: Signals, core: CoreHandle) -> Result<Self> {
        let handle = signals.handle();
        let worker = thread::Builder::new()
            .name("rivu-signals".into())
            .spawn(move || {
                // Coalesce signals; wait for capacity only in this worker.
                if let Some(signal) = signals.forever().next() {
                    tracing::info!(signal, "shutdown_signal_received");
                    let _ = core.request_shutdown();
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

impl MediaWorker {
    fn start(handle: CoreHandle, graphical: bool, artwork: Arc<ArtworkManager>) -> Result<Self> {
        let changes = handle.subscribe();
        let (stop, stop_receiver) = bounded(1);
        let worker = thread::Builder::new()
            .name("rivu-mpris-manager".into())
            .spawn(move || {
                let mut service = None;
                let mut enabled = false;
                loop {
                    let snapshot = handle.gui_snapshot();
                    if snapshot.system.shutting_down {
                        break;
                    }
                    if snapshot.system.config.mpris_enabled != enabled {
                        enabled = snapshot.system.config.mpris_enabled;
                        let status = if enabled {
                            match mpris::Mpris::start(
                                handle.clone(),
                                graphical,
                                Arc::clone(&artwork),
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
                        let _ = handle.send(Command::MprisStatus { status });
                    }
                    select! {
                        recv(stop_receiver) -> _ => break,
                        recv(changes) -> changed => {
                            if changed.is_err() {
                                break;
                            }
                        }
                    }
                }
                drop(service);
            })?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for MediaWorker {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
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
            App::start(
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
        assert!(App::start(directory.path().to_owned(), config, Vec::new(), false).is_err());
        let socket = directory.path().join("rivu.sock");
        assert!(!socket.exists());
        assert!(ipc::SocketOwner::acquire(&socket).is_ok());
    }
}
