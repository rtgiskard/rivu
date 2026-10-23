//! Playlist catalog, occurrence rows, drag and management surfaces.
mod editor;
mod model;
mod queries;
mod requests;
mod selection;
pub(super) use editor::PlaylistNameAction;
use requests::ViewRequestKind;
pub(super) use selection::EntryCommit;

use super::{
    ListFocus,
    components::track::{row_text, track_list_row},
    library::LibraryDrag,
};
use crate::{gui::queue::QueueDrag, model::Command};
use gpui::{AnyElement, Context, Div, SharedString, Window, div, prelude::*, px, rgb};

use crate::gui::list_interaction::TrackSource;
use crate::gui::{
    BrowserFocus,
    components::search::{new_search, set_search_counts},
    coordination::manual_actions::ManualIntentId,
    feature::FeatureContext,
};
use crate::projection::GuiSnapshot;
use gpui::{App, Entity, EventEmitter, FocusHandle, Focusable, Subscription};
use rivu_ui::loading::LoadingText;
use rivu_ui::{
    FocusScope, IconButton, IconButtonDensity, IconButtonGroup, IconButtonStyle, LIST_ROW_REMS,
    StatusKind, VirtualList, caption, column, danger_control, hover_actions,
    input::{Input, InputEvent},
    list_panel_header, list_row, list_row_height, list_viewport, panel_header, panel_surface,
    panel_toolbar, row,
    search::{SearchEvent, SearchField},
    status_message,
};
use std::{cell::RefCell, rc::Rc, sync::Arc};

#[derive(Clone, Copy)]
pub(in crate::gui) enum PlaylistSelectionRequest {
    Entry {
        id: i64,
        index: usize,
        shift: bool,
        multi: bool,
    },
    Navigate {
        down: bool,
        shift: bool,
        multi: bool,
    },
    Edge {
        last: bool,
    },
    All,
}

pub(in crate::gui) enum PlaylistEvent {
    Selection(PlaylistSelectionRequest),
    Ready(ManualIntentId, RefCell<Option<selection::EntryCommit>>),
    Failed(ManualIntentId),
    InvalidateManualIntent,
    SearchChanged,
    Metadata(i64),
    Focus(Option<BrowserFocus>),
    FocusName,
    ClearMenus,
    CancelInteraction,
    QueueReplace(RefCell<Option<crate::gui::coordination::queue_replace::QueueReplacement>>),
    AppendPlaylist(i64),
    ResolveLibrary(
        RefCell<Option<Vec<crate::gui::library::LibraryNode>>>,
        crate::gui::DragDestination,
    ),
    Transfer(
        RefCell<Option<crate::gui::list_interaction::TrackSource>>,
        crate::gui::DragDestination,
    ),
    ContentMenu(RefCell<Option<PlaylistContentMenu>>),
    Error(String),
}

pub(in crate::gui) struct PlaylistContentMenu {
    pub(in crate::gui) request: crate::gui::ContentMenuRequest,
    pub(in crate::gui) guard: crate::gui::list_interaction::ContentGuard,
    pub(in crate::gui) remove: Option<Command>,
}

pub(in crate::gui) struct Playlists {
    context: FeatureContext,
    state: Rc<GuiSnapshot>,
    i18n: Arc<crate::i18n::Localizer>,
    browser: requests::PlaylistState,
    playlist_search: Entity<SearchField>,
    playlist_entries_search: Entity<SearchField>,
    name_input: Entity<Input>,
    playlist_editor: Option<editor::PlaylistEditor>,
    playlist_action: Option<PlaylistAction>,
    loading_text: LoadingText<ViewRequestKind>,
    intent: Option<ManualIntentId>,
    window_visible: bool,
    panel_visible: bool,
    menu_catalog_demand: bool,
    library_selection_pending: bool,
    content_resolution: bool,
    focused: bool,
    layout_focus: FocusHandle,
    menu_scope: Option<FocusScope>,
    dialog_scope: Option<FocusScope>,
    _subscriptions: Vec<Subscription>,
}
impl EventEmitter<PlaylistEvent> for Playlists {}

impl Playlists {
    pub(in crate::gui) fn new(context: FeatureContext, cx: &mut Context<Self>) -> Self {
        let i18n = context.i18n.clone();
        let state = context.snapshot();
        let playlist_search = cx.new(|cx| new_search(&i18n, cx));
        let playlist_entries_search = cx.new(|cx| new_search(&i18n, cx));
        let name_input = cx.new(|cx| {
            Input::new(
                i18n.text("shell-playlist-name").to_owned(),
                "",
                i18n.text("shell-playlist-name").to_owned(),
                cx,
            )
        });
        let subscriptions = vec![
            cx.subscribe(&playlist_search, |this, _, event, cx| {
                let SearchEvent::Changed(query) = event;
                this.apply_playlist_search(query.clone(), cx);
            }),
            cx.subscribe(&playlist_entries_search, |this, _, event, cx| {
                let SearchEvent::Changed(query) = event;
                this.apply_playlist_entries_search(query.clone(), cx);
            }),
            cx.subscribe(&name_input, |this, _, event, cx| {
                if matches!(event, InputEvent::Submit) {
                    this.submit_playlist_name(cx);
                }
                if matches!(event, InputEvent::Changed { .. }) {
                    if let Some(editor) = this.playlist_editor.as_mut() {
                        editor.error = None;
                    }
                    cx.notify();
                }
            }),
        ];
        let mut browser = requests::PlaylistState::default();
        browser.configure_page_size(state.system.config.page_size as usize);
        Self {
            context,
            state,
            i18n,
            browser,
            playlist_search,
            playlist_entries_search,
            name_input,
            playlist_editor: None,
            playlist_action: None,
            loading_text: LoadingText::new(std::time::Duration::from_millis(150)),
            intent: None,
            window_visible: true,
            panel_visible: false,
            menu_catalog_demand: false,
            library_selection_pending: false,
            content_resolution: false,
            focused: false,
            layout_focus: cx.focus_handle(),
            menu_scope: None,
            dialog_scope: None,
            _subscriptions: subscriptions,
        }
    }

