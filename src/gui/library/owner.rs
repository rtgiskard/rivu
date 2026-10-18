//! Window-local Library owner: inputs, publication, demand and resource lifetimes.
use super::{
    Library, LibraryBrowser, LibraryCommit, LibraryEvent, LibraryMenuRequest, LibraryNode,
    TreeBranch,
};
use crate::{
    gui::{
        components::{
            compact_icon_button, icon_button,
            search::{ListSearch, SearchEvent},
        },
        feature::FeatureContext,
        manual_actions::ManualIntentId,
    },
    model::{Command, DirectoryRow},
    projection::GuiSnapshot,
};
use gpui::{AnyElement, App, Context, SharedString, Window, prelude::*};
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Instant};

// Position ticks and unrelated feature publications do not change this surface.
fn display_changed(previous: &GuiSnapshot, next: &GuiSnapshot) -> bool {
    let previous_config = &previous.system.config;
    let next_config = &next.system.config;
    previous.library.revision != next.library.revision
        || previous.library.structure_revision != next.library.structure_revision
        || previous_config.library_roots != next_config.library_roots
        || previous_config.page_size != next_config.page_size
        || previous_config.library_columns != next_config.library_columns
        || previous_config.ui_theme != next_config.ui_theme
        || previous_config.ui_font != next_config.ui_font
        || previous_config.ui_scale != next_config.ui_scale
        || previous.current_track().map(|track| track.id)
            != next.current_track().map(|track| track.id)
        || previous.playback.status != next.playback.status
        || previous.system.scanning != next.system.scanning
        || previous.system.scan_message != next.system.scan_message
        || previous.system.scan_progress != next.system.scan_progress
        || previous.system.shutting_down != next.system.shutting_down
}

impl Library {
    pub(in crate::gui) fn new(context: FeatureContext, cx: &mut Context<Self>) -> Self {
        let state = context.snapshot();
        let i18n = Arc::clone(&context.i18n);
        let library_search = cx.new(|cx| ListSearch::new(Arc::clone(&i18n), cx));
        let subscription = cx.subscribe(&library_search, |this, _, event, cx| {
            let SearchEvent::Changed(query) = event;
            this.apply_library_search(query.clone(), cx);
        });
        let mut library = LibraryBrowser::default();
        library.refresh_tree(state.system.config.page_size as usize);
        library.sync_projection(state.query_revisions(None));
        library.sync_root_counts(
            &state.system.config.library_roots,
            state.library.structure_revision,
        );
        Self {
            context,
            state,
            i18n,
            library,
            library_search,
            requests: Default::default(),
            loading_text: Default::default(),
            visible: false,
            available: false,
            focused: false,
            selection_intent: None,
            _search_subscription: subscription,
        }
    }

