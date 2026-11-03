//! Application lifecycle and services shared by graphical and headless instances.
//! CLI parsing and client-relative path translation remain at the binary boundary.
use rivu_metadata::artwork::ArtworkManager;

mod startup;

#[cfg(test)]
use crate::config::ConfigPersistence;
#[cfg(feature = "gui")]
use crate::tray::TrayController;
use crate::{
    core::{CoreHandle, CoreLifecycle, CoreRuntime},
    execution::TaskRuntime,
    ipc, mpris,
};
use anyhow::{Context, Result, bail};
use async_signal::{Signal, Signals};
#[cfg(test)]
use crossbeam_channel::bounded;
use futures::{FutureExt, StreamExt};
#[cfg(feature = "gui")]
mod gui_adapter;
#[cfg(feature = "gui")]
use rivu_gui::GuiHost;
use rivu_protocol::AppCommand;
use rivu_protocol::{model::Command, response::Ack};
#[cfg(test)]
use std::thread;
#[cfg(test)]
use std::time::Duration;
use std::{panic::AssertUnwindSafe, path::PathBuf, sync::Arc};
#[derive(Default)]
pub(crate) struct ControlState {
    #[cfg(any(test, feature = "gui", feature = "test-support"))]
    graphical: bool,
    #[cfg(any(test, feature = "gui", feature = "test-support"))]
    pub(crate) raise_pending: bool,
    #[cfg(any(test, feature = "gui", feature = "test-support"))]
    quit_pending: bool,
    shutting_down: bool,
    #[cfg(any(test, feature = "gui", feature = "test-support"))]
    wakeup: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Cloneable application endpoint, owned by App across window lifetimes.
#[derive(Clone)]
pub(crate) struct AppHandle {
    core: CoreHandle,
    runtime: CoreLifecycle,
    pub(crate) state: Arc<parking_lot::Mutex<ControlState>>,
}

impl AppHandle {
    pub(crate) fn new(runtime: &CoreRuntime, graphical: bool) -> Self {
        #[cfg(not(any(test, feature = "gui", feature = "test-support")))]
        debug_assert!(!graphical);
        let state = Arc::new(parking_lot::Mutex::new(ControlState {
            #[cfg(any(test, feature = "gui", feature = "test-support"))]
            graphical,
            ..ControlState::default()
        }));
        let completion_state = Arc::clone(&state);
        runtime.control.set_completion_callback(move || {
            let mut state = completion_state.lock();
            state.shutting_down = true;
            #[cfg(any(test, feature = "gui", feature = "test-support"))]
            {
                state.raise_pending = false;
                state.quit_pending = false;
                let wakeup = state.wakeup.clone();
                drop(state);
                if let Some(wakeup) = wakeup {
                    wakeup();
                }
            }
        });
        Self {
            core: runtime.handle.clone(),
            runtime: runtime.control.clone(),
            state,
        }
    }

    #[cfg(any(test, feature = "gui", feature = "test-support"))]
    pub(crate) fn can_raise(&self) -> bool {
        let state = self.state.lock();
        state.graphical && !state.shutting_down
    }

    #[cfg(not(any(test, feature = "gui", feature = "test-support")))]
    pub(crate) fn can_raise(&self) -> bool {
        false
    }

    #[cfg(not(any(test, feature = "gui", feature = "test-support")))]
    pub(crate) fn raise(&self) -> Result<()> {
        bail!("Rivu has no graphical host")
    }

    #[cfg(any(test, feature = "gui", feature = "test-support"))]
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

    #[cfg(any(test, feature = "gui"))]
    /// User-driven Quit is offered to the GUI draft guard before core shutdown.
    /// A single pending bit keeps tray callbacks bounded and nonblocking.
    pub(crate) fn request_user_shutdown(&self) -> Result<()> {
        let mut state = self.state.lock();
        if state.shutting_down {
            return Ok(());
        }
        if !state.graphical {
            drop(state);
            return self.request_shutdown();
        }
        let wakeup = if state.quit_pending {
            None
        } else {
            state.wakeup.clone()
        };
        state.quit_pending = true;
        drop(state);
        if let Some(wakeup) = wakeup {
            wakeup();
        }
        Ok(())
    }

    /// Forced IPC/signal shutdown bypasses UI confirmation and never blocks a GPUI callback.
    pub(crate) fn request_shutdown(&self) -> Result<()> {
        let mut state = self.state.lock();
        if state.shutting_down {
            return Ok(());
        }
        state.shutting_down = true;
        #[cfg(any(test, feature = "gui", feature = "test-support"))]
        {
            state.raise_pending = false;
            state.quit_pending = false;
        }
        #[cfg(any(test, feature = "gui", feature = "test-support"))]
        let wakeup = state.wakeup.clone();
        drop(state);
        let result = self.runtime.request_shutdown();
        #[cfg(any(test, feature = "gui", feature = "test-support"))]
        if let Some(wakeup) = wakeup {
            wakeup();
        }
        result
    }

