//! Window ownership and release of visual subscriptions and analysis demand.

use super::GuiApp;
use crate::{app::AppHandle, artwork::ArtworkManager, core::CoreHandle};
use anyhow::Result;
use gpui::*;
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::{Rc, Weak},
    sync::Arc,
};

pub(crate) struct UiSession {
    handle: CoreHandle,
    app: AppHandle,
    layout_path: PathBuf,
    artwork_manager: Arc<ArtworkManager>,
    self_weak: Weak<RefCell<UiSession>>,
    window: Option<WindowHandle<GuiApp>>,
}

impl UiSession {
    pub(crate) fn new(
        handle: CoreHandle,
        app: AppHandle,
        layout_path: PathBuf,
        artwork_manager: Arc<ArtworkManager>,
    ) -> Rc<RefCell<Self>> {
        Rc::new_cyclic(|self_weak| {
            RefCell::new(Self {
                handle,
                app,
                layout_path,
                artwork_manager,
                self_weak: self_weak.clone(),
                window: None,
            })
        })
    }

    pub(crate) fn request_shutdown(&self, cx: &mut App) {
        if let Some(window) = self.window
            && window
                .update(cx, |view, window, cx| {
                    window.activate_window();
                    view.request_quit(cx);
                })
                .is_ok()
        {
            return;
        }
        if let Err(error) = self.app.request_shutdown() {
            tracing::warn!(%error, "gui_quit_failed");
        }
    }

    pub(crate) fn has_window(&self) -> bool {
        self.window.is_some()
    }

    pub(crate) fn show_window(&mut self, cx: &mut App) -> Result<()> {
        cx.activate(true);
        if let Some(window) = self.window {
            if window
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                cx.activate(true);
                return Ok(());
            }
            // A close may have been processed before its observer ran.
            self.window_closed(window.window_id());
        }
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let handle = self.handle.clone();
        let layout_path = self.layout_path.clone();
        let artwork_manager = Arc::clone(&self.artwork_manager);
        let session = self.self_weak.clone();
        let window = cx.open_window(
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
                cx.new(|cx| {
                    let view =
                        GuiApp::new(handle, layout_path, artwork_manager, session, window, cx);
                    view.install_close_guard(window, cx);
                    view
                })
            },
        )?;
        self.window = Some(window);
        tracing::info!("gui_window_created");
        cx.activate(true);
        Ok(())
    }

    pub(crate) fn window_closed(&mut self, id: WindowId) {
        if self.window.is_some_and(|window| window.window_id() == id) {
            self.release_window();
        }
    }

    fn clear_window_state(&self) {
        self.handle.clear_wakeup();
        self.handle
            .set_display_demand(crate::analysis::DisplayDemand::default());
    }

    pub(crate) fn release_window(&mut self) {
        if self.window.take().is_some() {
            self.clear_window_state();
        }
    }
}

impl GuiApp {
    fn install_close_guard(&self, window: &Window, cx: &Context<Self>) {
        let view = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            // Always finish through the deferred close action. The platform must
            // not remove the window while its confirmation is still pending.
            let _ = view.update(cx, |view, cx| view.request_close_window(window, cx));
            false
        });
    }

    pub(super) fn request_close_window(&mut self, window: &Window, cx: &mut Context<Self>) {
        let id = self.begin_manual_intent();
        self.ready_manual_action(
            id,
            super::metadata::ManualAction::Close(window.window_handle()),
            cx,
        );
    }

    pub(super) fn request_quit(&mut self, cx: &mut Context<Self>) {
        let id = self.begin_manual_intent();
        self.ready_manual_action(id, super::metadata::ManualAction::Quit, cx);
    }

    pub(super) fn finish_quit(&self) {
        let Some(session) = self.session.upgrade() else {
            return;
        };
        let app = session.borrow().app.clone();
        if let Err(error) = app.request_shutdown() {
            tracing::warn!(%error, "gui_quit_failed");
        }
    }
}

impl Drop for UiSession {
    fn drop(&mut self) {
        self.release_window();
    }
}
