//! Standalone graphical client and its process-local shutdown endpoint.
use crate::{
    frontend::FrontendRuntime,
    gui::{self, session::UiSession},
};
use anyhow::{Context, Result, bail, ensure};
use futures::{StreamExt, channel::mpsc};
use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
use gpui::{App as GpuiApp, QuitMode, Subscription, Task};
use rivu_metadata::artwork::ArtworkManager;
use std::{
    cell::RefCell,
    os::{
        linux::net::SocketAddrExt,
        unix::ffi::OsStrExt,
        unix::net::{SocketAddr, UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

/// Drives desktop-local asynchronous work without Core's blocking-work leases.
struct TaskRuntime {
    handle: Arc<async_executor::Executor<'static>>,
    stop: Option<futures::channel::oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl TaskRuntime {
    fn start() -> Result<Self> {
        let handle = Arc::new(async_executor::Executor::new());
        let executor = Arc::clone(&handle);
        let (stop, stopped) = futures::channel::oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("rivu-desktop-async".into())
            .spawn(move || {
                async_io::block_on(executor.run(async {
                    let _ = stopped.await;
                }));
            })?;
        Ok(Self {
            handle,
            stop: Some(stop),
            worker: Some(worker),
        })
    }
}
impl Drop for TaskRuntime {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn is_no_instance(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            )
        })
    })
}

#[derive(Default)]
struct ControlState {
    raise_pending: bool,
    shutting_down: bool,
    wakeup: Option<Arc<dyn Fn() + Send + Sync>>,
}
/// Local-only endpoint: no method can shut down the service process.
#[derive(Clone, Default)]
pub(crate) struct DesktopControl {
    state: Arc<parking_lot::Mutex<ControlState>>,
}
impl DesktopControl {
    pub(crate) fn raise(&self) -> Result<()> {
        let mut state = self.state.lock();
        if state.shutting_down {
            bail!("Desktop is shutting down");
        }
        let wakeup = (!state.raise_pending)
            .then(|| state.wakeup.clone())
            .flatten();
        state.raise_pending = true;
        drop(state);
        if let Some(wakeup) = wakeup {
            wakeup();
        }
        Ok(())
    }
    /// Window close, signals and Core disconnection stop only this process.
    pub(crate) fn request_shutdown(&self) -> Result<()> {
        let mut state = self.state.lock();
        state.shutting_down = true;
        state.raise_pending = false;
        let wakeup = state.wakeup.clone();
        drop(state);
        if let Some(wakeup) = wakeup {
            wakeup();
        }
        Ok(())
    }
    pub(crate) fn is_shutting_down(&self) -> bool {
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
        self.state.lock().wakeup = None;
    }
    fn take_raise_request(&self) -> bool {
        let mut state = self.state.lock();
        std::mem::take(&mut state.raise_pending) && !state.shutting_down
    }
}

fn address(data_dir: &Path) -> Result<SocketAddr> {
    let core = rivu_protocol::wire::address(data_dir)?;
    let mut name = core
        .as_abstract_name()
        .context("Core IPC address must be abstract")?
        .to_vec();
    name.extend_from_slice(b".desktop");
    Ok(SocketAddr::from_abstract_name(name)?)
}
/// Acknowledged activation. A retiring desktop holds the connection until its
/// singleton and layout writer have been released, so callers can then reopen.
pub async fn raise_async(
    data_dir: &Path,
    expected_instance: u32,
    deadline: Option<Instant>,
) -> Result<bool> {
    ensure!(
        expected_instance != 0,
        "Desktop activation requires a Core instance"
    );
    let address = address(data_dir)?;
    let name = address
        .as_abstract_name()
        .context("Desktop address is not abstract")?;
    let mut path = [0; 108];
    ensure!(name.len() < path.len(), "Desktop address is too long");
    path[1..1 + name.len()].copy_from_slice(name);
    let path = Path::new(std::ffi::OsStr::from_bytes(&path[..1 + name.len()]));
    let exchange = async {
        let mut stream = async_io::Async::<UnixStream>::connect(path).await?;
        rivu_protocol::wire::verify_peer(&stream)?;
        let mut request = [0; 5];
        request[0] = b'R';
        request[1..].copy_from_slice(&expected_instance.to_le_bytes());
        let mut reply = [0];
        let exchanged = async {
            stream.write_all(&request).await?;
            stream.read_exact(&mut reply).await
        }
        .await;
        if let Err(error) = exchanged {
            return if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
            ) {
                // Retiring listeners also close requests still in their backlog.
                Ok(false)
            } else {
                Err(error.into())
            };
        }
        match reply[0] {
            b'R' => Ok(true),
            b'Q' => {
                ensure!(
                    stream.read(&mut reply).await? == 0,
                    "Invalid desktop retirement reply"
                );
                Ok(false)
            }
            b'S' => bail!("Desktop belongs to a different Core instance"),
            _ => bail!("Invalid desktop activation reply"),
        }
    };
    let timeout = async {
        async_io::Timer::at(deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(30)))
            .await;
        Err::<_, anyhow::Error>(std::io::Error::from(std::io::ErrorKind::TimedOut).into())
    };
    futures_lite::future::race(exchange, timeout).await
}

