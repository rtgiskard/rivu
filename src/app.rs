//! Headless application service owner. Desktops are independent IPC clients.
use rivu_metadata::artwork::ArtworkManager;
#[cfg(feature = "gui")]
mod desktop_host;
#[cfg(feature = "gui")]
mod desktop_launcher;
#[cfg(any(feature = "gui", feature = "tui"))]
mod frontend_host;
mod startup;
#[cfg(test)]
use crate::config::ConfigPersistence;
#[cfg(feature = "gui")]
use crate::tray;
use crate::{
    core::{CoreHandle, CoreLifecycle, CoreRuntime},
    execution::TaskRuntime,
    ipc, mpris,
};
use anyhow::{Context, Result, bail};
use async_signal::{Signal, Signals};
#[cfg(test)]
use crossbeam_channel::bounded;
#[cfg(feature = "gui")]
use desktop_launcher::DesktopLauncher;
use futures::{FutureExt, StreamExt};
use rivu_protocol::{AppCommand, model::Command, response::Ack};
use std::{
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
#[cfg(test)]
use std::{thread, time::Duration};

#[derive(Default)]
struct DesktopPresence {
    subscribers: parking_lot::Mutex<Vec<async_channel::Sender<()>>>,
}
impl DesktopPresence {
    fn notify(&self) {
        self.subscribers.lock().retain(|sender| {
            let _ = sender.try_send(());
            !sender.is_closed()
        });
    }
}
pub(crate) struct DesktopRegistration(Arc<DesktopPresence>);
impl Drop for DesktopRegistration {
    fn drop(&mut self) {
        self.0.notify();
    }
}
/// Service shutdown and remote graphical activation. Never owns a GUI.
#[derive(Clone)]
pub(crate) struct AppHandle {
    core: CoreHandle,
    runtime: CoreLifecycle,
    #[cfg(feature = "gui")]
    data_dir: Arc<PathBuf>,
    shutting_down: Arc<AtomicBool>,
    desktop: Arc<DesktopPresence>,
    #[cfg(feature = "gui")]
    activation: async_channel::Sender<()>,
    #[cfg(feature = "gui")]
    activation_requests: async_channel::Receiver<()>,
}
impl AppHandle {
    pub(crate) fn new(runtime: &CoreRuntime, data_dir: &Path) -> Self {
        #[cfg(feature = "gui")]
        let (activation, activation_requests) = async_channel::bounded(1);
        #[cfg(not(feature = "gui"))]
        let _ = data_dir;
        let shutting_down = Arc::new(AtomicBool::new(false));
        let completed = Arc::clone(&shutting_down);
        runtime.control.set_completion_callback(move || {
            completed.store(true, Ordering::Release);
        });
        Self {
            core: runtime.handle.clone(),
            runtime: runtime.control.clone(),
            #[cfg(feature = "gui")]
            data_dir: Arc::new(data_dir.to_owned()),
            shutting_down,
            desktop: Arc::new(DesktopPresence::default()),
            #[cfg(feature = "gui")]
            activation,
            #[cfg(feature = "gui")]
            activation_requests,
        }
    }
    pub(crate) fn register_desktop(&self) -> DesktopRegistration {
        self.desktop.notify();
        DesktopRegistration(Arc::clone(&self.desktop))
    }
    #[cfg(feature = "gui")]
    pub(crate) fn subscribe_desktop(&self) -> async_channel::Receiver<()> {
        let (sender, receiver) = async_channel::bounded(1);
        let _ = sender.try_send(());
        self.desktop.subscribers.lock().push(sender);
        receiver
    }
    pub(crate) fn can_raise(&self) -> bool {
        cfg!(feature = "gui") && !self.is_shutting_down()
    }
    pub(crate) fn raise(&self) -> Result<()> {
        if self.is_shutting_down() {
            bail!("Rivu is shutting down");
        }
        #[cfg(not(feature = "gui"))]
        bail!("Rivu was built without the graphical frontend");
        #[cfg(feature = "gui")]
        match self.activation.try_send(()) {
            Ok(()) | Err(async_channel::TrySendError::Full(())) => Ok(()),
            Err(async_channel::TrySendError::Closed(())) => bail!("Desktop launcher is stopped"),
        }
    }
    pub(crate) fn request_shutdown(&self) -> Result<()> {
        self.shutting_down.store(true, Ordering::Release);
        self.runtime.request_shutdown()
    }
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
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

/// Owns services and storage authority until core persistence and IPC drain finish.
pub struct App {
    handle: AppHandle,
    runtime: CoreRuntime,
    signals: Option<ShutdownSignals>,
    media: Option<mpris::MprisController>,
    #[cfg(feature = "gui")]
    tray: Option<tray::TrayController>,
    #[cfg(feature = "gui")]
    activation: Option<DesktopLauncher>,
    artwork: Arc<ArtworkManager>,
    server: Option<ipc::Server>,
    visual: Option<ipc::pcm::PcmServer>,
    waveform: Option<ipc::waveform::WaveServer>,
    session: Option<rivu_utils::logging::Session>,
    tasks: TaskRuntime,
    _socket_owner: ipc::SocketOwner,
}
#[cfg(feature = "gui")]
pub(crate) fn build_info() -> rivu_gui::BuildInfo {
    rivu_gui::BuildInfo {
        timestamp: env!("RIVU_BUILD_TIMESTAMP")
            .parse()
            .expect("build timestamp is an integer"),
        git_version: env!("RIVU_GIT_VERSION"),
        features: env!("RIVU_BUILD_FEATURES"),
    }
}

impl App {
    /// Attach a standalone desktop, starting an independent service only if absent.
    #[cfg(feature = "gui")]
    pub fn open_gui(data_dir: PathBuf, config_path: PathBuf, paths: Vec<PathBuf>) -> Result<()> {
        desktop_host::open(data_dir, config_path, paths)
    }
    /// Attach a terminal client, starting an independent service only if absent.
    #[cfg(feature = "tui")]
    pub fn open_tui(data_dir: PathBuf, config_path: PathBuf) -> Result<()> {
        let data_dir = frontend_host::absolute(data_dir)?;
        let config_path = frontend_host::absolute(config_path)?;
        frontend_host::ensure_service(&data_dir, &config_path)?;
        rivu_tui::run(&data_dir, "auto")
    }
    pub fn start(data_dir: PathBuf, config_path: PathBuf, paths: Vec<PathBuf>) -> Result<Self> {
        rivu_utils::logging::init(None)?;
        let socket_owner = startup::acquire_storage(&data_dir, &config_path)?;
        let signals =
            Signals::new([Signal::Int, Signal::Term]).context("Registering shutdown signals")?;
        let config = CoreRuntime::load_startup_config(&config_path)?;
        let artwork_limit_bytes = config.artwork_file_limit_mib as usize * 1024 * 1024;
        #[cfg(feature = "gui")]
        let tray_enabled = config.tray_enabled;
        #[cfg(feature = "gui")]
        let tray_i18n = Arc::new(rivu_presentation::i18n::Localizer::new(&config.ui_language));
        let tasks = TaskRuntime::start()?;
        let i18n = rivu_presentation::i18n::Localizer::for_cli("auto");
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
        let handle = AppHandle::new(&runtime, &data_dir);
        let signals = ShutdownSignals::start(signals, handle.clone(), &tasks.handle);
        let visual = ipc::pcm::PcmServer::start(
            &data_dir,
            runtime.handle.clone(),
            socket_owner.instance_id(),
        )?;
        let waveform = ipc::waveform::WaveServer::start(
            &data_dir,
            runtime.handle.clone(),
            socket_owner.instance_id(),
            &tasks.handle,
            &tasks.workers,
        )?;
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
            #[cfg(feature = "gui")]
            tray: None,
            #[cfg(feature = "gui")]
            activation: None,
            artwork,
            server: Some(server),
            session: Some(session),
            tasks,
            _socket_owner: socket_owner,
            visual: Some(visual),
            waveform: Some(waveform),
        };
        app.media = Some(mpris::MprisController::start(
            app.runtime.handle.clone(),
            app.handle.clone(),
            Arc::clone(&app.artwork),
            &app.tasks.handle,
            Arc::clone(&app.tasks.workers),
        ));
        #[cfg(feature = "gui")]
        {
            app.activation = Some(DesktopLauncher::start(
                app.handle.clone(),
                config_path,
                app._socket_owner.instance_id(),
                &app.tasks.handle,
            ));
            if tray_enabled {
                app.tray = Some(tray::TrayController::start(
                    app.runtime.handle.clone(),
                    app.handle.clone(),
                    tray_i18n,
                    &app.tasks.handle,
                ));
            }
        }
        if !paths.is_empty() {
            app.runtime.handle.send(Command::Scan {
                paths,
                force: false,
            })?;
        }
        #[cfg(any(feature = "gui", feature = "tui"))]
        frontend_host::detach_startup_diagnostics()?;
        println!(
            "{}",
            i18n.format(
                "core-ready",
                &[("path", data_dir.display().to_string().into())]
            )
        );
        Ok(app)
    }
    pub fn run(mut self) -> Result<()> {
        let joined = self.runtime.join();
        self.stop_services();
        joined
    }
    fn stop_services(&mut self) {
        #[cfg(feature = "gui")]
        {
            drop(self.tray.take());
            drop(self.activation.take());
        }
        drop(self.media.take());
        drop(self.server.take());
        drop(self.waveform.take());
        drop(self.visual.take());
        drop(self.signals.take());
        if let Some(mut session) = self.session.take() {
            session.shutdown();
        }
    }
}
impl Drop for App {
    fn drop(&mut self) {
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
        assert!(App::start(data_dir.clone(), config.clone(), Vec::new()).is_err());
        assert!(!config.exists());
        assert!(std::fs::read_dir(&data_dir).unwrap().next().is_none());
        assert!(ipc::SocketOwner::acquire(&data_dir).is_ok());
    }

    #[test]
    fn completed_service_rejects_activation() {
        let (directory, mut runtime) = runtime_fixture();
        runtime.control.request_shutdown().unwrap();
        runtime.join().unwrap();
        let app = AppHandle::new(&runtime, directory.path());
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
            let app = App::start(data_dir, config_path, Vec::new());
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
            let app = App::start(data_dir, config, Vec::new());
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
        worker.join().unwrap().unwrap_err();
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
        assert!(App::start(directory.path().to_owned(), config.clone(), Vec::new()).is_err());
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
        assert!(App::start(directory.path().to_owned(), config, Vec::new()).is_err());
        assert!(ipc::SocketOwner::acquire(directory.path()).is_ok());
    }

    #[test]
    fn ipc_commands_and_sql_queries_finish_with_the_entire_blocking_pool_occupied() {
        let (directory, mut runtime) = runtime_fixture();
        let owner = ipc::SocketOwner::acquire(directory.path()).unwrap();
        let app = AppHandle::new(&runtime, directory.path());
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

#[cfg(test)]
mod lifetime_tests {
    use super::*;

    #[test]
    fn desktop_registration_does_not_own_service_lifetime() {
        let directory = tempfile::tempdir().unwrap();
        let mut runtime =
            CoreRuntime::start(directory.path(), &directory.path().join("config.toml")).unwrap();
        let app = AppHandle::new(&runtime, directory.path());
        let desktop = app.register_desktop();
        drop(desktop);
        assert!(!app.is_shutting_down());
        assert!(!runtime.is_finished());
        assert_eq!(app.can_raise(), cfg!(feature = "gui"));
        app.request_shutdown().unwrap();
        runtime.join().unwrap();
    }

    #[cfg(feature = "gui")]
    #[test]
    fn activation_intents_coalesce_and_remain_available_after_desktop_exit() {
        let directory = tempfile::tempdir().unwrap();
        let mut runtime =
            CoreRuntime::start(directory.path(), &directory.path().join("config.toml")).unwrap();
        let app = AppHandle::new(&runtime, directory.path());
        for _ in 0..1000 {
            app.raise().unwrap();
        }
        assert!(app.activation_requests.try_recv().is_ok());
        assert!(app.activation_requests.try_recv().is_err());
        let changes = app.subscribe_desktop();
        assert!(changes.try_recv().is_ok());
        let desktop = app.register_desktop();
        assert!(changes.try_recv().is_ok());
        drop(desktop);
        assert!(changes.try_recv().is_ok());
        app.raise().unwrap();
        assert!(app.activation_requests.try_recv().is_ok());
        app.request_shutdown().unwrap();
        assert!(app.raise().is_err());
        runtime.join().unwrap();
    }

    #[cfg(not(feature = "gui"))]
    #[test]
    fn service_without_gui_support_rejects_graphical_activation() {
        let directory = tempfile::tempdir().unwrap();
        let mut runtime =
            CoreRuntime::start(directory.path(), &directory.path().join("config.toml")).unwrap();
        let app = AppHandle::new(&runtime, directory.path());
        assert!(!app.can_raise());
        assert!(app.raise().is_err());
        app.request_shutdown().unwrap();
        runtime.join().unwrap();
    }
}
