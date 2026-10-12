//! Desktop host ownership: windows, tray integration and application lifetime.

#[cfg(feature = "embed-nerd-font")]
use super::fonts;
use super::{GuiApp, input::Input};
use crate::{artwork::ArtworkManager, core::AppHandle, model::Command, tray::TrayController};
use anyhow::Result;
use futures::{StreamExt, channel::mpsc};
use gpui::*;
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::{Rc, Weak},
    sync::Arc,
};

pub(super) struct GuiHost {
    handle: AppHandle,
    layout_path: PathBuf,
    artwork_manager: Arc<ArtworkManager>,
    self_weak: Weak<RefCell<GuiHost>>,
    window: Option<WindowHandle<GuiApp>>,
    tray: Option<TrayController>,
    error: Option<anyhow::Error>,
    quitting: bool,
}

impl GuiHost {
    fn show_window(&mut self, cx: &mut App) {
        if self.quitting {
            return;
        }
        cx.activate(true);
        if self.handle.is_shutting_down() {
            self.quit(cx);
            return;
        }
        if let Some(window) = self.window {
            if window
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                cx.activate(true);
                return;
            }
            // A close may have been processed before its observer ran.
            self.window_closed(window.window_id());
        }
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let handle = self.handle.clone();
        let layout_path = self.layout_path.clone();
        let artwork_manager = Arc::clone(&self.artwork_manager);
        let host = self.self_weak.clone();
        match cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Rivu".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(760.), px(480.))),
                app_id: Some("rivu".into()),
                ..Default::default()
            },
            move |window, cx| {
                cx.new(|cx| GuiApp::new(handle, layout_path, artwork_manager, host, window, cx))
            },
        ) {
            Ok(window) => {
                self.window = Some(window);
                self.error = None;
                tracing::info!("gui_window_created");
                cx.activate(true);
            }
            Err(error) => {
                tracing::error!(%error, "Could not open Rivu window; use Show window to retry");
                self.error = Some(error.context("Could not open Rivu window"));
            }
        }
    }

    pub(super) fn reload_window(&mut self, cx: &mut App) {
        if self.quitting || self.handle.is_shutting_down() {
            return;
        }
        let Some(window) = self.window else {
            return;
        };
        let handle = self.handle.clone();
        let layout_path = self.layout_path.clone();
        let artwork_manager = Arc::clone(&self.artwork_manager);
        let host = self.self_weak.clone();
        self.clear_window_state();
        if let Err(error) = window.update(cx, |old, window, cx| {
            old.release_render_images(window, cx);
            window.replace_root(cx, |window, cx| {
                GuiApp::new(handle, layout_path, artwork_manager, host, window, cx)
            });
        }) {
            tracing::error!(%error, "Could not reload Rivu window; use Show window to retry");
            self.window_closed(window.window_id());
        }
    }

    fn handle_requests(&mut self, cx: &mut App) {
        if self.quitting {
            return;
        }
        if self.handle.is_shutting_down() {
            self.quit(cx);
        } else if self.handle.take_raise_request() {
            self.show_window(cx);
        }
    }

    fn window_closed(&mut self, id: WindowId) {
        if self.window.is_some_and(|window| window.window_id() == id) {
            self.window = None;
            self.clear_window_state();
        }
    }

    fn clear_window_state(&self) {
        self.handle.clear_wakeup();
        self.handle
            .set_visualizer_demand(crate::analysis::VisualizerDemand::default());
        let _ = self.handle.send(Command::Analysis { enabled: false });
    }

    fn shutdown(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        self.handle.clear_gui_opener();
        if self.window.take().is_some() {
            self.clear_window_state();
        }
        self.tray.take();
    }

    fn quit(&mut self, cx: &mut App) {
        self.shutdown();
        cx.quit();
    }
}

impl Drop for GuiHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn run(
    handle: AppHandle,
    layout_path: PathBuf,
    artwork_manager: Arc<ArtworkManager>,
) -> Result<()> {
    let tray = if handle.config_snapshot().tray_enabled {
        TrayController::start(handle.clone())?
    } else {
        None
    };
    let host = Rc::new_cyclic(|self_weak| {
        RefCell::new(GuiHost {
            handle: handle.clone(),
            layout_path,
            artwork_manager,
            self_weak: self_weak.clone(),
            window: None,
            tray,
            error: None,
            quitting: false,
        })
    });
    let app_host = host.clone();
    gpui_platform::application()
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx: &mut App| {
            #[cfg(feature = "embed-nerd-font")]
            if let Err(error) = fonts::register_symbols(cx) {
                app_host.borrow_mut().error =
                    Some(error.context("Could not load embedded icon font"));
                cx.quit();
                return;
            }
            Input::init(cx);
            let closed_host = app_host.clone();
            cx.on_window_closed(move |_, id| closed_host.borrow_mut().window_closed(id))
                .detach();
            let quit_host = app_host.clone();
            cx.on_app_quit(move |_| {
                quit_host.borrow_mut().shutdown();
                futures::future::ready(())
            })
            .detach();

            // Only Raise and core shutdown wake the host. Playback updates belong
            // to the window's separate receiver, which is removed on close.
            let (wake_sender, mut wake_receiver) = mpsc::channel::<()>(1);
            let wake_sender = parking_lot::Mutex::new(wake_sender);
            app_host.borrow().handle.set_gui_opener(move || {
                let _ = wake_sender.lock().try_send(());
            });
            let request_host = app_host.clone();
            cx.spawn(async move |cx| {
                while wake_receiver.next().await.is_some() {
                    cx.update(|cx| request_host.borrow_mut().handle_requests(cx));
                }
            })
            .detach();
            app_host.borrow_mut().show_window(cx);
        });
    let error = {
        let mut host = host.borrow_mut();
        host.shutdown();
        host.error.take()
    };
    if let Some(error) = error {
        return Err(error);
    }
    // Explicit platform quit also terminates the core. Do this after the GUI
    // loop, outside GPUI's short quit-observer deadline and without blocking UI.
    if !handle.is_shutting_down() {
        let response = handle.request_ack(Command::Shutdown);
        if !response.ok {
            anyhow::bail!(
                "{}",
                response
                    .error
                    .as_deref()
                    .unwrap_or("Could not shut down Rivu")
            );
        }
    }
    Ok(())
}