/// Kernel-owned singleton. The listener is released before retirement waiters.
struct Activation {
    stop: Option<futures::channel::oneshot::Sender<()>>,
    worker: Option<async_executor::Task<()>>,
}
impl Activation {
    fn bind(data_dir: &Path) -> Result<UnixListener> {
        Ok(UnixListener::bind_addr(&address(data_dir)?)?)
    }
    fn start(
        socket: UnixListener,
        control: DesktopControl,
        instance: u32,
        executor: &Arc<async_executor::Executor<'static>>,
    ) -> Result<Self> {
        use futures::FutureExt;
        let incoming = async_io::Async::new(socket)?;
        let (stop, stopped) = futures::channel::oneshot::channel();
        let worker = executor.spawn(async move {
            let mut stopped = stopped.fuse();
            loop {
                let (mut stream, _) = {
                    let accepted = incoming.accept().fuse();
                    futures::pin_mut!(accepted);
                    futures::select_biased! {
                        _ = stopped => break,
                        result = accepted => match result {
                            Ok(stream) => stream,
                            Err(error) => {
                                tracing::warn!(%error, "desktop_activation_failed");
                                let _ = control.request_shutdown();
                                break;
                            }
                        },
                    }
                };
                if rivu_protocol::wire::verify_peer(&stream).is_err() {
                    continue;
                }
                let mut request = [0; 5];
                let valid = {
                    let read = stream.read_exact(&mut request).fuse();
                    let timeout =
                        futures::FutureExt::fuse(async_io::Timer::after(Duration::from_secs(1)));
                    futures::pin_mut!(read, timeout);
                    futures::select_biased! {
                        _ = stopped => break,
                        result = read => result.is_ok(),
                        _ = timeout => false,
                    }
                };
                if !valid || request[0] != b'R' {
                    continue;
                }
                if u32::from_le_bytes(request[1..].try_into().unwrap()) != instance {
                    let _ = stream.write_all(b"S").await;
                    continue;
                }
                if control.raise().is_ok() {
                    let _ = stream.write_all(b"R").await;
                } else {
                    let _ = stream.write_all(b"Q").await;
                    let _ = stopped.await;
                    // No replacement may acquire the singleton before the old
                    // writer is flushed and the listening socket is closed.
                    drop(incoming);
                    drop(stream);
                    return;
                }
            }
        });
        Ok(Self {
            stop: Some(stop),
            worker: Some(worker),
        })
    }
}
impl Drop for Activation {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            async_io::block_on(worker);
        }
    }
}

pub(crate) struct Desktop {
    handle: DesktopControl,
    _runtime: FrontendRuntime,
    gui: Option<GuiHost>,
    activation: Option<Activation>,
    registration: Option<rivu_client::ClientSession>,
    signals: Option<async_executor::Task<()>>,
    _tasks: TaskRuntime,
}

/// GPUI integration state, not a second application or services owner.
struct GuiHost {
    session: Rc<RefCell<UiSession>>,
    window_closed: Option<Subscription>,
    app_quit: Option<Subscription>,
    wake_task: Option<Task<()>>,
    error: Option<anyhow::Error>,
    build_info: crate::BuildInfo,
    quitting: bool,
}

