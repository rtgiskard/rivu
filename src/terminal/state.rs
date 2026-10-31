use super::{
    paged::PagedList,
    render::track_label,
    worker::{QueryKind, QueryRequest, QueryResult},
};
use crate::{
    i18n::Localizer,
    keyboard::{self, Action, Context as KeyboardContext, Modifiers},
    projection::TuiSnapshot,
    query_tracking::QueryTracker,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use rivu_protocol::{
    model::{
        Command, DirectoryRow, LibraryRow, PlaybackStatus, PlaylistEntryRow, PlaylistSummary,
        Query, QueueRow,
    },
    response::ViewResponse,
};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

pub(super) struct TuiState {
    pub(super) snapshot: TuiSnapshot,
    pub(super) ui: UiState,
}

impl TuiState {
    pub(super) fn new(snapshot: TuiSnapshot, i18n: Arc<Localizer>) -> Self {
        let mut ui = UiState::new(i18n);
        ui.queue.total = snapshot.queue.total;
        ui.queue.loaded = true;
        ui.queue
            .selection
            .select((snapshot.queue.total != 0).then_some(0));
        Self { snapshot, ui }
    }

    pub(super) fn apply_snapshot(&mut self, next: TuiSnapshot) {
        let page_size_changed =
            self.snapshot.system.query_batch_size != next.system.query_batch_size;
        self.ui.sync_queue(&self.snapshot, &next);
        if page_size_changed {
            self.ui.queue.pages.clear();
            self.ui.queue.fetch_size = 0;
        }
        let active_changed = self
            .ui
            .view
            .sync_snapshot(&self.snapshot, &next, page_size_changed);
        if let Some(tree) = &mut self.ui.cached_library {
            tree.sync_snapshot(&self.snapshot, &next, page_size_changed);
        }
        if let Some(playlists) = &mut self.ui.cached_playlists {
            playlists.sync_snapshot(&self.snapshot, &next, page_size_changed);
        }
        self.ui.sync_details(&next);
        if active_changed || page_size_changed {
            self.ui.cancel_query();
        }
        if let Some(message) = snapshot_message(&self.snapshot, &next, &self.ui.i18n) {
            self.ui.set_notice(message, NoticeKind::Information);
        }
        self.snapshot = next;
    }
}

fn snapshot_message(
    previous: &TuiSnapshot,
    next: &TuiSnapshot,
    i18n: &Localizer,
) -> Option<String> {
    if previous.playback.status != next.playback.status {
        return Some(match next.playback.status {
            PlaybackStatus::Playing => i18n.text("terminal-playing").into(),
            PlaybackStatus::Paused => i18n.text("terminal-paused").into(),
            PlaybackStatus::Stopped => i18n.text("terminal-stopped").into(),
        });
    }
    if previous.queue.current_id != next.queue.current_id {
        return next.current_track().map(|track| {
            let label = track_label(&track.title, &track.artist);
            i18n.format("terminal-now-playing", &[("track", label.as_ref().into())])
        });
    }
    match next.queue.total.cmp(&previous.queue.total) {
        std::cmp::Ordering::Greater => Some(i18n.text("terminal-queue-added").into()),
        std::cmp::Ordering::Less => Some(i18n.text("terminal-queue-removed").into()),
        std::cmp::Ordering::Equal => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NoticeKind {
    Information,
    Warning,
    Error,
}

pub(super) struct Notice {
    pub(super) text: String,
    pub(super) kind: NoticeKind,
    pub(super) expires_at: Instant,
}

#[derive(Default)]
pub(super) struct Details {
    pub(super) target: Option<i64>,
    pub(super) source_label: &'static str,
    pub(super) state: DetailState,
    pub(super) stamp: Option<rivu_protocol::model::QueryStamp>,
    pub(super) scroll: usize,
    pub(super) max_scroll: usize,
}

#[derive(Default)]
pub(super) enum DetailState {
    #[default]
    Loading,
    Ready(Box<rivu_protocol::model::Track>),
    Removed,
    Error(String),
    Container(String),
}

#[derive(Default)]
pub(super) enum View {
    #[default]
    Queue,
    Search(LibrarySearch),
    Tree(LibraryTree),
    Playlists(PagedList<PlaylistSummary>),
    PlaylistDetail {
        playlists: (ListState, usize),
        detail: PlaylistDetail,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TopTab {
    Queue,
    Playlists,
    Library,
}

impl View {
    pub(super) fn tab(&self) -> TopTab {
        match self {
            Self::Queue => TopTab::Queue,
            Self::Playlists(_) | Self::PlaylistDetail { .. } => TopTab::Playlists,
            Self::Tree(_) | Self::Search(_) => TopTab::Library,
        }
    }

    fn query_kind(&self, selection: bool) -> Option<QueryKind> {
        match self {
            Self::Queue => Some(QueryKind::Queue),
            Self::Search(search) => Some(QueryKind::Search(search.applied_query.clone())),
            Self::Tree(tree) if tree.other_tracks => Some(tree.other_query()),
            Self::Tree(tree) if !tree.directory.as_os_str().is_empty() => {
                Some(QueryKind::Directory(tree.directory.clone()))
            }
            Self::Tree(_) => Some(QueryKind::OtherTracksCount),
            Self::Playlists(_) => Some(QueryKind::Playlists),
            Self::PlaylistDetail { detail, .. } => Some(match detail.entry_anchor {
                Some(entry_id) if selection => QueryKind::PlaylistSelection {
                    playlist_id: detail.playlist_id,
                    entry_id,
                },
                _ => QueryKind::Playlist(detail.playlist_id),
            }),
        }
    }

    // Hidden pages follow the same revision rules as the visible page, without
    // fetching data or disturbing the visible page's query generation.
    fn sync_snapshot(
        &mut self,
        previous: &TuiSnapshot,
        next: &TuiSnapshot,
        page_size_changed: bool,
    ) -> bool {
        let rows_changed = self
            .query_kind(false)
            .is_some_and(|kind| kind.changed(previous, next));
        let coordinates_changed = self
            .query_kind(false)
            .is_some_and(|kind| kind.changed_without_bitrate(previous, next));
        let query_changed = if matches!(self, Self::PlaylistDetail { detail, .. } if detail.entry_anchor.is_some())
        {
            self.query_kind(true)
                .is_some_and(|kind| kind.changed(previous, next))
        } else {
            rows_changed
        };
        if let Self::PlaylistDetail { detail, .. } = self {
            let compact = Query::PlaylistEntryLocations {
                query: None,
                playlist_id: detail.playlist_id,
                entry_ids: Vec::new(),
            };
            let scope = Some(detail.playlist_id);
            if compact.stamp(previous.query_revisions(scope))
                != compact.stamp(next.query_revisions(scope))
            {
                detail.entry_anchor = detail
                    .entry_anchor
                    .or(detail.selected_entry_id)
                    .or_else(|| detail.entries.selected().map(|entry| entry.id));
            }
            if !next
                .library
                .playlist_entry_revisions
                .contains_key(&detail.playlist_id)
            {
                let Self::PlaylistDetail { playlists, .. } = std::mem::take(self) else {
                    unreachable!();
                };
                *self = Self::Playlists(PagedList {
                    selection: playlists.0,
                    viewport_rows: playlists.1,
                    ..PagedList::default()
                });
                return true;
            }
        }
        if rows_changed && !coordinates_changed && !page_size_changed {
            self.invalidate_payload();
        } else if rows_changed || page_size_changed {
            match self {
                Self::Search(search) => search.results.clear(),
                Self::Tree(tree) => {
                    tree.other_count = None;
                    if tree.other_tracks {
                        tree.anchor = tree
                            .anchor
                            .or_else(|| {
                                tree.entries.selected().and_then(|entry| match entry {
                                    DirectoryRow::Track(track) => Some(track.id),
                                    _ => None,
                                })
                            })
                            .or(tree.selected_track_id);
                    }
                    tree.entries.clear();
                }
                Self::Playlists(playlists) => playlists.clear(),
                Self::PlaylistDetail { detail, .. } => detail.entries.clear(),
                Self::Queue => {}
            }
        }
        if previous.system.library_roots != next.system.library_roots
            && let Self::Tree(tree) = self
        {
            tree.roots = LibraryTree::roots(&next.system.library_roots);
            if !tree.other_tracks && tree.directory.as_os_str().is_empty() {
                tree.entries.clear();
            }
        }
        query_changed && !matches!(self, Self::Queue)
    }

    // Payload-only changes keep membership, counts and navigation coordinates.
    fn invalidate_payload(&mut self) {
        match self {
            Self::Search(search) => search.results.pages.clear(),
            Self::Tree(tree) => {
                tree.entries.pages.retain(|_, rows| {
                    !rows.iter().any(|row| matches!(row, DirectoryRow::Track(_)))
                });
            }
            Self::PlaylistDetail { detail, .. } => detail.entries.pages.clear(),
            Self::Queue => {}
            Self::Playlists(_) => {}
        }
    }
}

pub(super) struct UiState {
    pub(super) i18n: Arc<Localizer>,
    pub(super) view: View,
    // Each slot owns only its tab's view; switching moves bounded page caches.
    pub(super) cached_library: Option<View>,
    pub(super) cached_playlists: Option<View>,
    pub(super) queue: PagedList<QueueRow>,
    // Resolve the selected occurrence after edits before requesting its new viewport.
    pub(super) queue_anchor: Option<u64>,
    pub(super) queue_selected_id: Option<u64>,
    pub(super) show_help: bool,
    pub(super) help_max_scroll: usize,
    pub(super) help_scroll: usize,
    pub(super) message: Option<Notice>,
    // Started only after the initial IPC snapshot; None is permanently dismissed.
    pub(super) startup_hint_deadline: Option<Instant>,
    pub(super) details: Details,
    pub(super) details_active: bool,
    pub(super) requested: Option<QueryRequest>,
    pub(super) projection: QueryTracker,
    pub(super) failed_request: Option<QueryRequest>,
    pub(super) local_revision: u64,
    // Keyboard PageUp/PageDown distance follows the terminal viewport, not query limits.
    pub(super) page_size: usize,
}
impl UiState {
    pub(super) fn new(i18n: Arc<Localizer>) -> Self {
        Self {
            i18n,
            view: View::default(),
            cached_library: None,
            cached_playlists: None,
            queue: PagedList::default(),
            queue_anchor: None,
            queue_selected_id: None,
            show_help: false,
            help_max_scroll: 0,
            help_scroll: 0,
            message: None,
            startup_hint_deadline: Some(Instant::now() + Duration::from_secs(5)),
            details: Details::default(),
            details_active: false,
            requested: None,
            projection: QueryTracker::default(),
            failed_request: None,
            local_revision: 0,
            page_size: 0,
        }
    }

    pub(super) fn prepare_local_view(&mut self, state: &TuiSnapshot) -> bool {
        let View::Tree(tree) = &mut self.view else {
            return false;
        };
        if tree.other_tracks || !tree.directory.as_os_str().is_empty() {
            return false;
        }
        let limit = state.system.query_batch_size as usize;
        let mut changed = false;
        while let Some(offset) = tree.entries.next_offset(limit) {
            let rows = std::iter::once(DirectoryRow::OtherTracks)
                .chain(
                    tree.roots
                        .iter()
                        .cloned()
                        .map(|path| DirectoryRow::Directory { path }),
                )
                .skip(offset)
                .take(limit)
                .collect();
            tree.entries
                .insert(offset, limit, tree.roots.len() + 1, rows);
            changed = true;
        }
        changed
    }

    pub(super) fn query_request(&mut self, state: &TuiSnapshot) -> Option<QueryRequest> {
        if self.details_active {
            let kind = QueryKind::Track(self.details.target?);
            let query = kind.query(0, 1);
            let revisions = state.query_revisions(None).for_query(
                &query,
                &state.library.playlist_entry_revisions,
                &state.library.metadata_revisions,
            );
            let stamp = query.stamp(revisions);
            if self.details.stamp == Some(stamp) {
                return None;
            }
            self.details.state = DetailState::Loading;
            self.projection.sync(&query, revisions);
            return Some(QueryRequest {
                kind,
                offset: 0,
                limit: 1,
                generation: self.projection.generation(),
                stamp,
            });
        }
        if let View::Search(search) = &mut self.view
            && search.apply_pending(Instant::now())
        {
            self.cancel_query();
            self.mark_local_change();
        }
        let limit = state.system.query_batch_size as usize;
        let (kind, offset) = match &self.view {
            View::Queue if state.queue.total != 0 => match self.queue_anchor {
                Some(queue_id) => (QueryKind::QueueSelection(queue_id), 0),
                None => (QueryKind::Queue, self.queue.next_offset(limit)?),
            },
            View::Search(search) => {
                let offset = search.results.next_offset(limit)?;
                (QueryKind::Search(search.applied_query.clone()), offset)
            }
            View::Tree(tree) if tree.other_tracks => (
                tree.other_query(),
                if tree.anchor.is_some() {
                    0
                } else {
                    tree.entries.next_offset(limit)?
                },
            ),
            View::Tree(tree)
                if tree.directory.as_os_str().is_empty() && tree.other_count.is_none() =>
            {
                (QueryKind::OtherTracksCount, 0)
            }
            View::Tree(tree) if !tree.directory.as_os_str().is_empty() => (
                QueryKind::Directory(tree.directory.clone()),
                tree.entries.next_offset(limit)?,
            ),
            View::Playlists(playlists) => (QueryKind::Playlists, playlists.next_offset(limit)?),
            View::PlaylistDetail { detail, .. }
                if state
                    .library
                    .playlist_entry_revisions
                    .contains_key(&detail.playlist_id) =>
            {
                match detail.entry_anchor {
                    Some(entry_id) => (
                        QueryKind::PlaylistSelection {
                            playlist_id: detail.playlist_id,
                            entry_id,
                        },
                        0,
                    ),
                    None => (
                        QueryKind::Playlist(detail.playlist_id),
                        detail.entries.next_offset(limit)?,
                    ),
                }
            }
            _ => return None,
        };
        let query = kind.query(offset, limit);
        let revisions = state.query_revisions(kind.playlist_id());
        self.projection.sync(&query, revisions);
        let request = QueryRequest {
            kind,
            offset,
            limit,
            generation: self.projection.generation(),
            stamp: query.stamp(revisions),
        };
        if self.failed_request.as_ref() == Some(&request) {
            None
        } else {
            Some(request)
        }
    }

    pub(super) fn apply_query(
        &mut self,
        state: &TuiSnapshot,
        request: &QueryRequest,
        result: Result<QueryResult, String>,
    ) -> bool {
        if self.query_request(state).as_ref() != Some(request) {
            if self.requested.as_ref() == Some(request) {
                self.requested = None;
            }
            return false;
        }
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.requested = None;
                self.failed_request = Some(request.clone());
                if self.details_active {
                    self.details.stamp = Some(request.stamp);
                    self.details.state = DetailState::Error(error);
                    self.mark_local_change();
                    return true;
                }
                self.set_notice(error, NoticeKind::Error);
                return true;
            }
        };
        if !self
            .projection
            .accepts(&request.query(), request.generation, result.revisions)
        {
            self.requested = None;
            return false;
        }
        if self.details_active {
            self.details.state = match result.view {
                ViewResponse::Track(Some(track)) if Some(track.id) == self.details.target => {
                    DetailState::Ready(Box::new(track))
                }
                ViewResponse::Track(None) => DetailState::Removed,
                _ => DetailState::Error(self.i18n.text("terminal-unexpected-response").into()),
            };
            self.details.stamp = Some(request.stamp);
            self.requested = None;
            self.mark_local_change();
            return true;
        }
        match (&mut self.view, result.view) {
            (View::Queue, ViewResponse::QueuePage(page)) if request.kind == QueryKind::Queue => {
                self.queue
                    .insert(request.offset, request.limit, page.total, page.rows);
                self.queue_selected_id = self.queue.selected().map(|row| row.id);
            }
            (View::Queue, ViewResponse::QueueEntries(rows)) => {
                if let QueryKind::QueueSelection(queue_id) = request.kind {
                    if let Some(row) = rows.iter().find(|row| row.id == queue_id) {
                        self.queue.selection.select(Some(row.position));
                        self.queue.set_viewport(self.queue.viewport_rows);
                    }
                    self.queue_selected_id =
                        rows.iter().find(|row| row.id == queue_id).map(|row| row.id);
                    self.queue_anchor = None;
                } else {
                    self.set_notice(
                        self.i18n.text("terminal-unexpected-response").into(),
                        NoticeKind::Error,
                    );
                }
            }
            (View::Search(search), ViewResponse::LibraryPage(page)) => {
                search
                    .results
                    .insert(request.offset, request.limit, page.total, page.rows);
            }
            (View::Tree(tree), ViewResponse::LibraryPage(page)) if tree.other_tracks => {
                tree.entries.insert(
                    request.offset,
                    request.limit,
                    page.total,
                    page.rows.into_iter().map(DirectoryRow::Track).collect(),
                );
                tree.selected_track_id = tree.entries.selected().and_then(|entry| match entry {
                    DirectoryRow::Track(track) => Some(track.id),
                    _ => None,
                });
            }
            (View::Tree(tree), ViewResponse::OtherTracksCount(count)) => {
                tree.other_count = Some(count)
            }
            (View::Tree(tree), ViewResponse::LibraryLocate(location)) if tree.other_tracks => {
                let anchor = tree.anchor.take();
                tree.selected_track_id = None;
                if let rivu_protocol::model::LibraryLocateResult::Located(location) = location {
                    tree.selected_track_id = anchor;
                    tree.entries.total = location.total;
                    tree.entries.selection.select(Some(location.offset));
                    tree.entries.set_viewport(tree.entries.viewport_rows);
                }
            }
            (View::Tree(tree), ViewResponse::DirectoryPage(page)) => {
                tree.entries
                    .insert(request.offset, request.limit, page.total, page.rows);
            }
            (View::Playlists(playlists), ViewResponse::PlaylistSummaries(page)) => {
                playlists.insert(request.offset, request.limit, page.total, page.rows);
            }
            (View::PlaylistDetail { detail, .. }, ViewResponse::PlaylistEntries(page)) => {
                detail
                    .entries
                    .insert(request.offset, request.limit, page.total, page.rows);
                detail.selected_entry_id = detail.entries.selected().map(|entry| entry.id);
            }
            (
                View::PlaylistDetail { detail, .. },
                ViewResponse::PlaylistEntryLocations(entries),
            ) => {
                if let QueryKind::PlaylistSelection {
                    playlist_id,
                    entry_id,
                } = request.kind
                {
                    let entry = entries
                        .into_iter()
                        .find(|entry| entry.id == entry_id && entry.playlist_id == playlist_id);
                    detail.selected_entry_id = entry.as_ref().map(|entry| entry.id);
                    if let Some(entry) = entry {
                        detail.entries.total = entry.total;
                        detail.entries.loaded = true;
                        detail.entries.selection.select(Some(entry.position));
                        detail.entries.set_viewport(detail.entries.viewport_rows);
                    }
                    detail.entry_anchor = None;
                } else {
                    self.set_notice(
                        self.i18n.text("terminal-unexpected-response").into(),
                        NoticeKind::Error,
                    );
                }
            }
            _ => {
                self.set_notice(
                    self.i18n.text("terminal-unexpected-response").into(),
                    NoticeKind::Error,
                );
                return true;
            }
        }
        self.mark_local_change();
        true
    }

    fn mark_local_change(&mut self) {
        self.local_revision = self.local_revision.wrapping_add(1);
    }

    fn open_details(&mut self) {
        let (target, source_label, container) = match &self.view {
            View::Queue => (
                self.queue.selected().map(|row| row.track.id),
                "shell-panel-queue",
                None,
            ),
            View::Search(search) => (
                search.results.selected().map(|track| track.id),
                "terminal-search",
                None,
            ),
            View::PlaylistDetail { detail, .. } => (
                detail.entries.selected().map(|entry| entry.track_id),
                "shell-panel-playlists",
                None,
            ),
            View::Tree(tree) => match tree.entries.selected() {
                Some(DirectoryRow::Track(track)) => (Some(track.id), "shell-panel-library", None),
                Some(DirectoryRow::Directory { path }) => (
                    None,
                    "shell-panel-library",
                    Some(path.to_string_lossy().into_owned()),
                ),
                Some(DirectoryRow::OtherTracks) => (
                    None,
                    "library-other",
                    Some(self.i18n.text("library-other").into()),
                ),
                None => (None, "shell-panel-library", None),
            },
            View::Playlists(playlists) => (
                None,
                "shell-panel-playlists",
                playlists.selected().map(|playlist| playlist.name.clone()),
            ),
        };
        self.details = Details {
            target,
            source_label,
            state: container.map_or(DetailState::Loading, DetailState::Container),
            ..Details::default()
        };
        self.details_active = true;
        self.startup_hint_deadline = None;
        self.cancel_query();
        self.mark_local_change();
    }

    pub(super) fn set_notice(&mut self, text: String, kind: NoticeKind) {
        self.startup_hint_deadline = None;
        // Keep actionable failures visible when an unrelated playback update arrives.
        if kind == NoticeKind::Information
            && self.message.as_ref().is_some_and(|notice| {
                notice.kind != NoticeKind::Information && Instant::now() < notice.expires_at
            })
        {
            return;
        }
        self.message = Some(Notice {
            text,
            kind,
            expires_at: Instant::now() + Duration::from_secs(10),
        });
        self.mark_local_change();
    }

    pub(super) fn expire_message(&mut self) -> bool {
        if self
            .message
            .as_ref()
            .is_some_and(|notice| Instant::now() >= notice.expires_at)
        {
            self.message = None;
            return true;
        }
        false
    }

    pub(super) fn expire_startup_hint(&mut self, now: Instant) -> bool {
        if self
            .startup_hint_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.startup_hint_deadline = None;
            return true;
        }
        false
    }

    fn sync_details(&mut self, next: &TuiSnapshot) {
        let Some(track_id) = self.details.target else {
            return;
        };
        let query = Query::Track { track_id };
        let revisions = next.query_revisions(None).for_query(
            &query,
            &next.library.playlist_entry_revisions,
            &next.library.metadata_revisions,
        );
        if self
            .details
            .stamp
            .is_some_and(|stamp| stamp != query.stamp(revisions))
        {
            self.details.state = DetailState::Loading;
            self.details.stamp = None;
            if self.details_active {
                self.cancel_query();
            }
            self.mark_local_change();
        }
    }

    fn sync_queue(&mut self, previous: &TuiSnapshot, next: &TuiSnapshot) {
        if !QueryKind::Queue.changed(previous, next) {
            return;
        }
        if !QueryKind::Queue.changed_without_bitrate(previous, next) {
            self.queue.pages.clear();
            if matches!(self.view, View::Queue) {
                self.cancel_query();
            }
            return;
        }
        self.queue_anchor = self
            .queue_anchor
            .or_else(|| self.queue.selected().map(|row| row.id))
            .or(self.queue_selected_id);
        let selected = self.queue.selection.selected().unwrap_or(0);
        self.queue.clear();
        self.queue.total = next.queue.total;
        self.queue.loaded = true;
        self.queue
            .selection
            .select((next.queue.total != 0).then(|| selected.min(next.queue.total - 1)));
        if next.queue.total == 0 {
            self.queue_anchor = None;
            self.queue_selected_id = None;
        }
        self.queue.set_viewport(self.queue.viewport_rows);
        if matches!(self.view, View::Queue) {
            self.cancel_query();
        }
    }

    fn cancel_query(&mut self) {
        self.requested = None;
        self.failed_request = None;
        self.projection.cancel();
    }

    pub(super) fn open_search(&mut self) {
        self.store_page();
        self.view = View::Search(LibrarySearch::default());
        self.cancel_query();
        self.mark_local_change();
    }

    fn store_page(&mut self) {
        match std::mem::take(&mut self.view) {
            view @ View::Tree(_) => self.cached_library = Some(view),
            view @ (View::Playlists(_) | View::PlaylistDetail { .. }) => {
                self.cached_playlists = Some(view);
            }
            View::Queue | View::Search(_) => {}
        }
    }

    fn switch_tab(&mut self, backward: bool, state: &TuiSnapshot) {
        let target = match (self.view.tab(), backward) {
            (TopTab::Queue, false) | (TopTab::Library, true) => TopTab::Playlists,
            (TopTab::Playlists, false) | (TopTab::Queue, true) => TopTab::Library,
            (TopTab::Library, false) | (TopTab::Playlists, true) => TopTab::Queue,
        };
        self.store_page();
        self.view = match target {
            TopTab::Queue => View::Queue,
            TopTab::Playlists => self
                .cached_playlists
                .take()
                .unwrap_or_else(|| View::Playlists(PagedList::default())),
            TopTab::Library => self
                .cached_library
                .take()
                .unwrap_or_else(|| View::Tree(LibraryTree::new(&state.system.library_roots))),
        };
        self.startup_hint_deadline = None;
        self.cancel_query();
        self.mark_local_change();
    }

    pub(super) fn keyboard_context(&self) -> KeyboardContext {
        match &self.view {
            View::Queue => KeyboardContext::TuiQueue,
            View::Search(search) if search.focus == SearchFocus::Query => {
                KeyboardContext::TuiSearchQuery
            }
            View::Search(_) => KeyboardContext::TuiSearchResults,
            View::Tree(_) => KeyboardContext::TuiTree,
            View::Playlists(_) => KeyboardContext::TuiPlaylists,
            View::PlaylistDetail { .. } => KeyboardContext::TuiPlaylistTracks,
        }
    }

    pub(super) fn key_action(&mut self, key: KeyEvent, state: &TuiSnapshot) -> KeyAction {
        // Global terminal exit wins over editors and modal input ownership.
        if resolve_key(key, KeyboardContext::TuiHelp) == Some(Action::Close) {
            return KeyAction::Quit;
        }
        if self.details_active {
            let details = &mut self.details;
            match resolve_key(key, KeyboardContext::TuiHelp) {
                Some(Action::Cancel) => {
                    self.details_active = false;
                    self.cancel_query();
                }
                Some(Action::Up) => details.scroll = details.scroll.saturating_sub(1),
                Some(Action::Down) => {
                    details.scroll = details.scroll.saturating_add(1).min(details.max_scroll)
                }
                Some(Action::PageUp) => {
                    details.scroll = details.scroll.saturating_sub(self.page_size)
                }
                Some(Action::PageDown) => {
                    details.scroll = details
                        .scroll
                        .saturating_add(self.page_size)
                        .min(details.max_scroll)
                }
                Some(Action::First) => details.scroll = 0,
                Some(Action::Last) => details.scroll = details.max_scroll,
                _ => return KeyAction::Ignored,
            }
            self.mark_local_change();
            return KeyAction::Ignored;
        }
        let context = self.keyboard_context();
        if self.show_help {
            let action = resolve_key(key, KeyboardContext::TuiHelp);
            match action {
                Some(Action::Help | Action::Cancel) => self.show_help = false,
                Some(Action::Up) => self.help_scroll = self.help_scroll.saturating_sub(1),
                Some(Action::Down) => {
                    self.help_scroll = self.help_scroll.saturating_add(1).min(self.help_max_scroll)
                }
                Some(Action::PageUp) => {
                    self.help_scroll = self.help_scroll.saturating_sub(self.page_size)
                }
                Some(Action::PageDown) => {
                    self.help_scroll = self
                        .help_scroll
                        .saturating_add(self.page_size)
                        .min(self.help_max_scroll)
                }
                Some(Action::First) => self.help_scroll = 0,
                Some(Action::Last) => self.help_scroll = self.help_max_scroll,
                _ => return KeyAction::Ignored,
            }
            self.mark_local_change();
            return KeyAction::Ignored;
        }
        if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Char('x') {
            return if state.system.scanning
                && !state.system.scan_progress.as_ref().is_some_and(|progress| {
                    progress.phase == rivu_protocol::model::ScanPhase::Saving
                })
                && let Some(id) = state.system.scan_task_id
            {
                KeyAction::Command(Command::CancelScan { id })
            } else {
                KeyAction::Ignored
            };
        }
        if let View::Tree(tree) = &mut self.view
            && let Some(result) = tree.other_key(key)
        {
            self.cancel_query();
            self.mark_local_change();
            return result;
        }
        // The query editor owns all printable characters, including keys that
        // control playback elsewhere. Control/Alt/Super chords are never text.
        if context == KeyboardContext::TuiSearchQuery
            && !key.modifiers.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER
                    | KeyModifiers::HYPER
                    | KeyModifiers::META,
            )
            && let KeyCode::Char(character) = key.code
        {
            if let View::Search(search) = &mut self.view {
                search.query.push(character);
                if search.filter(Instant::now()) {
                    self.cancel_query();
                }
            }
            self.mark_local_change();
            return KeyAction::Ignored;
        }
        let Some(action) = resolve_key(key, context) else {
            return KeyAction::Ignored;
        };
        if matches!(action, Action::Help | Action::Search | Action::Traverse) {
            self.startup_hint_deadline = None;
        }
        match action {
            Action::Help => {
                self.show_help = true;
                self.help_scroll = 0;
                self.mark_local_change();
                return KeyAction::Ignored;
            }
            Action::Close => return KeyAction::Quit,
            Action::Details => {
                self.open_details();
                return KeyAction::Ignored;
            }
            Action::Favorite => {
                let track_id = match &self.view {
                    View::Queue => self.queue.selected().map(|row| row.track.id),
                    View::Search(search) => search.results.selected().map(|track| track.id),
                    View::PlaylistDetail { detail, .. } => {
                        detail.entries.selected().map(|entry| entry.track_id)
                    }
                    View::Tree(tree) => tree.entries.selected().and_then(|entry| match entry {
                        DirectoryRow::Track(track) => Some(track.id),
                        DirectoryRow::Directory { .. } | DirectoryRow::OtherTracks => None,
                    }),
                    View::Playlists(_) => None,
                };
                return track_id.map_or(KeyAction::Ignored, |track_id| {
                    KeyAction::Command(Command::ToggleFavorite {
                        track_ids: vec![track_id],
                    })
                });
            }
            Action::Search => return KeyAction::Search,
            Action::Traverse if !matches!(self.view, View::Search(_)) => {
                self.switch_tab(
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT),
                    state,
                );
                return KeyAction::Ignored;
            }
            _ => {}
        }
        if let Some(command) =
            keyboard::playback_command(action, &state.playback, state.queue.current_id)
        {
            return KeyAction::Command(command);
        }
        let page_size = self.page_size;
        let view = std::mem::take(&mut self.view);
        let mut changed = false;
        let mut command = None;
        match view {
            View::Search(mut search) => {
                let close = action == Action::Cancel && search.focus == SearchFocus::Query;
                match action {
                    Action::Traverse => {
                        search.focus = match search.focus {
                            SearchFocus::Query => SearchFocus::Results,
                            SearchFocus::Results => SearchFocus::Query,
                        };
                        changed = true;
                    }
                    Action::Cancel if !close => {
                        search.focus = SearchFocus::Query;
                        changed = true;
                    }
                    Action::Activate if search.focus == SearchFocus::Results => {
                        command = search
                            .results
                            .selected()
                            .map(|track| Command::Play { track_id: track.id });
                    }
                    Action::Activate => {
                        search.focus = SearchFocus::Results;
                        changed = true;
                    }
                    Action::Backspace => {
                        if search.query.pop().is_some() {
                            if search.filter(Instant::now()) {
                                self.cancel_query();
                            }
                            changed = true;
                        }
                    }
                    _ => changed = search.results.navigate_action(action, page_size),
                }
                self.view = if close {
                    self.cancel_query();
                    View::Queue
                } else {
                    View::Search(search)
                };
                changed |= close;
            }
            View::Tree(mut tree) => {
                let mut close = false;
                match action {
                    Action::Cancel | Action::Parent => {
                        changed = tree.parent();
                        close = action == Action::Cancel && !changed;
                        if changed || close {
                            self.cancel_query();
                        }
                    }
                    Action::Activate | Action::Expand => {
                        let directory = tree.directory.clone();
                        let was_other = tree.other_tracks;
                        if action == Action::Activate
                            || matches!(
                                tree.entries.selected(),
                                Some(DirectoryRow::Directory { .. } | DirectoryRow::OtherTracks)
                            )
                        {
                            command = tree.activate();
                        }
                        changed = tree.directory != directory || tree.other_tracks != was_other;
                        if changed {
                            self.cancel_query();
                        }
                    }
                    _ => {
                        changed = tree.entries.navigate_action(action, page_size);
                        if changed && tree.other_tracks {
                            tree.anchor = None;
                            tree.selected_track_id =
                                tree.entries.selected().and_then(|entry| match entry {
                                    DirectoryRow::Track(track) => Some(track.id),
                                    _ => None,
                                });
                        }
                    }
                }
                self.view = if close {
                    self.cached_library = Some(View::Tree(tree));
                    View::Queue
                } else {
                    View::Tree(tree)
                };
                changed |= close;
            }
            View::PlaylistDetail {
                playlists,
                mut detail,
            } => {
                let close = matches!(action, Action::Cancel | Action::Parent);
                match action {
                    Action::Activate => {
                        command = detail.entries.selected().map(|entry| Command::Play {
                            track_id: entry.track_id,
                        })
                    }
                    Action::Enqueue => {
                        command = detail.entries.selected().map(|entry| Command::Enqueue {
                            track_ids: vec![entry.track_id],
                        })
                    }
                    Action::Remove => {
                        command = detail
                            .entries
                            .selected()
                            .map(|entry| Command::RemovePlaylistEntry { entry_id: entry.id })
                    }
                    _ => {
                        changed = detail.entries.navigate_action(action, page_size);
                        if changed {
                            detail.entry_anchor = None;
                            detail.selected_entry_id =
                                detail.entries.selected().map(|entry| entry.id);
                        }
                    }
                }
                self.view = if close {
                    self.cancel_query();
                    View::Playlists(PagedList {
                        selection: playlists.0,
                        viewport_rows: playlists.1,
                        ..PagedList::default()
                    })
                } else {
                    View::PlaylistDetail { playlists, detail }
                };
                changed |= close;
            }
            View::Playlists(mut playlists) => {
                let close = action == Action::Cancel;
                match action {
                    Action::Expand => {
                        if let Some(playlist) = playlists.selected() {
                            let detail = PlaylistDetail {
                                playlist_id: playlist.id,
                                name: playlist.name.clone(),
                                entries: PagedList::default(),
                                entry_anchor: None,
                                selected_entry_id: None,
                            };
                            self.view = View::PlaylistDetail {
                                playlists: (playlists.selection, playlists.viewport_rows),
                                detail,
                            };
                            self.cancel_query();
                            self.mark_local_change();
                            return KeyAction::Ignored;
                        }
                    }
                    Action::Activate => {
                        command = playlists.selected().map(|playlist| Command::PlayPlaylist {
                            playlist_id: playlist.id,
                        })
                    }
                    _ => changed = playlists.navigate_action(action, page_size),
                }
                self.view = if close {
                    self.cancel_query();
                    self.cached_playlists = Some(View::Playlists(playlists));
                    View::Queue
                } else {
                    View::Playlists(playlists)
                };
                changed |= close;
            }
            View::Queue => match action {
                Action::Activate => {
                    command = self
                        .queue
                        .selected()
                        .map(|entry| Command::PlayQueue { queue_id: entry.id })
                }
                Action::Remove => {
                    command = self
                        .queue
                        .selected()
                        .map(|entry| Command::RemoveQueue { queue_id: entry.id })
                }
                _ => {
                    changed = self.queue.navigate_action(action, page_size);
                    if changed {
                        self.queue_anchor = None;
                        self.queue_selected_id = self.queue.selected().map(|row| row.id);
                    }
                }
            },
        }
        if changed {
            if matches!(action, Action::First | Action::Last) {
                self.cancel_query();
            }
            self.mark_local_change();
        }
        command.map_or(KeyAction::Ignored, KeyAction::Command)
    }
}