    pub(in crate::gui) fn reconcile(&mut self, state: Rc<GuiSnapshot>, cx: &mut Context<Self>) {
        if Rc::ptr_eq(&self.state, &state) {
            return;
        }
        let changes = self
            .browser
            .playlists()
            .refresh_changes(&self.state.library, &state.library);
        let selected = self.browser.playlists().selected();
        let changed = changes.summaries
            || changes.entries
            || self.state.system.config.page_size != state.system.config.page_size
            || self.state.library.playlist_total != state.library.playlist_total
            || self.state.current_track().map(|track| track.id)
                != state.current_track().map(|track| track.id)
            || self.state.system.shutting_down != state.system.shutting_down;
        self.browser
            .configure_page_size(state.system.config.page_size as usize);
        self.browser.playlists.reconcile_selection();
        if self
            .browser
            .playlists()
            .selected()
            .is_some_and(|id| !state.library.playlist_entry_revisions.contains_key(&id))
        {
            self.browser.remove_playlist_selection();
        }
        self.state = state;
        let refresh_selection = self.browser.sync_entry_projection(
            self.state
                .query_revisions(self.browser.playlists().selected()),
        );
        self.browser.refresh_playlists(changes);
        if (refresh_selection || selected != self.browser.playlists().selected())
            && let Some(intent) = self.intent
        {
            self.fail_manual_intent(intent, cx);
        }
        if refresh_selection {
            self.dispatch_entry_reconcile(cx);
        }
        if changes.summaries && self.panel_demand("playlists") {
            self.request_playlist_selection(cx);
        }
        self.sync_loading(cx);
        if changed || refresh_selection || selected != self.browser.playlists().selected() {
            cx.notify();
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.reconcile(self.context.snapshot(), cx);
    }
    fn tr(&self, id: &str) -> SharedString {
        self.context.tr(id)
    }
    fn report_error(&self, message: String, cx: &mut Context<Self>) {
        cx.emit(PlaylistEvent::Error(message));
    }
    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(error) = self.context.handle().send(command) {
            self.report_error(format!("{error:#}"), cx);
        }
        self.refresh(cx);
        cx.notify();
    }
    pub(in crate::gui) fn set_visibility(
        &mut self,
        window_visible: bool,
        panel_visible: bool,
        menu_catalog_demand: bool,
        cx: &mut Context<Self>,
    ) {
        let changed = (
            self.window_visible,
            self.panel_visible,
            self.menu_catalog_demand,
        ) != (window_visible, panel_visible, menu_catalog_demand);
        self.window_visible = window_visible;
        self.panel_visible = panel_visible;
        self.menu_catalog_demand = menu_catalog_demand;
        if !self.playlist_view_demand(ViewRequestKind::Summaries) {
            self.browser
                .cancel_playlist_read(ViewRequestKind::Summaries);
        }
        if !self.panel_demand("playlists") {
            self.browser.cancel_playlist_read(ViewRequestKind::Entries);
            self.browser
                .cancel_playlist_read(ViewRequestKind::Selection);
        }
        self.sync_loading(cx);
        if changed && self.playlist_view_demand(ViewRequestKind::Summaries) {
            let visible = self.browser.playlists().summaries().rows.visible();
            self.ensure_playlist_range(visible, cx);
            if self.panel_demand("playlists") {
                self.request_playlist_selection(cx);
                let visible = self.browser.playlists().entries().rows.visible();
                self.ensure_entry_range(visible, cx);
            }
        }
    }
    fn panel_demand(&self, _: &str) -> bool {
        self.window_visible && self.panel_visible && !self.state.system.shutting_down
    }
    fn playlist_view_demand(&self, kind: ViewRequestKind) -> bool {
        self.panel_demand("playlists")
            || (kind == ViewRequestKind::Summaries
                && self.window_visible
                && self.menu_catalog_demand
                && !self.state.system.shutting_down)
    }
    fn sync_loading(&mut self, cx: &mut Context<Self>) {
        let visible = self.panel_demand("playlists");
        let browser = &self.browser;
        self.loading_text
            .retain(|kind, token| visible && browser.view_loading(*kind, token));
        self.loading_text
            .schedule(cx, |this| &mut this.loading_text);
    }
    pub(in crate::gui) fn set_shell_context(
        &mut self,
        focus: FocusHandle,
        menu_scope: Option<FocusScope>,
        dialog_scope: Option<FocusScope>,
        cx: &mut Context<Self>,
    ) {
        let changed = self.layout_focus != focus
            || self.menu_scope.as_ref().map(FocusScope::handle)
                != menu_scope.as_ref().map(FocusScope::handle)
            || self.dialog_scope.as_ref().map(FocusScope::handle)
                != dialog_scope.as_ref().map(FocusScope::handle);
        self.layout_focus = focus;
        self.menu_scope = menu_scope;
        self.dialog_scope = dialog_scope;
        if changed {
            cx.notify();
        }
    }
    pub(in crate::gui) fn set_transfer_state(
        &mut self,
        library_selection_pending: bool,
        content_resolution: bool,
        cx: &mut Context<Self>,
    ) {
        if (self.library_selection_pending, self.content_resolution)
            != (library_selection_pending, content_resolution)
        {
            self.library_selection_pending = library_selection_pending;
            self.content_resolution = content_resolution;
            cx.notify();
        }
    }
    pub(in crate::gui) fn set_focused(&mut self, focused: bool, cx: &mut Context<Self>) {
        if self.focused != focused {
            self.focused = focused;
            cx.notify();
        }
    }
    pub(in crate::gui) fn cancel_window_reads(&mut self, cx: &mut Context<Self>) {
        self.browser.cancel_window_reads();
        self.loading_text.clear();
        cx.notify();
    }
    fn focus_layout(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.layout_focus.focus(window, cx);
        cx.emit(PlaylistEvent::Focus(None));
    }
    fn focus_list(&mut self, cx: &mut Context<Self>) {
        let changed = !self.focused
            || self.browser.focus() != Some(BrowserFocus::List(ListFocus::PlaylistTracks));
        self.focused = true;
        self.browser.focus_list(ListFocus::PlaylistTracks);
        if changed {
            cx.notify();
        }
        cx.emit(PlaylistEvent::Focus(Some(BrowserFocus::List(
            ListFocus::PlaylistTracks,
        ))));
    }
    fn focus_catalog(&mut self, cx: &mut Context<Self>) {
        let changed = !self.focused || self.browser.focus() != Some(BrowserFocus::Playlists);
        self.focused = true;
        self.browser.focus_playlists();
        if changed {
            cx.notify();
        }
        cx.emit(PlaylistEvent::Focus(Some(BrowserFocus::Playlists)));
    }
    fn menu_surface(&self, surface: gpui::Stateful<Div>) -> gpui::Stateful<Div> {
        if let Some(scope) = &self.menu_scope {
            scope.menu_surface(surface)
        } else {
            surface
        }
    }
    fn dialog_surface(&self, surface: gpui::Stateful<Div>) -> gpui::Stateful<Div> {
        if let Some(scope) = &self.dialog_scope {
            scope.surface(surface)
        } else {
            surface
        }
    }
    pub(in crate::gui) fn render_panel(
        &mut self,
        panel_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.playlists_panel(panel_id, window, cx)
    }
    pub(in crate::gui) fn focus_name(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.name_input.read(cx).focus_handle(cx).focus(window, cx);
    }
    pub(in crate::gui) fn clear_action(&mut self, cx: &mut Context<Self>) {
        self.playlist_action = None;
        cx.notify();
    }
    pub(in crate::gui) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.name_input.read(cx).is_focused(window)
            || self.playlist_search.read(cx).input_focused(window, cx)
            || self
                .playlist_entries_search
                .read(cx)
                .input_focused(window, cx)
    }
    pub(in crate::gui) fn focus_search(
        &mut self,
        entries: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let search = self.search_for_input(entries, window, cx);
        search.update(cx, |search, cx| search.focus(window, cx));
    }
    pub(in crate::gui) fn escape_search(
        &mut self,
        entries: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.search_for_input(entries, window, cx)
            .update(cx, |search, cx| search.escape(window, cx))
    }
    fn search_for_input(&self, entries: bool, window: &Window, cx: &App) -> &Entity<SearchField> {
        for search in [&self.playlist_search, &self.playlist_entries_search] {
            if search.read(cx).input_focused(window, cx) {
                return search;
            }
        }
        if entries && self.browser.playlists().selected().is_some() {
            &self.playlist_entries_search
        } else {
            &self.playlist_search
        }
    }
    pub(in crate::gui) fn cancel_selection(&mut self, cx: &mut Context<Self>) {
        let pending = self.intent.take().is_some();
        self.browser.playlist_selection_mut().cancel_manual();
        if pending {
            cx.notify();
        }
    }
    fn manual_intent_current(&self, intent: ManualIntentId) -> bool {
        self.intent == Some(intent)
    }
    fn fail_manual_intent(&mut self, intent: ManualIntentId, cx: &mut Context<Self>) {
        if self.intent == Some(intent) {
            self.intent = None;
            self.browser.playlist_selection_mut().cancel_manual();
            cx.emit(PlaylistEvent::Failed(intent));
        }
    }
    fn ready_entry_selection(
        &self,
        intent: ManualIntentId,
        commit: selection::EntryCommit,
        cx: &mut Context<Self>,
    ) {
        cx.emit(PlaylistEvent::Ready(intent, RefCell::new(Some(commit))));
    }
    pub(in crate::gui) fn play_selection(&mut self, cx: &mut Context<Self>) {
        if self.browser.playlists().track_selection.ready()
            && let Some(row) = self.browser.playlists().track_selection.active
        {
            self.send(
                Command::Play {
                    track_id: row.track_id,
                },
                cx,
            );
        }
    }
    pub(in crate::gui) fn editor_open(&self) -> bool {
        self.playlist_editor.is_some()
    }
    pub(in crate::gui) fn action(&self) -> Option<&PlaylistAction> {
        self.playlist_action.as_ref()
    }
    pub(in crate::gui) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.cancel_selection(cx);
        self.browser.remove_playlist_selection();
        cx.notify();
    }
    pub(in crate::gui) fn catalog_len(&self) -> usize {
        self.browser.playlists().summaries().rows.len()
    }
    pub(in crate::gui) fn catalog_row(
        &self,
        index: usize,
    ) -> Option<&crate::model::PlaylistSummary> {
        self.browser.playlists().summaries().rows.get(index)
    }
    pub(in crate::gui) fn catalog_loading_row(
        &self,
        index: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        self.paged_loading_row(ViewRequestKind::Summaries, index, cx)
    }
    fn request_queue_replace(
        &self,
        replacement: crate::gui::coordination::queue_replace::QueueReplacement,
        cx: &mut Context<Self>,
    ) {
        cx.emit(PlaylistEvent::QueueReplace(RefCell::new(Some(replacement))));
    }
    fn append_playlist_to_queue(&self, id: i64, cx: &mut Context<Self>) {
        cx.emit(PlaylistEvent::AppendPlaylist(id));
    }
    fn open_object_menu(
        &self,
        request: crate::gui::ContentMenuRequest,
        guard: crate::gui::list_interaction::ContentGuard,
        remove: Option<Command>,
        cx: &mut Context<Self>,
    ) {
        cx.emit(PlaylistEvent::ContentMenu(RefCell::new(Some(
            PlaylistContentMenu {
                request,
                guard,
                remove,
            },
        ))));
    }
    fn resolve_library_drag(
        &self,
        nodes: Vec<crate::gui::library::LibraryNode>,
        destination: crate::gui::DragDestination,
        cx: &mut Context<Self>,
    ) {
        cx.emit(PlaylistEvent::ResolveLibrary(
            RefCell::new(Some(nodes)),
            destination,
        ));
    }
    fn drop_content(
        &mut self,
        drag: &crate::gui::ContentDrag,
        destination: crate::gui::DragDestination,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if self.can_transfer_content(drag, destination)
            && let Some(source) = drag.source()
        {
            cx.emit(PlaylistEvent::Transfer(
                RefCell::new(Some(source.clone())),
                destination,
            ));
        }
    }
    fn transfer_queue_drag(
        &mut self,
        drag: &QueueDrag,
        destination: crate::gui::DragDestination,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if self.can_transfer_content(drag, destination) {
            cx.emit(PlaylistEvent::Transfer(
                RefCell::new(Some(TrackSource::Tracks(drag.track_ids()))),
                destination,
            ));
        }
    }
    pub(in crate::gui) fn transfer_playlist_drag(
        &mut self,
        drag: &PlaylistDrag,
        destination: crate::gui::DragDestination,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if !self.can_transfer_content(drag, destination) {
            return;
        }
        if drag.targets_source(destination) {
            self.move_playlist_entries(drag, None, cx);
        } else {
            cx.emit(PlaylistEvent::Transfer(
                RefCell::new(Some(TrackSource::Tracks(drag.track_ids()))),
                destination,
            ));
        }
    }
    fn can_transfer_content(
        &self,
        value: &dyn std::any::Any,
        destination: crate::gui::DragDestination,
    ) -> bool {
        if let crate::gui::DragDestination::Playlist(id) = destination
            && !self
                .state
                .library
                .playlist_entry_revisions
                .contains_key(&id)
        {
            return false;
        }
        if let Some(drag) = value.downcast_ref::<LibraryDrag>() {
            return drag.can_resolve(&self.state, self.library_selection_pending);
        }
        if let Some(drag) = value.downcast_ref::<QueueDrag>() {
            return drag.can_transfer(self.state.queue.revision, destination);
        }
        if let Some(drag) = value.downcast_ref::<PlaylistDrag>() {
            return if drag.targets_source(destination) {
                self.can_drop_playlist_entries(drag, None)
            } else {
                drag.can_transfer(&self.state, destination)
            };
        }
        if let Some(drag) = value.downcast_ref::<crate::gui::ContentDrag>() {
            return drag.source().is_some_and(|source| match source {
                TrackSource::Tracks(ids) => !ids.is_empty(),
                TrackSource::Album(_) => !self.content_resolution,
            });
        }
        false
    }
}

