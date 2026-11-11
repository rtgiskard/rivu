//! Window-owned content menus and asynchronous collection resolution.
use super::collection::{CollectionRead, resolve_album};
use crate::gui::{
    library::LibraryNode,
    list_interaction::{ContentGuard, ContentMenuRequest, DragDestination, TrackSource},
    playlists,
    window::WindowView,
};
use gpui::{prelude::*, *};
use rivu_protocol::model::{Command, Query};
use rivu_ui::{POPOVER_MAX_REMS, Popover, caption, menu_item};
use std::{collections::HashSet, path::PathBuf};

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoveTarget {
    Selection,
    LibraryRoot,
}

pub(in crate::gui) struct ContentMenu {
    source: TrackSource,
    album_revision: u64,
    nodes: Option<Vec<LibraryNode>>,
    tracks_only: bool,
    guard: ContentGuard,
    position: Point<Pixels>,
    play: Option<Command>,
    remove: Option<Command>,
    remove_label: &'static str,
    root: Option<PathBuf>,
    loading: bool,
    confirm_remove: Option<RemoveTarget>,
    pub(in crate::gui) playlists: bool,
    playlist_targets: PlaylistTargets,
    playlist_trigger: FocusHandle,
    restore_playlist_trigger: bool,
}

// Only the active logical row is menu-owned. Payload and asynchronous demand
// stay in Playlists, including unloaded rows at their absolute indices.
struct PlaylistTargets {
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    selected: std::cell::Cell<(usize, Option<i64>)>,
    revision: u64,
    query: Option<String>,
}

impl PlaylistTargets {
    fn new(cx: &mut App) -> Self {
        Self {
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            selected: std::cell::Cell::new((0, None)),
            revision: 0,
            query: None,
        }
    }

    fn current(&self, playlists: &playlists::Playlists) -> bool {
        playlists.catalog_identity() == (self.revision, self.query.as_deref())
    }

    fn select(&mut self, index: usize, playlists: &playlists::Playlists) {
        let (revision, query) = playlists.catalog_identity();
        self.revision = revision;
        if self.query.as_deref() != query {
            self.query = query.map(str::to_owned);
        }
        self.selected
            .set((index, playlists.catalog_row(index).map(|row| row.id)));
        self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
    }

    fn rendered_selection(&self, index: usize, id: i64, playlists: &playlists::Playlists) -> bool {
        let (active, expected) = self.selected.get();
        if active != index || !self.current(playlists) {
            return false;
        }
        // A requested row can become available asynchronously. Once bound, its
        // identity must not silently follow a replacement at the same index.
        if expected.is_none() {
            self.selected.set((index, Some(id)));
        }
        self.selected.get().1 == Some(id)
    }

    fn candidate(&self, playlists: &playlists::Playlists) -> Option<i64> {
        let (index, expected) = self.selected.get();
        self.current(playlists)
            .then_some(expected)
            .flatten()
            .filter(|id| {
                playlists
                    .catalog_row(index)
                    .is_some_and(|row| row.id == *id)
            })
    }
}

// Preserve every page-state transition used to invalidate deferred menu focus.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::gui) struct ContentMenuPage {
    loading: bool,
    playlists: bool,
    confirming_remove: bool,
}

impl ContentMenu {
    pub(in crate::gui) fn keyboard_page(&self) -> ContentMenuPage {
        ContentMenuPage {
            loading: self.loading,
            playlists: self.playlists,
            confirming_remove: self.confirm_remove.is_some(),
        }
    }

    pub(in crate::gui) fn focus_playlist_return(&mut self, window: &mut Window, cx: &mut App) {
        if std::mem::take(&mut self.restore_playlist_trigger) && !self.playlists && !self.loading {
            self.playlist_trigger.focus(window, cx);
        }
    }
}

#[derive(Clone, Copy)]
enum ContentAction {
    Play,
    Queue,
    Playlist(i64),
    CreatePlaylist(AnyWindowHandle),
    Favorite(bool),
    Remove,
    RemoveLibraryRoot,
}

impl WindowView {
    fn begin_content_resolution(&mut self, cx: &mut Context<Self>) -> Option<CollectionRead> {
        let Some(read) = self.content_resolution.begin() else {
            self.error = Some(self.i18n.text("li-source-busy").to_owned());
            cx.notify();
            return None;
        };
        Some(read)
    }

    fn finish_content_resolution(&mut self, read: &CollectionRead, cx: &mut Context<Self>) -> bool {
        if !self.content_resolution.finish(read) {
            return false;
        }
        self.refresh(cx);
        cx.notify();
        read.ticket.is_current() && self.window_visible && !self.state.system.shutting_down
    }

    fn timeout_content_resolution(
        &mut self,
        read: &CollectionRead,
        response: rivu_protocol::response::QueryResponse,
        current: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.content_resolution.timeout(read) {
            return;
        }
        self.replace_content_menu(None, cx);
        if current && self.window_visible && !self.state.system.shutting_down {
            self.error = response.result.err().map(|error| error.to_string());
            cx.notify();
        }
    }

    pub(in crate::gui) fn replace_content_menu(
        &mut self,
        menu: Option<ContentMenu>,
        cx: &mut Context<Self>,
    ) -> Option<ContentMenu> {
        self.content_resolution.cancel();
        let previous = std::mem::replace(&mut self.content_menu, menu);
        if previous.as_ref().is_some_and(|menu| menu.playlists)
            != self
                .content_menu
                .as_ref()
                .is_some_and(|menu| menu.playlists)
        {
            self.reconcile_feature_demand(cx);
        }
        if previous.is_some() || self.content_menu.is_some() {
            cx.notify();
        }
        previous
    }

    fn set_content_menu_playlists(&mut self, visible: bool, cx: &mut Context<Self>) {
        if let Some(menu) = &mut self.content_menu
            && menu.playlists != visible
        {
            menu.playlists = visible;
            menu.restore_playlist_trigger = !visible;
            if visible {
                menu.playlist_targets.select(0, self.playlists.read(cx));
                menu.playlist_targets
                    .scroll
                    .scroll_to_item_strict(0, ScrollStrategy::Top);
            }
            self.reconcile_feature_demand(cx);
            cx.notify();
        }
    }

    pub(in crate::gui) fn open_content_menu(
        &mut self,
        request: ContentMenuRequest,
        cx: &mut Context<Self>,
    ) {
        let guard = match request.source {
            TrackSource::Album { .. } => ContentGuard::Library(
                self.state.library.revision,
                self.state.library.structure_revision,
            ),
            TrackSource::Tracks(_) => ContentGuard::None,
        };
        self.open_object_menu(request, guard, None, None, None, cx);
    }