#[cfg(test)]
impl Default for UiState {
    fn default() -> Self {
        Self::new(Arc::new(Localizer::for_terminal("en-US")))
    }
}

pub(super) struct PlaylistDetail {
    pub(super) playlist_id: i64,
    pub(super) name: String,
    pub(super) entries: PagedList<PlaylistEntryRow>,
    pub(super) entry_anchor: Option<i64>,
    pub(super) selected_entry_id: Option<i64>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum SearchFocus {
    #[default]
    Query,
    Results,
}

#[derive(Default)]
pub(super) struct LibrarySearch {
    pub(super) query: String,
    pub(super) applied_query: String,
    pub(super) results: PagedList<LibraryRow>,
    pub(super) focus: SearchFocus,
    pub(super) query_deadline: Option<Instant>,
}

impl LibrarySearch {
    fn filter(&mut self, now: Instant) -> bool {
        let query = self.query.trim();
        if query == self.applied_query {
            self.query_deadline = None;
            return false;
        }
        if query.is_empty() {
            self.query_deadline = None;
            self.applied_query.clear();
            self.results = PagedList::default();
            return true;
        }
        self.query_deadline = Some(now + rivu_protocol::model::SEARCH_DEBOUNCE);
        false
    }

    fn apply_pending(&mut self, now: Instant) -> bool {
        if !self.query_deadline.is_some_and(|deadline| now >= deadline) {
            return false;
        }
        self.query_deadline = None;
        let query = self.query.trim();
        if query == self.applied_query {
            return false;
        }
        self.applied_query.clear();
        self.applied_query.push_str(query);
        self.results = PagedList::default();
        true
    }
}

/// Only the active directory's bounded pages are retained. Ancestors store
/// navigation coordinates, not directory contents.
#[derive(Default)]
pub(super) struct LibraryTree {
    pub(super) directory: PathBuf,
    pub(super) roots: Vec<PathBuf>,
    pub(super) ancestors: Vec<(PathBuf, ListState, usize)>,
    pub(super) entries: PagedList<DirectoryRow>,
    pub(super) other_tracks: bool,
    pub(super) query: String,
    pub(super) editing_query: bool,
    pub(super) favorite: Option<bool>,
    pub(super) missing: Option<bool>,
    pub(super) sort: rivu_protocol::model::LibrarySort,
    pub(super) anchor: Option<i64>,
    pub(super) selected_track_id: Option<i64>,
    pub(super) other_count: Option<usize>,
}

impl LibraryTree {
    fn other_query(&self) -> QueryKind {
        QueryKind::OtherTracks {
            query: self.query.clone(),
            favorite: self.favorite,
            missing: self.missing,
            sort: self.sort,
            anchor: self.anchor,
        }
    }
    fn other_key(&mut self, key: KeyEvent) -> Option<KeyAction> {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let target =
            self.other_tracks || matches!(self.entries.selected(), Some(DirectoryRow::OtherTracks));
        if target && alt {
            match key.code {
                KeyCode::Char('c' | 'C') => {
                    return Some(KeyAction::Command(Command::CheckOtherTracks {
                        force: key.modifiers.contains(KeyModifiers::SHIFT),
                    }));
                }
                KeyCode::Char('e') => {
                    return Some(KeyAction::Command(Command::EnqueueSources {
                        directories: Vec::new(),
                        track_ids: Vec::new(),
                        other_tracks: true,
                    }));
                }
                _ => {}
            }
        }
        if !self.other_tracks {
            return None;
        }
        let mut refresh = false;
        if self.editing_query && !alt && !key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Enter | KeyCode::Esc => self.editing_query = false,
                KeyCode::Backspace => refresh = self.query.pop().is_some(),
                KeyCode::Char(character) => {
                    self.query.push(character);
                    refresh = true;
                }
                _ => return None,
            }
        } else {
            match (alt, key.code) {
                (false, KeyCode::Char('/')) => self.editing_query = true,
                (true, KeyCode::Char('f')) => {
                    self.favorite = if self.favorite.is_some() {
                        None
                    } else {
                        Some(true)
                    };
                    refresh = true;
                }
                (true, KeyCode::Char('m')) => {
                    self.missing = if self.missing.is_some() {
                        None
                    } else {
                        Some(true)
                    };
                    refresh = true;
                }
                (true, KeyCode::Char('s')) => {
                    self.sort = match self.sort {
                        rivu_protocol::model::LibrarySort::Id => {
                            rivu_protocol::model::LibrarySort::Album
                        }
                        rivu_protocol::model::LibrarySort::Album => {
                            rivu_protocol::model::LibrarySort::MostPlayed
                        }
                        rivu_protocol::model::LibrarySort::MostPlayed => {
                            rivu_protocol::model::LibrarySort::Id
                        }
                    };
                    refresh = true;
                }
                (false, KeyCode::Char('e')) => {
                    return Some(
                        self.entries
                            .selected()
                            .and_then(|entry| match entry {
                                DirectoryRow::Track(track) => {
                                    Some(KeyAction::Command(Command::Enqueue {
                                        track_ids: vec![track.id],
                                    }))
                                }
                                _ => None,
                            })
                            .unwrap_or(KeyAction::Ignored),
                    );
                }
                _ => return None,
            }
        }
        if refresh {
            self.anchor = self
                .anchor
                .or_else(|| {
                    self.entries.selected().and_then(|entry| match entry {
                        DirectoryRow::Track(track) => Some(track.id),
                        _ => None,
                    })
                })
                .or(self.selected_track_id);
            let selection = std::mem::take(&mut self.entries.selection);
            let viewport_rows = self.entries.viewport_rows;
            self.entries = PagedList {
                selection,
                viewport_rows,
                ..PagedList::default()
            };
        }
        Some(KeyAction::Ignored)
    }

    fn roots(configured: &[PathBuf]) -> Vec<PathBuf> {
        let mut roots: Vec<_> = configured
            .iter()
            .filter(|path| !path.as_os_str().is_empty())
            .cloned()
            .collect();
        roots.sort();
        roots.dedup();
        roots
    }

    fn new(configured: &[PathBuf]) -> Self {
        Self {
            roots: Self::roots(configured),
            ..Self::default()
        }
    }

    fn activate(&mut self) -> Option<Command> {
        match self.entries.selected()? {
            DirectoryRow::OtherTracks => {
                self.other_tracks = true;
                self.ancestors.push((
                    PathBuf::new(),
                    std::mem::take(&mut self.entries.selection),
                    self.entries.viewport_rows,
                ));
                self.entries = PagedList::default();
                None
            }
            DirectoryRow::Directory { path } => {
                let path = path.clone();
                self.ancestors.push((
                    std::mem::replace(&mut self.directory, path),
                    std::mem::take(&mut self.entries.selection),
                    self.entries.viewport_rows,
                ));
                self.entries = PagedList::default();
                None
            }
            DirectoryRow::Track(track) => Some(Command::Play { track_id: track.id }),
        }
    }

    fn parent(&mut self) -> bool {
        let Some((directory, selection, viewport_rows)) = self.ancestors.pop() else {
            return false;
        };
        self.directory = directory;
        self.other_tracks = false;
        self.editing_query = false;
        self.anchor = None;
        self.selected_track_id = None;
        self.entries = PagedList {
            selection,
            viewport_rows,
            ..PagedList::default()
        };
        true
    }
}