    pub(in crate::gui) fn reconcile(&mut self, state: Rc<GuiSnapshot>, cx: &mut Context<Self>) {
        let display_changed = display_changed(&self.state, &state);
        if self.state.system.config.library_roots != state.system.config.library_roots {
            self.library.roots_changed();
        }
        self.library
            .refresh_tree(state.system.config.page_size as usize);
        let projection_changed = self.library.sync_projection(state.query_revisions(None));
        if projection_changed {
            self.requests.cancel_directory_demand();
        }
        if self.library.range_selection.target.is_none()
            && let Some(intent) = self.selection_intent
        {
            self.fail_selection(intent, cx);
        }
        self.library.sync_root_counts(
            &state.system.config.library_roots,
            state.library.structure_revision,
        );
        self.state = state;
        if !self.demanded() {
            if let Some(intent) = self.library.navigation_intent {
                self.fail_selection(intent, cx);
            }
            self.cancel_reads();
        }
        self.sync_loading_text(cx);
        if display_changed || projection_changed {
            cx.notify();
        }
    }

    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.reconcile(self.context.snapshot(), cx);
    }

    pub(in crate::gui) fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if !self.demanded() {
            if let Some(intent) = self.library.navigation_intent {
                self.fail_selection(intent, cx);
            }
            self.cancel_reads();
        }
        self.sync_loading_text(cx);
        cx.notify();
    }

    pub(in crate::gui) fn set_available(&mut self, available: bool) {
        self.available = available;
    }

    pub(in crate::gui) fn set_focused(&mut self, focused: bool, cx: &mut Context<Self>) {
        if self.focused != focused {
            self.focused = focused;
            cx.notify();
        }
    }

    pub(super) fn demanded(&self) -> bool {
        self.visible && !self.state.system.shutting_down
    }

    pub(in crate::gui) fn cancel_reads(&mut self) {
        if self.requests.reads_pending() || self.library.search_counts_pending() {
            self.library.cancel_pending_reads();
            self.requests.cancel_directory_demand();
        }
    }

    pub(in crate::gui) fn cancel_selection(&mut self, cx: &mut Context<Self>) {
        let pending = self.selection_intent.take().is_some();
        self.library.cancel_manual_selection();
        if pending {
            cx.notify();
        }
    }

    pub(in crate::gui) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.cancel_selection(cx);
        self.library.clear_selection();
        cx.notify();
    }

    pub(super) fn manual_intent_current(&self, intent: ManualIntentId) -> bool {
        self.selection_intent == Some(intent)
    }

    pub(super) fn ready_selection(
        &mut self,
        intent: ManualIntentId,
        commit: LibraryCommit,
        cx: &mut Context<Self>,
    ) {
        if self.manual_intent_current(intent) {
            cx.emit(LibraryEvent::Ready(intent, RefCell::new(Some(commit))));
        }
    }

    pub(super) fn fail_selection(&mut self, intent: ManualIntentId, cx: &mut Context<Self>) {
        if self.manual_intent_current(intent) {
            self.cancel_selection(cx);
            cx.emit(LibraryEvent::Failed(intent));
        }
    }

    pub(super) fn report_error(&self, error: String, cx: &mut Context<Self>) {
        cx.emit(LibraryEvent::Error(error));
    }

    pub(super) fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(error) = self.context.handle().send(command) {
            self.report_error(format!("{error:#}"), cx);
        }
        cx.notify();
    }

    pub(super) fn tr(&self, id: &str) -> SharedString {
        self.context.tr(id)
    }

    pub(in crate::gui) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.library_search.read(cx).input_focused(window, cx)
    }

    pub(in crate::gui) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.library_search
            .update(cx, |search, cx| search.focus(window, cx));
    }

    pub(in crate::gui) fn escape_search(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.library_search
            .update(cx, |search, cx| search.escape(window, cx))
    }

    pub(in crate::gui) fn render_panel(
        &mut self,
        panel_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.library_panel(panel_id, window, cx)
    }

    pub(super) fn directory_row_selected(&self, row: &DirectoryRow) -> bool {
        match row {
            DirectoryRow::Directory { path } => self.library.selected_nodes().iter().any(|node| {
                matches!(node, LibraryNode::Directory(selected) if selected.as_ref() == path.as_path())
            }),
            DirectoryRow::Track(row) => self.library.selected().contains(&row.id),
        }
    }

    pub(super) fn open_object_menu(
        &self,
        request: crate::gui::ContentMenuRequest,
        guard: crate::gui::list_interaction::ContentGuard,
        remove: Option<Command>,
        nodes: Option<Vec<LibraryNode>>,
        root: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        cx.emit(LibraryEvent::ContentMenu(RefCell::new(Some(
            LibraryMenuRequest {
                request,
                guard,
                remove,
                nodes,
                root,
            },
        ))));
    }

    pub(super) fn start_loading_text(
        &mut self,
        branch: TreeBranch,
        token: u64,
        cx: &mut Context<Self>,
    ) {
        self.loading_text.begin(branch, token, Instant::now());
        self.loading_text
            .schedule(cx, |this| &mut this.loading_text);
    }

    pub(super) fn finish_loading_text(
        &mut self,
        branch: &TreeBranch,
        token: u64,
        cx: &mut Context<Self>,
    ) {
        self.loading_text.finish(branch, token);
        self.loading_text
            .schedule(cx, |this| &mut this.loading_text);
    }

    pub(super) fn sync_loading_text(&mut self, cx: &mut Context<Self>) {
        let visible = self.demanded();
        let tree = self.library.tree();
        self.loading_text
            .retain(|branch, token| visible && tree.request_pending(branch, token));
        self.loading_text
            .schedule(cx, |this| &mut this.loading_text);
    }

    pub(super) fn track_favorite_toggle(
        &self,
        id: impl Into<gpui::ElementId>,
        group: &'static str,
        track_id: i64,
        favorite: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = crate::gui::theme::palette(cx);
        compact_icon_button(
            theme,
            icon_button(
                id,
                if favorite { "󰓎" } else { "󰓐" },
                self.tr(if favorite {
                    "gl-unfavorite"
                } else {
                    "gl-favorite"
                }),
                cx,
                move |this, _, cx| {
                    this.send(
                        Command::SetFavorite {
                            track_ids: vec![track_id],
                            favorite: !favorite,
                        },
                        cx,
                    )
                },
            ),
        )
        .when(favorite, |view| view.text_color(gpui::rgb(theme.accent)))
        .opacity(0.)
        .group_hover(group, |style| style.opacity(1.))
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
    }

    pub(super) fn choose_library_root(&mut self, cx: &mut Context<Self>) {
        let i18n = Arc::clone(&self.i18n);
        cx.spawn(async move |this, cx| {
            let result = ashpd::desktop::file_chooser::SelectedFiles::open_file()
                .title(i18n.text("shell-add-library-folder"))
                .accept_label(i18n.text("shell-add"))
                .directory(true)
                .modal(true)
                .send()
                .await
                .and_then(|request| request.response());
            let _ = this.update(cx, |this, cx| {
                match crate::gui::chooser_result(result) {
                    Ok(Some(files)) => {
                        if let Some(path) = crate::gui::chooser_path(&files)
                            && let Err(error) = this.context.handle().update_gui_config(
                                crate::core::GuiConfigChange::AddLibraryRoot(path),
                            )
                        {
                            this.report_error(format!("{error:#}"), cx);
                        }
                    }
                    Ok(None) => return,
                    Err(error) => this.report_error(
                        this.i18n.format(
                            "shell-open-library-error",
                            &[("error", error.to_string().into())],
                        ),
                        cx,
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl gpui::Render for Library {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_panel(0, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_and_transport_only_publications_do_not_change_library_display() {
        let previous = GuiSnapshot::default();
        assert!(!display_changed(&previous, &previous));
        let mut next = previous.clone();
        next.playback.position += 1.;
        next.playback.last_heard_at = Some(1);
        next.playback.volume = 0.5;
        next.queue.revision += 1;
        next.library.playlist_revision += 1;
        next.system.revision += 1;
        assert!(!display_changed(&previous, &next));
    }

    #[test]
    fn library_content_scan_and_display_configuration_changes_repaint() {
        let previous = GuiSnapshot::default();
        let mut next = previous.clone();
        next.library.revision += 1;
        assert!(display_changed(&previous, &next));
        next = previous.clone();
        next.system.scanning = !previous.system.scanning;
        assert!(display_changed(&previous, &next));
        next = previous.clone();
        Arc::make_mut(&mut next.system.config)
            .library_columns
            .artist = !previous.system.config.library_columns.artist;
        assert!(display_changed(&previous, &next));
        next = previous.clone();
        next.playback.status = crate::model::PlaybackStatus::Playing;
        assert!(display_changed(&previous, &next));
    }
}
