use super::*;
use rivu_protocol::response::ViewResponse;
use rivu_ui::{POPOVER_MAX_REMS, Popover, caption, menu_item};

#[derive(Clone)]
pub(super) enum TrackSource {
    Tracks(Vec<i64>),
    Album(rivu_protocol::model::AlbumKey),
}

pub(super) struct ContentDrag {
    source: std::cell::OnceCell<TrackSource>,
}

impl ContentDrag {
    pub(super) fn new(source: TrackSource) -> Self {
        let drag = Self::pending();
        drag.initialize(source);
        drag
    }
    pub(super) fn pending() -> Self {
        Self {
            source: std::cell::OnceCell::new(),
        }
    }
    pub(super) fn initialize(&self, source: TrackSource) {
        let _ = self.source.set(source);
    }
    pub(super) fn source(&self) -> Option<&TrackSource> {
        self.source.get()
    }
}

#[derive(Clone)]
pub(super) struct ContentMenuRequest {
    pub(super) source: TrackSource,
    pub(super) position: Point<Pixels>,
    pub(super) play: Option<Command>,
}

#[derive(Clone)]
pub(super) enum ContentGuard {
    None,
    Library(u64, u64),
    Queue(u64),
    Playlist(i64, u64),
}

impl ContentGuard {
    fn current(&self, app: &GuiApp) -> bool {
        match *self {
            Self::None => true,
            Self::Library(library, structure) => {
                app.state.library.revision == library
                    && app.state.library.structure_revision == structure
            }
            Self::Queue(revision) => app.state.queue.revision == revision,
            Self::Playlist(id, revision) => {
                app.state.library.playlist_entry_revisions.get(&id) == Some(&revision)
            }
        }
    }
}

#[derive(Clone, Copy)]
enum RemoveTarget {
    Selection,
    LibraryRoot,
}

pub(super) struct ContentMenu {
    source: TrackSource,
    album: Option<rivu_protocol::model::AlbumKey>,
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
    pub(super) playlists: bool,
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
pub(super) struct ContentMenuPage {
    loading: bool,
    playlists: bool,
    confirming_remove: bool,
}

impl ContentMenu {
    pub(super) fn keyboard_page(&self) -> ContentMenuPage {
        ContentMenuPage {
            loading: self.loading,
            playlists: self.playlists,
            confirming_remove: self.confirm_remove.is_some(),
        }
    }

    pub(super) fn focus_playlist_return(&mut self, window: &mut Window, cx: &mut App) {
        if !self.playlists && std::mem::take(&mut self.restore_playlist_trigger) {
            self.playlist_trigger.focus(window, cx);
        }
    }
}

#[derive(Clone, Copy)]
enum ContentAction {
    Play,
    Queue,
    Playlist(i64),
    Favorite(bool),
    Remove,
    RemoveLibraryRoot,
}

struct CollectionRead {
    worker: u64,
    ticket: crate::core::QueryTicket,
}

// Only one collection worker may run at a time. Cancellation retires its read
// identity, not its physical gate; only that worker's completion releases it.
#[derive(Default)]
pub(super) struct CollectionResolution {
    epoch: crate::core::QueryEpoch,
    worker: Option<u64>,
}

impl CollectionResolution {
    pub(super) fn pending(&self) -> bool {
        self.worker.is_some()
    }

    fn begin(&mut self) -> Option<CollectionRead> {
        if self.pending() {
            return None;
        }
        self.epoch.advance();
        let worker = self.epoch.generation();
        self.worker = Some(worker);
        Some(CollectionRead {
            worker,
            ticket: self.epoch.ticket(),
        })
    }

    pub(super) fn cancel(&mut self) {
        self.epoch.cancel();
    }

    fn timeout(&mut self, read: &CollectionRead) -> bool {
        if self.worker != Some(read.worker) || !read.ticket.is_current() {
            return false;
        }
        self.cancel();
        true
    }

