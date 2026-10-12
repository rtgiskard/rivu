//! Window ownership and release of visual subscriptions and analysis demand.

use super::GuiApp;
use crate::{artwork::ArtworkManager, core::CoreHandle, model::Command};
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
    layout_path: PathBuf,
    artwork_manager: Arc<ArtworkManager>,
    self_weak: Weak<RefCell<UiSession>>,
    window: Option<WindowHandle<GuiApp>>,
}

impl UiSession {
    pub(crate) fn new(
        handle: CoreHandle,
        layout_path: PathBuf,
        artwork_manager: Arc<ArtworkManager>,
    ) -> Rc<RefCell<Self>> {
        Rc::new_cyclic(|self_weak| {
            RefCell::new(Self {
                handle,
                layout_path,
                artwork_manager,
                self_weak: self_weak.clone(),
                window: None,
            })
        })
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
                cx.new(|cx| GuiApp::new(handle, layout_path, artwork_manager, session, window, cx))
            },
        )?;
        self.window = Some(window);
        tracing::info!("gui_window_created");
        cx.activate(true);
        Ok(())
    }

    pub(crate) fn reload_window(&mut self, cx: &mut App) {
        if self.handle.is_shutting_down() {
            return;
        }
        let Some(window) = self.window else {
            return;
        };
        let handle = self.handle.clone();
        let layout_path = self.layout_path.clone();
        let artwork_manager = Arc::clone(&self.artwork_manager);
        let session = self.self_weak.clone();
        self.clear_window_state();
        if let Err(error) = window.update(cx, |old, window, cx| {
            old.release_render_images(window, cx);
            window.replace_root(cx, |window, cx| {
                GuiApp::new(handle, layout_path, artwork_manager, session, window, cx)
            });
        }) {
            tracing::error!(%error, "Could not reload Rivu window; use Show window to retry");
            self.window_closed(window.window_id());
        }
    }

    pub(crate) fn window_closed(&mut self, id: WindowId) {
        if self.window.is_some_and(|window| window.window_id() == id) {
            self.release_window();
        }
    }

    fn clear_window_state(&self) {
        self.handle.clear_wakeup();
        self.handle
            .set_visualizer_demand(crate::analysis::VisualizerDemand::default());
        let _ = self.handle.send(Command::Analysis { enabled: false });
    }

    pub(crate) fn release_window(&mut self) {
        if self.window.take().is_some() {
            self.clear_window_state();
        }
    }
}

impl Drop for UiSession {
    fn drop(&mut self) {
        self.release_window();
    }
}
