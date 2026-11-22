//! Window-owned content menus.
use crate::gui::{
    library::LibraryNode,
    list_interaction::{ContentGuard, ContentMenuRequest, DragDestination, TrackSource},
    playlists,
    window::WindowView,
};
use gpui::{prelude::*, *};
use rivu_protocol::model::Command;
use rivu_ui::{POPOVER_MAX_REMS, Popover, caption, menu_item};
use std::{collections::HashSet, path::PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::gui) enum RemoveTarget {
    Selection,
    LibraryRoot,
}

pub(in crate::gui) struct ContentMenu {
    source: TrackSource,
    nodes: Option<Vec<LibraryNode>>,
    tracks_only: bool,
    guard: ContentGuard,
    position: Point<Pixels>,
    play: Option<Command>,
    remove: Option<Command>,
    remove_label: &'static str,
    root: Option<PathBuf>,
    page: ContentMenuPage,
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::gui) enum ContentMenuPage {
    Actions,
    Playlists,
    ConfirmRemove(RemoveTarget),
}

impl ContentMenu {
    pub(in crate::gui) fn keyboard_page(&self) -> ContentMenuPage {
        self.page
    }

    pub(in crate::gui) fn playlist_picker_open(&self) -> bool {
        self.page == ContentMenuPage::Playlists
    }