    fn finish(&mut self, read: &CollectionRead) -> bool {
        if self.worker != Some(read.worker) {
            return false;
        }
        self.worker = None;
        true
    }
}

fn query_collection_page(
    query_page: &mut impl FnMut(
        Query,
        crate::core::QueryTicket,
    ) -> rivu_protocol::response::QueryResponse,
    query: Query,
    ticket: &crate::core::QueryTicket,
) -> Result<rivu_protocol::response::QueryResponse> {
    anyhow::ensure!(
        ticket.is_current(),
        rivu_protocol::response::QueryError::Cancelled
    );
    let response = query_page(query, ticket.clone());
    anyhow::ensure!(
        ticket.is_current(),
        rivu_protocol::response::QueryError::Cancelled
    );
    Ok(response)
}

// Each request is bounded; no subset is published before the complete collection succeeds.
fn resolve_album(
    mut query_page: impl FnMut(
        Query,
        crate::core::QueryTicket,
    ) -> rivu_protocol::response::QueryResponse,
    key: rivu_protocol::model::AlbumKey,
    expected: rivu_protocol::model::QueryStamp,
    ticket: &crate::core::QueryTicket,
) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    let mut total = None;
    let scope = Query::AlbumTracks {
        key,
        query: None,
        offset: 0,
        limit: rivu_protocol::model::MAX_QUERY_ROWS,
    };
    loop {
        let mut query = scope.clone();
        if let Query::AlbumTracks { offset, .. } = &mut query {
            *offset = ids.len();
        }
        let response = query_collection_page(&mut query_page, query, ticket)?;
        let result = response.result?;
        anyhow::ensure!(
            scope.stamp(response.revisions) == expected,
            "collection changed"
        );
        let ViewResponse::AlbumTracks(page) = result else {
            anyhow::bail!("unexpected album response");
        };
        anyhow::ensure!(
            total.is_none_or(|total| total == page.total),
            "collection changed"
        );
        total = Some(page.total);
        anyhow::ensure!(
            valid_page(ids.len(), page.rows.len(), page.total),
            "incomplete album response"
        );
        ids.extend(page.rows.into_iter().map(|track| track.id));
        if ids.len() == page.total {
            return Ok(ids);
        }
    }
}

fn valid_page(offset: usize, rows: usize, total: usize) -> bool {
    offset <= total
        && rows <= rivu_protocol::model::MAX_QUERY_ROWS
        && rows <= total - offset
        && (rows > 0 || offset == total)
}

impl GuiApp {
    fn begin_content_resolution(&mut self, cx: &mut Context<Self>) -> Option<CollectionRead> {
        let Some(read) = self.content_resolution.begin() else {
            self.error = Some(self.i18n.text("li-source-busy").to_owned());
            cx.notify();
            return None;
        };
        self.sync_transfer_state(cx);
        Some(read)
    }