pub(super) struct PlaylistDrag {
    i18n: std::sync::Arc<crate::i18n::Localizer>,
    playlist_id: i64,
    source_id: i64,
    // GPUI keeps the original on_drag value; the constructor creates only its preview.
    entry_ids: std::cell::OnceCell<Vec<i64>>,
    track_ids: std::cell::OnceCell<Vec<i64>>,
    revision: u64,
    stamp: crate::model::QueryStamp,
    selection_generation: u64,
}

impl PlaylistDrag {
    pub(super) fn targets_source(&self, destination: super::DragDestination) -> bool {
        matches!(destination, super::DragDestination::Playlist(id) if id == self.playlist_id)
    }

    pub(super) fn can_transfer(
        &self,
        state: &GuiSnapshot,
        destination: super::DragDestination,
    ) -> bool {
        !self.targets_source(destination)
            && state
                .library
                .playlist_entry_revisions
                .get(&self.playlist_id)
                == Some(&self.revision)
            && self.track_ids.get().is_some_and(|ids| !ids.is_empty())
    }

    pub(super) fn track_ids(&self) -> Vec<i64> {
        self.track_ids.get().unwrap().clone()
    }

    fn accepts(&self, target: Option<i64>) -> bool {
        self.entry_ids
            .get()
            .is_some_and(|ids| !ids.is_empty() && target.is_none_or(|id| !ids.contains(&id)))
    }