    pub(in crate::gui) fn open_object_menu(
        &mut self,
        request: ContentMenuRequest,
        guard: ContentGuard,
        remove: Option<Command>,
        nodes: Option<Vec<LibraryNode>>,
        root: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if matches!(&request.source, TrackSource::Tracks(ids) if ids.is_empty())
            && nodes.as_ref().is_none_or(Vec::is_empty)
        {
            self.replace_content_menu(None, cx);
            cx.notify();
            return;
        }
        self.panel_menu = None;
        self.playlists
            .update(cx, |playlists, cx| playlists.clear_action(cx));
        self.queue_replace = None;
        let tracks_only = nodes.as_ref().is_none_or(|nodes| {
            nodes
                .iter()
                .all(|node| matches!(node, LibraryNode::Track(_)))
        });
        let remove_label = match &remove {
            Some(Command::RemoveQueueEntries { .. }) => "gl-remove-queue-selected",
            Some(Command::RemovePlaylistEntries { .. }) => "gl-remove-playlist",
            Some(Command::RemoveHistory { .. }) => "gl-remove-history",
            _ => "gl-remove-library-selected",
        };
        let album_revision = match &request.source {
            TrackSource::Album { revision, .. } => *revision,
            TrackSource::Tracks(_) => self.state.library.album_revision,
        };
        let menu = Some(ContentMenu {
            source: request.source,
            album_revision,
            nodes,
            tracks_only,
            guard,
            position: request.position,
            play: request.play,
            remove,
            remove_label,
            root,
            loading: false,
            confirm_remove: None,
            playlists: false,
            playlist_targets: PlaylistTargets::new(cx),
            playlist_trigger: cx.focus_handle(),
            restore_playlist_trigger: false,
        });
        self.replace_content_menu(menu, cx);
        cx.notify();
    }