impl Desktop {
    pub(crate) fn open(
        data_dir: PathBuf,
        expected: u32,
        build_info: crate::BuildInfo,
    ) -> Result<()> {
        let socket = loop {
            match Activation::bind(&data_dir) {
                Ok(socket) => break socket,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::AddrInUse) =>
                {
                    match async_io::block_on(raise_async(&data_dir, expected, None)) {
                        Ok(true) => return Ok(()),
                        Ok(false) => {}
                        Err(error) if is_no_instance(&error) => {}
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error).context("Acquiring desktop singleton"),
            }
        };
        Self::start(&data_dir, socket, expected, build_info)?.run_gui()
    }
    fn start(
        data_dir: &Path,
        socket: UnixListener,
        expected: u32,
        build_info: crate::BuildInfo,
    ) -> Result<Self> {
        let tasks = TaskRuntime::start()?;
        let runtime = FrontendRuntime::connect(data_dir, expected)?;
        let config = runtime.handle.config_snapshot();
        let handle = DesktopControl::default();
        let disconnected = handle.clone();
        runtime.handle.set_disconnect_callback(move || {
            let _ = disconnected.request_shutdown();
        });
        let layouts = gui::session::LoadedLayouts::load(data_dir.join("layout.json"));
        let artwork = Arc::new(
            ArtworkManager::new(data_dir)
                .with_default_image(rivu_presentation::DEFAULT_IMAGE)
                .with_max_image_bytes(config.artwork_file_limit_mib as usize * 1024 * 1024),
        );
        let i18n = Arc::new(rivu_presentation::i18n::Localizer::new(&config.ui_language));
        let activation = Activation::start(socket, handle.clone(), expected, &tasks.handle)?;
        let mut registration = rivu_client::client_session(data_dir);
        registration.bind_instance(runtime.handle.instance_id())?;
        let ack = registration.register_desktop()?;
        ensure!(
            ack.is_success(),
            "{}",
            ack.error
                .as_deref()
                .unwrap_or("Desktop registration failed")
        );
        let mut signals =
            async_signal::Signals::new([async_signal::Signal::Int, async_signal::Signal::Term])?;
        let signalled = handle.clone();
        let signals = tasks.handle.spawn(async move {
            if signals.next().await.is_some() {
                let _ = signalled.request_shutdown();
            }
        });
        let gui = GuiHost {
            session: UiSession::new(runtime.handle.clone(), layouts, artwork, i18n),
            window_closed: None,
            app_quit: None,
            wake_task: None,
            error: None,
            build_info,
            quitting: false,
        };
        Ok(Self {
            handle,
            _runtime: runtime,
            gui: Some(gui),
            activation: Some(activation),
            registration: Some(registration),
            signals: Some(signals),
            _tasks: tasks,
        })
    }
    fn show_window(&mut self, cx: &mut GpuiApp) {
        let gui = self.gui.as_mut().expect("graphical application");
        let result = gui.session.borrow_mut().show_window(cx);
        match result {
            Ok(()) => gui.error = None,
            Err(error) => {
                tracing::error!(%error, "Could not open Rivu window");
                gui.error = Some(error.context("Could not open Rivu window"));
                self.quit_gui(cx);
            }
        }
    }

    fn handle_requests(owner: &Rc<RefCell<Self>>, cx: &mut GpuiApp) {
        let mut app = owner.borrow_mut();
        if app.gui.as_ref().is_none_or(|gui| gui.quitting) {
            return;
        }
        if app.handle.is_shutting_down() {
            app.quit_gui(cx);
            return;
        }
        if app.handle.take_raise_request() {
            app.show_window(cx);
        }
    }

    fn shutdown_gui(&mut self) {
        let Some(gui) = self.gui.as_mut() else {
            return;
        };
        gui.quitting = true;
        let _ = self.handle.request_shutdown();
        // Stop local producers before releasing the UI receiver.
        self.handle.withdraw_gui();
        gui.wake_task.take();
        gui.window_closed.take();
        gui.app_quit.take();
        gui.session.borrow_mut().release_window();
        // Worker joins and final layout persistence stay outside GPUI's
        // deadline-limited quit observer.
    }

