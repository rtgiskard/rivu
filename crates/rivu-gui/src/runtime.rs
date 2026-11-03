//! GPUI host lifecycle. Core and service retirement belongs to the application.
use crate::{
    gui::{self, session::UiSession},
    host::HostHandle,
};
use anyhow::Result;
use futures::{StreamExt, channel::mpsc};
use gpui::{App as GpuiApp, GraphicalEnvironment, QuitMode, Subscription, Task, WindowingRequest};
use rivu_metadata::artwork::ArtworkManager;
use rivu_presentation::i18n::Localizer;
use std::{cell::RefCell, rc::Rc, sync::Arc};

pub struct GuiHost {
    handle: HostHandle,
    ui: GuiState,
}

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

impl GuiHost {
    pub fn new(
        handle: HostHandle,
        layouts: crate::LoadedLayouts,
        artwork: Arc<ArtworkManager>,
        i18n: Arc<Localizer>,
    ) -> Self {
        Self {
            ui: GuiState {
                session: UiSession::new(handle.clone(), layouts, artwork, i18n),
                window_closed: None,
                app_quit: None,
                wake_task: None,
                windowing_task: None,
                environment: None,
                headless: false,
                error: None,
                quitting: false,
            },
            handle,
        }
    }
    fn show_window(&mut self, cx: &mut GpuiApp) {
        let gui = &mut self.ui;
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
        if app.ui.quitting {
            return;
        }
        let gui = &app.ui;
        if app.handle.is_shutting_down() {
            app.quit_gui(cx);
            return;
        }
        if app.handle.take_quit_request() {
            let session = Rc::clone(&gui.session);
            // The draft guard updates the root entity. Do not retain the App
            // owner borrow across that update or its eventual close observers.
            drop(app);
            session.borrow().request_shutdown(cx);
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
        let gui = &app.ui;
        if gui.quitting
            || gui.headless
            || gui.windowing_task.is_some()
            || gui.environment.is_none()
            || gui.session.borrow().has_window()
            || app.handle.is_shutting_down()
        {
            return;
        }
        let raising = app.handle.raise_pending();
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
                let gui = &mut app.ui;
                gui.windowing_task.take();
                match result {
                    Ok(()) => {
                        gui.headless = headless;
                        if headless && !app.handle.raise_pending() {
                            cx.release_unused_resources();
                        }
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
        owner.borrow_mut().ui.windowing_task = Some(task);
    }

    fn shutdown_gui(&mut self) {
        let gui = &mut self.ui;
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

    pub fn run(self) -> Result<()> {
        let app = Rc::new(RefCell::new(self));
        let gpui_owner = Rc::clone(&app);
        gpui_platform::application()
            .with_quit_mode(QuitMode::Explicit)
            .run(move |cx: &mut GpuiApp| {
                if let Err(error) = gui::init(cx) {
                    let mut app = gpui_owner.borrow_mut();
                    app.ui.error = Some(error);
                    app.quit_gui(cx);
                    return;
                }

                // App owns these listeners across window close; weak captures
                // cannot retain the application after the event loop returns.
                let closed_owner = Rc::downgrade(&gpui_owner);
                let window_closed = cx.on_window_closed(move |cx, id| {
                    if let Some(owner) = closed_owner.upgrade() {
                        owner.borrow().ui.session.borrow_mut().window_closed(id);
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
                gpui_owner
                    .borrow()
                    .handle
                    .set_control_wakeup(Arc::new(move || {
                        let _ = wake_sender.lock().try_send(());
                    }));
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
                let gui = &mut app.ui;
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
        let layout_save = app.ui.session.borrow().flush_layouts();
        if let Err(error) = &layout_save {
            tracing::error!(%error, "Could not save final desktop layout");
        }
        let shutdown = app.handle.request_shutdown();
        app.ui.error.take().map_or(shutdown.and(layout_save), Err)
    }
}

impl Drop for GuiHost {
    fn drop(&mut self) {
        self.shutdown_gui();
    }
}
