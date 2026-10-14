//! Application lifecycle and services shared by graphical and headless instances.
//! CLI parsing and client-relative path translation remain at the binary boundary.

use crate::{
    artwork::ArtworkManager,
    config::Config,
    core::{CoreHandle, Runtime, RuntimeControl},
    gui::{self, session::UiSession},
    ipc, logging,
    model::Command,
    mpris,
    response::Ack,
    tray::TrayController,
};
use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{bounded, select};
use futures::{StreamExt, channel::mpsc};
use gpui::{App as GpuiApp, GraphicalEnvironment, QuitMode, Subscription, Task, WindowingRequest};
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::{Handle as SignalHandle, Signals},
};
use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc, thread};

/// Application requests are never business commands or core view state.
#[derive(Clone, Debug)]
pub enum AppCommand {
    Core(Command),
    ShowWindow,
    Shutdown,
}

impl From<Command> for AppCommand {
    fn from(command: Command) -> Self {
        Self::Core(command)
    }
}

#[derive(Default)]
struct ControlState {
    graphical: bool,
    raise_pending: bool,
    shutting_down: bool,
    wakeup: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Cloneable application endpoint, owned by App across window lifetimes.
#[derive(Clone)]
pub(crate) struct AppHandle {
    core: CoreHandle,
    runtime: RuntimeControl,
    state: Arc<parking_lot::Mutex<ControlState>>,
}

impl AppHandle {
    pub(crate) fn new(runtime: &Runtime, graphical: bool) -> Self {
        let state = Arc::new(parking_lot::Mutex::new(ControlState {
            graphical,
            ..ControlState::default()
        }));
        let completion_state = Arc::clone(&state);
        runtime.control.set_completion_callback(move || {
            let mut state = completion_state.lock();
            state.shutting_down = true;
            state.raise_pending = false;
            let wakeup = state.wakeup.clone();
            drop(state);
            if let Some(wakeup) = wakeup {
                wakeup();
            }
        });
        Self {
            core: runtime.handle.clone(),
            runtime: runtime.control.clone(),
            state,
        }
    }

    pub(crate) fn can_raise(&self) -> bool {
        let state = self.state.lock();
        state.graphical && !state.shutting_down
    }

    pub(crate) fn raise(&self) -> Result<()> {
        let mut state = self.state.lock();
        if !state.graphical {
            bail!("Rivu has no graphical host");
        }
        if state.shutting_down {
            bail!("Rivu is shutting down");
        }
        let wakeup = if state.raise_pending {
            None
        } else {
            state.wakeup.clone()
        };
        state.raise_pending = true;
        drop(state);
        if let Some(wakeup) = wakeup {
            wakeup();
        }
        Ok(())
    }

    /// Constant-capacity shutdown signal; never blocks a GPUI callback.
    pub(crate) fn request_shutdown(&self) -> Result<()> {
        let mut state = self.state.lock();
        if state.shutting_down {
            return Ok(());
        }
        state.shutting_down = true;
        state.raise_pending = false;
        let wakeup = state.wakeup.clone();
        drop(state);
        let result = self.runtime.request_shutdown();
        if let Some(wakeup) = wakeup {
            wakeup();
        }
        result
    }

    fn is_shutting_down(&self) -> bool {
        self.state.lock().shutting_down
    }

    fn set_wakeup(&self, callback: impl Fn() + Send + Sync + 'static) {
        let wakeup: Arc<dyn Fn() + Send + Sync> = Arc::new(callback);
        let mut state = self.state.lock();
        state.wakeup = Some(Arc::clone(&wakeup));
        let pending = state.raise_pending || state.shutting_down;
        drop(state);
        if pending {
            wakeup();
        }
    }

    fn withdraw_gui(&self) {
        let mut state = self.state.lock();
        state.graphical = false;
        state.raise_pending = false;
        state.wakeup = None;
    }

    fn take_raise_request(&self) -> bool {
        let mut state = self.state.lock();
        std::mem::take(&mut state.raise_pending) && !state.shutting_down
    }