    pub(in crate::gui) fn focus_playlist_return(&mut self, window: &mut Window, cx: &mut App) {
        if std::mem::take(&mut self.restore_playlist_trigger)
            && self.page == ContentMenuPage::Actions
        {
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
    pub(in crate::gui) fn replace_content_menu(
        &mut self,
        menu: Option<ContentMenu>,
        cx: &mut Context<Self>,
    ) -> Option<ContentMenu> {
        let previous = std::mem::replace(&mut self.content_menu, menu);
        if previous
            .as_ref()
            .is_some_and(ContentMenu::playlist_picker_open)
            != self
                .content_menu
                .as_ref()
                .is_some_and(ContentMenu::playlist_picker_open)
        {
            self.reconcile_feature_demand(cx);
        }
        if previous.is_some() || self.content_menu.is_some() {
            cx.notify();
        }
        previous
    }

    fn set_content_menu_page(&mut self, page: ContentMenuPage, cx: &mut Context<Self>) {
        if let Some(menu) = &mut self.content_menu
            && menu.page != page
        {
            let was_picking = menu.playlist_picker_open();
            menu.page = page;
            let picking = menu.playlist_picker_open();
            if was_picking != picking {
                menu.restore_playlist_trigger = !picking;
                if picking {
                    menu.playlist_targets.select(0, self.playlists.read(cx));
                    menu.playlist_targets
                        .scroll
                        .scroll_to_item_strict(0, ScrollStrategy::Top);
                }
                self.reconcile_feature_demand(cx);
            }
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
        let menu = Some(ContentMenu {
            source: request.source,
            nodes,
            tracks_only,
            guard,
            position: request.position,
            play: request.play,
            remove,
            remove_label,
            root,
            page: ContentMenuPage::Actions,
            playlist_targets: PlaylistTargets::new(cx),
            playlist_trigger: cx.focus_handle(),
            restore_playlist_trigger: false,
        });
        self.replace_content_menu(menu, cx);
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
        // These album mutations carry the original revision to Core, which
        // checks it at execution time and reports how to recover if stale.
        let album_mutation = matches!(menu.source, TrackSource::Album { .. })
            && matches!(
                action,
                ContentAction::Favorite(_) | ContentAction::CreatePlaylist(_)
            );
        if !album_mutation
            && (!menu.guard.current(&self.state)
                || matches!(menu.source, TrackSource::Album { revision, .. }
                    if revision != self.state.library.album_revision))
        {
            self.replace_content_menu(None, cx);
            self.error = Some(self.i18n.text("li-source-changed").to_owned());
            cx.notify();
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
                        TrackSource::Album { key, revision } => self.send_content_collection(
                            rivu_protocol::model::CollectionSource::Album { key, revision },
                            destination,
                            cx,
                        ),
                        TrackSource::Tracks(track_ids) => {
                            self.send_content_tracks(track_ids, destination, cx);
                        }
                    }
                }
            }
            ContentAction::Favorite(favorite) => match menu.source {
                TrackSource::Album { key, revision } => {
                    self.send(
                        Command::SetAlbumFavorite {
                            key,
                            revision,
                            favorite,
                        },
                        cx,
                    );
                }
                TrackSource::Tracks(mut track_ids) => {
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
            },
            ContentAction::CreatePlaylist(window) => {
                let action = match menu.source {
                    TrackSource::Tracks(track_ids) => {
                        playlists::PlaylistNameAction::Create { track_ids }
                    }
                    TrackSource::Album { key, revision } => {
                        playlists::PlaylistNameAction::CreateFromAlbum { key, revision }
                    }
                };
                let playlists = self.playlists.clone();
                cx.defer(move |cx| {
                    let _ = window.update(cx, |_, window, cx| {
                        playlists.update(cx, |playlists, cx| {
                            playlists.begin_playlist_name(action, String::new(), window, cx);
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
        match self.content_menu.as_ref().map(|menu| menu.page) {
            Some(ContentMenuPage::Playlists | ContentMenuPage::ConfirmRemove(_)) => {
                self.set_content_menu_page(ContentMenuPage::Actions, cx);
            }
            _ => {
                self.replace_content_menu(None, cx);
            }
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
            .filter(|menu| menu.playlist_picker_open() && menu.playlist_targets.focus == *focus)
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
        if self.content_menu.as_ref().is_some_and(|menu| {
            menu.playlist_picker_open() && menu.playlist_targets.focus == *focus
        }) && self
            .playlists
            .read(cx)
            .catalog_row(index)
            .is_some_and(|row| row.id == id)
        {
            self.content_action(ContentAction::Playlist(id), cx);
        }
    }

    // Rendered controls belong to both this menu instance and this page.
    fn content_menu_callback(
        state: &ContentMenu,
        callback: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static {
        let focus = state.playlist_targets.focus.clone();
        let page = state.page;
        move |this, window, cx| {
            if this
                .content_menu
                .as_ref()
                .is_some_and(|menu| menu.playlist_targets.focus == focus && menu.page == page)
            {
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
        });
        let mut menu = Popover::menu(theme, "content-menu", window)
            .map(|surface| self.menu_surface(surface, cx))
            .on_mouse_down_out(cx.listener(move |this, _, window, cx| {
                dismiss(this, window, cx);
            }));
        if let ContentMenuPage::ConfirmRemove(target) = state.page {
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
        } else if state.playlist_picker_open() {
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
                                        let now = cx.background_executor().now();
                                        range
                                            .map(|index| {
                                                let Some((id, name)) = this
                                                    .playlists
                                                    .read(cx)
                                                    .catalog_display_row(index, now)
                                                    .map(|playlist| {
                                                        (
                                                            playlist.id,
                                                            SharedString::from(
                                                                playlist.name.clone(),
                                                            ),
                                                        )
                                                    })
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
                                                let row = rivu_ui::menu_item_style(
                                                    rivu_ui::theme::palette(cx),
                                                    ("content-playlist-target", id as u64),
                                                    name.clone(),
                                                    div().min_w_0().truncate().child(
                                                        rivu_ui::truncated_label(
                                                            "content-playlist-target-label",
                                                            name,
                                                        ),
                                                    ),
                                                    cx,
                                                );
                                                if this
                                                    .playlists
                                                    .read(cx)
                                                    .catalog_row(index)
                                                    .is_none()
                                                {
                                                    return rivu_ui::disabled_control(
                                                        rivu_ui::theme::palette(cx),
                                                        row,
                                                        true,
                                                    )
                                                    .h(row_height);
                                                }
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
                                                    row,
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
                                    menu.playlist_picker_open()
                                        && menu.playlist_targets.focus == demand_focus
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
                        this.back_content_menu(cx);
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
                            this.set_content_menu_page(ContentMenuPage::Playlists, cx);
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
                        this.set_content_menu_page(
                            ContentMenuPage::ConfirmRemove(RemoveTarget::Selection),
                            cx,
                        );
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
                                this.set_content_menu_page(
                                    ContentMenuPage::ConfirmRemove(RemoveTarget::LibraryRoot),
                                    cx,
                                );
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
        test_support::{CoreRuntime, save_quiet_config},
    };
    use core::prelude::v1::test;
    use rivu_protocol::{
        model::{AlbumKey, Query},
        response::ViewResponse,
    };
    use std::sync::Arc;

    struct Fixture {
        app: Entity<WindowView>,
        window: AnyWindowHandle,
        _save: LayoutPersistence,
        runtime: CoreRuntime,
        _directory: tempfile::TempDir,
        album: AlbumKey,
        other: AlbumKey,
        ids: [i64; 4],
    }

    fn fixture(cx: &mut TestAppContext) -> Fixture {
        cx.executor().allow_parking();
        cx.update(rivu_ui::input::Input::init);
        let directory = tempfile::tempdir().unwrap();
        let config = save_quiet_config(directory.path());
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
        // Album order must differ from insertion order for the playlist check.
        for (track_id, title) in ids.into_iter().zip(["B", "C", "A", "Outside"]) {
            let ack =
                futures::executor::block_on(runtime.handle.request_command(Command::EditTrack {
                    track_id,
                    title: Some(title.into()),
                    artist: None,
                    album: Some(album.title.clone()),
                    decoder_hint: None,
                    release_date: None,
                }));
            assert!(ack.is_success(), "{:?}", ack.error());
        }
        let ack =
            futures::executor::block_on(runtime.handle.request_command(Command::CreatePlaylist {
                name: "Target".into(),
            }));
        assert!(ack.is_success(), "{:?}", ack.error());
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
            // Keep real Core wakeups off GPUI's deterministic scheduler.
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
            ids,
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
        assert_eq!(menu.page, ContentMenuPage::Actions);
        assert!(matches!(&menu.source, TrackSource::Album { key, .. } if key == album));
        assert_eq!(app.content_favorite_actions(cx), (true, true));
    }

    fn command_fence(runtime: &CoreRuntime) {
        // Commands and queries have independent queues. Observe a harmless
        // command after the action before asserting its real catalog effects.
        let ack = futures::executor::block_on(
            runtime
                .handle
                .request_command(Command::Volume { value: 0.0 }),
        );
        assert!(ack.is_success(), "{:?}", ack.error());
    }

    fn favorites(runtime: &CoreRuntime, album: &AlbumKey) -> Vec<bool> {
        command_fence(runtime);
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

    fn queued_tracks(runtime: &CoreRuntime) -> Vec<i64> {
        command_fence(runtime);
        let ViewResponse::QueuePage(page) = runtime
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
        assert_eq!(page.total, page.rows.len());
        page.rows.into_iter().map(|row| row.track.id).collect()
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
                    source: TrackSource::Tracks(vec![fixture.ids[0]]),
                    position: point(px(10.), px(10.)),
                    play: None,
                },
                ContentGuard::None,
                Some(Command::RemoveTracks {
                    track_ids: vec![fixture.ids[0]],
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
        assert_eq!(queued_tracks(&fixture.runtime), [fixture.ids[3]]);
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
                app.set_content_menu_page(
                    ContentMenuPage::ConfirmRemove(RemoveTarget::LibraryRoot),
                    cx,
                );
            },
            cx,
        );
        assert!(fixture.app.read_with(cx, |app, _| {
            app.content_menu.as_ref().unwrap().page
                == ContentMenuPage::ConfirmRemove(RemoveTarget::LibraryRoot)
        }));
        assert_eq!(favorites(&fixture.runtime, &fixture.album).len(), 3);
        change_and_click_menu(
            &fixture,
            confirm,
            |app, cx| {
                app.open_object_menu(
                    ContentMenuRequest {
                        source: TrackSource::Tracks(vec![fixture.ids[0]]),
                        position: point(px(10.), px(10.)),
                        play: None,
                    },
                    ContentGuard::None,
                    Some(Command::RemoveTracks {
                        track_ids: vec![fixture.ids[0]],
                    }),
                    None,
                    None,
                    cx,
                );
            },
            cx,
        );
        fixture.app.read_with(cx, |app, _| {
            assert_eq!(
                app.content_menu.as_ref().unwrap().page,
                ContentMenuPage::Actions
            );
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
            app.content_menu.as_ref().unwrap().page
                == ContentMenuPage::ConfirmRemove(RemoveTarget::Selection)
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
        let queue_len = || queued_tracks(&fixture.runtime).len();
        let revision = fixture.app.update(cx, |app, cx| {
            app.refresh(cx);
            app.state.library.album_revision
        });
        let ack = futures::executor::block_on(fixture.runtime.handle.request_command(
            Command::EditTrack {
                track_id: fixture.ids[0],
                title: Some("Changed during drag".into()),
                artist: None,
                album: None,
                decoder_hint: None,
                release_date: None,
            },
        ));
        assert!(ack.is_success(), "{:?}", ack.error());
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
    fn album_favorites_dispatch_directly_and_preserve_full_album_scope(cx: &mut TestAppContext) {
        let fixture = fixture(cx);
        for favorite in [true, false] {
            fixture.app.update(cx, |app, cx| {
                open_album(app, &fixture.album, cx);
                app.content_action(ContentAction::Favorite(favorite), cx);
                assert!(app.content_menu.is_none());
            });
            command_fence(&fixture.runtime);
            assert_eq!(favorites(&fixture.runtime, &fixture.album), [favorite; 3]);
            assert_eq!(favorites(&fixture.runtime, &fixture.other), [false]);
        }
    }

    fn open_album_name(fixture: &Fixture, cx: &mut TestAppContext) -> Entity<playlists::Playlists> {
        let (owner, revision) = fixture.app.update(cx, |app, cx| {
            open_album(app, &fixture.album, cx);
            app.set_content_menu_page(ContentMenuPage::Playlists, cx);
            app.content_action(ContentAction::CreatePlaylist(fixture.window), cx);
            assert!(app.content_menu.is_none());
            (app.playlists.clone(), app.state.library.album_revision)
        });
        owner.read_with(cx, |owner, _| {
            assert_eq!(
                owner.staged_create_album(),
                Some((&fixture.album, revision))
            );
        });
        owner
    }

    fn playlist_id(runtime: &CoreRuntime, name: &str) -> Option<i64> {
        let ViewResponse::PlaylistSummaries(page) = runtime
            .handle
            .query(Query::PlaylistSummaries {
                query: None,
                offset: 0,
                limit: 10,
            })
            .result
            .unwrap()
        else {
            panic!("expected playlist summaries");
        };
        page.rows
            .iter()
            .find(|row| row.name == name)
            .map(|row| row.id)
    }

    #[gpui::test]
    async fn album_name_dialog_stages_immediately_and_submits_the_full_album(
        cx: &mut TestAppContext,
    ) {
        let fixture = fixture(cx);
        let owner = open_album_name(&fixture, cx);
        assert_eq!(playlist_id(&fixture.runtime, "From Album"), None);
        owner.update(cx, |owner, cx| owner.submit_test_name("From Album", cx));
        cx.condition(&owner, |owner, _| !owner.editor_open()).await;
        let ViewResponse::PlaylistEntries(page) = fixture
            .runtime
            .handle
            .query(Query::PlaylistEntries {
                playlist_id: playlist_id(&fixture.runtime, "From Album").unwrap(),
                query: None,
                offset: 0,
                limit: 10,
            })
            .result
            .unwrap()
        else {
            panic!("expected playlist entries");
        };
        let [b, c, a, _outside] = fixture.ids;
        assert_eq!(
            page.rows.iter().map(|row| row.track_id).collect::<Vec<_>>(),
            [a, b, c]
        );
    }

    const RESELECT_ALBUM: &str = "Library changed. Reselect the album and try again.";

    fn change_album(fixture: &Fixture) {
        let ack = futures::executor::block_on(fixture.runtime.handle.request_command(
            Command::EditTrack {
                track_id: fixture.ids[0],
                title: None,
                artist: None,
                album: Some("Changed".into()),
                decoder_hint: None,
                release_date: None,
            },
        ));
        assert!(ack.is_success(), "{:?}", ack.error());
    }

    #[gpui::test]
    async fn stale_album_name_submit_keeps_the_name_and_prompts_reselection(
        cx: &mut TestAppContext,
    ) {
        let fixture = fixture(cx);
        let owner = open_album_name(&fixture, cx);
        change_album(&fixture);
        owner.update(cx, |owner, cx| {
            owner.submit_test_name(" Keep this name ", cx)
        });
        cx.condition(&owner, |owner, cx| owner.test_name_error(cx).is_some())
            .await;
        owner.read_with(cx, |owner, cx| {
            assert_eq!(
                owner.test_name_error(cx),
                Some((" Keep this name ", RESELECT_ALBUM))
            );
        });
        assert_eq!(playlist_id(&fixture.runtime, "Keep this name"), None);
    }

    #[gpui::test]
    fn stale_album_favorite_action_rejects_without_mutating(cx: &mut TestAppContext) {
        let fixture = fixture(cx);
        fixture
            .app
            .update(cx, |app, cx| open_album(app, &fixture.album, cx));
        change_album(&fixture);
        fixture.app.update(cx, |app, cx| {
            app.content_action(ContentAction::Favorite(true), cx);
            assert!(app.content_menu.is_none());
        });
        command_fence(&fixture.runtime);
        assert_eq!(
            fixture
                .runtime
                .handle
                .gui_snapshot()
                .system
                .last_error
                .as_deref(),
            Some(RESELECT_ALBUM)
        );
        assert_eq!(favorites(&fixture.runtime, &fixture.album), [false; 2]);
        assert_eq!(favorites(&fixture.runtime, &fixture.other), [false]);
    }
}