    // Only favorites and creating a playlist need concrete IDs. Keep the menu's
    // cancellation owner alive until the complete, revision-fenced read finishes.
    fn resolve_content_action(&mut self, action: ContentAction, cx: &mut Context<Self>) {
        let Some(menu) = self.content_menu.as_ref() else {
            return;
        };
        let TrackSource::Album { key, .. } = &menu.source else {
            return;
        };
        let key = key.clone();
        let guard = menu.guard.clone();
        let album_revision = menu.album_revision;
        let Some(read) = self.begin_content_resolution(cx) else {
            return;
        };
        self.set_content_menu_playlists(false, cx);
        self.content_menu.as_mut().unwrap().loading = true;
        let ticket = read.ticket.clone();
        let query = Query::AlbumTracks {
            key: key.clone(),
            query: None,
            offset: 0,
            limit: rivu_protocol::model::MAX_QUERY_ROWS,
        };
        let stamp = query.stamp(self.state.query_revisions(None));
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let result = crate::gui::query::run(
                handle,
                cx,
                move |handle| async move {
                    resolve_album(
                        |query, ticket| {
                            let handle = handle.clone();
                            async move { handle.query_if_current(query, ticket).await }
                        },
                        key,
                        stamp,
                        &ticket,
                    )
                    .await
                },
                |response, cx| {
                    let _ = this.update(cx, |this, cx| {
                        let current = guard.current(&this.state)
                            && album_revision == this.state.library.album_revision;
                        this.timeout_content_resolution(&read, response, current, cx);
                    });
                },
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                if !this.finish_content_resolution(&read, cx) {
                    return;
                }
                match result {
                    Ok(ids)
                        if guard.current(&this.state)
                            && album_revision == this.state.library.album_revision =>
                    {
                        let menu = this.content_menu.as_mut().unwrap();
                        menu.source = TrackSource::Tracks(ids);
                        menu.loading = false;
                        this.content_action(action, cx);
                    }
                    Ok(_) => {
                        this.replace_content_menu(None, cx);
                        this.error = Some(this.i18n.text("li-source-changed").to_owned());
                    }
                    Err(error)
                        if matches!(
                            error.downcast_ref::<rivu_protocol::response::QueryError>(),
                            Some(
                                rivu_protocol::response::QueryError::RevisionChanged
                                    | rivu_protocol::response::QueryError::Cancelled
                            )
                        ) =>
                    {
                        this.replace_content_menu(None, cx);
                    }
                    Err(error) => {
                        this.replace_content_menu(None, cx);
                        this.error = Some(error.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::gui) fn drop_content(
        &mut self,
        source: TrackSource,
        destination: DragDestination,
        cx: &mut Context<Self>,
    ) {
        if !destination.current(&self.state) {
            return;
        }
        match source {
            TrackSource::Tracks(ids) => self.send_content_tracks(ids, destination, cx),
            TrackSource::Album { key, revision } => {
                if revision != self.state.library.album_revision {
                    return;
                }
                let source = rivu_protocol::model::CollectionSource::Album { key, revision };
                self.send_content_collection(source, destination, cx);
            }
        }
    }

    fn send_content_collection(
        &mut self,
        source: rivu_protocol::model::CollectionSource,
        destination: DragDestination,
        cx: &mut Context<Self>,
    ) {
        use rivu_protocol::model::{CollectionDestination, CollectionInsertion};
        if !destination.current(&self.state) {
            return;
        }
        let destination = match destination {
            DragDestination::Queue => CollectionDestination::Queue { insertion: None },
            DragDestination::QueueAt { index, revision } => CollectionDestination::Queue {
                insertion: Some(CollectionInsertion {
                    index: index as u64,
                    revision,
                }),
            },
            DragDestination::Playlist(playlist_id) => {
                let Some(&revision) = self
                    .state
                    .library
                    .playlist_entry_revisions
                    .get(&playlist_id)
                else {
                    return;
                };
                CollectionDestination::Playlist {
                    playlist_id,
                    revision,
                    index: None,
                }
            }
            DragDestination::PlaylistAt {
                playlist_id,
                index,
                revision,
            } => CollectionDestination::Playlist {
                playlist_id,
                revision,
                index: Some(index as u64),
            },
        };
        self.send(
            Command::TransferCollection {
                source,
                destination,
            },
            cx,
        );
    }

    pub(in crate::gui) fn send_content_tracks(
        &mut self,
        track_ids: Vec<i64>,
        destination: DragDestination,
        cx: &mut Context<Self>,
    ) {
        if track_ids.is_empty() || !destination.current(&self.state) {
            return;
        }
        let command = match destination {
            DragDestination::Queue => Command::Enqueue { track_ids },
            DragDestination::Playlist(playlist_id) => {
                if !self
                    .state
                    .library
                    .playlist_entry_revisions
                    .contains_key(&playlist_id)
                {
                    return;
                }
                Command::AddPlaylist {
                    playlist_id,
                    track_ids,
                }
            }
            DragDestination::QueueAt { index, revision } => Command::InsertQueue {
                track_ids,
                index,
                expected_revision: revision,
            },
            DragDestination::PlaylistAt {
                playlist_id,
                index,
                revision,
            } => Command::InsertPlaylist {
                playlist_id,
                track_ids,
                index,
                expected_revision: revision,
            },
        };
        self.send(command, cx);
    }

    fn content_favorite_actions(&self, cx: &App) -> (bool, bool) {
        match self
            .content_menu
            .as_ref()
            .and_then(|menu| menu.remove.as_ref())
        {
            Some(Command::RemoveHistory { track_ids }) => {
                self.history.read(cx).favorite_actions(track_ids)
            }
            _ => (true, true),
        }
    }

    fn content_action(&mut self, action: ContentAction, cx: &mut Context<Self>) {
        self.refresh(cx);
        if let ContentAction::Favorite(favorite) = action {
            let (can_favorite, can_unfavorite) = self.content_favorite_actions(cx);
            if !(if favorite {
                can_favorite
            } else {
                can_unfavorite
            }) {
                return;
            }
        }
        let Some(menu) = self.content_menu.as_ref() else {
            return;
        };
        if menu.loading
            || !menu.guard.current(&self.state)
            || (matches!(menu.source, TrackSource::Album { .. })
                && menu.album_revision != self.state.library.album_revision)
        {
            self.replace_content_menu(None, cx);
            self.error = Some(self.i18n.text("li-source-changed").to_owned());
            cx.notify();
            return;
        }
        if matches!(
            action,
            ContentAction::Favorite(_) | ContentAction::CreatePlaylist(_)
        ) && matches!(menu.source, TrackSource::Album { .. })
        {
            self.resolve_content_action(action, cx);
            return;
        }
        let menu = self.replace_content_menu(None, cx).unwrap();
        match action {
            ContentAction::Play => {
                if let Some(command) = menu.play {
                    if let Command::PlayAlbum { key } = command {
                        self.request_album_play(key, cx);
                    } else {
                        self.send(command, cx);
                    }
                }
            }
            ContentAction::Queue | ContentAction::Playlist(_) => {
                let destination = match action {
                    ContentAction::Playlist(id) => DragDestination::Playlist(id),
                    _ => DragDestination::Queue,
                };
                if let Some(nodes) = menu.nodes {
                    self.library.update(cx, |library, cx| {
                        library.resolve_library_drag(&nodes, destination, cx);
                    });
                } else {
                    match menu.source {
                        TrackSource::Album { key, .. } => self.send_content_collection(
                            rivu_protocol::model::CollectionSource::Album {
                                key,
                                revision: menu.album_revision,
                            },
                            destination,
                            cx,
                        ),
                        TrackSource::Tracks(track_ids) => {
                            self.send_content_tracks(track_ids, destination, cx);
                        }
                    }
                }
            }
            ContentAction::Favorite(favorite) => {
                let TrackSource::Tracks(mut track_ids) = menu.source else {
                    unreachable!("album favorites resolve before dispatch");
                };
                let mut seen = HashSet::new();
                track_ids.retain(|id| seen.insert(*id));
                if !track_ids.is_empty() {
                    self.send(
                        Command::SetFavorite {
                            track_ids,
                            favorite,
                        },
                        cx,
                    );
                }
            }
            ContentAction::CreatePlaylist(window) => {
                let TrackSource::Tracks(track_ids) = menu.source else {
                    unreachable!("album playlist creation resolves before dispatch");
                };
                let playlists = self.playlists.clone();
                cx.defer(move |cx| {
                    let _ = window.update(cx, |_, window, cx| {
                        playlists.update(cx, |playlists, cx| {
                            playlists.begin_playlist_name(
                                playlists::PlaylistNameAction::Create { track_ids },
                                String::new(),
                                window,
                                cx,
                            );
                        });
                    });
                });
            }
            ContentAction::Remove => {
                if let Some(command) = menu.remove {
                    self.send(command, cx);
                }
            }
            ContentAction::RemoveLibraryRoot => {
                if let Some(root) = menu.root {
                    if let Err(error) = self.handle.update_config(
                        rivu_protocol::model::ConfigMutation::RemoveLibraryRoot(root),
                    ) {
                        self.error = Some(format!("{error:#}"));
                    }
                    self.refresh(cx);
                }
            }
        }
        cx.notify();
    }

    pub(in crate::gui) fn back_content_menu(&mut self, cx: &mut Context<Self>) {
        if self
            .content_menu
            .as_ref()
            .is_some_and(|menu| menu.playlists || menu.confirm_remove.is_some())
        {
            self.set_content_menu_playlists(false, cx);
            if let Some(menu) = &mut self.content_menu {
                menu.confirm_remove = None;
            }
            cx.notify();
        } else {
            self.replace_content_menu(None, cx);
        }
    }

    fn content_playlist_key(
        &mut self,
        focus: &FocusHandle,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        if event.keystroke.modifiers.modified()
            || !matches!(
                key,
                "up" | "down" | "pageup" | "pagedown" | "home" | "end" | "enter" | "space"
            )
        {
            return;
        }
        self.refresh(cx);
        let Some(menu) = self
            .content_menu
            .as_mut()
            .filter(|menu| menu.playlists && menu.playlist_targets.focus == *focus)
        else {
            return;
        };
        window.prevent_default();
        cx.stop_propagation();
        let playlists = self.playlists.read(cx);
        let targets = &mut menu.playlist_targets;
        if matches!(key, "enter" | "space") {
            if let Some(id) = targets.candidate(playlists) {
                self.content_action(ContentAction::Playlist(id), cx);
            }
            return;
        }
        let count = playlists.catalog_len();
        let index = targets.selected.get().0.min(count.saturating_sub(1));
        if count == 0 || (key == "up" && index == 0) || (key == "down" && index + 1 == count) {
            self.shell.scope().advance(key == "up", window, cx);
            return;
        }
        let next = match key {
            "up" => index - 1,
            "down" => index + 1,
            "pageup" | "pagedown" => {
                let height = targets.scroll.0.borrow().base_handle.bounds().size.height;
                let row_height = rivu_ui::menu_row_height(window, cx);
                let visible = ((f32::from(height) / f32::from(row_height)).floor() as usize).max(1);
                if key == "pagedown" {
                    index.saturating_add(visible).min(count - 1)
                } else {
                    index.saturating_sub(visible)
                }
            }
            "home" => 0,
            "end" => count - 1,
            _ => unreachable!(),
        };
        targets.select(next, playlists);
        cx.notify();
    }

    fn choose_content_playlist(
        &mut self,
        focus: &FocusHandle,
        index: usize,
        id: i64,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if self
            .content_menu
            .as_ref()
            .is_some_and(|menu| menu.playlists && menu.playlist_targets.focus == *focus)
            && self
                .playlists
                .read(cx)
                .catalog_row(index)
                .is_some_and(|row| row.id == id)
        {
            self.content_action(ContentAction::Playlist(id), cx);
        }
    }

    // Rendered controls belong to both this menu instance and this page. Async
    // collection completion deliberately uses content_action directly instead.
    fn content_menu_callback(
        state: &ContentMenu,
        callback: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static {
        let focus = state.playlist_targets.focus.clone();
        let page = state.keyboard_page();
        let remove_target = state.confirm_remove;
        move |this, window, cx| {
            if this.content_menu.as_ref().is_some_and(|menu| {
                menu.playlist_targets.focus == focus
                    && menu.keyboard_page() == page
                    && menu.confirm_remove == remove_target
            }) {
                callback(this, window, cx);
            }
        }
    }

    pub(in crate::gui) fn content_menu_overlay(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let Some(state) = self.content_menu.as_ref() else {
            return div().into_any_element();
        };
        let dismiss = Self::content_menu_callback(state, |this, _, cx| {
            this.replace_content_menu(None, cx);
            cx.stop_propagation();
            cx.notify();
        });
        let mut menu = Popover::menu(theme, "content-menu", window)
            .map(|surface| self.menu_surface(surface, cx))
            .on_mouse_down_out(cx.listener(move |this, _, window, cx| {
                dismiss(this, window, cx);
            }));
        if state.loading {
            menu = menu.child(caption(theme, self.tr("shell-loading")));
        } else if let Some(target) = state.confirm_remove {
            let (description, label, action) = match target {
                RemoveTarget::Selection => (
                    "li-confirm-remove",
                    state.remove_label,
                    ContentAction::Remove,
                ),
                RemoveTarget::LibraryRoot => (
                    "li-confirm-remove-root",
                    "li-remove-root-confirm",
                    ContentAction::RemoveLibraryRoot,
                ),
            };
            let description = if target == RemoveTarget::Selection
                && let Some(Command::RemoveTracks { track_ids }) = &state.remove
            {
                self.i18n.format(
                    "li-confirm-remove-library",
                    &[("count", track_ids.len().into())],
                )
            } else {
                self.i18n.text(description).to_owned()
            };
            menu = menu
                .child(caption(theme, description))
                .child(menu_item(
                    "content-cancel-remove",
                    self.tr("action-cancel"),
                    cx,
                    Self::content_menu_callback(state, |this, _, cx| {
                        this.replace_content_menu(None, cx);
                        cx.notify();
                    }),
                ))
                .child(rivu_ui::danger_menu_item(
                    theme,
                    menu_item(
                        "content-confirm-remove",
                        self.tr(label),
                        cx,
                        Self::content_menu_callback(state, move |this, _, cx| {
                            this.content_action(action, cx)
                        }),
                    )
                    .debug_selector(|| "content-confirm-remove".into()),
                ));
        } else if state.playlists {
            menu = menu.child(rivu_ui::menu_title(theme, self.tr("li-playlist")));
            if state.tracks_only {
                menu = menu.child(menu_item(
                    "content-new-playlist",
                    self.tr("playlist-create"),
                    cx,
                    Self::content_menu_callback(state, |this, window, cx| {
                        this.content_action(
                            ContentAction::CreatePlaylist(window.window_handle()),
                            cx,
                        );
                    }),
                ));
            }
            let count = self.playlists.read(cx).catalog_len();
            let row_height = rivu_ui::menu_row_height(window, cx);
            let height = (row_height * count.max(1) as f32)
                .min(window.rem_size() * POPOVER_MAX_REMS)
                .min(window.viewport_size().height * 0.5);
            let target_focus = state.playlist_targets.focus.clone();
            let render_focus = target_focus.clone();
            let demand_focus = target_focus.clone();
            menu = menu
                .child(
                    div()
                        .id("content-playlist-navigation")
                        .role(gpui::Role::Menu)
                        .aria_label(self.tr("li-playlist"))
                        .tab_index(0)
                        .tab_stop(count > 0)
                        .track_focus(&state.playlist_targets.focus)
                        .capture_key_down(cx.listener(move |this, event, window, cx| {
                            this.content_playlist_key(&target_focus, event, window, cx);
                        }))
                        .child(
                            rivu_ui::VirtualList::new(
                                "content-playlist-targets",
                                count,
                                &state.playlist_targets.scroll,
                                cx.processor(
                                    move |this, range: std::ops::Range<usize>, _window, cx| {
                                        range
                                            .map(|index| {
                                                let Some((id, name)) =
                                                    this.playlists.read(cx).catalog_row(index).map(
                                                        |playlist| {
                                                            (
                                                                playlist.id,
                                                                SharedString::from(
                                                                    playlist.name.clone(),
                                                                ),
                                                            )
                                                        },
                                                    )
                                                else {
                                                    return this.playlists.update(
                                                        cx,
                                                        |playlists, cx| {
                                                            playlists.catalog_loading_row(
                                                                index, row_height, cx,
                                                            )
                                                        },
                                                    );
                                                };
                                                let selected = this
                                                    .content_menu
                                                    .as_ref()
                                                    .is_some_and(|menu| {
                                                        menu.playlist_targets.focus == render_focus
                                                            && menu
                                                                .playlist_targets
                                                                .rendered_selection(
                                                                    index,
                                                                    id,
                                                                    this.playlists.read(cx),
                                                                )
                                                    });
                                                let focus = render_focus.clone();
                                                rivu_ui::activate_control(
                                                    rivu_ui::menu_item_style(
                                                        rivu_ui::theme::palette(cx),
                                                        ("content-playlist-target", id as u64),
                                                        name.clone(),
                                                        div().min_w_0().truncate().child(name),
                                                        cx,
                                                    ),
                                                    cx,
                                                    move |this, _, cx| {
                                                        this.choose_content_playlist(
                                                            &focus, index, id, cx,
                                                        )
                                                    },
                                                )
                                                .tab_stop(false)
                                                .aria_selected(selected)
                                                .aria_position_in_set(index + 1)
                                                .aria_size_of_set(count)
                                                .when(selected, |row| {
                                                    row.bg(
                                                        rgb(rivu_ui::theme::palette(cx).selected),
                                                    )
                                                })
                                                .debug_selector(move || {
                                                    format!("content-playlist-target-{id}")
                                                })
                                                .h(row_height)
                                            })
                                            .collect::<Vec<_>>()
                                    },
                                ),
                            )
                            .on_viewport_deferred(cx.weak_entity(), move |this, range, cx| {
                                if this.content_menu.as_ref().is_some_and(|menu| {
                                    menu.playlists && menu.playlist_targets.focus == demand_focus
                                }) {
                                    this.playlists.update(cx, |playlists, cx| {
                                        playlists.ensure_playlist_range(range, cx);
                                    });
                                }
                            })
                            .h(height)
                            .w_full(),
                        ),
                )
                .child(rivu_ui::menu_back(
                    "content-back",
                    self.tr("shell-back"),
                    cx,
                    Self::content_menu_callback(state, |this, _, cx| {
                        this.set_content_menu_playlists(false, cx);
                    }),
                ));
        } else {
            if state.play.is_some() {
                menu = menu.child(menu_item(
                    "content-play",
                    self.tr("playback-play"),
                    cx,
                    Self::content_menu_callback(state, |this, _, cx| {
                        this.content_action(ContentAction::Play, cx)
                    }),
                ));
            }
            menu = menu
                .child(
                    menu_item(
                        "content-queue",
                        self.tr("li-queue"),
                        cx,
                        Self::content_menu_callback(state, |this, _, cx| {
                            this.content_action(ContentAction::Queue, cx)
                        }),
                    )
                    .debug_selector(|| "content-queue".into()),
                )
                .child(
                    menu_item(
                        "content-playlist",
                        self.tr("li-playlist"),
                        cx,
                        Self::content_menu_callback(state, |this, _, cx| {
                            this.set_content_menu_playlists(true, cx);
                        }),
                    )
                    .track_focus(&state.playlist_trigger)
                    .tab_stop(true),
                );
            let remove_history = matches!(&state.remove, Some(Command::RemoveHistory { .. }));
            if remove_history {
                menu = menu.child(menu_item(
                    "content-remove",
                    self.tr(state.remove_label),
                    cx,
                    Self::content_menu_callback(state, |this, _, cx| {
                        this.content_action(ContentAction::Remove, cx)
                    }),
                ));
            }
            if state.tracks_only {
                let (can_favorite, can_unfavorite) = self.content_favorite_actions(cx);
                menu = menu
                    .child(rivu_ui::menu_separator(theme))
                    .child(rivu_ui::disabled_control(
                        theme,
                        menu_item(
                            "content-favorite",
                            self.tr("li-favorite"),
                            cx,
                            Self::content_menu_callback(state, |this, _, cx| {
                                this.content_action(ContentAction::Favorite(true), cx)
                            }),
                        ),
                        !can_favorite,
                    ))
                    .child(rivu_ui::disabled_control(
                        theme,
                        menu_item(
                            "content-unfavorite",
                            self.tr("li-unfavorite"),
                            cx,
                            Self::content_menu_callback(state, |this, _, cx| {
                                this.content_action(ContentAction::Favorite(false), cx)
                            }),
                        ),
                        !can_unfavorite,
                    ));
            }
            if state.remove.is_some() && !remove_history {
                let item = menu_item(
                    "content-remove",
                    self.tr(state.remove_label),
                    cx,
                    Self::content_menu_callback(state, |this, _, cx| {
                        if let Some(menu) = &mut this.content_menu {
                            menu.confirm_remove = Some(RemoveTarget::Selection);
                        }
                        cx.notify();
                    }),
                )
                .debug_selector(|| "content-remove".into());
                menu = menu
                    .child(rivu_ui::menu_separator(theme))
                    .child(rivu_ui::danger_menu_item(theme, item));
            }
            if state.root.is_some() {
                menu = menu
                    .child(rivu_ui::menu_separator(theme))
                    .child(rivu_ui::danger_menu_item(
                        theme,
                        menu_item(
                            "content-remove-root",
                            self.tr("li-remove-root"),
                            cx,
                            Self::content_menu_callback(state, |this, _, cx| {
                                if let Some(menu) = &mut this.content_menu {
                                    menu.confirm_remove = Some(RemoveTarget::LibraryRoot);
                                }
                                cx.notify();
                            }),
                        ),
                    ));
            }
        }
        anchored()
            .position(state.position)
            .snap_to_window()
            .child(menu)
            .into_any_element()
    }
}

impl WindowView {
    pub(in crate::gui) fn append_playlist_to_queue(
        &mut self,
        playlist_id: i64,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        let Some(revision) = self
            .state
            .library
            .playlist_entry_revisions
            .get(&playlist_id)
            .copied()
        else {
            return;
        };
        self.send_content_collection(
            rivu_protocol::model::CollectionSource::Playlist {
                playlist_id,
                revision,
            },
            DragDestination::Queue,
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        gui::layout::persistence::{LayoutPersistence, LoadedLayouts},
        test_support::{ConfigFixture, CoreRuntime},
    };
    use core::prelude::v1::test;
    use rivu_protocol::{model::AlbumKey, response::ViewResponse};
    use std::sync::Arc;

    struct Fixture {
        app: Entity<WindowView>,
        window: AnyWindowHandle,
        _save: LayoutPersistence,
        runtime: CoreRuntime,
        _directory: tempfile::TempDir,
        album: AlbumKey,
        other: AlbumKey,
        member: i64,
    }

    fn fixture(cx: &mut TestAppContext) -> Fixture {
        cx.executor().allow_parking();
        cx.update(rivu_ui::input::Input::init);
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        rivu_format::config::Config {
            volume: 0.0,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            ..Default::default()
        }
        .save_fixture(&config)
        .unwrap();
        let album = AlbumKey {
            directory: directory.path().join("album"),
            title: "Album".into(),
        };
        let other = AlbumKey {
            directory: directory.path().join("other"),
            title: album.title.clone(),
        };
        let paths = [
            album.directory.join("one.wav"),
            album.directory.join("two.wav"),
            album.directory.join("three.wav"),
            other.directory.join("outside.wav"),
        ];
        let catalog =
            rivu::test_support::catalog::CatalogFixture::open(&directory.path().join("library.db"))
                .unwrap();
        catalog
            .seed_tracks(
                directory.path().to_owned(),
                paths
                    .iter()
                    .map(|path| (path.clone(), "Track".into()))
                    .collect(),
            )
            .unwrap();
        let ids = paths.map(|path| catalog.track_id(&path).unwrap().unwrap());
        drop(catalog);
        let runtime = CoreRuntime::start(directory.path(), &config).unwrap();
        for track_id in ids {
            let ack = futures::executor::block_on(runtime.handle.request_ack(Command::EditTrack {
                track_id,
                title: None,
                artist: None,
                album: Some(album.title.clone()),
                decoder_hint: None,
            }))
            .ack;
            assert!(ack.is_success(), "{:?}", ack.error);
        }
        let ack =
            futures::executor::block_on(runtime.handle.request_ack(Command::CreatePlaylist {
                name: "Target".into(),
            }))
            .ack;
        assert!(ack.is_success(), "{:?}", ack.error);
        let save =
            LayoutPersistence::new(LoadedLayouts::load(directory.path().join("layouts.json")));
        let save_handle = save.handle();
        let handle = runtime.handle.clone();
        let artwork = Arc::new(rivu_metadata::artwork::ArtworkManager::new(
            directory.path(),
        ));
        let window = cx.add_window(move |window, cx| {
            let mut app = WindowView::new(
                handle,
                save_handle,
                artwork,
                Default::default(),
                Arc::new(rivu_presentation::i18n::Localizer::new("en-US")),
                window,
                cx,
            );
            // Keep real Core wakeups off GPUI's deterministic scheduler. Tests
            // enable the window only when an explicitly chosen read must finish.
            app.handle.set_wakeup(|| {});
            app.window_visible = false;
            app.reconcile_feature_demand(cx);
            app
        });
        let app = window.root(cx).unwrap();
        Fixture {
            _directory: directory,
            runtime,
            _save: save,
            app,
            window: window.into(),
            album,
            other,
            member: ids[0],
        }
    }

    fn open_album(app: &mut WindowView, album: &AlbumKey, cx: &mut Context<WindowView>) {
        app.refresh(cx);
        app.open_content_menu(
            ContentMenuRequest {
                source: TrackSource::Album {
                    key: album.clone(),
                    revision: app.state.library.album_revision,
                },
                position: point(px(10.), px(10.)),
                play: Some(Command::PlayAlbum { key: album.clone() }),
            },
            cx,
        );
        let menu = app.content_menu.as_ref().unwrap();
        assert!(!menu.loading);
        assert!(matches!(&menu.source, TrackSource::Album { key, .. } if key == album));
        assert_eq!(app.content_favorite_actions(cx), (true, true));
    }

    fn favorites(runtime: &CoreRuntime, album: &AlbumKey) -> Vec<bool> {
        // Commands and queries have independent queues. Observe a harmless
        // command after the action before asserting its real catalog effects.
        let ack =
            futures::executor::block_on(runtime.handle.request_ack(Command::Volume { value: 0.0 }))
                .ack;
        assert!(ack.is_success(), "{:?}", ack.error);
        let ViewResponse::AlbumTracks(page) = runtime
            .handle
            .query(Query::AlbumTracks {
                key: album.clone(),
                query: None,
                offset: 0,
                limit: rivu_protocol::model::MAX_QUERY_ROWS,
            })
            .result
            .unwrap()
        else {
            panic!("expected album tracks");
        };
        assert_eq!(page.total, page.rows.len());
        page.rows.into_iter().map(|row| row.favorite).collect()
    }

    struct MenuWindow(Entity<WindowView>);

    impl Render for MenuWindow {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().relative().size_full().child(
                self.0
                    .update(cx, |app, cx| app.content_menu_overlay(window, cx)),
            )
        }
    }

    fn overlay_fixture(cx: &mut TestAppContext) -> Fixture {
        let fixture = fixture(cx);
        let app = fixture.app.clone();
        cx.update_window(fixture.window, |_, window, cx| {
            window.replace_root(cx, |_, cx| {
                cx.observe(&app, |_, _, cx| cx.notify()).detach();
                MenuWindow(app)
            });
        })
        .unwrap();
        fixture
    }

    fn dispatch_menu_click(window: &mut Window, cx: &mut App, position: Point<Pixels>) {
        window.dispatch_event(
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
    }

    fn change_and_click_menu(
        fixture: &Fixture,
        position: Point<Pixels>,
        change: impl FnOnce(&mut WindowView, &mut Context<WindowView>),
        cx: &mut TestAppContext,
    ) {
        // GPUI test mode draws dirty windows at effect flush. Keep mutation and
        // input within this update so the event actually uses the old frame.
        cx.update_window(fixture.window, |_, window, cx| {
            fixture.app.update(cx, change);
            dispatch_menu_click(window, cx, position);
        })
        .unwrap();
    }

    fn draw_menu(fixture: &Fixture, cx: &mut TestAppContext) {
        cx.run_until_parked();
        cx.update_window(fixture.window, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
    }

    fn click_menu(fixture: &Fixture, position: Point<Pixels>, cx: &mut TestAppContext) {
        cx.update_window(fixture.window, |_, window, cx| {
            dispatch_menu_click(window, cx, position);
        })
        .unwrap();
    }

    fn menu_point(
        fixture: &Fixture,
        selector: &'static str,
        cx: &mut TestAppContext,
    ) -> Point<Pixels> {
        VisualTestContext::from_window(fixture.window, cx)
            .debug_bounds(selector)
            .expect("rendered menu control")
            .center()
    }

    fn open_removable(fixture: &Fixture, cx: &mut TestAppContext) {
        fixture.app.update(cx, |app, cx| {
            app.open_object_menu(
                ContentMenuRequest {
                    source: TrackSource::Tracks(vec![fixture.member]),
                    position: point(px(10.), px(10.)),
                    play: None,
                },
                ContentGuard::None,
                Some(Command::RemoveTracks {
                    track_ids: vec![fixture.member],
                }),
                None,
                None,
                cx,
            );
        });
    }

    #[gpui::test]
    fn replaced_content_menu_rejects_old_mouse_frame_and_accepts_fresh_queue(
        cx: &mut TestAppContext,
    ) {
        let fixture = overlay_fixture(cx);
        fixture
            .app
            .update(cx, |app, cx| open_album(app, &fixture.album, cx));
        draw_menu(&fixture, cx);
        let queue = menu_point(&fixture, "content-queue", cx);
        change_and_click_menu(
            &fixture,
            queue,
            |app, cx| open_album(app, &fixture.other, cx),
            cx,
        );
        fixture.app.read_with(cx, |app, _| {
            assert!(matches!(&app.content_menu.as_ref().unwrap().source,
                TrackSource::Album { key, .. } if key == &fixture.other));
        });
        change_and_click_menu(
            &fixture,
            point(px(900.), px(650.)),
            |app, cx| {
                open_album(app, &fixture.other, cx);
            },
            cx,
        );
        assert!(
            fixture
                .app
                .read_with(cx, |app, _| app.content_menu.is_some())
        );
        draw_menu(&fixture, cx);
        let queue = menu_point(&fixture, "content-queue", cx);
        click_menu(&fixture, queue, cx);
        assert!(
            fixture
                .app
                .read_with(cx, |app, _| app.content_menu.is_none())
        );
        let ack = futures::executor::block_on(
            fixture
                .runtime
                .handle
                .request_ack(Command::Volume { value: 0.0 }),
        )
        .ack;
        assert!(ack.is_success());
        let ViewResponse::QueuePage(page) = fixture
            .runtime
            .handle
            .query(Query::QueuePage {
                query: None,
                offset: 0,
                limit: 10,
            })
            .result
            .unwrap()
        else {
            panic!("queue page")
        };
        assert_eq!(page.total, 1, "only the fresh menu's album is queued");
    }

    #[gpui::test]
    fn replaced_content_confirmation_rejects_old_mouse_and_fresh_remove_still_works(
        cx: &mut TestAppContext,
    ) {
        let fixture = overlay_fixture(cx);
        open_removable(&fixture, cx);
        draw_menu(&fixture, cx);
        let remove = menu_point(&fixture, "content-remove", cx);
        click_menu(&fixture, remove, cx);
        draw_menu(&fixture, cx);
        let confirm = menu_point(&fixture, "content-confirm-remove", cx);
        // Even within one instance, Selection and LibraryRoot are distinct
        // confirmation pages. Keep the Selection frame while changing target.
        change_and_click_menu(
            &fixture,
            confirm,
            |app, cx| {
                app.content_menu.as_mut().unwrap().confirm_remove = Some(RemoveTarget::LibraryRoot);
                cx.notify();
            },
            cx,
        );
        assert!(fixture.app.read_with(cx, |app, _| {
            app.content_menu.as_ref().unwrap().confirm_remove == Some(RemoveTarget::LibraryRoot)
        }));
        assert_eq!(favorites(&fixture.runtime, &fixture.album).len(), 3);
        change_and_click_menu(
            &fixture,
            confirm,
            |app, cx| {
                app.open_object_menu(
                    ContentMenuRequest {
                        source: TrackSource::Tracks(vec![fixture.member]),
                        position: point(px(10.), px(10.)),
                        play: None,
                    },
                    ContentGuard::None,
                    Some(Command::RemoveTracks {
                        track_ids: vec![fixture.member],
                    }),
                    None,
                    None,
                    cx,
                );
            },
            cx,
        );
        fixture.app.read_with(cx, |app, _| {
            assert!(app.content_menu.as_ref().unwrap().confirm_remove.is_none());
        });
        assert_eq!(favorites(&fixture.runtime, &fixture.album).len(), 3);
        draw_menu(&fixture, cx);
        let remove = menu_point(&fixture, "content-remove", cx);
        let queue = menu_point(&fixture, "content-queue", cx);
        cx.update_window(fixture.window, |_, window, cx| {
            dispatch_menu_click(window, cx, remove);
            // The retained main-page frame cannot enqueue from confirmation.
            dispatch_menu_click(window, cx, queue);
        })
        .unwrap();
        assert!(fixture.app.read_with(cx, |app, _| {
            app.content_menu.as_ref().unwrap().confirm_remove.is_some()
        }));
        draw_menu(&fixture, cx);
        let confirm = menu_point(&fixture, "content-confirm-remove", cx);
        click_menu(&fixture, confirm, cx);
        assert!(
            fixture
                .app
                .read_with(cx, |app, _| app.content_menu.is_none())
        );
        assert_eq!(favorites(&fixture.runtime, &fixture.album).len(), 2);
    }

    #[gpui::test]
    fn album_drop_rejects_stale_origin_and_transfers_current_full_album(cx: &mut TestAppContext) {
        let fixture = fixture(cx);
        let queue_len = || {
            let ack = futures::executor::block_on(
                fixture
                    .runtime
                    .handle
                    .request_ack(Command::Volume { value: 0.0 }),
            )
            .ack;
            assert!(ack.is_success(), "{:?}", ack.error);
            let ViewResponse::QueuePage(page) = fixture
                .runtime
                .handle
                .query(Query::QueuePage {
                    query: None,
                    offset: 0,
                    limit: rivu_protocol::model::MAX_QUERY_ROWS,
                })
                .result
                .unwrap()
            else {
                panic!("expected queue page");
            };
            page.total
        };
        let revision = fixture.app.update(cx, |app, cx| {
            app.refresh(cx);
            app.state.library.album_revision
        });
        let ack =
            futures::executor::block_on(fixture.runtime.handle.request_ack(Command::EditTrack {
                track_id: fixture.member,
                title: Some("Changed during drag".into()),
                artist: None,
                album: None,
                decoder_hint: None,
            }))
            .ack;
        assert!(ack.is_success(), "{:?}", ack.error);
        fixture.app.update(cx, |app, cx| {
            app.refresh(cx);
            assert_ne!(app.state.library.album_revision, revision);
            app.drop_content(
                TrackSource::Album {
                    key: fixture.album.clone(),
                    revision,
                },
                DragDestination::Queue,
                cx,
            );
        });
        assert_eq!(queue_len(), 0);
        fixture.app.update(cx, |app, cx| {
            app.refresh(cx);
            app.drop_content(
                TrackSource::Album {
                    key: fixture.album.clone(),
                    revision: app.state.library.album_revision,
                },
                DragDestination::Queue,
                cx,
            );
        });
        assert_eq!(queue_len(), 3);
    }

    #[gpui::test]
    fn album_open_dismiss_and_scoped_actions_do_not_claim_a_collection_worker(
        cx: &mut TestAppContext,
    ) {
        let fixture = fixture(cx);
        fixture.app.update(cx, |app, cx| {
            open_album(app, &fixture.album, cx);
            assert!(!app.content_resolution.pending());
            app.back_content_menu(cx);
            assert!(app.content_menu.is_none());
            assert!(!app.content_resolution.pending());

            // Even a cancelled read that has not physically retired must not
            // block opening a menu or dispatching its album-scoped commands.
            let retired = app.content_resolution.begin().unwrap();
            for action in [
                ContentAction::Play,
                ContentAction::Queue,
                ContentAction::Playlist(1),
            ] {
                open_album(app, &fixture.album, cx);
                assert!(!retired.ticket.is_current());
                app.content_action(action, cx);
                assert!(app.content_menu.is_none());
                assert!(app.content_resolution.pending());
            }
            assert!(app.content_resolution.finish(&retired));
        });
    }

    #[gpui::test]
    async fn album_favorites_resolve_on_selection_and_preserve_full_album_scope(
        cx: &mut TestAppContext,
    ) {
        let fixture = fixture(cx);
        for favorite in [true, false] {
            fixture.app.update(cx, |app, cx| {
                open_album(app, &fixture.album, cx);
                assert!(!app.content_resolution.pending());
                app.window_visible = true;
                app.content_action(ContentAction::Favorite(favorite), cx);
                assert!(app.content_resolution.pending());
                assert!(app.content_menu.as_ref().unwrap().loading);
            });
            cx.condition(&fixture.app, |app, _| {
                !app.content_resolution.pending() && app.content_menu.is_none()
            })
            .await;
            assert_eq!(favorites(&fixture.runtime, &fixture.album), [favorite; 3]);
            assert_eq!(favorites(&fixture.runtime, &fixture.other), [false]);
        }
    }

    #[gpui::test]
    async fn creating_a_playlist_from_an_album_resolves_only_after_selection(
        cx: &mut TestAppContext,
    ) {
        let fixture = fixture(cx);
        fixture.app.update(cx, |app, cx| {
            open_album(app, &fixture.album, cx);
            app.set_content_menu_playlists(true, cx);
            assert!(!app.content_resolution.pending());
            app.window_visible = true;
            app.content_action(ContentAction::CreatePlaylist(fixture.window), cx);
            assert!(app.content_resolution.pending());
            assert!(app.content_menu.as_ref().unwrap().loading);
        });
        cx.condition(&fixture.app, |app, cx| {
            !app.content_resolution.pending()
                && app.content_menu.is_none()
                && app.playlists.read(cx).editor_open()
        })
        .await;
        assert_eq!(favorites(&fixture.runtime, &fixture.album), [false; 3]);
    }

    #[gpui::test]
    async fn dismissing_chosen_album_id_actions_cancels_without_early_worker_retirement(
        cx: &mut TestAppContext,
    ) {
        let fixture = fixture(cx);
        for action in [
            ContentAction::Favorite(true),
            ContentAction::CreatePlaylist(fixture.window),
        ] {
            fixture.app.update(cx, |app, cx| {
                open_album(app, &fixture.album, cx);
                app.window_visible = true;
                app.set_content_menu_playlists(true, cx);
                app.content_action(action, cx);
                assert!(app.content_resolution.pending());
                assert!(app.content_menu.as_ref().unwrap().loading);
                app.back_content_menu(cx);
                assert!(app.content_menu.is_none());
                assert!(app.content_resolution.pending());
            });
            cx.condition(&fixture.app, |app, _| !app.content_resolution.pending())
                .await;
            assert_eq!(favorites(&fixture.runtime, &fixture.album), [false; 3]);
            assert!(
                !fixture
                    .app
                    .read_with(cx, |app, cx| app.playlists.read(cx).editor_open())
            );
        }
    }

    #[gpui::test]
    fn loading_album_actions_keep_keyboard_cancellation_until_worker_retirement(
        cx: &mut TestAppContext,
    ) {
        fn draw(handle: AnyWindowHandle, cx: &mut TestAppContext) {
            cx.run_until_parked();
            cx.update_window(handle, |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            })
            .unwrap();
            cx.run_until_parked();
            cx.update_window(handle, |_, window, cx| {
                window.simulate_next_frame(cx);
                window.refresh();
                window.draw(cx).clear(cx);
            })
            .unwrap();
            cx.run_until_parked();
        }

        let fixture = fixture(cx);
        for key in ["escape", "left"] {
            fixture.app.update(cx, |app, cx| {
                open_album(app, &fixture.album, cx);
                app.set_content_menu_playlists(true, cx);
            });
            draw(fixture.window, cx);
            // Hold the physical reader deterministically, without racing a real
            // AlbumTracks response against the loading page's deferred focus.
            let read = fixture.app.update(cx, |app, cx| {
                let read = app.begin_content_resolution(cx).unwrap();
                app.set_content_menu_playlists(false, cx);
                app.content_menu.as_mut().unwrap().loading = true;
                cx.notify();
                read
            });
            draw(fixture.window, cx);
            cx.update_window(fixture.window, |_, window, cx| {
                let keystroke = Keystroke::parse(key).unwrap();
                window.dispatch_event(
                    KeyDownEvent {
                        keystroke: keystroke.clone(),
                        is_held: false,
                        prefer_character_input: false,
                    }
                    .to_platform_input(),
                    cx,
                );
                window.dispatch_event(KeyUpEvent { keystroke }.to_platform_input(), cx);
            })
            .unwrap();
            fixture.app.update(cx, |app, cx| {
                assert!(app.content_menu.is_none(), "{key} must dismiss loading");
                assert!(!read.ticket.is_current());
                assert!(app.content_resolution.pending());
                assert!(!app.finish_content_resolution(&read, cx));
                assert!(!app.content_resolution.pending());
                assert!(!app.playlists.read(cx).editor_open());
            });
        }
    }

    #[gpui::test]
    fn stale_album_favorite_action_rejects_without_resolving_or_mutating(cx: &mut TestAppContext) {
        let fixture = fixture(cx);
        fixture.app.update(cx, |app, cx| {
            open_album(app, &fixture.album, cx);
        });
        let ack =
            futures::executor::block_on(fixture.runtime.handle.request_ack(Command::EditTrack {
                track_id: fixture.member,
                title: None,
                artist: None,
                album: Some("Changed".into()),
                decoder_hint: None,
            }))
            .ack;
        assert!(ack.is_success(), "{:?}", ack.error);
        fixture.app.update(cx, |app, cx| {
            app.content_action(ContentAction::Favorite(true), cx);
            assert!(app.content_menu.is_none());
            assert!(!app.content_resolution.pending());
            assert_eq!(
                app.error.as_deref(),
                Some(app.i18n.text("li-source-changed"))
            );
        });
        assert_eq!(favorites(&fixture.runtime, &fixture.album), [false; 2]);
        assert_eq!(favorites(&fixture.runtime, &fixture.other), [false]);
    }
}