    fn quit_gui(&mut self, cx: &mut GpuiApp) {
        self.shutdown_gui();
        cx.quit();
    }

    fn observe_window_close(owner: &Rc<RefCell<Self>>, cx: &mut GpuiApp) -> Subscription {
        let owner = Rc::downgrade(owner);
        cx.on_window_closed(move |cx, id| {
            if let Some(owner) = owner.upgrade() {
                let mut app = owner.borrow_mut();
                let gui = app.gui.as_mut().expect("graphical application");
                if !gui.session.borrow_mut().window_closed(id) {
                    return;
                }
                // Reject activation as soon as the final close commits,
                // but quit only after the removed Window's stack unwinds.
                gui.quitting = true;
                let _ = app.handle.request_shutdown();
                drop(app);
                let owner = Rc::downgrade(&owner);
                cx.defer(move |cx| {
                    if let Some(owner) = owner.upgrade() {
                        owner.borrow_mut().quit_gui(cx);
                    }
                });
            }
        })
    }

    fn run_gui(self) -> Result<()> {
        let app = Rc::new(RefCell::new(self));
        let gpui_owner = Rc::clone(&app);
        gpui_platform::application()
            .with_quit_mode(QuitMode::Explicit)
            .run(move |cx: &mut GpuiApp| {
                cx.set_global(
                    gpui_owner
                        .borrow()
                        .gui
                        .as_ref()
                        .expect("graphical application")
                        .build_info,
                );
                if let Err(error) = gui::init(cx) {
                    let mut app = gpui_owner.borrow_mut();
                    app.gui.as_mut().expect("graphical application").error = Some(error);
                    app.quit_gui(cx);
                    return;
                }

                // Weak captures cannot retain the application after its event
                // loop returns.
                let window_closed = Self::observe_window_close(&gpui_owner, cx);
                let quit_owner = Rc::downgrade(&gpui_owner);
                let app_quit = cx.on_app_quit(move |_| {
                    if let Some(app) = quit_owner.upgrade() {
                        app.borrow_mut().shutdown_gui();
                    }
                    futures::future::ready(())
                });

                // Playback wakes the view separately; this receiver handles
                // activation and forced shutdown for the graphical process.
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
        let layout_save = app
            .gui
            .as_ref()
            .expect("graphical application")
            .session
            .borrow()
            .flush_layouts();
        if let Err(error) = &layout_save {
            tracing::error!(%error, "Could not save final desktop layout");
        }
        app.activation.take();
        app.registration.take();
        if let Some(signals) = app.signals.take() {
            async_io::block_on(signals.cancel());
        }
        let connection = app._runtime.shutdown();
        let error = app
            .gui
            .as_mut()
            .expect("graphical application")
            .error
            .take();
        error.map_or(connection.and(layout_save), Err)
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        self.shutdown_gui();
        drop(self.activation.take());
        drop(self.registration.take());
        if let Some(signals) = self.signals.take() {
            async_io::block_on(signals.cancel());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;
    use rivu::test_support::ipc::socket_test_directory;

    #[test]
    fn activation_coalesces_and_shutdown_retires_desktop() {
        let control = DesktopControl::default();
        for _ in 0..1000 {
            control.raise().unwrap();
        }
        let (wake, wakes) = std::sync::mpsc::sync_channel(1);
        control.set_wakeup(move || {
            let _ = wake.try_send(());
        });
        assert_eq!(wakes.try_recv(), Ok(()));
        assert!(control.take_raise_request());
        assert!(!control.take_raise_request());
        assert!(!control.is_shutting_down());
        control.raise().unwrap();
        assert_eq!(wakes.try_recv(), Ok(()));
        assert!(control.take_raise_request());
        control.raise().unwrap();
        control.request_shutdown().unwrap();
        assert!(control.is_shutting_down());
        assert!(!control.take_raise_request());
        assert!(control.raise().is_err());
    }

    #[test]
    fn retiring_desktop_releases_singleton_before_activation_waiters() {
        if rivu::test_support::run_in_subprocess(
            "desktop::tests::retiring_desktop_releases_singleton_before_activation_waiters",
            Duration::from_secs(15),
        ) {
            return;
        }
        use std::io::{Read, Write};
        let directory = socket_test_directory();
        let tasks = TaskRuntime::start().unwrap();
        let control = DesktopControl::default();
        let activation = Activation::start(
            Activation::bind(directory.path()).unwrap(),
            control.clone(),
            7,
            &tasks.handle,
        )
        .unwrap();
        assert!(async_io::block_on(raise_async(directory.path(), 7, None)).unwrap());
        assert!(control.take_raise_request());
        assert!(async_io::block_on(raise_async(directory.path(), 8, None)).is_err());
        assert!(!control.take_raise_request());
        assert!(!control.is_shutting_down());
        control.request_shutdown().unwrap();
        let mut waiter = UnixStream::connect_addr(&address(directory.path()).unwrap()).unwrap();
        waiter
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        waiter.write_all(&[b'R', 7, 0, 0, 0]).unwrap();
        let mut reply = [0];
        waiter.read_exact(&mut reply).unwrap();
        assert_eq!(reply, *b"Q");
        waiter.set_nonblocking(true).unwrap();
        assert_eq!(
            waiter.read(&mut reply).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(activation);
        assert_eq!(waiter.read(&mut reply).unwrap(), 0);
        assert!(Activation::bind(directory.path()).is_ok());
    }

    #[test]
    fn bound_but_unready_desktop_does_not_disable_startup_deadline() {
        let directory = socket_test_directory();
        let _listener = Activation::bind(directory.path()).unwrap();
        let error =
            async_io::block_on(raise_async(directory.path(), 7, Some(Instant::now()))).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn independent_gui_activation_uses_existing_service_and_kernel_singleton() {
        const CHILD_DIRECTORY: &str = "RIVU_TEST_ACTIVATE_DIRECTORY";
        if let Some(directory) = std::env::var_os(CHILD_DIRECTORY) {
            // The library must activate the existing singleton without creating
            // another GPUI application or owning service bootstrap.
            let directory = PathBuf::from(directory);
            let mut session = rivu_client::client_session(&directory);
            session
                .get_state(rivu_protocol::response::StateSections::default())
                .unwrap();
            Desktop::open(
                directory,
                session.instance_id().unwrap(),
                crate::test_support::build_info(),
            )
            .unwrap();
            return;
        }
        if rivu::test_support::run_in_subprocess(
            "desktop::tests::independent_gui_activation_uses_existing_service_and_kernel_singleton",
            Duration::from_secs(15),
        ) {
            return;
        }
        let directory = socket_test_directory();
        let runtime = crate::test_support::CoreRuntime::start(
            directory.path(),
            &directory.path().join("config.toml"),
        )
        .unwrap();
        let tasks = TaskRuntime::start().unwrap();
        let control = DesktopControl::default();
        let (wake, wakes) = std::sync::mpsc::sync_channel(1);
        control.set_wakeup(move || {
            let _ = wake.try_send(());
        });
        let activation = Activation::start(
            Activation::bind(directory.path()).unwrap(),
            control.clone(),
            runtime.handle.instance_id(),
            &tasks.handle,
        )
        .unwrap();
        assert!(Activation::bind(directory.path()).is_err());
        let mut registration = rivu_client::client_session(directory.path());
        assert!(registration.register_desktop().unwrap().is_success());
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact").arg("desktop::tests::independent_gui_activation_uses_existing_service_and_kernel_singleton")
            .env(CHILD_DIRECTORY, directory.path()).status().unwrap();
        assert!(status.success());
        wakes.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(control.take_raise_request());
        drop(activation);
        drop(registration);
        assert!(Activation::bind(directory.path()).is_ok());
        assert!(
            rivu_client::get_state(
                directory.path(),
                rivu_protocol::response::StateSections::ALL
            )
            .is_ok()
        );
        assert!(
            async_io::block_on(raise_async(
                directory.path(),
                runtime.handle.instance_id(),
                None
            ))
            .is_err()
        );
    }

    #[test]
    fn dropping_desktop_never_shuts_down_the_service() {
        if rivu::test_support::run_in_subprocess(
            "desktop::tests::dropping_desktop_never_shuts_down_the_service",
            Duration::from_secs(15),
        ) {
            return;
        }
        let directory = socket_test_directory();
        let runtime = crate::test_support::CoreRuntime::start(
            directory.path(),
            &directory.path().join("config.toml"),
        )
        .unwrap();
        let desktop = Desktop::start(
            directory.path(),
            Activation::bind(directory.path()).unwrap(),
            runtime.handle.instance_id(),
            crate::test_support::build_info(),
        )
        .unwrap();
        desktop.handle.request_shutdown().unwrap();
        drop(desktop);
        assert!(
            rivu_client::get_state(
                directory.path(),
                rivu_protocol::response::StateSections::ALL
            )
            .is_ok()
        );
        assert!(Activation::bind(directory.path()).is_ok());
    }

    #[gpui::test]
    fn final_window_close_defers_gui_quit_and_preserves_service_and_layout(
        cx: &mut gpui::TestAppContext,
    ) {
        if rivu::test_support::run_in_subprocess(
            "desktop::tests::final_window_close_defers_gui_quit_and_preserves_service_and_layout",
            Duration::from_secs(15),
        ) {
            return;
        }
        let directory = socket_test_directory();
        let runtime = crate::test_support::CoreRuntime::start(
            directory.path(),
            &directory.path().join("config.toml"),
        )
        .unwrap();
        let owner = Rc::new(RefCell::new(
            Desktop::start(
                directory.path(),
                Activation::bind(directory.path()).unwrap(),
                runtime.handle.instance_id(),
                crate::test_support::build_info(),
            )
            .unwrap(),
        ));
        let control = owner.borrow().handle.clone();
        let window = cx.update(|cx| {
            rivu_ui::input::Input::init(cx);
            let closed = Desktop::observe_window_close(&owner, cx);
            let mut app = owner.borrow_mut();
            app.gui.as_mut().unwrap().window_closed = Some(closed);
            app.show_window(cx);
            // The real Core must not drive the deterministic GPUI scheduler.
            app._runtime.handle.set_wakeup(|| {});
            cx.windows().into_iter().next().unwrap()
        });
        cx.update_window(window, |_, window, _| {
            window.remove_window();
            assert!(!control.is_shutting_down());
        })
        .unwrap();
        cx.run_until_parked();
        assert!(control.is_shutting_down());
        assert!(owner.borrow().gui.as_ref().unwrap().quitting);
        assert!(!directory.path().join("layout.json").exists());
        owner
            .borrow()
            .gui
            .as_ref()
            .unwrap()
            .session
            .borrow()
            .flush_layouts()
            .unwrap();
        assert!(directory.path().join("layout.json").is_file());
        drop(owner);
        assert!(
            rivu_client::get_state(
                directory.path(),
                rivu_protocol::response::StateSections::ALL
            )
            .is_ok()
        );
        assert!(Activation::bind(directory.path()).is_ok());
    }

    #[test]
    fn core_quit_forces_desktop_exit_even_with_pending_activation() {
        let directory = socket_test_directory();
        let runtime = crate::test_support::CoreRuntime::start(
            directory.path(),
            &directory.path().join("config.toml"),
        )
        .unwrap();
        let desktop = Desktop::start(
            directory.path(),
            Activation::bind(directory.path()).unwrap(),
            runtime.handle.instance_id(),
            crate::test_support::build_info(),
        )
        .unwrap();
        desktop.handle.raise().unwrap();
        assert!(!desktop.handle.is_shutting_down());
        let (wake, wakes) = std::sync::mpsc::sync_channel(1);
        desktop.handle.set_wakeup(move || {
            let _ = wake.try_send(());
        });
        let _ = wakes.try_recv();
        assert!(
            rivu_client::request_ack(directory.path(), rivu_protocol::AppCommand::Shutdown)
                .unwrap()
                .is_success()
        );
        wakes.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(desktop.handle.is_shutting_down());
        assert!(!desktop.handle.take_raise_request());
        drop(desktop);
    }
}