    fn finish_content_resolution(&mut self, read: &CollectionRead, cx: &mut Context<Self>) -> bool {
        if !self.content_resolution.finish(read) {
            return false;
        }
        self.sync_transfer_state(cx);
        self.refresh(cx);
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

    pub(super) fn replace_content_menu(
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

    pub(super) fn open_content_menu(
        &mut self,
        request: ContentMenuRequest,
        cx: &mut Context<Self>,
    ) {
        let guard = match request.source {
            TrackSource::Album(_) => ContentGuard::Library(
                self.state.library.revision,
                self.state.library.structure_revision,
            ),
            TrackSource::Tracks(_) => ContentGuard::None,
        };
        self.open_object_menu(request, guard, None, None, None, cx);
    }

    pub(super) fn open_object_menu(
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
        if matches!(request.source, TrackSource::Album(_)) && self.content_resolution.pending() {
            self.replace_content_menu(None, cx);
            self.error = Some(self.i18n.text("li-source-busy").to_owned());
            cx.notify();
            return;
        }
        let loading = matches!(request.source, TrackSource::Album(_));
        let album = match &request.source {
            TrackSource::Album(key) => Some(key.clone()),
            _ => None,
        };
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
            _ => "gl-remove-library-selected",
        };
        let menu = Some(ContentMenu {
            source: request.source,
            album: album.clone(),
            album_revision: self.state.library.album_revision,
            nodes,
            tracks_only,
            guard: guard.clone(),
            position: request.position,
            play: request.play,
            remove,
            remove_label,
            root,
            loading,
            confirm_remove: None,
            playlists: false,
            playlist_targets: PlaylistTargets::new(cx),
            playlist_trigger: cx.focus_handle(),
            restore_playlist_trigger: false,
        });
        self.replace_content_menu(menu, cx);
        if let Some(key) = album {
            let Some(read) = self.begin_content_resolution(cx) else {
                self.replace_content_menu(None, cx);
                return;
            };
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
                    move |handle| {
                        resolve_album(
                            |query, ticket| handle.query_if_current(query, ticket),
                            key,
                            stamp,
                            &ticket,
                        )
                    },
                    |response, cx| {
                        let _ = this.update(cx, |this, cx| {
                            let current = guard.current(this);
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
                        Ok(ids) if guard.current(this) => {
                            let menu = this.content_menu.as_mut().unwrap();
                            menu.source = TrackSource::Tracks(ids);
                            menu.loading = false;
                        }
                        Ok(_) => {
                            this.replace_content_menu(None, cx);
                            this.error = Some(this.i18n.text("li-source-changed").to_owned());
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
        }
        cx.notify();
    }

    pub(super) fn drop_content(
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
            TrackSource::Album(key) => {
                let source = rivu_protocol::model::CollectionSource::Album {
                    key,
                    revision: self.state.library.album_revision,
                };
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

    pub(super) fn send_content_tracks(
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

    fn content_action(&mut self, action: ContentAction, cx: &mut Context<Self>) {
        self.refresh(cx);
        let Some(menu) = self.replace_content_menu(None, cx) else {
            return;
        };
        if menu.loading || !menu.guard.current(self) {
            self.error = Some(self.i18n.text("li-source-changed").to_owned());
            cx.notify();
            return;
        }
        let TrackSource::Tracks(mut track_ids) = menu.source else {
            return;
        };
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
                } else if let Some(key) = menu.album {
                    let revision = menu.album_revision;
                    self.send_content_collection(
                        rivu_protocol::model::CollectionSource::Album { key, revision },
                        destination,
                        cx,
                    );
                } else {
                    self.send_content_tracks(track_ids, destination, cx);
                }
            }
            ContentAction::Favorite(favorite) => {
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
            ContentAction::Remove => {
                if let Some(command) = menu.remove {
                    self.send(command, cx);
                }
            }
            ContentAction::RemoveLibraryRoot => {
                if let Some(root) = menu.root {
                    if let Err(error) = self
                        .handle
                        .update_gui_config(crate::core::GuiConfigChange::RemoveLibraryRoot(root))
                    {
                        self.error = Some(format!("{error:#}"));
                    }
                    self.refresh(cx);
                }
            }
        }
        cx.notify();
    }

    pub(super) fn back_content_menu(&mut self, cx: &mut Context<Self>) {
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
                let row_height = rivu_ui::list_row_height(window, cx);
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

    pub(super) fn content_menu_overlay(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let Some(state) = self.content_menu.as_ref() else {
            return div().into_any_element();
        };
        let mut menu = Popover::menu(theme, "content-menu", window)
            .map(|surface| self.menu_surface(surface, cx))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.replace_content_menu(None, cx);
                cx.stop_propagation();
                cx.notify();
            }));
        if state.loading {
            menu = menu.child(caption(theme, self.tr("gl-loading")));
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
            menu = menu
                .child(caption(theme, self.tr(description)))
                .child(menu_item(
                    "content-cancel-remove",
                    self.tr("gl-cancel"),
                    cx,
                    |this, _, cx| {
                        this.replace_content_menu(None, cx);
                        cx.notify();
                    },
                ))
                .child(rivu_ui::danger_menu_item(
                    theme,
                    menu_item(
                        "content-confirm-remove",
                        self.tr(label),
                        cx,
                        move |this, _, cx| this.content_action(action, cx),
                    ),
                ));
        } else if state.playlists {
            menu = menu.child(rivu_ui::menu_title(theme, self.tr("li-playlist")));
            if state.tracks_only {
                menu = menu.child(menu_item(
                    "content-new-playlist",
                    self.tr("shell-new-playlist"),
                    cx,
                    |this, window, cx| {
                        this.refresh(cx);
                        let Some(menu) = this.replace_content_menu(None, cx) else {
                            return;
                        };
                        if !menu.guard.current(this) {
                            this.error = Some(this.i18n.text("li-source-changed").to_owned());
                            cx.notify();
                            return;
                        }
                        let TrackSource::Tracks(track_ids) = menu.source else {
                            return;
                        };
                        this.playlists.update(cx, |playlists, cx| {
                            playlists.begin_playlist_name(
                                playlists::PlaylistNameAction::Create { track_ids },
                                String::new(),
                                window,
                                cx,
                            );
                        });
                    },
                ));
            }
            let count = self.playlists.read(cx).catalog_len();
            let height = (rivu_ui::list_row_height(window, cx) * count.max(1) as f32)
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
                                    move |this, range: std::ops::Range<usize>, window, cx| {
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
                                                    return this
                                                        .playlists
                                                        .update(cx, |playlists, cx| {
                                                            playlists.catalog_loading_row(index, cx)
                                                        })
                                                        .h(rivu_ui::list_row_height(window, cx));
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
                                                .h(rivu_ui::list_row_height(window, cx))
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
                    |this, _, cx| {
                        this.set_content_menu_playlists(false, cx);
                    },
                ));
        } else {
            if state.play.is_some() {
                menu = menu.child(menu_item(
                    "content-play",
                    self.tr("li-play"),
                    cx,
                    |this, _, cx| this.content_action(ContentAction::Play, cx),
                ));
            }
            menu = menu
                .child(menu_item(
                    "content-queue",
                    self.tr("li-queue"),
                    cx,
                    |this, _, cx| this.content_action(ContentAction::Queue, cx),
                ))
                .child(
                    menu_item(
                        "content-playlist",
                        self.tr("li-playlist"),
                        cx,
                        |this, _, cx| {
                            this.set_content_menu_playlists(true, cx);
                        },
                    )
                    .track_focus(&state.playlist_trigger)
                    .tab_stop(true),
                );
            if state.tracks_only {
                menu = menu
                    .child(rivu_ui::menu_separator(theme))
                    .child(menu_item(
                        "content-favorite",
                        self.tr("li-favorite"),
                        cx,
                        |this, _, cx| this.content_action(ContentAction::Favorite(true), cx),
                    ))
                    .child(menu_item(
                        "content-unfavorite",
                        self.tr("li-unfavorite"),
                        cx,
                        |this, _, cx| this.content_action(ContentAction::Favorite(false), cx),
                    ));
            }
            if state.remove.is_some() {
                menu = menu
                    .child(rivu_ui::menu_separator(theme))
                    .child(rivu_ui::danger_menu_item(
                        theme,
                        menu_item(
                            "content-remove",
                            self.tr(state.remove_label),
                            cx,
                            |this, _, cx| {
                                if let Some(menu) = &mut this.content_menu {
                                    menu.confirm_remove = Some(RemoveTarget::Selection);
                                }
                                cx.notify();
                            },
                        ),
                    ));
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
                            |this, _, cx| {
                                if let Some(menu) = &mut this.content_menu {
                                    menu.confirm_remove = Some(RemoveTarget::LibraryRoot);
                                }
                                cx.notify();
                            },
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

impl GuiApp {
    pub(super) fn append_playlist_to_queue(&mut self, playlist_id: i64, cx: &mut Context<Self>) {
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
    use super::{CollectionResolution, ContentDrag, TrackSource, resolve_album, valid_page};
    use crate::core::QueryEpoch;
    use rivu_protocol::{
        model::Query,
        response::{QueryError, QueryRevisions, ViewResponse},
    };
    use std::path::PathBuf;

    #[test]
    fn collection_timeout_retires_logical_owner_but_not_physical_worker() {
        {
            let mut resolution = CollectionResolution::default();
            let read = resolution.begin().unwrap();
            assert!(resolution.timeout(&read));
            assert!(!read.ticket.is_current());
            assert!(resolution.pending());
            assert!(resolution.begin().is_none());
            assert!(!resolution.timeout(&read));
            assert!(resolution.finish(&read));
            let latest = resolution.begin().unwrap();
            assert!(!resolution.timeout(&read));
            assert!(!resolution.finish(&read));
            assert!(latest.ticket.is_current());
            resolution.cancel();
            assert!(!resolution.timeout(&latest));
            assert!(resolution.pending());
            assert!(resolution.finish(&latest));
        }
    }

    #[test]
    fn collection_timeout_during_pagination_discards_late_pages_and_partial_ids() {
        let Query::AlbumTracks { key, .. } = album_query() else {
            unreachable!()
        };
        let revisions = QueryRevisions::default();
        for timeout_at in 1..=2 {
            let mut resolution = CollectionResolution::default();
            let read = resolution.begin().unwrap();
            let mut calls = 0;
            let result = resolve_album(
                |_, _| {
                    calls += 1;
                    if calls == timeout_at {
                        assert!(resolution.timeout(&read));
                        assert!(resolution.pending());
                    }
                    album_page(&[calls as i64], 2, revisions)
                },
                key.clone(),
                album_query().stamp(revisions),
                &read.ticket,
            );
            assert!(matches!(
                result.unwrap_err().downcast_ref::<QueryError>(),
                Some(QueryError::Cancelled)
            ));
            assert_eq!(calls, timeout_at);
            assert!(resolution.pending());
            assert!(resolution.finish(&read));
        }
    }

    #[test]
    fn drag_freezes_exactly_once_and_preserves_occurrences() {
        let drag = ContentDrag::pending();
        assert!(drag.source().is_none());
        drag.initialize(TrackSource::Tracks(vec![8, 3, 8]));
        drag.initialize(TrackSource::Tracks(vec![99]));
        assert!(matches!(drag.source(), Some(TrackSource::Tracks(ids)) if ids == &[8, 3, 8]));
    }

    #[test]
    fn collection_pages_reject_partial_and_overflowing_results() {
        assert!(valid_page(0, 0, 0));
        assert!(valid_page(3, 2, 5));
        assert!(!valid_page(3, 0, 5));
        assert!(!valid_page(3, 3, 5));
        assert!(!valid_page(6, 0, 5));
        assert!(!valid_page(
            0,
            rivu_protocol::model::MAX_QUERY_ROWS + 1,
            usize::MAX
        ));
    }

    fn album_query() -> Query {
        Query::AlbumTracks {
            key: rivu_protocol::model::AlbumKey {
                directory: PathBuf::from("/album"),
                title: "Album".into(),
            },
            query: None,
            offset: 0,
            limit: rivu_protocol::model::MAX_QUERY_ROWS,
        }
    }

    fn album_page(
        ids: &[i64],
        total: usize,
        revisions: rivu_protocol::response::QueryRevisions,
    ) -> rivu_protocol::response::QueryResponse {
        rivu_protocol::response::QueryResponse {
            revisions,
            result: Ok(ViewResponse::AlbumTracks(rivu_protocol::model::Page {
                total,
                rows: ids
                    .iter()
                    .map(|id| rivu_protocol::model::LibraryRow {
                        id: *id,
                        path: Default::default(),
                        title: String::new(),
                        artist: String::new(),
                        album: String::new(),
                        duration: None,
                        codec: String::new(),
                        bitrate_bps: None,
                        bitrate_exact: false,
                        favorite: false,
                        missing: false,
                        play_count: 0,
                    })
                    .collect(),
            })),
        }
    }

    #[test]
    fn album_resolution_keeps_complete_order_across_pages() {
        let query = album_query();
        let Query::AlbumTracks { key, .. } = query.clone() else {
            unreachable!()
        };
        let revisions = rivu_protocol::response::QueryRevisions {
            library: 8,
            structure: 4,
            ..Default::default()
        };
        let mut offsets = Vec::new();
        let mut epoch = QueryEpoch::default();
        let ticket = epoch.ticket();
        let ids = resolve_album(
            |query, _| {
                let Query::AlbumTracks { offset, limit, .. } = query else {
                    unreachable!()
                };
                offsets.push(offset);
                assert_eq!(limit, rivu_protocol::model::MAX_QUERY_ROWS);
                if offset == 0 {
                    album_page(&[9, 2], 4, revisions)
                } else {
                    album_page(&[8, 3], 4, revisions)
                }
            },
            key,
            query.stamp(revisions),
            &ticket,
        )
        .unwrap();
        assert_eq!(offsets, [0, 2]);
        assert_eq!(ids, [9, 2, 8, 3]);
    }

    #[test]
    fn album_resolution_never_returns_partial_stale_or_failed_pages() {
        let query = album_query();
        let Query::AlbumTracks { key, .. } = query.clone() else {
            unreachable!()
        };
        let revisions = rivu_protocol::response::QueryRevisions {
            library: 8,
            structure: 4,
            ..Default::default()
        };
        for failure in 0..4 {
            let mut calls = 0;
            let mut epoch = QueryEpoch::default();
            let ticket = epoch.ticket();
            let result = resolve_album(
                |_, _| {
                    calls += 1;
                    if calls == 1 {
                        return album_page(&[9, 2], 4, revisions);
                    }
                    match failure {
                        0 => album_page(
                            &[8, 3],
                            4,
                            rivu_protocol::response::QueryRevisions {
                                library: 9,
                                ..revisions
                            },
                        ),
                        1 => album_page(
                            &[8, 3],
                            4,
                            rivu_protocol::response::QueryRevisions {
                                structure: 5,
                                ..revisions
                            },
                        ),
                        2 => album_page(&[], 4, revisions),
                        _ => rivu_protocol::response::QueryResponse {
                            revisions,
                            result: Err(rivu_protocol::response::QueryError::Message(
                                "read failed".into(),
                            )),
                        },
                    }
                },
                key.clone(),
                query.stamp(revisions),
                &ticket,
            );
            assert!(result.is_err());
            assert_eq!(calls, 2);
        }
    }

    #[test]
    fn menu_cancellation_keeps_physical_gate_until_worker_finishes() {
        let mut resolution = CollectionResolution::default();
        let read = resolution.begin().unwrap();
        resolution.cancel();
        assert!(!read.ticket.is_current());
        assert!(resolution.pending());
        assert!(resolution.begin().is_none());
        assert!(resolution.finish(&read));
        let next = resolution.begin().unwrap();
        assert!(!resolution.finish(&read));
        assert!(next.ticket.is_current());
        assert!(resolution.finish(&next));
    }

    #[test]
    fn destroying_collection_owner_cancels_menu_resolution() {
        let mut resolution = CollectionResolution::default();
        let read = resolution.begin().unwrap();
        drop(resolution);
        assert!(!read.ticket.is_current());
    }

    #[test]
    fn album_cancellation_stops_pages_without_publishing_or_releasing_worker() {
        let query = album_query();
        let Query::AlbumTracks { key, .. } = query.clone() else {
            unreachable!()
        };
        let revisions = QueryRevisions::default();
        {
            // Cancel before dispatch, during an intermediate page, or during the final page.
            for cancel_at in 0..=2 {
                let mut resolution = CollectionResolution::default();
                let read = resolution.begin().unwrap();
                if cancel_at == 0 {
                    resolution.cancel();
                }
                let mut calls = 0;
                let result = resolve_album(
                    |_, ticket| {
                        assert!(ticket.is_current());
                        calls += 1;
                        if calls == cancel_at {
                            resolution.cancel();
                        }
                        album_page(&[calls as i64], 2, revisions)
                    },
                    key.clone(),
                    query.stamp(revisions),
                    &read.ticket,
                );
                assert!(matches!(
                    result.unwrap_err().downcast_ref::<QueryError>(),
                    Some(QueryError::Cancelled)
                ));
                assert_eq!(calls, cancel_at);
                assert!(resolution.pending());
                assert!(resolution.begin().is_none());
                assert!(resolution.finish(&read));
                assert!(!resolution.pending());
            }
        }
    }
}
