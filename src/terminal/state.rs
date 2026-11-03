use super::{
    paged::PagedList,
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
        Command, DirectoryRow, HistoryEntry, LibraryRow, PlaylistEntryRow, PlaylistSummary, Query,
        QueueRow,
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
        let selected = self
            .ui
            .view
            .query_kind(false)
            .filter(|kind| {
                kind.clone().changed_without_bitrate(
                    &self.snapshot,
                    &next,
                    &self.ui.search.applied_query,
                )
            })
            .and_then(|_| self.ui.selected_identity());
        self.ui.sync_queue(&self.snapshot, &next);
        if page_size_changed {
            self.ui.queue.pages.clear();
            self.ui.queue.fetch_size = 0;
        }
        let was_playlist_detail = matches!(self.ui.view, View::PlaylistDetail { .. });
        let active_changed = self.ui.view.sync_snapshot(
            &self.snapshot,
            &next,
            page_size_changed,
            &self.ui.search.applied_query,
        );
        if was_playlist_detail && matches!(self.ui.view, View::Playlists(_)) {
            self.ui.clear_search(&next);
        }
        if let Some(tree) = &mut self.ui.cached_library {
            tree.sync_snapshot(&self.snapshot, &next, page_size_changed, "");
        }
        if let Some(playlists) = &mut self.ui.cached_playlists {
            playlists.sync_snapshot(&self.snapshot, &next, page_size_changed, "");
        }
        if let Some(recent) = &mut self.ui.cached_recent {
            recent.sync_snapshot(&self.snapshot, &next, page_size_changed, "");
        }
        self.ui.sync_details(&next);
        if active_changed || page_size_changed {
            if self.ui.refresh_anchor.is_none() {
                self.ui.refresh_anchor = selected;
            }
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
pub(super) struct RecentTracks {
    pub(super) source: Arc<Vec<HistoryEntry>>,
    pub(super) entries: PagedList<usize>,
}

impl RecentTracks {
    fn selected(&self) -> Option<&HistoryEntry> {
        self.source.get(*self.entries.selected()?)
    }

    fn sync(&mut self, source: &Arc<Vec<HistoryEntry>>, query: &str) {
        if self.entries.loaded && Arc::ptr_eq(&self.source, source) {
            return;
        }
        let selected_id = self.selected().map(|entry| entry.track_id);
        let old_position = self.entries.selection.selected().unwrap_or(0);
        let mut indices = self.entries.pages.remove(&0).unwrap_or_default();
        indices.clear();
        let query = query.to_lowercase();
        indices.extend((0..source.len().min(256)).filter(|&index| {
            query.is_empty() || source[index].title.to_lowercase().contains(&query)
        }));
        indices.sort_unstable_by_key(|&index| {
            (
                std::cmp::Reverse(source[index].played_at),
                std::cmp::Reverse(source[index].track_id),
            )
        });
        let selected = selected_id
            .and_then(|id| {
                indices
                    .iter()
                    .position(|&index| source[index].track_id == id)
            })
            .unwrap_or(old_position.min(indices.len().saturating_sub(1)));
        self.source = Arc::clone(source);
        let total = indices.len();
        self.entries.clear();
        self.entries
            .selection
            .select((total != 0).then_some(selected));
        self.entries.insert(0, total.max(1), total, indices);
    }
}

#[derive(Default)]
pub(super) enum View {
    #[default]
    Queue,
    LibraryResults(PagedList<LibraryRow>),
    Tree(LibraryTree),
    Playlists(PagedList<PlaylistSummary>),
    Recent(RecentTracks),
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
    Recent,
}

impl View {
    pub(super) fn tab(&self) -> TopTab {
        match self {
            Self::Queue => TopTab::Queue,
            Self::Playlists(_) | Self::PlaylistDetail { .. } => TopTab::Playlists,
            Self::Tree(_) | Self::LibraryResults(_) => TopTab::Library,
            Self::Recent(_) => TopTab::Recent,
        }
    }

    fn query_kind(&self, selection: bool) -> Option<QueryKind> {
        match self {
            Self::Recent(_) => None,
            Self::Queue => Some(QueryKind::Queue),
            Self::LibraryResults(_) => Some(QueryKind::Search(String::new())),
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
        query: &str,
    ) -> bool {
        if let Self::Recent(recent) = self {
            recent.sync(&next.library.history, query);
            return false;
        }
        let rows_changed = self
            .query_kind(false)
            .is_some_and(|kind| kind.changed(previous, next, query));
        let coordinates_changed = self
            .query_kind(false)
            .is_some_and(|kind| kind.changed_without_bitrate(previous, next, query));
        let query_changed = if matches!(self, Self::PlaylistDetail { detail, .. } if detail.entry_anchor.is_some())
        {
            self.query_kind(true)
                .is_some_and(|kind| kind.changed(previous, next, query))
        } else {
            rows_changed
        };
        if let Self::PlaylistDetail { detail, .. } = self {
            let compact = Query::PlaylistEntryLocations {
                query: (!query.is_empty()).then(|| query.to_owned()),
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
        if let Self::Tree(tree) = self
            && QueryKind::OtherTracksCount.changed(previous, next, "")
        {
            tree.other_present = None;
        }
        if rows_changed && !coordinates_changed && !page_size_changed {
            self.invalidate_payload();
        } else if rows_changed || page_size_changed {
            match self {
                Self::LibraryResults(results) => results.clear(),
                Self::Tree(tree) => {
                    if tree.other_tracks || !tree.directory.as_os_str().is_empty() {
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
                Self::Queue | Self::Recent(_) => {}
            }
        }
        if previous.system.library_roots != next.system.library_roots
            && let Self::Tree(tree) = self
        {
            tree.roots = LibraryTree::roots(&next.system.library_roots);
            tree.other_present = None;
            if !tree.other_tracks && tree.directory.as_os_str().is_empty() {
                tree.entries.clear();
            }
        }
        query_changed && !matches!(self, Self::Queue)
    }

    // Payload-only changes keep membership, counts and navigation coordinates.
    fn invalidate_payload(&mut self) {
        match self {
            Self::LibraryResults(results) => results.pages.clear(),
            Self::Tree(tree) => {
                tree.entries.pages.retain(|_, rows| {
                    !rows.iter().any(|row| matches!(row, DirectoryRow::Track(_)))
                });
            }
            Self::PlaylistDetail { detail, .. } => detail.entries.pages.clear(),
            Self::Queue | Self::Recent(_) => {}
            Self::Playlists(_) => {}
        }
    }
}

pub(super) struct UiState {
    pub(super) i18n: Arc<Localizer>,
    pub(super) view: View,
    pub(super) search: SearchState,
    refresh_anchor: Option<i64>,
    library_selected_id: Option<i64>,
    // Each slot owns only its tab's view; switching moves bounded page caches.
    pub(super) cached_library: Option<View>,
    pub(super) cached_playlists: Option<View>,
    pub(super) cached_recent: Option<View>,
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
            search: SearchState::default(),
            refresh_anchor: None,
            library_selected_id: None,
            cached_library: None,
            cached_playlists: None,
            cached_recent: None,
            queue: PagedList::default(),
            queue_anchor: None,
            queue_selected_id: None,
            show_help: false,
            help_max_scroll: 0,
            help_scroll: 0,
            message: None,
            startup_hint_deadline: Some(Instant::now() + Duration::from_secs(4)),
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
        // Do not clamp the saved selection against a provisional root-only total.
        let Some(other_present) = tree.other_present else {
            return false;
        };
        let limit = state.system.query_batch_size as usize;
        let mut changed = false;
        while let Some(offset) = tree.entries.next_offset(limit) {
            let rows = tree
                .roots
                .iter()
                .cloned()
                .map(|path| DirectoryRow::Directory { path })
                .chain(other_present.then_some(DirectoryRow::OtherTracks))
                .skip(offset)
                .take(limit)
                .collect();
            tree.entries.insert(
                offset,
                limit,
                tree.roots.len() + usize::from(other_present),
                rows,
            );
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
                filter: None,
                offset: 0,
                limit: 1,
                generation: self.projection.generation(),
                stamp,
            });
        }
        let limit = state.system.query_batch_size as usize;
        let (kind, offset) = match &self.view {
            View::Queue if state.queue.total != 0 => match self.queue_anchor {
                Some(queue_id) => (QueryKind::QueueSelection(queue_id), 0),
                None => (QueryKind::Queue, self.queue.next_offset(limit)?),
            },
            View::LibraryResults(results) => {
                if let Some(track_id) = self.refresh_anchor {
                    (QueryKind::LibrarySelection(track_id), 0)
                } else {
                    (
                        QueryKind::Search(self.search.applied_query.clone()),
                        results.next_offset(limit)?,
                    )
                }
            }
            View::Tree(tree) if tree.other_tracks => (
                tree.other_query(),
                if tree.anchor.is_some() {
                    0
                } else {
                    tree.entries.next_offset(limit)?
                },
            ),
            View::Tree(tree) if !tree.directory.as_os_str().is_empty() => match tree.anchor {
                Some(track_id) => (
                    QueryKind::DirectorySelection {
                        track_id,
                        path: tree.directory.clone(),
                    },
                    0,
                ),
                None => (
                    QueryKind::Directory(tree.directory.clone()),
                    tree.entries.next_offset(limit)?,
                ),
            },
            View::Tree(tree) if tree.other_present.is_none() => (QueryKind::OtherTracksCount, 0),
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
        let filter = (!matches!(kind, QueryKind::Search(_))
            && !self.search.applied_query.is_empty())
        .then(|| self.search.applied_query.clone());
        let mut request = QueryRequest {
            kind,
            filter,
            offset,
            limit,
            generation: 0,
            stamp: Query::OtherTracksCount.stamp(state.query_revisions(None)),
        };
        let query = request.query();
        let revisions = state.query_revisions(request.kind.playlist_id());
        self.projection.sync(&query, revisions);
        request.generation = self.projection.generation();
        request.stamp = query.stamp(revisions);
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
                        self.queue.selection.select(Some(row.offset));
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
            (View::LibraryResults(rows), ViewResponse::LibraryLocate(location)) => {
                self.library_selected_id = None;
                if let rivu_protocol::model::LibraryLocateResult::Located(location) = location {
                    self.library_selected_id = self.refresh_anchor;
                    rows.total = location.total;
                    rows.loaded = true;
                    rows.selection.select(Some(location.offset));
                    rows.set_viewport(rows.viewport_rows);
                }
                self.refresh_anchor = None;
            }
            (View::LibraryResults(results), ViewResponse::LibraryPage(page)) => {
                results.insert(request.offset, request.limit, page.total, page.rows);
                if let Some(row) = results.selected() {
                    self.library_selected_id = Some(row.id);
                }
            }
            (View::Tree(tree), ViewResponse::OtherTracksCount(total))
                if !tree.other_tracks && tree.directory.as_os_str().is_empty() =>
            {
                tree.other_present = Some(total != 0);
                tree.entries.clear();
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
            (View::Tree(tree), ViewResponse::LibraryLocate(location)) => {
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
                tree.selected_track_id = tree.entries.selected().and_then(|entry| match entry {
                    DirectoryRow::Track(track) => Some(track.id),
                    _ => None,
                });
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
                        detail.entries.selection.select(Some(entry.offset));
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
        self.prepare_local_view(state);
        self.mark_local_change();
        true
    }

    fn mark_local_change(&mut self) {
        self.local_revision = self.local_revision.wrapping_add(1);
    }

    fn open_details(&mut self) {
        let (target, source_label, container) = match &self.view {
            View::Recent(recent) => (
                recent.selected().map(|entry| entry.track_id),
                "terminal-recent",
                None,
            ),
            View::Queue => (
                self.queue.selected().map(|row| row.track.id),
                "shell-panel-queue",
                None,
            ),
            View::LibraryResults(results) => (
                results.selected().map(|track| track.id),
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
            expires_at: Instant::now() + Duration::from_secs(6),
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
        let filter = if matches!(self.view, View::Queue) {
            self.search.applied_query.as_str()
        } else {
            ""
        };
        if !QueryKind::Queue.changed(previous, next, filter) {
            return;
        }
        if !QueryKind::Queue.changed_without_bitrate(previous, next, filter) {
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
        let filtered_total = self.queue.total;
        self.queue.clear();
        if self.search.applied_query.is_empty() || !matches!(self.view, View::Queue) {
            self.queue.total = next.queue.total;
            self.queue.loaded = true;
            self.queue
                .selection
                .select((next.queue.total != 0).then(|| selected.min(next.queue.total - 1)));
        } else {
            self.queue.total = filtered_total;
            self.queue.loaded = true;
        }
        if next.queue.total == 0 {
            self.queue.total = 0;
            self.queue.loaded = true;
            self.queue.selection.select(None);
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
        self.search.active = true;
        self.search.editing = true;
        self.search.query.clone_from(&self.search.applied_query);
        self.mark_local_change();
    }

    fn reset_results(&mut self, state: &TuiSnapshot) {
        self.refresh_anchor = None;
        match &mut self.view {
            View::Queue => {
                self.queue = PagedList {
                    viewport_rows: self.queue.viewport_rows,
                    ..PagedList::default()
                };
                self.queue.loaded = state.queue.total == 0;
                self.queue_anchor = None;
                self.queue_selected_id = None;
            }
            View::LibraryResults(results) => {
                *results = PagedList {
                    viewport_rows: results.viewport_rows,
                    ..PagedList::default()
                };
                self.library_selected_id = None;
            }
            View::Tree(tree) => {
                tree.entries = PagedList {
                    viewport_rows: tree.entries.viewport_rows,
                    ..PagedList::default()
                };
                tree.anchor = None;
                tree.selected_track_id = None;
            }
            View::Playlists(rows) => {
                *rows = PagedList {
                    viewport_rows: rows.viewport_rows,
                    ..PagedList::default()
                }
            }
            View::PlaylistDetail { detail, .. } => {
                detail.entries = PagedList {
                    viewport_rows: detail.entries.viewport_rows,
                    ..PagedList::default()
                };
                detail.entry_anchor = None;
                detail.selected_entry_id = None;
            }
            View::Recent(recent) => {
                recent.entries = PagedList::default();
                recent.sync(&state.library.history, &self.search.applied_query);
            }
        }
        self.cancel_query();
    }

    fn clear_search(&mut self, state: &TuiSnapshot) {
        if !self.search.active {
            return;
        }
        self.search = SearchState::default();
        if matches!(self.view, View::LibraryResults(_)) {
            self.view = View::Tree(LibraryTree::new(&state.system.library_roots));
        }
        self.reset_results(state);
        self.mark_local_change();
    }

    fn submit_search(&mut self, state: &TuiSnapshot) {
        self.search.applied_query = self.search.query.trim().to_owned();
        self.search.editing = false;
        if self.search.applied_query.is_empty() {
            self.clear_search(state);
            return;
        }
        if matches!(&self.view, View::Tree(tree) if !tree.other_tracks && tree.directory.as_os_str().is_empty())
        {
            self.view = View::LibraryResults(PagedList::default());
        }
        self.reset_results(state);
        self.mark_local_change();
    }

    fn selected_identity(&self) -> Option<i64> {
        match &self.view {
            View::LibraryResults(rows) => rows
                .selected()
                .map(|row| row.id)
                .or(self.library_selected_id),
            _ => None,
        }
    }

    fn store_page(&mut self) {
        match std::mem::take(&mut self.view) {
            view @ View::Tree(_) => self.cached_library = Some(view),
            view @ View::Recent(_) => self.cached_recent = Some(view),
            view @ (View::Playlists(_) | View::PlaylistDetail { .. }) => {
                self.cached_playlists = Some(view);
            }
            View::Queue | View::LibraryResults(_) => {}
        }
    }

    fn switch_tab(&mut self, backward: bool, state: &TuiSnapshot) {
        self.clear_search(state);
        let target = match (self.view.tab(), backward) {
            (TopTab::Queue, false) | (TopTab::Library, true) => TopTab::Playlists,
            (TopTab::Playlists, false) | (TopTab::Recent, true) => TopTab::Library,
            (TopTab::Library, false) | (TopTab::Queue, true) => TopTab::Recent,
            (TopTab::Recent, false) | (TopTab::Playlists, true) => TopTab::Queue,
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
            TopTab::Recent => self.cached_recent.take().unwrap_or_else(|| {
                let mut recent = RecentTracks::default();
                recent.sync(&state.library.history, "");
                View::Recent(recent)
            }),
        };
        self.startup_hint_deadline = None;
        self.cancel_query();
        self.mark_local_change();
    }

    pub(super) fn keyboard_context(&self) -> KeyboardContext {
        if self.search.editing {
            return KeyboardContext::TuiSearchQuery;
        }
        match &self.view {
            View::Queue => KeyboardContext::TuiQueue,
            View::Recent(_) => KeyboardContext::TuiRecent,
            View::LibraryResults(_) => KeyboardContext::TuiSearchResults,
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
        if !self.search.editing
            && let View::Tree(tree) = &mut self.view
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
            self.search.query.push(character);
            self.mark_local_change();
            return KeyAction::Ignored;
        }
        if self.search.active && resolve_key(key, context) == Some(Action::Cancel) {
            self.clear_search(state);
            return KeyAction::Ignored;
        }
        if self.search.editing {
            match resolve_key(key, context) {
                Some(Action::Activate) => self.submit_search(state),
                Some(Action::Backspace) => {
                    self.search.query.pop();
                    self.mark_local_change();
                }
                Some(Action::Traverse) => self.switch_tab(
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT),
                    state,
                ),
                Some(Action::Help) => {
                    self.show_help = true;
                    self.help_scroll = 0;
                    self.mark_local_change();
                }
                _ => {}
            }
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
                    View::Recent(recent) => recent.selected().map(|entry| entry.track_id),
                    View::LibraryResults(results) => results.selected().map(|track| track.id),
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
            Action::Traverse => {
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
            View::Recent(mut recent) => {
                if action == Action::Activate {
                    command = recent.selected().map(|entry| Command::Play {
                        track_id: entry.track_id,
                    });
                } else if action == Action::Cancel {
                    self.cached_recent = Some(View::Recent(recent));
                    self.view = View::Queue;
                    self.cancel_query();
                    self.mark_local_change();
                    return KeyAction::Ignored;
                } else {
                    changed = recent.entries.navigate_action(action, page_size);
                }
                self.view = View::Recent(recent);
            }
            View::LibraryResults(mut results) => {
                if action == Action::Activate {
                    command = results
                        .selected()
                        .map(|track| Command::Play { track_id: track.id });
                } else {
                    changed = results.navigate_action(action, page_size);
                    if changed {
                        self.library_selected_id = results.selected().map(|row| row.id);
                    }
                }
                self.view = View::LibraryResults(results);
            }
            View::Tree(mut tree) => {
                let mut close = false;
                let mut transition = false;
                match action {
                    Action::Cancel | Action::Parent => {
                        changed = tree.parent();
                        close = action == Action::Cancel && !changed;
                        transition = changed || close;
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
                        transition = changed;
                        if transition
                            && self.search.active
                            && let Some((_, selection, _)) = tree.ancestors.last_mut()
                        {
                            *selection = ListState::default();
                        }
                        if changed {
                            self.cancel_query();
                        }
                    }
                    _ => {
                        changed = tree.entries.navigate_action(action, page_size);
                        if changed {
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
                if transition && self.search.active {
                    self.clear_search(state);
                }
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
                if close {
                    self.clear_search(state);
                }
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
                                playlists: (
                                    if self.search.active {
                                        ListState::default()
                                    } else {
                                        playlists.selection
                                    },
                                    playlists.viewport_rows,
                                ),
                                detail,
                            };
                            self.clear_search(state);
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
            if matches!(
                action,
                Action::Up
                    | Action::Down
                    | Action::PageUp
                    | Action::PageDown
                    | Action::First
                    | Action::Last
            ) {
                self.refresh_anchor = None;
            }
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

#[derive(Default)]
pub(super) struct SearchState {
    pub(super) query: String,
    pub(super) applied_query: String,
    pub(super) editing: bool,
    pub(super) active: bool,
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
    other_present: Option<bool>,
    pub(super) favorite: Option<bool>,
    pub(super) missing: Option<bool>,
    pub(super) sort: rivu_protocol::model::LibrarySort,
    pub(super) anchor: Option<i64>,
    pub(super) selected_track_id: Option<i64>,
}

impl LibraryTree {
    fn other_query(&self) -> QueryKind {
        QueryKind::OtherTracks {
            query: String::new(),
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
        match (alt, key.code) {
            (true, KeyCode::Char('f')) => {
                self.favorite = if self.favorite.is_some() {
                    None
                } else {
                    Some(true)
                };
            }
            (true, KeyCode::Char('m')) => {
                self.missing = if self.missing.is_some() {
                    None
                } else {
                    Some(true)
                };
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