    #[cfg(any(test, feature = "gui", feature = "test-support"))]
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.state.lock().shutting_down
    }

    #[cfg(any(test, feature = "gui", feature = "test-support"))]
    pub(crate) fn set_wakeup(&self, callback: impl Fn() + Send + Sync + 'static) {
        let wakeup: Arc<dyn Fn() + Send + Sync> = Arc::new(callback);
        let mut state = self.state.lock();
        state.wakeup = Some(Arc::clone(&wakeup));
        let pending = state.raise_pending || state.quit_pending || state.shutting_down;
        drop(state);
        if pending {
            wakeup();
        }
    }

    #[cfg(any(test, feature = "gui", feature = "test-support"))]
    pub(crate) fn withdraw_gui(&self) {
        let mut state = self.state.lock();
        state.graphical = false;
        state.raise_pending = false;
        state.quit_pending = false;
        state.wakeup = None;
    }

    #[cfg(any(test, feature = "gui", feature = "test-support"))]
    pub(crate) fn take_raise_request(&self) -> bool {
        let mut state = self.state.lock();
        std::mem::take(&mut state.raise_pending) && !state.shutting_down
    }

    #[cfg(any(test, feature = "gui", feature = "test-support"))]
    pub(crate) fn take_quit_request(&self) -> bool {
        let mut state = self.state.lock();
        std::mem::take(&mut state.quit_pending) && !state.shutting_down
    }

    pub(crate) async fn request_ack_async(&self, command: AppCommand) -> Ack {
        match command {
            AppCommand::Core(command) => self.core.request_ack(command).await,
            AppCommand::ShowWindow => {
                Ack::from_result(self.raise(), self.core.publication_revision())
            }
            AppCommand::Shutdown => {
                let _ = self.request_shutdown();
                self.runtime
                    .shutdown_ack_async(|| self.core.publication_revision())
                    .await
            }
        }
    }
}

/// Owns one instance's core runtime, external services, and optional GUI session.
/// Closing a window releases the session's view, never the application services.
/// App retains socket acquisition until core, media, and transport have stopped.
pub struct App {
    handle: AppHandle,
    runtime: CoreRuntime,
    signals: Option<ShutdownSignals>,
    media: Option<mpris::MprisController>,
    artwork: Arc<ArtworkManager>,
    #[cfg(feature = "gui")]
    tray: Option<TrayController>,
    #[cfg(feature = "gui")]
    gui: Option<GuiHost>,
    server: Option<ipc::Server>,
    session: Option<rivu_utils::logging::Session>,
    // Field order keeps execution alive through all owners, including GUI teardown.
    tasks: TaskRuntime,
    // Retire the socket last, including after partial startup and core join.
    _socket_owner: ipc::SocketOwner,
}

