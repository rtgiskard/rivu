//! Window ownership and release of visual subscriptions and analysis demand.
use rivu_metadata::artwork::ArtworkManager;

pub(crate) use super::layout::persistence::LoadedLayouts;
use super::{WindowView, layout::persistence::LayoutPersistence};
use crate::frontend::FrontendHandle;
use anyhow::Result;
use gpui::*;
use rivu_presentation::i18n::Localizer;
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::Arc,
};

pub(crate) struct UiSession {
    handle: FrontendHandle,
    layout_save: LayoutPersistence,
    artwork_manager: Arc<ArtworkManager>,
    i18n: Arc<Localizer>,
    self_weak: Weak<RefCell<UiSession>>,
    window: Option<WindowHandle<WindowView>>,
}

impl UiSession {
    pub(crate) fn new(
        handle: FrontendHandle,
        layouts: LoadedLayouts,
        artwork_manager: Arc<ArtworkManager>,
        i18n: Arc<Localizer>,
    ) -> Rc<RefCell<Self>> {
        Rc::new_cyclic(|self_weak| {
            RefCell::new(Self {
                handle,
                layout_save: LayoutPersistence::new(layouts),
                artwork_manager,
                i18n,
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
        let layout_save = self.layout_save.handle();
        let artwork_manager = Arc::clone(&self.artwork_manager);
        let i18n = Arc::clone(&self.i18n);
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
                    let view = WindowView::new(
                        handle,
                        layout_save,
                        artwork_manager,
                        session,
                        i18n,
                        window,
                        cx,
                    );
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

    pub(crate) fn window_closed(&mut self, id: WindowId) -> bool {
        if !self.window.is_some_and(|window| window.window_id() == id) {
            return false;
        }
        self.release_window();
        true
    }

    /// Called once after the GPUI event loop returns, not by the close observer
    /// or the deadline-limited quit observer. The final latest save must finish
    /// before normal process teardown, and its error must reach the caller.
    pub(crate) fn flush_layouts(&self) -> Result<()> {
        self.layout_save.flush().map_err(anyhow::Error::msg)
    }

    fn clear_window_state(&self) {
        self.layout_save.handle().clear_wakeup();
        self.handle.clear_wakeup();
        self.handle
            .set_display_demand(rivu_analysis::DisplayDemand::default());
        self.handle.set_waveform_demand(false);
    }

    pub(crate) fn release_window(&mut self) {
        if self.window.take().is_some() {
            self.clear_window_state();
        }
    }
}

impl WindowView {
    fn install_close_guard(&self, window: &Window, cx: &Context<Self>) {
        let view = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            // Native and menu closes share the manual-action path and its
            // deferred removal, rather than letting the platform remove directly.
            let _ = view.update(cx, |view, cx| view.request_close_window(window, cx));
            false
        });
    }

    pub(super) fn request_close_window(&mut self, window: &Window, cx: &mut Context<Self>) {
        let id = self.begin_manual_intent(cx);
        self.ready_manual_action(
            id,
            super::coordination::manual_actions::ManualAction::Close(window.window_handle()),
            cx,
        );
    }

    pub(super) fn request_quit(&mut self, cx: &mut Context<Self>) {
        let id = self.begin_manual_intent(cx);
        self.ready_manual_action(
            id,
            super::coordination::manual_actions::ManualAction::Quit,
            cx,
        );
    }

    pub(super) fn finish_quit(&self) {
        let Some(session) = self.session.upgrade() else {
            return;
        };
        let app = session.borrow().handle.clone();
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
#[cfg(test)]
mod tests {
    use super::{LoadedLayouts, UiSession};
    use gpui::TestAppContext;
    use rivu_metadata::artwork::ArtworkManager;
    use rivu_presentation::i18n::Localizer;
    use std::{sync::Arc, time::Duration};

    #[gpui::test]
    fn application_quit_shuts_down_core(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let runtime = crate::test_support::CoreRuntime::start(
            directory.path(),
            &directory.path().join("config.toml"),
        )
        .unwrap();
        let (stopped, shutdown) = std::sync::mpsc::sync_channel(1);
        runtime.handle.set_disconnect_callback(move || {
            let _ = stopped.try_send(());
        });
        let session = UiSession::new(
            runtime.handle.clone(),
            LoadedLayouts::load(directory.path().join("layout.json")),
            Arc::new(ArtworkManager::new(directory.path())),
            Arc::new(Localizer::new("en-US")),
        );
        cx.update(|cx| {
            rivu_ui::input::Input::init(cx);
            session.borrow_mut().show_window(cx).unwrap();
        });
        let window = session.borrow().window.unwrap();
        window
            .update(cx, |view, _, cx| view.request_quit(cx))
            .unwrap();
        shutdown.recv_timeout(Duration::from_secs(5)).unwrap();
        let state = rivu_client::get_state(
            directory.path(),
            rivu_protocol::response::StateSections::ALL,
        )
        .unwrap();
        assert!(state.system.unwrap().shutting_down);
    }
}