    fn command(&self, target: Option<i64>) -> Option<Command> {
        if !self.accepts(target) {
            return None;
        }
        let entry_ids = self.entry_ids.get()?;
        Some(Command::MovePlaylistEntries {
            playlist_id: self.playlist_id,
            entry_ids: entry_ids.clone(),
            before_entry_id: target,
            expected_revision: self.revision,
        })
    }
}

#[derive(Clone)]
pub(super) struct PlaylistTarget {
    id: i64,
    name: String,
}

#[derive(Clone)]
pub(super) enum PlaylistAction {
    Menu(PlaylistTarget, gpui::Point<gpui::Pixels>),
    Delete(PlaylistTarget),
}

impl Playlists {
    pub(in crate::gui) fn can_drop_playlist_entries(
        &self,
        drag: &PlaylistDrag,
        target: Option<i64>,
    ) -> bool {
        self.browser.playlists().selected() == Some(drag.playlist_id)
            && self.browser.playlists().entries().query.is_none()
            && self.browser.playlists().entries().playlist_id == Some(drag.playlist_id)
            && self.browser.playlists().selection_generation() == drag.selection_generation
            && self.playlist_occurrence_stamp(drag.playlist_id) == drag.stamp
            && self.playlist_revision(drag.playlist_id) == drag.revision
            && drag.accepts(target)
    }

    pub(super) fn move_playlist_entries(
        &mut self,
        drag: &PlaylistDrag,
        target_id: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if !self.can_drop_playlist_entries(drag, target_id) {
            return;
        }
        if let Some(command) = drag.command(target_id) {
            self.send(command, cx);
        }
    }