impl App {
    /// Show an existing instance, or start a graphical application if none exists.
    /// Import paths must already be resolved against the calling client's cwd.
    #[cfg(feature = "gui")]
    pub fn open_gui(data_dir: PathBuf, config_path: PathBuf, paths: Vec<PathBuf>) -> Result<()> {
        match rivu_client::request_ack(&data_dir, AppCommand::ShowWindow) {
            Ok(ack) if ack.is_success() => {
                if !paths.is_empty() {
                    let ack = rivu_client::request_ack(
                        &data_dir,
                        Command::Scan {
                            paths,
                            force: false,
                        },
                    )?;
                    if !ack.is_success() {
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
        #[cfg(not(feature = "gui"))]
        if graphical {
            bail!("Rivu was built without the graphical frontend");
        }
        rivu_utils::logging::init(None)?;
        let socket_owner = startup::acquire_storage(&data_dir, &config_path)?;
        let signals =
            Signals::new([Signal::Int, Signal::Term]).context("Registering shutdown signals")?;
        let config = CoreRuntime::load_startup_config(&config_path)?;
        let artwork_limit_bytes = config.artwork_file_limit_mib as usize * 1024 * 1024;
        let tasks = TaskRuntime::start()?;
        #[cfg(feature = "gui")]
        let layouts = graphical.then(|| {
            let path = data_dir.join("layout.json");
            let workers = Arc::clone(&tasks.workers);
            tasks.handle.spawn(async move {
                workers
                    .execute(move || rivu_gui::LoadedLayouts::load(path))
                    .await
            })
        });
        let i18n = Arc::new(if graphical {
            rivu_presentation::i18n::Localizer::new(&config.ui_language)
        } else {
            rivu_presentation::i18n::Localizer::for_cli("auto")
        });
        let session = rivu_utils::logging::Session::start(
            &data_dir,
            &crate::config::logging_options(&config),
        )?;
        let runtime = CoreRuntime::start_with_logging(
            &data_dir,
            &config_path,
            config,
            session.control(),
            Arc::clone(&tasks.handle),
            Arc::clone(&tasks.workers),
        )?;
        let handle = AppHandle::new(&runtime, graphical);
        let signals = ShutdownSignals::start(signals, handle.clone(), &tasks.handle);
        let server = ipc::Server::start(
            &socket_owner,
            runtime.handle.clone(),
            handle.clone(),
            &tasks.handle,
        )?;
        let artwork = Arc::new(
            ArtworkManager::new(&data_dir)
                .with_default_image(rivu_presentation::DEFAULT_IMAGE)
                .with_max_image_bytes(artwork_limit_bytes),
        );
        let mut app = Self {
            handle,
            runtime,
            signals: Some(signals),
            media: None,
            artwork,
            #[cfg(feature = "gui")]
            tray: None,
            #[cfg(feature = "gui")]
            gui: None,
            server: Some(server),
            session: Some(session),
            tasks,
            _socket_owner: socket_owner,
        };
        app.media = Some(mpris::MprisController::start(
            app.runtime.handle.clone(),
            app.handle.clone(),
            Arc::clone(&app.artwork),
            &app.tasks.handle,
            Arc::clone(&app.tasks.workers),
        ));
        if !paths.is_empty() {
            app.runtime.handle.send(Command::Scan {
                paths,
                force: false,
            })?;
        }
        if graphical {
            #[cfg(feature = "gui")]
            {
                let port = gui_adapter::GuiAdapter::new(
                    app.runtime.handle.clone(),
                    app.handle.clone(),
                    Arc::clone(&app.tasks.workers),
                );
                app.gui = Some(GuiHost::new(
                    rivu_gui::HostHandle::new(port),
                    futures::executor::block_on(layouts.expect("graphical layout preparation"))?,
                    Arc::clone(&app.artwork),
                    Arc::clone(&i18n),
                ));
                if app.runtime.handle.config_snapshot().tray_enabled {
                    app.tray = Some(TrayController::start(
                        app.runtime.handle.clone(),
                        app.handle.clone(),
                        Arc::clone(&i18n),
                        &app.tasks.handle,
                    ));
                }
            }
        } else {
            println!(
                "{}",
                i18n.format(
                    "core-ready",
                    &[("path", data_dir.display().to_string().into())]
                )
            );
        }
        Ok(app)
    }

    /// Run until application shutdown. Core and services retire outside GPUI.
    pub fn run(mut self) -> Result<()> {
        #[cfg(feature = "gui")]
        if let Some(gui) = self.gui.take() {
            let result = gui.run();
            drop(self.tray.take());
            return self.finish(result);
        }
        self.finish(Ok(()))
    }

    fn finish(&mut self, interface_result: Result<()>) -> Result<()> {
        if interface_result.is_err() {
            let _ = self.handle.request_shutdown();
        }
        let join_result = self.runtime.join();
        // Keep IPC/socket acquisition and logging alive through core and MPRIS stop.
        self.stop_services();
        interface_result?;
        join_result
    }

    /// Drain producers before flushing, only after core join and outside GPUI.
    fn stop_services(&mut self) {
        drop(self.media.take());
        // Server::drop drains its owned sessions before the async executor stops.
        drop(self.server.take());
        drop(self.signals.take());
        if let Some(mut session) = self.session.take() {
            session.shutdown();
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        #[cfg(feature = "gui")]
        drop(self.gui.take());
        #[cfg(feature = "gui")]
        drop(self.tray.take());
        if !self.runtime.is_finished() {
            let _ = self.handle.request_shutdown();
        }
        let _ = self.runtime.join();
        self.stop_services();
    }
}

struct ShutdownSignals {
    worker: Option<async_executor::Task<()>>,
}

impl ShutdownSignals {
    fn start(
        mut signals: Signals,
        app: AppHandle,
        executor: &Arc<async_executor::Executor<'static>>,
    ) -> Self {
        let worker = executor.spawn(async move {
            if let Some(signal) = signals.next().await {
                match signal {
                    Ok(signal) => {
                        tracing::info!(?signal, "shutdown_signal_received");
                        let _ = app.request_shutdown();
                    }
                    Err(error) => tracing::warn!(%error, "shutdown_signal_failed"),
                }
            }
        });
        Self {
            worker: Some(worker),
        }
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            // Services retire while the application's executor is still running.
            if async_io::block_on(AssertUnwindSafe(worker.cancel()).catch_unwind()).is_err() {
                tracing::warn!("shutdown_signal_task_panicked");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Full applications share the process-wide logging session.
    static APP_SESSION: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    fn runtime_fixture() -> (tempfile::TempDir, CoreRuntime) {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        crate::config::Config {
            media_read_buffer_mib: 1,
            ffmpeg_enabled: false,
            ..Default::default()
        }
        .save(&config)
        .unwrap();
        let runtime = CoreRuntime::start(directory.path(), &config).unwrap();
        (directory, runtime)
    }

    #[test]
    fn invalid_host_config_path_fails_before_logging_and_retires_socket() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("data");
        let config = directory
            .path()
            .join(OsString::from_vec(b"config-\xff.toml".to_vec()));
        let error = App::start(data_dir.clone(), config.clone(), Vec::new(), false)
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("Configuration path is not valid UTF-8")
        );
        assert!(!config.exists());
        assert!(std::fs::read_dir(&data_dir).unwrap().next().is_none());
        assert!(ipc::SocketOwner::acquire(&data_dir).is_ok());
    }

    #[cfg(feature = "gui")]
    #[test]
    fn opening_existing_gui_does_not_validate_unused_config_path() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let (directory, mut runtime) = runtime_fixture();
        let owner = ipc::SocketOwner::acquire(directory.path()).unwrap();
        let app = AppHandle::new(&runtime, true);
        let server = ipc::Server::start(
            &owner,
            runtime.handle.clone(),
            app.clone(),
            runtime.test_executor(),
        )
        .unwrap();
        let unused_config = directory
            .path()
            .join(OsString::from_vec(b"unused-\xff.toml".to_vec()));
        let original = b"intentionally not valid TOML";
        std::fs::write(&unused_config, original).unwrap();
        App::open_gui(
            directory.path().to_owned(),
            unused_config.clone(),
            Vec::new(),
        )
        .unwrap();
        assert!(app.take_raise_request());
        assert_eq!(std::fs::read(&unused_config).unwrap(), original);
        app.request_shutdown().unwrap();
        runtime.join().unwrap();
        drop(server);
        drop(owner);
    }

    #[cfg(not(feature = "gui"))]
    #[test]
    fn disabled_graphical_start_rejects_before_touching_storage() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("data");
        let config = directory.path().join("config.toml");
        let error = App::start(data_dir.clone(), config.clone(), Vec::new(), true)
            .err()
            .unwrap();
        assert!(error.to_string().contains("without the graphical frontend"));
        assert!(!data_dir.exists());
        assert!(!config.exists());
    }

    #[test]
    fn user_quit_coalesces_without_starting_core_shutdown() {
        let (_directory, mut runtime) = runtime_fixture();
        let app = AppHandle::new(&runtime, true);
        for _ in 0..1000 {
            app.request_user_shutdown().unwrap();
        }
        let (wake, wakes) = bounded(1);
        app.set_wakeup(move || {
            let _ = wake.try_send(());
        });
        assert_eq!(wakes.try_recv(), Ok(()));
        assert!(!app.is_shutting_down());
        assert!(!runtime.is_finished());
        assert!(app.take_quit_request());
        assert!(!app.take_quit_request());
        // Cancelling the GUI confirmation consumes only this intent; another
        // tray Quit must be deliverable without an earlier shutdown leaking.
        app.request_user_shutdown().unwrap();
        assert_eq!(wakes.try_recv(), Ok(()));
        assert!(app.take_quit_request());
        assert!(!app.is_shutting_down());
        app.request_user_shutdown().unwrap();
        app.request_shutdown().unwrap();
        assert!(!app.take_quit_request());
        runtime.join().unwrap();
    }

    #[test]
    fn headless_user_quit_shuts_down_without_a_gui_guard() {
        let (_directory, mut runtime) = runtime_fixture();
        let app = AppHandle::new(&runtime, false);
        app.request_user_shutdown().unwrap();
        assert!(app.is_shutting_down());
        assert!(!app.take_quit_request());
        runtime.join().unwrap();
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
        let _session = APP_SESSION.lock();
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().to_path_buf();
        let config_path = data_dir.join("config.toml");
        crate::config::Config {
            media_read_buffer_mib: 1,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            tray_enabled: false,
            ..Default::default()
        }
        .save(&config_path)
        .unwrap();
        let (ready, started) = bounded(1);
        let worker = thread::spawn(move || {
            let app = App::start(data_dir, config_path, Vec::new(), false);
            ready
                .send(
                    app.as_ref()
                        .map(|_| ())
                        .map_err(|error| format!("{error:#}")),
                )
                .unwrap();
            app?.run()
        });
        started
            .recv_timeout(std::time::Duration::from_secs(15))
            .unwrap()
            .unwrap();
        let ack = rivu_client::request_ack(directory.path(), AppCommand::Shutdown).unwrap();
        assert!(ack.is_success());
        worker.join().unwrap().unwrap();
        assert!(ipc::SocketOwner::acquire(directory.path()).is_ok());
    }

    #[test]
    fn accepted_shutdown_returns_final_save_failure_after_services_retire() {
        let _session = APP_SESSION.lock();
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        crate::config::Config {
            media_read_buffer_mib: 1,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            tray_enabled: false,
            ..Default::default()
        }
        .save(&config_path)
        .unwrap();
        let data_dir = directory.path().to_owned();
        let config = config_path.clone();
        let (ready, started) = bounded(1);
        let worker = thread::spawn(move || {
            let app = App::start(data_dir, config, Vec::new(), false);
            ready
                .send(
                    app.as_ref()
                        .map(|_| ())
                        .map_err(|error| format!("{error:#}")),
                )
                .unwrap();
            app?.run()
        });
        started
            .recv_timeout(Duration::from_secs(15))
            .unwrap()
            .unwrap();
        // An externally edited invalid file must not be overwritten. A quick
        // control changes desired config, guaranteeing a final save attempt.
        let invalid = "[playback]\nvolume_percent = 101\n";
        std::fs::write(&config_path, invalid).unwrap();
        assert!(
            rivu_client::request_ack(directory.path(), Command::Volume { value: 0.25 })
                .unwrap()
                .is_success()
        );
        let ack = rivu_client::request_ack(directory.path(), AppCommand::Shutdown).unwrap();
        assert!(ack.is_success());
        let error = worker.join().unwrap().unwrap_err();
        assert!(format!("{error:#}").contains("Saving configuration"));
        assert_eq!(std::fs::read_to_string(config_path).unwrap(), invalid);
        // App's error is observable only after transport/core teardown, including
        // the accepted IPC reply. Ownership can immediately be reacquired.
        assert!(ipc::SocketOwner::acquire(directory.path()).is_ok());
        let database = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
        database
            .execute_batch("BEGIN EXCLUSIVE; ROLLBACK;")
            .unwrap();
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
                &rivu_protocol::wire::address(directory.path()).unwrap()
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

    #[test]
    fn ipc_commands_and_sql_queries_finish_with_the_entire_blocking_pool_occupied() {
        let (directory, mut runtime) = runtime_fixture();
        let owner = ipc::SocketOwner::acquire(directory.path()).unwrap();
        let app = AppHandle::new(&runtime, false);
        let server = ipc::Server::start(
            &owner,
            runtime.handle.clone(),
            app.clone(),
            runtime.test_executor(),
        )
        .unwrap();
        let (started, running) = bounded(4);
        let (release, released) = bounded::<()>(0);
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let started = started.clone();
                let released = released.clone();
                let workers = Arc::clone(runtime.test_workers());
                runtime.test_executor().spawn(async move {
                    workers
                        .execute(move || {
                            started.send(()).unwrap();
                            let _ = released.recv();
                        })
                        .await
                })
            })
            .collect();
        for _ in 0..4 {
            running.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert!(
            rivu_client::request_ack(directory.path(), Command::Volume { value: 0.4 })
                .unwrap()
                .is_success()
        );
        assert_eq!(
            rivu_client::get_state(
                directory.path(),
                rivu_protocol::response::StateSections::ALL
            )
            .unwrap()
            .playback
            .unwrap()
            .playback
            .volume,
            0.4
        );
        assert!(matches!(
            rivu_client::query(
                directory.path(),
                rivu_protocol::model::Query::Track { track_id: 1 }
            )
            .unwrap()
            .result,
            Ok(rivu_protocol::response::ViewResponse::Track(None))
        ));
        // No waiter borrowed a blocking slot; producers can run as soon as
        // capacity is released, without restarting or replaying any request.
        drop(release);
        for worker in workers {
            futures::executor::block_on(worker).unwrap();
        }
        assert!(
            rivu_client::request_ack(directory.path(), Command::OptimizeDatabase)
                .unwrap()
                .is_success()
        );
        app.request_shutdown().unwrap();
        runtime.join().unwrap();
        drop(server);
    }
}
