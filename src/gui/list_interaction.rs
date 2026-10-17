use super::*;
use super::{library::LibraryDrag, playlists::PlaylistDrag, queue::QueueDrag};
use crate::gui::components::{POPOVER_MAX_REMS, caption, context_menu_container, menu_item};
use crate::response::ViewResponse;

#[derive(Clone)]
pub(super) enum TrackSource {
    Tracks(Vec<i64>),
    Album(crate::model::AlbumKey),
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
    token: u64,
    source: TrackSource,
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
}

impl ContentMenu {
    pub(super) fn keyboard_page(&self) -> u8 {
        u8::from(self.loading)
            | (u8::from(self.playlists) << 1)
            | (u8::from(self.confirm_remove.is_some()) << 2)
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

// Each request is bounded; no subset is published before the complete collection succeeds.
fn resolve_album(
    mut query_page: impl FnMut(Query) -> crate::response::QueryResponse,
    key: crate::model::AlbumKey,
    expected: crate::model::QueryStamp,
) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    let mut total = None;
    let scope = Query::AlbumTracks {
        key,
        query: None,
        offset: 0,
        limit: crate::model::MAX_QUERY_ROWS,
    };
    loop {
        let mut query = scope.clone();
        if let Query::AlbumTracks { offset, .. } = &mut query {
            *offset = ids.len();
        }
        let response = query_page(query);
        anyhow::ensure!(
            scope.stamp(response.revisions) == expected,
            "collection changed"
        );
        let ViewResponse::AlbumTracks(page) = response
            .result
            .map_err(|error| anyhow::anyhow!(error.to_string()))?
        else {
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
        && rows <= crate::model::MAX_QUERY_ROWS
        && rows <= total - offset
        && (rows > 0 || offset == total)
}

impl GuiApp {
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
            self.content_menu = None;
            cx.notify();
            return;
        }
        if matches!(request.source, TrackSource::Album(_)) && self.content_resolution {
            self.content_menu = None;
            self.error = Some(self.i18n.text("li-source-busy").to_owned());
            cx.notify();
            return;
        }
        self.content_generation = self.content_generation.wrapping_add(1);
        let token = self.content_generation;
        let loading = matches!(request.source, TrackSource::Album(_));
        let album = match &request.source {
            TrackSource::Album(key) => Some(key.clone()),
            _ => None,
        };
        self.panel_menu = None;
        self.playlist_action = None;
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
        self.content_menu = Some(ContentMenu {
            token,
            source: request.source,
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
        });
        if let Some(key) = album {
            self.content_resolution = true;
            let query = Query::AlbumTracks {
                key: key.clone(),
                query: None,
                offset: 0,
                limit: crate::model::MAX_QUERY_ROWS,
            };
            let stamp = query.stamp(self.state.query_revisions(None));
            let handle = self.handle.clone();
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { resolve_album(|query| handle.query(query), key, stamp) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.content_resolution = false;
                    this.refresh(cx);
                    if !this
                        .content_menu
                        .as_ref()
                        .is_some_and(|menu| menu.token == token)
                    {
                        return;
                    }
                    match result {
                        Ok(ids) if guard.current(this) => {
                            let menu = this.content_menu.as_mut().unwrap();
                            menu.source = TrackSource::Tracks(ids);
                            menu.loading = false;
                        }
                        Ok(_) => {
                            this.content_menu = None;
                            this.error = Some(this.i18n.text("li-source-changed").to_owned());
                        }
                        Err(error) => {
                            this.content_menu = None;
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

    pub(super) fn can_drop_content(&self, drag: &ContentDrag) -> bool {
        match drag.source() {
            Some(TrackSource::Tracks(ids)) => !ids.is_empty(),
            Some(TrackSource::Album(_)) => !self.content_resolution,
            None => false,
        }
    }

    pub(super) fn drop_content(
        &mut self,
        drag: &ContentDrag,
        destination: DragDestination,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = drag.source() else {
            return;
        };
        match source {
            TrackSource::Tracks(ids) => self.send_content_tracks(ids.clone(), destination, cx),
            TrackSource::Album(key) => {
                self.refresh(cx);
                if self.content_resolution {
                    self.error = Some(self.i18n.text("li-source-busy").to_owned());
                    cx.notify();
                    return;
                }
                self.content_resolution = true;
                let generation = self.content_generation;
                let guard = ContentGuard::Library(
                    self.state.library.revision,
                    self.state.library.structure_revision,
                );
                let query = Query::AlbumTracks {
                    key: key.clone(),
                    query: None,
                    offset: 0,
                    limit: crate::model::MAX_QUERY_ROWS,
                };
                let stamp = query.stamp(self.state.query_revisions(None));
                let target_revision = match destination {
                    DragDestination::Playlist(id) => self
                        .state
                        .library
                        .playlist_entry_revisions
                        .get(&id)
                        .copied(),
                    _ => None,
                };
                let key = key.clone();
                let handle = self.handle.clone();
                cx.spawn(async move |this, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(
                            async move { resolve_album(|query| handle.query(query), key, stamp) },
                        )
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        this.content_resolution = false;
                        this.refresh(cx);
                        if this.content_generation != generation
                            || !this.window_visible
                            || this.state.system.shutting_down
                        {
                            return;
                        }
                        let target_current = match destination {
                            DragDestination::Playlist(id) => {
                                target_revision.is_some()
                                    && this
                                        .state
                                        .library
                                        .playlist_entry_revisions
                                        .get(&id)
                                        .copied()
                                        == target_revision
                            }
                            _ => true,
                        };
                        match result {
                            Ok(ids) if guard.current(this) && target_current => {
                                this.send_content_tracks(ids, destination, cx)
                            }
                            Ok(_) => {
                                this.error = Some(this.i18n.text("li-source-changed").to_owned())
                            }
                            Err(error) => this.error = Some(error.to_string()),
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
    }

    pub(super) fn send_content_tracks(
        &mut self,
        track_ids: Vec<i64>,
        destination: DragDestination,
        cx: &mut Context<Self>,
    ) {
        if track_ids.is_empty() {
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
        };
        self.send(command, cx);
    }

    fn content_action(&mut self, action: ContentAction, cx: &mut Context<Self>) {
        self.refresh(cx);
        let Some(menu) = self.content_menu.take() else {
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
                    self.resolve_library_drag(&nodes, destination, cx);
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
        if let Some(menu) = &mut self.content_menu {
            if menu.playlists || menu.confirm_remove.is_some() {
                menu.playlists = false;
                menu.confirm_remove = None;
            } else {
                self.content_menu = None;
            }
        }
        cx.notify();
    }

    pub(super) fn content_menu_overlay(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = crate::gui::theme::palette(cx);
        let Some(state) = self.content_menu.as_ref() else {
            return div().into_any_element();
        };
        let mut menu = context_menu_container(theme, "content-menu", window)
            .map(|surface| self.menu_surface(surface))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.content_menu = None;
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
                        this.content_menu = None;
                        cx.notify();
                    },
                ))
                .child(components::danger_menu_item(
                    theme,
                    menu_item(
                        "content-confirm-remove",
                        self.tr(label),
                        cx,
                        move |this, _, cx| this.content_action(action, cx),
                    ),
                ));
        } else if state.playlists {
            menu = menu.child(components::menu_title(theme, self.tr("li-playlist")));
            if state.tracks_only {
                menu = menu.child(menu_item(
                    "content-new-playlist",
                    self.tr("shell-new-playlist"),
                    cx,
                    |this, window, cx| {
                        this.refresh(cx);
                        let Some(menu) = this.content_menu.take() else {
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
                        this.begin_playlist_name(
                            playlists::editor::PlaylistNameAction::Create { track_ids },
                            String::new(),
                            window,
                            cx,
                        );
                    },
                ));
            }
            let count = self.browser.playlists().summaries().rows.len();
            let height = (window.rem_size() * components::LIST_ROW_REMS * count.max(1) as f32)
                .min(window.rem_size() * POPOVER_MAX_REMS)
                .min(window.viewport_size().height * 0.5);
            menu = menu
                .child(
                    uniform_list(
                        "content-playlist-targets",
                        count,
                        cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                            range
                                .map(|index| {
                                    let Some(playlist) =
                                        this.browser.playlists().summaries().rows.get(index)
                                    else {
                                        return this
                                            .paged_loading_row(
                                                ViewRequestKind::Summaries,
                                                index,
                                                cx,
                                            )
                                            .h(gpui::rems(components::LIST_ROW_REMS));
                                    };
                                    let id = playlist.id;
                                    menu_item(
                                        ("content-playlist-target", id as u64),
                                        div().min_w_0().truncate().child(playlist.name.clone()),
                                        cx,
                                        move |this, _, cx| {
                                            this.content_action(ContentAction::Playlist(id), cx)
                                        },
                                    )
                                    .h(gpui::rems(components::LIST_ROW_REMS))
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .with_decoration(paged::viewport_demand(
                        cx.processor(|this, range, _, cx| this.ensure_playlist_range(range, cx)),
                    ))
                    .h(height)
                    .w_full(),
                )
                .child(components::menu_back(
                    "content-back",
                    self.tr("shell-back"),
                    cx,
                    |this, _, cx| {
                        if let Some(menu) = &mut this.content_menu {
                            menu.playlists = false;
                        }
                        cx.notify();
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
                .child(menu_item(
                    "content-playlist",
                    self.tr("li-playlist"),
                    cx,
                    |this, _, cx| {
                        if let Some(menu) = &mut this.content_menu {
                            menu.playlists = true;
                        }
                        this.ensure_playlist_range(0..1, cx);
                        cx.notify();
                    },
                ));
            if state.tracks_only {
                menu = menu
                    .child(components::menu_separator(theme))
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
                menu = menu.child(components::menu_separator(theme)).child(
                    components::danger_menu_item(
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
                    ),
                );
            }
            if state.root.is_some() {
                menu = menu.child(components::menu_separator(theme)).child(
                    components::danger_menu_item(
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
                    ),
                );
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
    pub(super) fn can_transfer_content(
        &self,
        value: &dyn std::any::Any,
        destination: super::DragDestination,
    ) -> bool {
        if let super::DragDestination::Playlist(id) = destination
            && !self
                .state
                .library
                .playlist_entry_revisions
                .contains_key(&id)
        {
            return false;
        }
        if let Some(drag) = value.downcast_ref::<super::ContentDrag>() {
            return self.can_drop_content(drag);
        }
        if let Some(drag) = value.downcast_ref::<LibraryDrag>() {
            return self.can_resolve_library_drag(drag);
        }
        if let Some(drag) = value.downcast_ref::<QueueDrag>() {
            return drag.can_transfer(self, destination);
        }
        if let Some(drag) = value.downcast_ref::<PlaylistDrag>() {
            return drag.can_transfer(self, destination);
        }
        false
    }

    pub(super) fn transfer_queue_drag(
        &mut self,
        drag: &QueueDrag,
        destination: super::DragDestination,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if !self.can_transfer_content(drag, destination) {
            return;
        }
        let ids = drag.track_ids();
        self.send_content_tracks(ids, destination, cx);
    }

    pub(super) fn transfer_playlist_drag(
        &mut self,
        drag: &PlaylistDrag,
        destination: super::DragDestination,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if !self.can_transfer_content(drag, destination) {
            return;
        }
        if drag.targets_source(destination) {
            self.move_playlist_entries(drag, None, cx);
        } else {
            self.send_content_tracks(drag.track_ids(), destination, cx);
        }
    }

    pub(super) fn append_playlist_to_queue(&mut self, playlist_id: i64, cx: &mut Context<Self>) {
        self.refresh(cx);
        if self.content_resolution {
            self.error = Some(self.i18n.text("li-source-busy").to_owned());
            cx.notify();
            return;
        }
        if !self
            .state
            .library
            .playlist_entry_revisions
            .contains_key(&playlist_id)
        {
            return;
        }
        self.content_resolution = true;
        let generation = self.content_generation;
        let revision = self.playlist_revision(playlist_id);
        let revisions = self.state.query_revisions(Some(playlist_id));
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let result: anyhow::Result<Vec<i64>> = cx
                .background_executor()
                .spawn(async move {
                    let mut after = None;
                    let mut ids = Vec::new();
                    loop {
                        let query = crate::model::Query::PlaylistEntryBatch {
                            query: None,
                            playlist_id,
                            after,
                            limit: crate::model::MAX_QUERY_ROWS,
                            expected: Some(revisions),
                        };
                        let response = handle.query(query.clone());
                        anyhow::ensure!(
                            query.stamp(response.revisions) == query.stamp(revisions),
                            "playlist changed"
                        );
                        let crate::response::ViewResponse::PlaylistEntryBatch(batch) = response
                            .result
                            .map_err(|error| anyhow::anyhow!(error.to_string()))?
                        else {
                            anyhow::bail!("unexpected playlist response");
                        };
                        anyhow::ensure!(
                            batch.rows.len() <= crate::model::MAX_QUERY_ROWS
                                && (batch.next.is_none()
                                    || (!batch.rows.is_empty() && batch.next != after)),
                            "incomplete playlist response"
                        );
                        ids.extend(batch.rows.into_iter().map(|row| row.track_id));
                        let Some(next) = batch.next else {
                            return Ok(ids);
                        };
                        after = Some(next);
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.content_resolution = false;
                this.refresh(cx);
                if this.content_generation != generation
                    || !this.window_visible
                    || this.state.system.shutting_down
                {
                    return;
                }
                match result {
                    Ok(ids)
                        if this
                            .state
                            .library
                            .playlist_entry_revisions
                            .get(&playlist_id)
                            == Some(&revision) =>
                    {
                        this.send_content_tracks(ids, super::DragDestination::Queue, cx)
                    }
                    Ok(_) => this.error = Some(this.i18n.text("li-source-changed").to_owned()),
                    Err(error) => this.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }
}
#[cfg(test)]
mod tests {
    use super::{ContentDrag, TrackSource, resolve_album, valid_page};
    use crate::{model::Query, response::ViewResponse};
    use std::path::PathBuf;

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
        assert!(!valid_page(0, crate::model::MAX_QUERY_ROWS + 1, usize::MAX));
    }

    fn album_query() -> Query {
        Query::AlbumTracks {
            key: crate::model::AlbumKey {
                directory: PathBuf::from("/album"),
                title: "Album".into(),
            },
            query: None,
            offset: 0,
            limit: crate::model::MAX_QUERY_ROWS,
        }
    }

    fn album_page(
        ids: &[i64],
        total: usize,
        revisions: crate::response::QueryRevisions,
    ) -> crate::response::QueryResponse {
        crate::response::QueryResponse {
            revisions,
            result: Ok(ViewResponse::AlbumTracks(crate::model::Page {
                total,
                rows: ids
                    .iter()
                    .map(|id| crate::model::LibraryRow {
                        id: *id,
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
        let revisions = crate::response::QueryRevisions {
            library: 8,
            structure: 4,
            ..Default::default()
        };
        let mut offsets = Vec::new();
        let ids = resolve_album(
            |query| {
                let Query::AlbumTracks { offset, limit, .. } = query else {
                    unreachable!()
                };
                offsets.push(offset);
                assert_eq!(limit, crate::model::MAX_QUERY_ROWS);
                if offset == 0 {
                    album_page(&[9, 2], 4, revisions)
                } else {
                    album_page(&[8, 3], 4, revisions)
                }
            },
            key,
            query.stamp(revisions),
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
        let revisions = crate::response::QueryRevisions {
            library: 8,
            structure: 4,
            ..Default::default()
        };
        for failure in 0..4 {
            let mut calls = 0;
            let result = resolve_album(
                |_| {
                    calls += 1;
                    if calls == 1 {
                        return album_page(&[9, 2], 4, revisions);
                    }
                    match failure {
                        0 => album_page(
                            &[8, 3],
                            4,
                            crate::response::QueryRevisions {
                                library: 9,
                                ..revisions
                            },
                        ),
                        1 => album_page(
                            &[8, 3],
                            4,
                            crate::response::QueryRevisions {
                                structure: 5,
                                ..revisions
                            },
                        ),
                        2 => album_page(&[], 4, revisions),
                        _ => crate::response::QueryResponse {
                            revisions,
                            result: Err(crate::response::QueryError::Message("read failed".into())),
                        },
                    }
                },
                key.clone(),
                query.stamp(revisions),
            );
            assert!(result.is_err());
            assert_eq!(calls, 2);
        }
    }
}