pub(super) enum KeyAction {
    Quit,
    Search,
    Command(Command),
    Ignored,
}

pub(super) fn resolve_key(key: KeyEvent, context: KeyboardContext) -> Option<Action> {
    if key
        .modifiers
        .intersects(KeyModifiers::HYPER | KeyModifiers::META)
    {
        return None;
    }
    let modifiers = Modifiers {
        control: key.modifiers.contains(KeyModifiers::CONTROL),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        shift: key.modifiers.contains(KeyModifiers::SHIFT) || key.code == KeyCode::BackTab,
        platform: key.modifiers.contains(KeyModifiers::SUPER),
        function: false,
    };
    let mut encoded = [0_u8; 4];
    let name = match key.code {
        KeyCode::Char(' ') => "space",
        KeyCode::Char(character) => character.encode_utf8(&mut encoded),
        KeyCode::Left => "left",
        KeyCode::Right => "right",
        KeyCode::Up => "up",
        KeyCode::Down => "down",
        KeyCode::Home => "home",
        KeyCode::End => "end",
        KeyCode::PageUp => "pageup",
        KeyCode::PageDown => "pagedown",
        KeyCode::Enter => "enter",
        KeyCode::Esc => "escape",
        KeyCode::Tab | KeyCode::BackTab => "tab",
        KeyCode::Backspace => "backspace",
        KeyCode::Delete => "delete",
        KeyCode::F(1) => "f1",
        _ => return None,
    };
    keyboard::resolve(name, modifiers, context)
}

#[cfg(test)]
mod search_tests;

#[cfg(test)]
pub(super) mod tests;