    pub(super) fn dismiss_playlist_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.playlist_action = None;
        self.focus_layout(window, cx);
        cx.notify();
    }

    pub(super) fn playlist_action_overlay(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let Some(action) = self.playlist_action.clone() else {
            return div().into_any_element();
        };
        if let PlaylistAction::Menu(target, position) = action {
            let id = target.id;
            let has_tracks = self.state.current_track().is_some();
            let rename = target.clone();
            let delete = target.clone();
            let menu = rivu_ui::Popover::menu(theme, "playlist-row-menu", window)
                .map(|surface| self.menu_surface(surface))
                .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                    this.dismiss_playlist_action(window, cx);
                    cx.stop_propagation();
                }))
                .child(rivu_ui::menu_item(
                    "playlist-row-play",
                    self.tr("li-replace-queue"),
                    cx,
                    move |this, window, cx| {
                        let name = match this.playlist_action.take() {
                            Some(PlaylistAction::Menu(target, _)) => target.name,
                            _ => return,
                        };
                        this.focus_layout(window, cx);
                        this.request_queue_replace(
                            super::coordination::queue_replace::QueueReplacement::Playlist {
                                id,
                                name,
                            },
                            cx,
                        );
                    },
                ))
                .child(rivu_ui::menu_item(
                    "playlist-row-queue",
                    self.tr("li-queue"),
                    cx,
                    move |this, window, cx| {
                        this.dismiss_playlist_action(window, cx);
                        this.append_playlist_to_queue(id, cx);
                    },
                ))
                .child(rivu_ui::menu_separator(theme))
                .child(
                    rivu_ui::menu_item(
                        "playlist-row-add",
                        self.tr("gl-add-current"),
                        cx,
                        move |this, window, cx| {
                            let Some(track_id) = this.state.current_track().map(|track| track.id)
                            else {
                                return;
                            };
                            this.dismiss_playlist_action(window, cx);
                            this.send(
                                Command::AddPlaylist {
                                    playlist_id: id,
                                    track_ids: vec![track_id],
                                },
                                cx,
                            );
                        },
                    )
                    .opacity(if has_tracks { 1. } else { 0.4 })
                    .when(!has_tracks, |view| view.cursor_default()),
                )
                .child(rivu_ui::menu_item(
                    "playlist-row-rename",
                    self.tr("gl-rename"),
                    cx,
                    move |this, window, cx| {
                        this.playlist_action = None;
                        this.begin_playlist_name(
                            editor::PlaylistNameAction::Rename {
                                playlist_id: rename.id,
                            },
                            rename.name.clone(),
                            window,
                            cx,
                        );
                    },
                ))
                .child(rivu_ui::danger_menu_item(
                    theme,
                    rivu_ui::menu_item(
                        "playlist-row-delete",
                        self.tr("gl-delete"),
                        cx,
                        move |this, _, cx| {
                            this.playlist_action = Some(PlaylistAction::Delete(delete.clone()));
                            cx.notify();
                        },
                    ),
                ));
            return gpui::anchored()
                .position(position)
                .snap_to_window()
                .child(menu)
                .into_any_element();
        }
        let PlaylistAction::Delete(target) = action else {
            unreachable!()
        };
        let id = target.id;
        let title = self.i18n.format(
            "gl-confirm-delete-playlist",
            &[("name", target.name.as_str().into())],
        );
        let detail = self.i18n.text("gl-delete-playlist-note").to_owned();
        rivu_ui::Dialog::backdrop("playlist-action-modal")
            .map(|surface| self.dialog_surface(surface))
            .child(
                rivu_ui::Dialog::scroll_card(theme, "playlist-action-card", window)
                    .w(gpui::rems(26.25))
                    .child(panel_header(theme, title, detail).flex_shrink_0())
                    .child(
                        row()
                            .flex_shrink_0()
                            .justify_end()
                            .child(
                                IconButton::new(
                                    "playlist-action-cancel",
                                    "",
                                    self.tr("gl-cancel"),
                                )
                                .build(
                                    IconButtonStyle::Surface,
                                    IconButtonDensity::Regular,
                                    cx,
                                    |this, window, cx| this.dismiss_playlist_action(window, cx),
                                ),
                            )
                            .child(danger_control(
                                theme,
                                IconButton::new(
                                    "playlist-action-confirm",
                                    "󰆴",
                                    self.tr("gl-delete-playlist"),
                                )
                                .build(
                                    IconButtonStyle::Surface,
                                    IconButtonDensity::Regular,
                                    cx,
                                    move |this, window, cx| {
                                        this.dismiss_playlist_action(window, cx);
                                        if this.browser.playlists().selected() == Some(id) {
                                            this.browser.remove_playlist_selection();
                                        }
                                        this.send(Command::DeletePlaylist { playlist_id: id }, cx);
                                    },
                                ),
                            )),
                    ),
            )
            .into_any_element()
    }

    fn empty_playlist_view(
        &self,
        kind: ViewRequestKind,
        panel_id: u64,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let rows_error = if kind == ViewRequestKind::Entries {
            self.browser.playlists().entries().rows.error(0)
        } else {
            self.browser.playlists().summaries().rows.error(0)
        };
        let pending = self.browser.view_pending(kind);
        let playlist_id = self.browser.playlists().selected();
        let message = if let Some(error) = rows_error {
            error
        } else if pending {
            self.i18n.text("gl-loading")
        } else if kind == ViewRequestKind::Entries
            && self.browser.playlists().selected_total().is_none()
        {
            self.i18n.text("gl-playlist-count-unknown")
        } else if (kind == ViewRequestKind::Entries
            && self.browser.playlists().entries().query.is_some()
            && self
                .browser
                .playlists()
                .selected_total()
                .is_some_and(|total| total > 0))
            || (kind == ViewRequestKind::Summaries
                && self.browser.playlists().summaries().query.is_some()
                && self.state.library.playlist_total > 0)
        {
            self.i18n.text("search-no-matches")
        } else if kind == ViewRequestKind::Entries {
            self.i18n.text("gl-playlist-empty")
        } else {
            self.i18n.text("gl-no-playlists")
        };
        let message: SharedString = message.to_owned().into();
        row()
            .h(gpui::rems(LIST_ROW_REMS))
            .flex_shrink_0()
            .w_full()
            .overflow_hidden()
            .min_h_0()
            .child(
                status_message(
                    theme,
                    message.clone(),
                    if rows_error.is_some() {
                        StatusKind::Error
                    } else {
                        StatusKind::Info
                    },
                )
                .id((
                    if kind == ViewRequestKind::Entries {
                        "playlist-entries-empty-status"
                    } else {
                        "playlist-catalog-empty-status"
                    },
                    panel_id,
                ))
                .flex_1()
                .min_w_0()
                .truncate()
                .when(
                    pending && !self.loading_text.show(&kind, rows_error.is_some()),
                    |text| text.invisible(),
                )
                .tooltip(rivu_ui::text_tooltip(message)),
            )
            .when(rows_error.is_some(), |view| {
                view.child(
                    IconButton::new(
                        (
                            if kind == ViewRequestKind::Entries {
                                "playlist-entries-empty-retry"
                            } else {
                                "playlist-catalog-empty-retry"
                            },
                            panel_id,
                        ),
                        "󰑐",
                        self.tr("gl-retry"),
                    )
                    .build(
                        IconButtonStyle::Surface,
                        IconButtonDensity::Regular,
                        cx,
                        move |this, _, cx| {
                            if this.browser.view_pending(kind) {
                                return;
                            }
                            if kind == ViewRequestKind::Entries {
                                if this.browser.playlists().selected() == playlist_id
                                    && this.browser.playlists().entries().rows.error(0).is_some()
                                    && let Some(id) = playlist_id
                                {
                                    this.request_playlist_entries(id, 0, cx);
                                }
                            } else if this.browser.playlists().summaries().rows.error(0).is_some() {
                                this.request_playlist_summaries(0, cx);
                            }
                        },
                    ),
                )
            })
    }

    pub(super) fn playlists_panel(
        &mut self,
        panel_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let owner = cx.weak_entity();
        window.defer(cx, move |_, cx| {
            let _ = owner.update(cx, |this, cx| this.request_playlist_selection(cx));
        });
        let matched = self.browser.playlists().summaries().rows.total();
        let total = self.state.library.playlist_total;
        self.playlist_search.update(cx, |search, cx| {
            if let Some(matched) = matched {
                set_search_counts(search, matched, total, &self.i18n, cx);
            } else {
                search.clear_counts(cx);
            }
        });
        let matched = self.browser.playlists().entries().rows.total();
        let total = self.browser.playlists().selected_total();
        self.playlist_entries_search.update(cx, |search, cx| {
            if let (Some(matched), Some(total)) = (matched, total) {
                set_search_counts(search, matched, total, &self.i18n, cx);
            } else {
                search.clear_counts(cx);
            }
        });
        let row_height = list_row_height(window, cx);
        let playlist_id = self.browser.playlists().selected();
        let selection_entry = self.browser.playlists().selected_entry();
        let selection_unavailable = !self.browser.playlists().track_selection.ready();
        let selection_pending = self.browser.playlists().track_selection.busy;
        let selection_message: SharedString = if !selection_unavailable {
            "".into()
        } else if selection_pending {
            self.tr("gl-selection-refreshing")
        } else {
            self.tr("gl-selection-unavailable")
        };
        let selection_status = row()
            .flex_shrink_0()
            .min_w_0()
            .min_h_8()
            .overflow_hidden()
            .child(
                status_message(theme, selection_message.clone(), StatusKind::Info)
                    .id(("playlist-selection-status", panel_id))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .tooltip(rivu_ui::text_tooltip(selection_message)),
            )
            .child(div().min_w_16().min_h_8().flex_shrink_0().when(
                selection_unavailable && !selection_pending,
                |slot| {
                    slot.child(
                        IconButton::new(
                            ("retry-playlist-entry", panel_id),
                            "󰑐",
                            self.tr("gl-retry"),
                        )
                        .build(
                            IconButtonStyle::Surface,
                            IconButtonDensity::Regular,
                            cx,
                            move |this, _, cx| {
                                this.refresh(cx);
                                if this.browser.playlists().selected() == playlist_id
                                    && this.browser.playlists().selected_entry() == selection_entry
                                    && !this.browser.playlists().track_selection.ready()
                                    && !this.browser.playlists().track_selection.busy
                                {
                                    this.refresh_entry_selection(cx);
                                    cx.notify();
                                }
                            },
                        ),
                    )
                },
            ));
        let summary_status = self.browser.playlist_lookup_failure().map(|(key, error)| {
            let message: SharedString = self
                .i18n
                .format(
                    "gl-playlist-summary-unavailable",
                    &[("error", error.into())],
                )
                .into();
            row()
                .flex_shrink_0()
                .min_w_0()
                .min_h_8()
                .overflow_hidden()
                .child(
                    status_message(theme, message.clone(), StatusKind::Error)
                        .id(("playlist-summary-status", panel_id))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .tooltip(rivu_ui::text_tooltip(message)),
                )
                .child(
                    IconButton::new(
                        ("retry-playlist-summary", panel_id),
                        "󰑐",
                        self.tr("gl-retry"),
                    )
                    .build(
                        IconButtonStyle::Surface,
                        IconButtonDensity::Regular,
                        cx,
                        move |this, _, cx| this.retry_playlist_selection(key, cx),
                    ),
                )
        });
        let catalog_len = self.browser.playlists().summaries().rows.len();
        self.context.scroll.borrow_mut().register_list(
            ("playlist-catalog", panel_id),
            &self.browser.playlists().summaries().scroll,
        );
        let catalog = VirtualList::new(
            ("playlist-catalog", panel_id),
            catalog_len,
            &self.browser.playlists().summaries().scroll,
            cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                range
                    .map(|index| {
                        let Some(p) = this.browser.playlists().summaries().rows.get(index) else {
                            return this
                                .paged_loading_row(ViewRequestKind::Summaries, index, cx)
                                .h(list_row_height(window, cx))
                                .into_any_element();
                        };
                        let id = p.id;
                        let name = gpui::SharedString::from(p.name.clone());
                        let play_name = name.clone();
                        let menu_name = name.clone();
                        let context_name = name.clone();
                        let playable = p.entry_count > 0;
                        list_row(
                            theme,
                            ("playlist", id as u64),
                            this.browser.playlists().selected() == Some(id),
                        )
                        .h(list_row_height(window, cx))
                        .when(
                            (this.focused && this.layout_focus.is_focused(window))
                                && this.browser.focus() == Some(super::BrowserFocus::Playlists)
                                && this.browser.playlists().selected() == Some(id),
                            |row| row.border_color(rgb(theme.accent)),
                        )
                        .group("playlist-catalog-row")
                        .gap_0()
                        .child(row_text(
                            theme,
                            ("playlist-text", id as u64),
                            name.clone(),
                            this.i18n
                                .format("gl-track-count", &[("count", (p.entry_count).into())]),
                        ))
                        .child(
                            row()
                                .opacity(0.)
                                .group_hover("playlist-catalog-row", |style| style.opacity(1.))
                                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .child(
                                    IconButtonGroup::new(IconButtonStyle::Quiet, IconButtonDensity::Compact)
                                        .button(
                                            IconButton::new(("playlist-row-play", id as u64), "", this.tr("gl-play-playlist")).enabled(playable),
                                            cx,
                                            move |this, window, cx| {
                                        if !playable {
                                            return;
                                        }
                                        this.focus_layout(window, cx);
                                        this.request_queue_replace(
                                            super::coordination::queue_replace::QueueReplacement::Playlist {
                                                id,
                                                name: play_name.to_string(),
                                            },
                                            cx,
                                        );
                                    },
                                        )
                                        .button(
                                            IconButton::new(("playlist-row-menu", id as u64), "󰇙", this.tr("gl-playlist-actions")),
                                            cx,
                                            move |this, window, cx| {
                                        this.focus_layout(window, cx);
                                        cx.emit(PlaylistEvent::ClearMenus);

                                        this.playlist_action = Some(PlaylistAction::Menu(
                                            PlaylistTarget {
                                                id,
                                                name: menu_name.to_string(),
                                            },
                                            window.mouse_position(),
                                        ));
                                        cx.notify();
                                    },
                                        )
                                        .into_div(),
                                ),
                        )
                        .on_mouse_down(
                            gpui::MouseButton::Right,
                            cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                                this.focus_layout(window, cx);
                                cx.emit(PlaylistEvent::ClearMenus);

                                this.focus_catalog(cx);
                                this.select_playlist_container(
                                    id,
                                    context_name.to_string(),
                                    index,
                                    cx,
                                );
                                this.playlist_action = Some(PlaylistAction::Menu(
                                    PlaylistTarget {
                                        id,
                                        name: context_name.to_string(),
                                    },
                                    event.position,
                                ));
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.focus_layout(window, cx);
                            this.focus_catalog(cx);
                            this.select_playlist_container(id, name.to_string(), index, cx);
                            cx.notify();
                        }))
                        .on_drop(cx.listener(move |this, drag: &LibraryDrag, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.resolve_library_drag(
                                drag.nodes().to_vec(),
                                super::DragDestination::Playlist(id),
                                cx,
                            );
                        }))
                        .on_drop(
                            cx.listener(move |this, drag: &super::ContentDrag, window, cx| {
                                window.prevent_default();
                                cx.stop_propagation();
                                this.drop_content(drag, super::DragDestination::Playlist(id), cx);
                            }),
                        )
                        .on_drop(cx.listener(move |this, drag: &QueueDrag, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.transfer_queue_drag(
                                drag,
                                super::DragDestination::Playlist(id),
                                cx,
                            );
                        }))
                        .on_drop(cx.listener(move |this, drag: &PlaylistDrag, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                            this.transfer_playlist_drag(
                                drag,
                                super::DragDestination::Playlist(id),
                                cx,
                            );
                        }))
                        .can_drop({
                            let owner = cx.weak_entity();
                            move |value, _, cx| {
                                owner.upgrade().is_some_and(|owner| {
                                    owner.read(cx).can_transfer_content(
                                        value,
                                        super::DragDestination::Playlist(id),
                                    )
                                })
                            }
                        })
                        .drag_over::<LibraryDrag>(|style, _, _, _| style.bg(rgb(theme.highlight)))
                        .drag_over::<super::ContentDrag>(|style, _, _, _| {
                            style.bg(rgb(theme.highlight))
                        })
                        .drag_over::<QueueDrag>(|style, _, _, _| style.bg(rgb(theme.highlight)))
                        .drag_over::<PlaylistDrag>(|style, _, _, _| style.bg(rgb(theme.highlight)))
                        .into_any_element()
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .on_viewport_deferred(cx.weak_entity(), Self::ensure_playlist_range)
        .h_full()
        .min_h_0()
        .w_full();
        let catalog = if catalog_len == 0 {
            self.empty_playlist_view(ViewRequestKind::Summaries, panel_id, cx)
                .into_any_element()
        } else {
            catalog
                .scrollbar(("playlist-catalog-scrollbar", panel_id))
                .into_any_element()
        };
        let mut lists = column()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(list_viewport(catalog).when(playlist_id.is_some(), |view| {
                view.flex_none()
                    .h(row_height * catalog_len.max(1) as f32 + px(2.))
                    .max_h(gpui::relative(1. / 3.))
            }));
        if let Some(playlist_id) = playlist_id {
            lists = lists.child(
                panel_toolbar()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.focus_list(cx)),
                            )
                            .child(caption(
                                theme,
                                total.map_or_else(
                                    || "—".to_owned(),
                                    |count| {
                                        self.i18n
                                            .format("gl-track-count", &[("count", count.into())])
                                    },
                                ),
                            )),
                    )
                    .child(self.playlist_entries_search.clone()),
            );
            self.context.scroll.borrow_mut().register_list(
                ("playlist-entries", panel_id),
                &self.browser.playlists().entries().scroll,
            );
            let entries_len = self.browser.playlists().entries().rows.len();
            let entries = VirtualList::new(
                ("playlist-entries", panel_id),
                entries_len,
                &self.browser.playlists().entries().scroll,
                cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                    range
                        .map(|index| {
                            let Some(e) = this.browser.playlists().entries().rows.get(index) else {
                                return this
                                    .paged_loading_row(ViewRequestKind::Entries, index, cx)
                                    .h(list_row_height(window, cx))
                                    .into_any_element();
                            };
                            let id = e.id;
                            let track_id = e.track_id;
                            let title = e.title.clone();
                            let favorite = e.favorite;
                            let revision = this.playlist_revision(playlist_id);
                            let stamp = this.playlist_occurrence_stamp(playlist_id);
                            let selection_generation =
                                this.browser.playlists().selection_generation();
                            let drag_owner = cx.entity().downgrade();
                            let drag = PlaylistDrag {
                                i18n: std::sync::Arc::clone(&this.i18n),
                                playlist_id,
                                source_id: id,
                                entry_ids: Default::default(),
                                track_ids: Default::default(),
                                revision,
                                stamp,
                                selection_generation,
                            };
                            let detail = format!(
                                "{}{}{}",
                                e.artist,
                                if e.artist.is_empty() || e.album.is_empty() {
                                    ""
                                } else {
                                    " / "
                                },
                                e.album
                            );
                            track_list_row(
                                theme,
                                ("playlist-entry", id as u64),
                                ("playlist-entry-text", id as u64),
                                (title, detail),
                                e.position,
                                this.state
                                    .current_track()
                                    .is_some_and(|track| track.id == track_id),
                                this.browser.playlists().track_selection.contains(id),
                            )
                            .h(list_row_height(window, cx))
                            .when(
                                (this.focused && this.layout_focus.is_focused(window))
                                    && this.browser.focus()
                                        == Some(super::BrowserFocus::List(
                                            ListFocus::PlaylistTracks,
                                        ))
                                    && this.browser.playlists().selected_entry() == Some(id),
                                |row| row.border_color(rgb(theme.accent)),
                            )
                            .group("playlist-track-row")
                            .relative()
                            .gap_0()
                            .child(
                                hover_actions(
                                    theme,
                                    "playlist-track-row",
                                    this.browser.playlists().track_selection.contains(id),
                                )
                                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation()
                                })
                                .child(
                                    IconButtonGroup::new(
                                        IconButtonStyle::Quiet,
                                        IconButtonDensity::Compact,
                                    )
                                    .button(
                                        IconButton::new(
                                            ("playlist-entry-play", id as u64),
                                            "",
                                            this.tr("gl-play-track"),
                                        ),
                                        cx,
                                        move |this, _, cx| {
                                            this.send(Command::Play { track_id }, cx)
                                        },
                                    )
                                    .button(
                                        IconButton::new(
                                            ("playlist-entry-favorite", id as u64),
                                            if favorite { "󰓎" } else { "󰓐" },
                                            this.tr(if favorite {
                                                "gl-unfavorite"
                                            } else {
                                                "gl-favorite"
                                            }),
                                        )
                                        .toggled(favorite),
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
                                    )
                                    .button(
                                        IconButton::new(
                                            ("playlist-entry-remove", id as u64),
                                            "",
                                            this.tr("gl-remove-playlist"),
                                        ),
                                        cx,
                                        move |this, _, cx| {
                                            this.refresh(cx);
                                            if this.browser.playlists().selected()
                                                != Some(playlist_id)
                                                || this.playlist_occurrence_stamp(playlist_id)
                                                    != stamp
                                            {
                                                return;
                                            }
                                            this.send(
                                                Command::RemovePlaylistEntries {
                                                    playlist_id,
                                                    entry_ids: vec![id],
                                                    expected_revision: revision,
                                                },
                                                cx,
                                            );
                                        },
                                    )
                                    .into_div(),
                                ),
                            )
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    this.focus_layout(window, cx);
                                    this.focus_list(cx);
                                }),
                            )
                            .on_mouse_down(
                                gpui::MouseButton::Right,
                                cx.listener(
                                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                                        this.focus_layout(window, cx);
                                        this.focus_list(cx);
                                        if this.browser.playlists().track_selection.contains(id)
                                            && !this.browser.playlists().track_selection.ready()
                                        {
                                            cx.stop_propagation();
                                            return;
                                        }
                                        if !this.browser.playlists().track_selection.contains(id) {
                                            this.select_playlist_entry(id, index, false, false, cx);
                                        }
                                        let rows =
                                            this.browser.playlists().track_selection.ordered();
                                        let track_ids = if rows.iter().any(|row| row.id == id) {
                                            rows.iter().map(|row| row.track_id).collect()
                                        } else {
                                            vec![track_id]
                                        };
                                        let entry_ids = if rows.iter().any(|row| row.id == id) {
                                            rows.iter().map(|row| row.id).collect()
                                        } else {
                                            vec![id]
                                        };
                                        this.open_object_menu(
                                            super::ContentMenuRequest {
                                                source: TrackSource::Tracks(track_ids),
                                                position: event.position,
                                                play: Some(Command::Play { track_id }),
                                            },
                                            super::list_interaction::ContentGuard::Playlist(
                                                playlist_id,
                                                revision,
                                            ),
                                            Some(Command::RemovePlaylistEntries {
                                                playlist_id,
                                                entry_ids,
                                                expected_revision: revision,
                                            }),
                                            cx,
                                        );
                                        cx.stop_propagation();
                                    },
                                ),
                            )
                            .on_click(cx.listener(
                                move |this, event: &gpui::ClickEvent, window, cx| {
                                    this.focus_layout(window, cx);
                                    this.focus_list(cx);
                                    let modifiers = event.modifiers();
                                    this.select_playlist_entry(
                                        id,
                                        index,
                                        modifiers.shift,
                                        modifiers.control,
                                        cx,
                                    );
                                    if event.click_count() == 2 {
                                        this.send(Command::Play { track_id }, cx);
                                    }
                                    cx.notify();
                                },
                            ))
                            .when(
                                !selection_unavailable
                                    && this.browser.playlists().entries().query.is_none(),
                                |row| {
                                    row.on_drag(drag, move |drag, _, window, cx| {
                                        rivu_ui::begin_list_drag(window, cx);
                                        drag.entry_ids.get_or_init(|| {
                                            drag_owner
                                                .update(cx, |this, cx| {
                                                    this.refresh(cx);
                                                    if this.playlist_revision(drag.playlist_id)
                                                        != drag.revision
                                                        || this
                                                            .browser
                                                            .playlists()
                                                            .selection_generation()
                                                            != drag.selection_generation
                                                        || this
                                                            .browser
                                                            .playlists()
                                                            .entries()
                                                            .query
                                                            .is_some()
                                                        || !this
                                                            .browser
                                                            .playlists()
                                                            .track_selection
                                                            .ready()
                                                    {
                                                        return Vec::new();
                                                    }
                                                    if !this
                                                        .browser
                                                        .playlists()
                                                        .track_selection
                                                        .contains(drag.source_id)
                                                    {
                                                        this.select_playlist_entry(
                                                            drag.source_id,
                                                            index,
                                                            false,
                                                            false,
                                                            cx,
                                                        );
                                                    }
                                                    let tracks = if this
                                                        .browser
                                                        .playlists()
                                                        .track_selection
                                                        .contains(drag.source_id)
                                                    {
                                                        this.browser
                                                            .playlists()
                                                            .track_selection
                                                            .ordered()
                                                            .iter()
                                                            .map(|row| row.track_id)
                                                            .collect()
                                                    } else {
                                                        vec![track_id]
                                                    };
                                                    let _ = drag.track_ids.set(tracks);
                                                    this.browser
                                                        .playlists()
                                                        .track_selection
                                                        .drag_ids(drag.source_id)
                                                })
                                                .unwrap_or_default()
                                        });
                                        let label = drag
                                            .i18n
                                            .format(
                                                "gl-move-playlist-entries",
                                                &[(
                                                    "count",
                                                    drag.entry_ids.get().map_or(0, Vec::len).into(),
                                                )],
                                            )
                                            .into();
                                        cx.new(|_| rivu_ui::ListDragPreview { label })
                                    })
                                },
                            )
                            .on_drop(cx.listener(move |this, drag: &PlaylistDrag, window, cx| {
                                window.prevent_default();
                                cx.stop_propagation();
                                if drag.playlist_id == playlist_id {
                                    this.move_playlist_entries(drag, Some(id), cx);
                                } else {
                                    this.transfer_playlist_drag(
                                        drag,
                                        super::DragDestination::Playlist(playlist_id),
                                        cx,
                                    );
                                }
                            }))
                            .can_drop({
                                let owner = cx.weak_entity();
                                move |value, _, cx| {
                                    owner.upgrade().is_some_and(|owner| {
                                        value.downcast_ref::<PlaylistDrag>().is_some_and(|drag| {
                                            let app = owner.read(cx);
                                            if drag.playlist_id == playlist_id {
                                                app.can_drop_playlist_entries(drag, Some(id))
                                            } else {
                                                app.can_transfer_content(
                                                    drag,
                                                    super::DragDestination::Playlist(playlist_id),
                                                )
                                            }
                                        })
                                    })
                                }
                            })
                            .drag_over::<PlaylistDrag>(|style, _, _, _| {
                                style.bg(rgb(theme.highlight))
                            })
                            .into_any_element()
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .on_viewport_deferred(cx.weak_entity(), move |this, range, cx| {
                if this.browser.playlists().entries().playlist_id == Some(playlist_id) {
                    this.ensure_entry_range(range, cx);
                }
            })
            .on_drop(cx.listener(move |this, drag: &PlaylistDrag, window, cx| {
                window.prevent_default();
                this.transfer_playlist_drag(
                    drag,
                    super::DragDestination::Playlist(playlist_id),
                    cx,
                );
            }))
            .can_drop({
                let owner = cx.weak_entity();
                move |value, _, cx| {
                    owner.upgrade().is_some_and(|owner| {
                        value.downcast_ref::<PlaylistDrag>().is_some_and(|drag| {
                            owner.read(cx).can_transfer_content(
                                drag,
                                super::DragDestination::Playlist(playlist_id),
                            )
                        })
                    })
                }
            })
            .h_full()
            .min_h_0()
            .w_full();
            let entries = if entries_len == 0 {
                self.empty_playlist_view(ViewRequestKind::Entries, panel_id, cx)
                    .into_any_element()
            } else {
                entries
                    .scrollbar(("playlist-entries-scrollbar", panel_id))
                    .into_any_element()
            };
            lists = lists.child(
                column()
                    .flex_1()
                    .min_h_0()
                    .child(
                        list_viewport(entries)
                            .flex_none()
                            .flex_shrink_1()
                            .h(row_height * entries_len.max(1) as f32 + px(2.))
                            .max_h(gpui::relative(1.)),
                    )
                    .child(
                        div()
                            .id(("playlist-append", panel_id))
                            .flex_1()
                            .min_h_4()
                            .w_full()
                            .on_drop(cx.listener(move |this, drag: &PlaylistDrag, window, cx| {
                                window.prevent_default();
                                this.transfer_playlist_drag(
                                    drag,
                                    super::DragDestination::Playlist(playlist_id),
                                    cx,
                                );
                            }))
                            .can_drop({
                                let owner = cx.weak_entity();
                                move |value, _, cx| {
                                    owner.upgrade().is_some_and(|owner| {
                                        value.downcast_ref::<PlaylistDrag>().is_some_and(|drag| {
                                            owner.read(cx).can_transfer_content(
                                                drag,
                                                super::DragDestination::Playlist(playlist_id),
                                            )
                                        })
                                    })
                                }
                            })
                            .drag_over::<PlaylistDrag>(|style, _, _, _| {
                                style.bg(rgb(theme.highlight))
                            }),
                    ),
            );
        }
        panel_surface(("playlists-panel", panel_id))
            .on_drop(cx.listener(|this, drag: &LibraryDrag, window, cx| {
                window.prevent_default();
                if let Some(id) = this.browser.playlists().selected() {
                    this.resolve_library_drag(
                        drag.nodes().to_vec(),
                        super::DragDestination::Playlist(id),
                        cx,
                    );
                }
            }))
            .can_drop({
                let owner = cx.weak_entity();
                move |value, _, cx| {
                    owner.upgrade().is_some_and(|owner| {
                        let app = owner.read(cx);
                        app.browser.playlists().selected().is_some_and(|id| {
                            app.can_transfer_content(value, super::DragDestination::Playlist(id))
                        })
                    })
                }
            })
            .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .on_drop(cx.listener(|this, drag: &super::ContentDrag, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                if let Some(id) = this.browser.playlists().selected() {
                    this.drop_content(drag, super::DragDestination::Playlist(id), cx);
                }
            }))
            .on_drop(cx.listener(|this, drag: &QueueDrag, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                if let Some(id) = this.browser.playlists().selected() {
                    this.transfer_queue_drag(drag, super::DragDestination::Playlist(id), cx);
                }
            }))
            .on_drop(cx.listener(|this, drag: &PlaylistDrag, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                if let Some(id) = this.browser.playlists().selected() {
                    this.transfer_playlist_drag(drag, super::DragDestination::Playlist(id), cx);
                }
            }))
            .drag_over::<super::ContentDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .drag_over::<QueueDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .drag_over::<PlaylistDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .child(
                row()
                    .min_h_8()
                    .flex_wrap()
                    .flex_shrink_0()
                    .items_end()
                    .child(
                        list_panel_header(
                            theme,
                            self.tr("gl-playlists"),
                            if let Some(total) = self.browser.playlists().summaries().rows.total() {
                                self.i18n
                                    .format(
                                        "gl-playlist-count",
                                        &[
                                            ("total", total.into()),
                                            (
                                                "selected",
                                                self.browser
                                                    .playlists()
                                                    .track_selection
                                                    .len()
                                                    .into(),
                                            ),
                                        ],
                                    )
                                    .into()
                            } else {
                                self.tr("gl-loading")
                            },
                        )
                        .flex_1()
                        .min_w_12()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.focus_catalog(cx)),
                        ),
                    )
                    .child(self.playlist_search.clone())
                    .child(
                        IconButton::new(
                            ("playlist-create", panel_id),
                            "",
                            self.tr("gl-new-playlist"),
                        )
                        .build(
                            IconButtonStyle::Surface,
                            IconButtonDensity::Regular,
                            cx,
                            |this, window, cx| {
                                this.begin_playlist_name(
                                    editor::PlaylistNameAction::Create {
                                        track_ids: Vec::new(),
                                    },
                                    String::new(),
                                    window,
                                    cx,
                                );
                            },
                        ),
                    )
                    .child(
                        IconButton::new(
                            ("playlist-import", panel_id),
                            "",
                            self.tr("gl-import-m3u"),
                        )
                        .build(
                            IconButtonStyle::Surface,
                            IconButtonDensity::Regular,
                            cx,
                            |this, _, cx| this.choose_playlist_import(cx),
                        ),
                    )
                    .child(
                        IconButton::new(
                            ("playlist-export", panel_id),
                            "",
                            self.tr("gl-export-playlist"),
                        )
                        .enabled(playlist_id.is_some())
                        .build(
                            IconButtonStyle::Surface,
                            IconButtonDensity::Regular,
                            cx,
                            move |this, _, cx| {
                                if let Some(id) = playlist_id {
                                    this.choose_playlist_export(id, cx);
                                }
                            },
                        ),
                    ),
            )
            .when(selection_unavailable, |panel| panel.child(selection_status))
            .when_some(summary_status, |panel, status| panel.child(status))
            .child(lists)
            .into_any_element()
    }
}

#[cfg(test)]
mod playlist_drag_tests {
    use super::PlaylistDrag;

    #[test]
    fn playlist_drop_rejects_empty_groups_and_selected_targets() {
        let drag = PlaylistDrag {
            i18n: std::sync::Arc::new(crate::i18n::Localizer::new("en-US")),
            playlist_id: 8,
            source_id: 40,
            entry_ids: std::cell::OnceCell::from(vec![7, 40, 2]),
            track_ids: std::cell::OnceCell::from(vec![9, 9, 3]),
            revision: 19,
            stamp: Default::default(),
            selection_generation: 4,
        };
        for target in [7, 40, 2] {
            assert!(drag.command(Some(target)).is_none());
        }
        let empty = PlaylistDrag {
            entry_ids: Default::default(),
            ..drag
        };
        assert!(empty.command(None).is_none());
    }
}