    pub(crate) fn request_ack(&self, command: AppCommand) -> Ack {
        match command {
            AppCommand::Core(command) => self.core.request_ack(command),
            AppCommand::ShowWindow => {
                let result = self.raise();
                Ack {
                    ok: result.is_ok(),
                    error: result.err().map(|error| error.to_string()),
                    revision: self.core.revision(),
                    scan_id: None,
                    relocation_id: None,
                }
            }
            AppCommand::Shutdown => {
                let _ = self.request_shutdown();
                self.runtime.shutdown_ack(|| self.core.revision())
            }
        }
    }
}

/// Owns one instance's core runtime, external services, and optional GUI session.
/// Closing a window releases the session's view, never the application services.
/// App retains socket acquisition until core, media, and transport have stopped.
pub struct App {
    handle: AppHandle,
    runtime: Runtime,
    signals: Option<ShutdownSignals>,
    media: Option<MediaWorker>,
    artwork: Arc<ArtworkManager>,
    tray: Option<TrayController>,
    gui: Option<GuiState>,
    server: Option<ipc::Server>,
    session: Option<logging::Session>,
    // Retire the socket last, including after partial startup and core join.
    _socket_owner: ipc::SocketOwner,
}

/// GPUI integration state, not a second application or services owner.
struct GuiState {
    session: Rc<RefCell<UiSession>>,
    window_closed: Option<Subscription>,
    app_quit: Option<Subscription>,
    wake_task: Option<Task<()>>,
    windowing_task: Option<Task<()>>,
    environment: Option<GraphicalEnvironment>,
    headless: bool,
    error: Option<anyhow::Error>,
    quitting: bool,
}

impl App {
    /// Show an existing instance, or start a graphical application if none exists.
    /// Import paths must already be resolved against the calling client's cwd.
    pub fn open_gui(data_dir: PathBuf, config_path: PathBuf, paths: Vec<PathBuf>) -> Result<()> {
        match ipc::request_ack(&data_dir, &AppCommand::ShowWindow) {
            Ok(ack) if ack.ok => {
                if !paths.is_empty() {
                    let ack = ipc::request_ack(
                        &data_dir,
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
        let socket_owner = ipc::SocketOwner::acquire(&data_dir)?;
        let signals = Signals::new([SIGINT, SIGTERM]).context("Registering shutdown signals")?;
        let config = Config::load(&config_path)?;
        let session = logging::Session::start(&data_dir, &config)?;
        let runtime =
            Runtime::start_with_logging(&data_dir, &config_path, config, session.control())?;
        let handle = AppHandle::new(&runtime, graphical);
        let signals = ShutdownSignals::start(signals, handle.clone())?;
        let server = ipc::Server::start(&socket_owner, runtime.handle.clone(), handle.clone())?;
        let artwork = Arc::new(ArtworkManager::new(&data_dir));
        let mut app = Self {
            handle,
            runtime,
            signals: Some(signals),
            media: None,
            artwork,
            tray: None,
            gui: None,
            server: Some(server),
            session: Some(session),
            _socket_owner: socket_owner,
        };
        app.media = Some(MediaWorker::start(
            app.runtime.handle.clone(),
            app.handle.clone(),
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
                    app.handle.clone(),
                    data_dir.join("layout.json"),
                    Arc::clone(&app.artwork),
                ),
                window_closed: None,
                app_quit: None,
                wake_task: None,
                windowing_task: None,
                environment: None,
                headless: false,
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
                tracing::error!(%error, "Could not open Rivu window; activate the tray icon or use rivu gui to retry");
                gui.error = Some(error.context("Could not open Rivu window"));
            }
        }
    }

    fn handle_requests(owner: &Rc<RefCell<Self>>, cx: &mut GpuiApp) {
        let mut app = owner.borrow_mut();
        let Some(gui) = app.gui.as_ref().filter(|gui| !gui.quitting) else {
            return;
        };
        if app.handle.is_shutting_down() {
            app.quit_gui(cx);
            return;
        }
        // Leave Raise pending while a connection switch is in flight. Its
        // completion reconciles the coalesced request, including a late Quit.
        if gui.windowing_task.is_some() || !app.handle.take_raise_request() {
            return;
        }
        if gui.headless {
            let environment = gui
                .environment
                .clone()
                .expect("detached display environment");
            drop(app);
            Self::switch_display(owner, WindowingRequest::Windowed(environment), cx);
        } else {
            app.show_window(cx);
        }
    }

    fn idle_display(owner: &Rc<RefCell<Self>>, cx: &mut GpuiApp) {
        let app = owner.borrow();
        let Some(gui) = app.gui.as_ref() else {
            return;
        };
        if gui.quitting
            || gui.headless
            || gui.windowing_task.is_some()
            || gui.environment.is_none()
            || gui.session.borrow().has_window()
            || app.handle.is_shutting_down()
        {
            return;
        }
        let raising = app.handle.state.lock().raise_pending;
        drop(app);
        if raising {
            Self::handle_requests(owner, cx);
        } else {
            Self::switch_display(owner, WindowingRequest::Headless, cx);
        }
    }

    fn switch_display(owner: &Rc<RefCell<Self>>, request: WindowingRequest, cx: &mut GpuiApp) {
        let headless = matches!(request, WindowingRequest::Headless);
        let transition = cx.request_windowing(request);
        let weak_owner = Rc::downgrade(owner);
        let task = cx.spawn(async move |cx| {
            let result = transition.await;
            cx.update(|cx| {
                let Some(owner) = weak_owner.upgrade() else {
                    return;
                };
                let mut app = owner.borrow_mut();
                let gui = app.gui.as_mut().expect("graphical application");
                gui.windowing_task.take();
                match result {
                    Ok(()) => {
                        gui.headless = headless;
                        if !headless && !app.handle.is_shutting_down() {
                            app.show_window(cx);
                        }
                    }
                    Err(error) if headless => {
                        tracing::warn!(%error, "Could not release the idle display connection");
                    }
                    Err(error) => {
                        tracing::error!(%error, "Could not reconnect the display; activate the tray icon or use rivu gui to retry");
                        gui.error = Some(error.context("Could not reconnect the display"));
                    }
                }
                drop(app);
                Self::handle_requests(&owner, cx);
            });
        });
        owner
            .borrow_mut()
            .gui
            .as_mut()
            .expect("graphical application")
            .windowing_task = Some(task);
    }

    fn shutdown_gui(&mut self) {
        let Some(gui) = self.gui.as_mut() else {
            return;
        };
        if gui.quitting {
            return;
        }
        gui.quitting = true;
        let _ = self.handle.request_shutdown();
        // Withdraw CanRaise and stop producers before releasing the UI receiver.
        self.handle.withdraw_gui();
        gui.wake_task.take();
        gui.windowing_task.take();
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
            self.tray = TrayController::start(self.runtime.handle.clone(), self.handle.clone())?;
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
                let window_closed = cx.on_window_closed(move |cx, id| {
                    if let Some(owner) = closed_owner.upgrade() {
                        if let Some(gui) = owner.borrow().gui.as_ref() {
                            gui.session.borrow_mut().window_closed(id);
                        }
                        // The close observer still runs inside the removed
                        // Window's stack frame. Detach only after it is dropped.
                        let owner = Rc::downgrade(&owner);
                        cx.defer(move |cx| {
                            if let Some(owner) = owner.upgrade() {
                                Self::idle_display(&owner, cx);
                            }
                        });
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
                gpui_owner.borrow().handle.set_wakeup(move || {
                    let _ = wake_sender.lock().try_send(());
                });
                let request_owner = Rc::downgrade(&gpui_owner);
                let wake_task = cx.spawn(async move |cx| {
                    while wake_receiver.next().await.is_some() {
                        let Some(app) = request_owner.upgrade() else {
                            break;
                        };
                        cx.update(|cx| Self::handle_requests(&app, cx));
                    }
                });
                let mut app = gpui_owner.borrow_mut();
                let gui = app.gui.as_mut().expect("graphical application");
                gui.environment = cx.graphical_environment();
                gui.window_closed = Some(window_closed);
                gui.app_quit = Some(app_quit);
                gui.wake_task = Some(wake_task);

                // Reconcile signals and Raise accepted before registration.
                if app.handle.is_shutting_down() {
                    app.quit_gui(cx);
                } else {
                    app.handle.take_raise_request();
                    app.show_window(cx);
                }
            });

        let mut app = app.borrow_mut();
        app.shutdown_gui();
        drop(app.tray.take());
        // Blocking core acknowledgment and worker joins are outside GPUI.
        let shutdown = app.handle.request_shutdown();
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
            let _ = self.handle.request_shutdown();
        }
        let join_result = self.runtime.join();
        // Keep IPC/socket acquisition and logging alive through core and MPRIS stop.
        self.stop_services();
        interface_result?;
        join_result.map_err(|_| anyhow!("Rivu core worker panicked"))
    }

    /// Drain producers before flushing, only after core join and outside GPUI.
    fn stop_services(&mut self) {
        drop(self.media.take());
        // Server::drop stops acceptance and drains accepted operations before joining.
        drop(self.server.take());
        drop(self.signals.take());
        if let Some(mut session) = self.session.take() {
            session.shutdown();
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.shutdown_gui();
        drop(self.tray.take());
        if !self.runtime.is_finished() {
            let _ = self.handle.request_shutdown();
        }
        let _ = self.runtime.join();
        self.stop_services();
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
                // Coalesce signals; wait for capacity only in this worker.
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

impl MediaWorker {
    fn start(handle: CoreHandle, app: AppHandle, artwork: Arc<ArtworkManager>) -> Result<Self> {
        let changes = handle.subscribe();
        let (stop, stop_receiver) = bounded(1);
        let worker = thread::Builder::new()
            .name("rivu-mpris-manager".into())
            .spawn(move || {
                let mut service = None;
                let mut enabled = false;
                loop {
                    let state = handle.mpris_service_state();
                    if state.shutting_down {
                        break;
                    }
                    if state.enabled != enabled {
                        enabled = state.enabled;
                        let status = if enabled {
                            match mpris::Mpris::start(
                                handle.clone(),
                                app.clone(),
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

    fn runtime_fixture() -> (tempfile::TempDir, Runtime) {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        crate::config::Config {
            media_read_buffer_mb: 1,
            ffmpeg_enabled: false,
            ..Default::default()
        }
        .save(&config)
        .unwrap();
        let runtime = Runtime::start(directory.path(), &config).unwrap();
        (directory, runtime)
    }

    #[test]
    fn raises_coalesce_across_registration_and_shutdown_withdraws_host() {
        let (_directory, mut runtime) = runtime_fixture();
        let app = AppHandle::new(&runtime, true);
        for _ in 0..1000 {
            app.raise().unwrap();
        }
        let (wake, wakes) = bounded(1);
        app.set_wakeup(move || {
            let _ = wake.try_send(());
        });
        assert_eq!(wakes.try_recv(), Ok(()));
        assert!(app.take_raise_request());
        assert!(!app.take_raise_request());
        app.raise().unwrap();
        assert_eq!(wakes.try_recv(), Ok(()));
        app.request_shutdown().unwrap();
        assert!(!app.can_raise());
        assert!(app.raise().is_err());
        assert!(!app.take_raise_request());
        runtime.join().unwrap();
    }

    #[test]
    fn completion_reconciles_before_and_after_ui_registration() {
        for register_first in [false, true] {
            let (_directory, mut runtime) = runtime_fixture();
            let app = AppHandle::new(&runtime, true);
            let (wake, wakes) = bounded(1);
            let callback = move || {
                let _ = wake.try_send(());
            };
            if register_first {
                app.set_wakeup(callback.clone());
            }
            // Core completion, not an App shutdown request, must wake its owner.
            runtime.control.request_shutdown().unwrap();
            runtime.join().unwrap();
            if !register_first {
                app.set_wakeup(callback);
            }
            assert_eq!(wakes.try_recv(), Ok(()));
            assert!(app.is_shutting_down());
            assert!(!app.can_raise());
        }
    }

    #[test]
    fn app_endpoint_created_after_core_completion_reconciles_shutdown() {
        let (_directory, mut runtime) = runtime_fixture();
        runtime.control.request_shutdown().unwrap();
        runtime.join().unwrap();
        let app = AppHandle::new(&runtime, true);
        assert!(app.is_shutting_down());
        assert!(!app.can_raise());
        assert!(app.raise().is_err());
    }

    #[test]
    fn ipc_shutdown_ack_survives_app_teardown_before_socket_retirement() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().to_path_buf();
        let config_path = data_dir.join("config.toml");
        crate::config::Config {
            media_read_buffer_mb: 1,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            tray_enabled: false,
            ..Default::default()
        }
        .save(&config_path)
        .unwrap();
        let (ready, started) = bounded(1);
        let worker = thread::spawn(move || {
            let app = App::start(data_dir, config_path, Vec::new(), false)?;
            ready.send(()).unwrap();
            app.run()
        });
        started
            .recv_timeout(std::time::Duration::from_secs(15))
            .unwrap();
        let ack = ipc::request_ack(directory.path(), &AppCommand::Shutdown).unwrap();
        assert!(ack.ok);
        worker.join().unwrap().unwrap();
        assert!(ipc::SocketOwner::acquire(directory.path()).is_ok());
    }

    #[test]
    fn duplicate_start_does_not_touch_runtime_storage_or_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let owner = ipc::SocketOwner::acquire(directory.path()).unwrap();
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
        assert!(
            std::os::unix::net::UnixStream::connect_addr(
                &ipc::endpoint::address(directory.path()).unwrap()
            )
            .is_ok()
        );
        drop(owner);
    }

    #[test]
    fn failed_runtime_start_retires_its_bound_socket() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        std::fs::write(&config, b"invalid config intentionally").unwrap();
        assert!(App::start(directory.path().to_owned(), config, Vec::new(), false).is_err());
        assert!(ipc::SocketOwner::acquire(directory.path()).is_ok());
    }
}
