use super::{
    paged::{ContainerIdentity, PagedList},
    worker::{QueryRequest, QueryResult, ReadDemand, ReadKind, ReadSpec},
};
use crate::projection::TuiSnapshot;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use rivu_presentation::{
    history::{activity_key, compare_recent, playback_activity, reorder_changed_rows},
    i18n::Localizer,
    keyboard::{self, Action, Context as KeyboardContext, Modifiers},
};
use rivu_protocol::query_tracking::QueryTracker;
use rivu_protocol::{
    model::{
        Command, DirectoryRow, HistoryEntry, LibraryRow, PlaylistEntryRow, PlaylistSummary, Query,
        QueueRow,
    },
    response::{QueryError, ViewResponse},
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
        let queue_filter = if matches!(self.ui.view, View::Queue) {
            self.ui.search.applied_query.as_str()
        } else {
            ""
        };
        self.ui
            .queue
            .sync_snapshot(&self.snapshot, &next, queue_filter, page_size_changed);
        let was_playlist_detail = matches!(self.ui.view, View::PlaylistDetail { .. });
        self.ui.view.sync_snapshot(
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
        if let Some(message) = snapshot_message(&self.snapshot, &next, &self.ui.i18n) {
            self.ui.set_notice(message, NoticeKind::Success);
        }
        self.snapshot = next;
        if self
            .ui
            .queue_replace
            .as_ref()
            .is_some_and(|pending| !pending.valid(&self.ui, &self.snapshot))
        {
            self.ui.queue_replace = None;
            self.ui.mark_local_change();
        }
        self.ui.reconcile_read(&self.snapshot);
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
    Success,
    Warning,
    Error,
}

pub(super) struct Notice {
    pub(super) text: String,
    pub(super) kind: NoticeKind,
    pub(super) expires_at: Instant,
}

pub(super) struct QueueReplaceConfirmation {
    playlist_id: i64,
    queue_revision: u64,
    playlist_revision: Option<u64>,
    pub(super) prompt: String,
}

impl QueueReplaceConfirmation {
    fn valid(&self, ui: &UiState, state: &TuiSnapshot) -> bool {
        state.configuration.confirm_queue_replace
            && state.queue.total != 0
            && state.queue.revision == self.queue_revision
            && state
                .library
                .playlist_entry_revisions
                .get(&self.playlist_id)
                .copied()
                == self.playlist_revision
            && matches!(&ui.view, View::Playlists(rows)
                if rows.selected().is_some_and(|row| row.id == self.playlist_id))
            && !ui.search.editing
            && !ui.show_help
            && !ui.details_active
            && ui.command_input.is_none()
    }
}

#[derive(Default)]
pub(super) struct Details {
    pub(super) target: Option<i64>,
    pub(super) source_label: &'static str,
    pub(super) state: DetailState,
    pub(super) stamp: Option<rivu_protocol::model::QueryStamp>,
    pub(super) scroll: usize,
    pub(super) max_scroll: usize,
    pub(super) error_scroll: usize,
    pub(super) error_max_scroll: usize,
    pub(super) layout: Option<super::render::DetailsLayout>,
    pub(super) failed_request: Option<Arc<ReadSpec>>,
}

impl Details {
    fn current_stamp(&self, state: &TuiSnapshot) -> Option<rivu_protocol::model::QueryStamp> {
        let query = Query::Track {
            track_id: self.target?,
        };
        let revisions = state.query_revisions(None).for_query(
            &query,
            &state.library.playlist_entry_revisions,
            &state.library.metadata_revisions,
        );
        Some(query.stamp(revisions))
    }

    fn demand(&self, state: &TuiSnapshot) -> Option<ReadDemand<'_>> {
        let track_id = self.target?;
        if self.stamp.is_some() && self.stamp == self.current_stamp(state) {
            return None;
        }
        let demand = ReadDemand {
            kind: ReadKind::Track(track_id),
            filter: None,
            offset: 0,
            limit: 1,
        };
        (!self
            .failed_request
            .as_ref()
            .is_some_and(|failed| demand.matches(failed, state)))
        .then_some(demand)
    }

    fn sync_snapshot(&mut self, state: &TuiSnapshot) -> bool {
        if self.stamp.is_some() && self.stamp != self.current_stamp(state) {
            self.stamp = None;
            self.state = DetailState::Loading;
            return true;
        }
        false
    }

    fn accept(&mut self, stamp: rivu_protocol::model::QueryStamp, view: ViewResponse) -> bool {
        let state = match view {
            ViewResponse::Track(Some(track)) if Some(track.id) == self.target => {
                DetailState::Ready(Arc::new(track))
            }
            ViewResponse::Track(None) => DetailState::Removed,
            _ => return false,
        };
        self.error_scroll = 0;
        self.stamp = Some(stamp);
        self.state = state;
        true
    }
}

#[derive(Clone, Default)]
pub(super) enum DetailState {
    #[default]
    Loading,
    Retired,
    Ready(Arc<rivu_protocol::model::Track>),
    Removed,
    Error(Arc<str>),
    Container(Arc<str>),
}

#[derive(Default)]
pub(super) struct RecentTracks {
    pub(super) source: Arc<Vec<HistoryEntry>>,
    pub(super) entries: PagedList<usize>,
    pub(super) playback: Option<(i64, i64)>,
}

impl RecentTracks {
    fn selected(&self) -> Option<&HistoryEntry> {
        self.source.get(*self.entries.selected()?)
    }

    pub(super) fn played_at(&self, entry: &HistoryEntry) -> i64 {
        activity_key(entry, self.playback).0
    }

    fn sync(&mut self, state: &TuiSnapshot, query: &str) {
        let source = &state.library.history;
        let playback = playback_activity(
            state.playback.status,
            state.current_track().map(|track| track.id),
            state.playback.last_heard_at,
        );
        let rebuild = !self.entries.loaded || !Arc::ptr_eq(&self.source, source);
        let previous_playback = self.playback;
        if !rebuild && previous_playback == playback {
            return;
        }
        let selected_id = self.selected().map(|entry| entry.track_id);
        let old_position = self.entries.selection.selected().unwrap_or(0);
        self.playback = playback;
        let compare = |a, b| compare_recent(source, playback, true, a, b);
        if rebuild {
            let mut indices = self.entries.pages.remove(&0).unwrap_or_default();
            indices.clear();
            let normalized_query = query.to_lowercase();
            indices.extend((0..source.len().min(256)).filter(|&index| {
                normalized_query.is_empty()
                    || source[index]
                        .title
                        .to_lowercase()
                        .contains(&normalized_query)
            }));
            indices.sort_unstable_by(|&a, &b| compare(a, b));
            self.source = Arc::clone(source);
            let total = indices.len();
            self.entries.clear();
            self.entries.insert(0, total.max(1), total, indices);
        } else if let Some(indices) = self.entries.pages.get_mut(&0) {
            let affected = [previous_playback, playback].map(|current| {
                current.and_then(|(id, _)| {
                    indices
                        .iter()
                        .position(|&index| source[index].track_id == id)
                })
            });
            if reorder_changed_rows(indices, affected, compare).is_none() {
                return;
            }
        }
        self.entries.columns = None;
        let indices = &self.entries.pages[&0];
        let selected = selected_id
            .and_then(|id| {
                indices
                    .iter()
                    .position(|&index| source[index].track_id == id)
            })
            .unwrap_or(old_position.min(indices.len().saturating_sub(1)));
        self.entries
            .selection
            .select((!indices.is_empty()).then_some(selected));
        self.entries.set_viewport(self.entries.viewport_rows);
    }
}

#[derive(Default)]
pub(super) struct QueuePage {
    rows: PagedList<QueueRow>,
    pub(super) anchor: Option<u64>,
    selected_id: Option<u64>,
}

impl std::ops::Deref for QueuePage {
    type Target = PagedList<QueueRow>;
    fn deref(&self) -> &Self::Target {
        &self.rows
    }
}

impl std::ops::DerefMut for QueuePage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.rows
    }
}

impl QueuePage {
    fn sync_snapshot(
        &mut self,
        previous: &TuiSnapshot,
        next: &TuiSnapshot,
        filter: &str,
        page_size_changed: bool,
    ) {
        if page_size_changed {
            self.rows.clear_pages();
            self.rows.fetch_size = 0;
        }
        let changes = ReadKind::Queue.changes(previous, next, filter);
        if !changes.rows_changed {
            return;
        }
        if !changes.coordinates_changed {
            self.rows.clear_pages();
            return;
        }
        self.anchor = self
            .anchor
            .or_else(|| self.rows.selected().map(|row| row.id))
            .or(self.selected_id);
        let selected = self.rows.selection.selected().unwrap_or(0);
        let filtered_total = self.rows.total;
        self.rows.clear();
        if filter.is_empty() {
            self.rows.total = next.queue.total;
            self.rows.loaded = true;
            self.rows
                .selection
                .select((next.queue.total != 0).then(|| selected.min(next.queue.total - 1)));
        } else {
            self.rows.total = filtered_total;
            // An old empty match set cannot suppress the refreshed first page.
            self.rows.loaded = filtered_total != 0;
        }
        if next.queue.total == 0 {
            self.rows.total = 0;
            self.rows.loaded = true;
            self.rows.selection.select(None);
            self.anchor = None;
            self.selected_id = None;
        }
        self.rows.set_viewport(self.rows.viewport_rows);
    }

    fn demand(&self, state: &TuiSnapshot, limit: usize) -> Option<(ReadKind<'_>, usize)> {
        if state.queue.total == 0 {
            return None;
        }
        Some(match self.anchor {
            Some(id) => (ReadKind::QueueSelection(id), 0),
            None => (ReadKind::Queue, self.rows.next_offset(limit)?),
        })
    }

    fn refilter(&mut self, state: &TuiSnapshot) {
        self.anchor = self
            .anchor
            .or_else(|| self.rows.selected().map(|row| row.id))
            .or(self.selected_id);
        self.rows.clear();
        self.rows.selection.select(Some(0));
        if state.queue.total == 0 {
            self.rows.loaded = true;
            self.rows.selection.select(None);
            self.anchor = None;
            self.selected_id = None;
        }
    }

    fn accept_page(&mut self, request: &ReadSpec, page: rivu_protocol::model::Page<QueueRow>) {
        self.rows
            .insert(request.offset, request.limit, page.total, page.rows);
        if self.rows.total == 0 {
            self.selected_id = None;
        } else if let Some(row) = self.rows.selected() {
            self.selected_id = Some(row.id);
        }
    }

    fn locate(&mut self, queue_id: u64, rows: &[QueueRow]) {
        let row = rows.iter().find(|row| row.id == queue_id);
        if let Some(row) = row {
            self.rows.selection.select(Some(row.offset));
            self.rows.set_viewport(self.rows.viewport_rows);
        }
        self.selected_id = row.map(|row| row.id);
        self.anchor = None;
    }
}
#[derive(Default)]
pub(super) struct LibraryPage {
    rows: PagedList<LibraryRow>,
    anchor: Option<i64>,
    selected_id: Option<i64>,
}

impl From<PagedList<LibraryRow>> for LibraryPage {
    fn from(rows: PagedList<LibraryRow>) -> Self {
        Self {
            rows,
            ..Self::default()
        }
    }
}

impl std::ops::Deref for LibraryPage {
    type Target = PagedList<LibraryRow>;
    fn deref(&self) -> &Self::Target {
        &self.rows
    }
}

impl std::ops::DerefMut for LibraryPage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.rows
    }
}

impl LibraryPage {
    fn selected_identity(&self) -> Option<i64> {
        self.rows.selected().map(|row| row.id).or(self.selected_id)
    }

    fn invalidate(&mut self, coordinates_changed: bool) {
        if coordinates_changed {
            self.anchor = self.anchor.or_else(|| self.selected_identity());
        }
        self.rows.clear();
    }

    fn refilter(&mut self) {
        self.invalidate(true);
        self.rows.selection.select(Some(0));
    }

    fn demand<'a>(&'a self, query: &'a str, limit: usize) -> Option<(ReadKind<'a>, usize)> {
        Some(match self.anchor {
            Some(track_id) => (ReadKind::LibrarySelection(track_id), 0),
            None => (ReadKind::Search(query), self.rows.next_offset(limit)?),
        })
    }

    fn locate(&mut self, location: rivu_protocol::model::LibraryLocateResult) {
        self.selected_id = None;
        if let rivu_protocol::model::LibraryLocateResult::Located(location) = location {
            self.selected_id = self.anchor;
            self.rows.total = location.total;
            self.rows.loaded = true;
            self.rows.selection.select(Some(location.offset));
            self.rows.set_viewport(self.rows.viewport_rows);
        }
        self.anchor = None;
    }

    fn accept_page(&mut self, request: &ReadSpec, page: rivu_protocol::model::Page<LibraryRow>) {
        self.rows
            .insert(request.offset, request.limit, page.total, page.rows);
        if let Some(row) = self.rows.selected() {
            self.selected_id = Some(row.id);
        } else if self.rows.total == 0 {
            self.selected_id = None;
        }
    }

    fn navigate(&mut self, action: Action, page_size: usize) -> bool {
        let changed = self.rows.navigate_action(action, page_size);
        if changed {
            self.anchor = None;
            self.selected_id = self.rows.selected().map(|row| row.id);
        }
        changed
    }
}
#[derive(Default)]
pub(super) enum View {
    #[default]
    Queue,
    LibraryResults(LibraryPage),
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

    fn query_kind(&self, selection: bool) -> Option<ReadKind<'_>> {
        match self {
            Self::Recent(_) => None,
            Self::Queue => Some(ReadKind::Queue),
            Self::LibraryResults(_) => Some(ReadKind::Search("")),
            Self::Tree(tree) if tree.other_tracks => Some(tree.other_query()),
            Self::Tree(tree) if !tree.directory.as_os_str().is_empty() => {
                Some(ReadKind::Directory(&tree.directory))
            }
            Self::Tree(_) => Some(ReadKind::OtherTracksCount),
            Self::Playlists(_) => Some(ReadKind::Playlists),
            Self::PlaylistDetail { detail, .. } => Some(match detail.entry_anchor {
                Some(entry_id) if selection => ReadKind::PlaylistSelection {
                    playlist_id: detail.playlist_id,
                    entry_id,
                },
                _ => ReadKind::Playlist(detail.playlist_id),
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
            recent.sync(next, query);
            return false;
        }
        let changes = self
            .query_kind(false)
            .map(|kind| kind.changes(previous, next, query))
            .unwrap_or_default();
        let rows_changed = changes.rows_changed;
        let coordinates_changed = changes.coordinates_changed;
        let query_changed = if matches!(self, Self::PlaylistDetail { detail, .. } if detail.entry_anchor.is_some())
        {
            self.query_kind(true)
                .is_some_and(|kind| kind.changes(previous, next, query).rows_changed)
        } else {
            rows_changed
        };
        if let Self::PlaylistDetail { detail, playlists } = self {
            if ReadKind::PlaylistSummary(detail.playlist_id)
                .changes(previous, next, "")
                .rows_changed
            {
                detail.summary_dirty = true;
            }
            let compact = Query::PlaylistEntryLocations {
                query: (!query.is_empty()).then(String::new),
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
                let playlists = std::mem::take(playlists);
                *self = Self::Playlists(PagedList {
                    selection: playlists.0,
                    viewport_rows: playlists.1,
                    ..PagedList::default()
                });
                return true;
            }
        }
        if let Self::Tree(tree) = self
            && ReadKind::OtherTracksCount
                .changes(previous, next, "")
                .rows_changed
        {
            tree.other_present = None;
        }
        if rows_changed && !coordinates_changed && !page_size_changed {
            self.invalidate_payload();
        } else if coordinates_changed && matches!(self, Self::Tree(_) | Self::Playlists(_)) {
            self.retain_container_identity();
        } else if rows_changed || page_size_changed {
            match self {
                Self::LibraryResults(results) => results.invalidate(coordinates_changed),
                Self::Tree(tree) => {
                    if tree.entries.identity_lookup.is_none()
                        && (tree.other_tracks || !tree.directory.as_os_str().is_empty())
                    {
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
            Self::LibraryResults(results) => results.clear_pages(),
            Self::Tree(tree) => {
                tree.entries.retain_pages(|_, rows| {
                    !rows.iter().any(|row| matches!(row, DirectoryRow::Track(_)))
                });
            }
            Self::PlaylistDetail { detail, .. } => detail.entries.clear_pages(),
            Self::Queue | Self::Recent(_) => {}
            Self::Playlists(_) => {}
        }
    }
}
impl View {
    fn demand<'a>(
        &'a self,
        state: &TuiSnapshot,
        queue: &'a QueuePage,
        query: &'a str,
    ) -> Option<ReadDemand<'a>> {
        let limit = state.system.query_batch_size as usize;
        let available = |kind, offset| {
            let filter = (!matches!(
                kind,
                ReadKind::Search(_)
                    | ReadKind::OtherTracksCount
                    | ReadKind::PlaylistSummary(_)
                    | ReadKind::Track(_)
            ) && !query.is_empty())
            .then_some(query);
            let demand = ReadDemand {
                kind,
                filter,
                offset,
                limit: if matches!(kind, ReadKind::PlaylistSummary(_)) {
                    1
                } else {
                    limit
                },
            };
            let failed = self.failure(queue, matches!(kind, ReadKind::PlaylistSummary(_)));
            (!failed.is_some_and(|failed| demand.matches(failed, state))).then_some(demand)
        };
        if let Some((kind, offset)) = self.query_demand(state, queue, query, limit)
            && let Some(demand) = available(kind, offset)
        {
            return Some(demand);
        }
        let Self::PlaylistDetail { detail, .. } = self else {
            return None;
        };
        detail
            .summary_dirty
            .then(|| available(ReadKind::PlaylistSummary(detail.playlist_id), 0))
            .flatten()
    }

    fn query_demand<'a>(
        &'a self,
        state: &TuiSnapshot,
        queue: &'a QueuePage,
        query: &'a str,
        limit: usize,
    ) -> Option<(ReadKind<'a>, usize)> {
        Some(match self {
            View::Queue => queue.demand(state, limit)?,
            View::LibraryResults(results) => results.demand(query, limit)?,
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
                    ReadKind::DirectorySelection {
                        track_id,
                        path: &tree.directory,
                    },
                    0,
                ),
                None => {
                    let offset = tree.entries.next_offset(limit)?;
                    (ReadKind::Directory(&tree.directory), offset)
                }
            },
            View::Tree(tree) if tree.other_present.is_none() => (ReadKind::OtherTracksCount, 0),
            View::Playlists(playlists) => (ReadKind::Playlists, playlists.next_offset(limit)?),
            View::PlaylistDetail { detail, .. }
                if state
                    .library
                    .playlist_entry_revisions
                    .contains_key(&detail.playlist_id) =>
            {
                match detail.entry_anchor {
                    Some(entry_id) => (
                        ReadKind::PlaylistSelection {
                            playlist_id: detail.playlist_id,
                            entry_id,
                        },
                        0,
                    ),
                    None => (
                        ReadKind::Playlist(detail.playlist_id),
                        detail.entries.next_offset(limit)?,
                    ),
                }
            }
            _ => return None,
        })
    }

    fn failure<'a>(&'a self, queue: &'a QueuePage, summary: bool) -> Option<&'a ReadSpec> {
        let (page, title) = match self {
            View::Queue => (&queue.rows.failed_request, &queue.rows.failed_summary),
            View::LibraryResults(rows) => (&rows.rows.failed_request, &rows.rows.failed_summary),
            View::Tree(tree) => (&tree.entries.failed_request, &tree.entries.failed_summary),
            View::Playlists(rows) => (&rows.failed_request, &rows.failed_summary),
            View::PlaylistDetail { detail, .. } => (
                &detail.entries.failed_request,
                &detail.entries.failed_summary,
            ),
            View::Recent(recent) => (
                &recent.entries.failed_request,
                &recent.entries.failed_summary,
            ),
        };
        if summary {
            title.as_deref()
        } else {
            page.as_deref()
        }
    }

    fn failure_slot<'a>(
        &'a mut self,
        queue: &'a mut QueuePage,
        summary: bool,
    ) -> &'a mut Option<Arc<ReadSpec>> {
        let (page, title) = match self {
            View::Queue => (
                &mut queue.rows.failed_request,
                &mut queue.rows.failed_summary,
            ),
            View::LibraryResults(rows) => {
                (&mut rows.rows.failed_request, &mut rows.rows.failed_summary)
            }
            View::Tree(tree) => (
                &mut tree.entries.failed_request,
                &mut tree.entries.failed_summary,
            ),
            View::Playlists(rows) => (&mut rows.failed_request, &mut rows.failed_summary),
            View::PlaylistDetail { detail, .. } => (
                &mut detail.entries.failed_request,
                &mut detail.entries.failed_summary,
            ),
            View::Recent(recent) => (
                &mut recent.entries.failed_request,
                &mut recent.entries.failed_summary,
            ),
        };
        if summary { title } else { page }
    }
}

impl View {
    fn accept(
        &mut self,
        queue: &mut QueuePage,
        request: &QueryRequest,
        view: ViewResponse,
    ) -> Result<(), ()> {
        match (self, view) {
            (View::Queue, ViewResponse::QueuePage(page)) if request.demand.kind() == ReadKind::Queue => {
                queue.accept_page(&request.demand, page);
            }
            (View::Queue, ViewResponse::QueueEntries(rows)) => {
                if let ReadKind::QueueSelection(queue_id) = request.demand.kind() {
                    queue.locate(queue_id, &rows);
                } else {
                    return Err(());
                }
            }
            (View::LibraryResults(rows), ViewResponse::LibraryLocate(location)) => rows.locate(location),
            (View::LibraryResults(rows), ViewResponse::LibraryPage(page)) => rows.accept_page(&request.demand, page),
            (View::Tree(tree), ViewResponse::OtherTracksCount(total))
                if !tree.other_tracks && tree.directory.as_os_str().is_empty() =>
            {
                tree.other_present = Some(total != 0);
                tree.entries.clear();
            }
            (View::Tree(tree), ViewResponse::LibraryPage(page)) if tree.other_tracks => {
                tree.entries.insert(
                    request.demand.offset,
                    request.demand.limit,
                    page.total,
                    page.rows.into_iter().map(DirectoryRow::Track).collect(),
                );
                if let Some(DirectoryRow::Track(track)) = tree.entries.selected() {
                    tree.selected_track_id = Some(track.id);
                } else if tree.entries.total == 0 {
                    tree.selected_track_id = None;
                }
            }
            (View::Tree(tree), ViewResponse::LibraryLocate(location)) => {
                let anchor = tree.anchor.take();
                tree.selected_track_id = None;
                if let rivu_protocol::model::LibraryLocateResult::Located(location) = location {
                    tree.selected_track_id = anchor;
                    tree.entries.total = location.total;
                    tree.entries.loaded = true;
                    tree.entries.selection.select(Some(location.offset));
                    tree.entries.set_viewport(tree.entries.viewport_rows);
                }
            }
            (View::Tree(tree), ViewResponse::DirectoryPage(page)) => {
                if tree.entries.resolve_identity(request.demand.offset, page.total, &page.rows, matches_directory_identity) {
                    tree.entries.insert(request.demand.offset, request.demand.limit, page.total, page.rows);
                }
                if tree.entries.identity_lookup.is_some() {
                    tree.selected_track_id = None;
                } else if let Some(DirectoryRow::Track(track)) = tree.entries.selected() {
                    tree.selected_track_id = Some(track.id);
                } else if tree.entries.total == 0 {
                    tree.selected_track_id = None;
                }
            }
            (View::Playlists(playlists), ViewResponse::PlaylistSummaries(page)) => {
                if playlists.resolve_identity(request.demand.offset, page.total, &page.rows, |row, id| {
                    matches!(id, ContainerIdentity::Playlist(target) if row.id == *target)
                }) {
                    playlists.insert(request.demand.offset, request.demand.limit, page.total, page.rows);
                }
            }
            (View::PlaylistDetail { detail, .. }, ViewResponse::PlaylistSummary(summary))
                if request.demand.kind() == ReadKind::PlaylistSummary(detail.playlist_id) =>
            {
                if let Some(summary) = summary
                    && summary.id == detail.playlist_id
                {
                    detail.name = summary.name;
                }
                detail.summary_dirty = false;
            }
            (View::PlaylistDetail { detail, .. }, ViewResponse::PlaylistEntries(page)) => {
                detail
                    .entries
                    .insert(request.demand.offset, request.demand.limit, page.total, page.rows);
                if let Some(entry) = detail.entries.selected() {
                    detail.selected_entry_id = Some(entry.id);
                } else if detail.entries.total == 0 {
                    detail.selected_entry_id = None;
                }
            }
            (
                View::PlaylistDetail { detail, .. },
                ViewResponse::PlaylistEntryLocations(entries),
            ) => {
                if let ReadKind::PlaylistSelection {
                    playlist_id,
                    entry_id,
                } = request.demand.kind()
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
                    return Err(());
                }
            }
            _ => {
                return Err(());
            }
        }
        Ok(())
    }
}

impl View {
    fn retain_container_identity(&mut self) {
        match self {
            View::Tree(tree) => {
                let container = tree
                    .entries
                    .identity_lookup
                    .as_ref()
                    .map(|(id, _, _)| id.clone())
                    .or_else(|| tree.entries.selected().and_then(directory_identity));
                if container.is_some() {
                    tree.anchor = None;
                    tree.selected_track_id = None;
                } else {
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
                tree.entries.retain_identity(container);
                if tree.anchor.is_some() {
                    tree.entries.identity_lookup = None;
                    tree.entries.selection.select(Some(0));
                }
            }
            View::Playlists(rows) => {
                let identity = rows
                    .identity_lookup
                    .as_ref()
                    .map(|(id, _, _)| id.clone())
                    .or_else(|| {
                        rows.selected()
                            .map(|row| ContainerIdentity::Playlist(row.id))
                    });
                rows.retain_identity(identity);
            }
            _ => unreachable!("only container views retain container identities"),
        }
    }

    fn refilter(&mut self, queue: &mut QueuePage, state: &TuiSnapshot, query: &str) {
        match self {
            View::Queue => queue.refilter(state),
            View::LibraryResults(results) => results.refilter(),
            View::Tree(_) | View::Playlists(_) => self.retain_container_identity(),
            View::PlaylistDetail { detail, .. } => {
                detail.entry_anchor = detail
                    .entry_anchor
                    .or_else(|| detail.entries.selected().map(|entry| entry.id))
                    .or(detail.selected_entry_id);
                detail.entries.clear();
                detail.entries.selection.select(Some(0));
            }
            View::Recent(recent) => {
                let selected_id = recent.selected().map(|entry| entry.track_id);
                recent.entries.loaded = false;
                recent.sync(state, query);
                if selected_id
                    .is_some_and(|id| recent.selected().is_none_or(|entry| entry.track_id != id))
                {
                    recent
                        .entries
                        .selection
                        .select((recent.entries.total != 0).then_some(0));
                    recent.entries.set_viewport(recent.entries.viewport_rows);
                }
            }
        }
    }
}

#[derive(Default)]
pub(super) struct ReadCoordinator {
    pub(super) requested: Option<QueryRequest>,
    tracker: QueryTracker,
}

impl ReadCoordinator {
    fn retire(&mut self) {
        self.requested = None;
        self.tracker.cancel();
    }

    fn submit(&mut self, demand: ReadSpec, state: &TuiSnapshot) {
        // A new page demand is a new submission even when protocol identity
        // deliberately excludes its offset and limit (including A-B-A).
        self.tracker.cancel();
        let submission = self
            .tracker
            .submit(demand.protocol_query(), demand.revisions(state));
        self.requested = Some(QueryRequest {
            demand: Arc::new(demand),
            submission,
        });
    }
}

pub(super) struct UiState {
    pub(super) i18n: Arc<Localizer>,
    pub(super) view: View,
    pub(super) search: SearchState,
    pub(super) command_input: Option<String>,
    // Each slot owns only its tab's view; switching moves bounded page caches.
    pub(super) cached_library: Option<View>,
    pub(super) cached_playlists: Option<View>,
    pub(super) cached_recent: Option<View>,
    pub(super) queue: QueuePage,
    pub(super) show_help: bool,
    pub(super) help_max_scroll: usize,
    pub(super) help_scroll: usize,
    pub(super) help_layout: Option<super::render::HelpLayout>,
    pub(super) message: Option<Notice>,
    pub(super) queue_replace: Option<QueueReplaceConfirmation>,
    // Started only after the initial IPC snapshot; None is permanently dismissed.
    pub(super) startup_hint_deadline: Option<Instant>,
    pub(super) details: Details,
    pub(super) details_active: bool,
    pub(super) reads: ReadCoordinator,
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
            command_input: None,
            cached_library: None,
            cached_playlists: None,
            cached_recent: None,
            queue: QueuePage::default(),
            show_help: false,
            help_max_scroll: 0,
            help_scroll: 0,
            help_layout: None,
            message: None,
            queue_replace: None,
            startup_hint_deadline: Some(Instant::now() + Duration::from_secs(4)),
            details: Details::default(),
            details_active: false,
            reads: ReadCoordinator::default(),
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
            let rows: Vec<_> = tree
                .roots
                .iter()
                .cloned()
                .map(|path| DirectoryRow::Directory { path })
                .chain(other_present.then_some(DirectoryRow::OtherTracks))
                .skip(offset)
                .take(limit)
                .collect();
            let total = tree.roots.len() + usize::from(other_present);
            if tree
                .entries
                .resolve_identity(offset, total, &rows, matches_directory_identity)
            {
                tree.entries.insert(offset, limit, total, rows);
            }
            changed = true;
        }
        changed
    }

    fn reconcile_read(&mut self, state: &TuiSnapshot) {
        if self.reads.requested.as_ref().is_some_and(|request| {
            !self
                .next_demand(state)
                .is_some_and(|demand| demand.matches(&request.demand, state))
        }) {
            self.reads.retire();
        }
    }

    // A pending demand is borrowed on every input-loop tick. Own its query only
    // when it actually replaces the worker's submission.
    pub(super) fn prepare_query_request(&mut self, state: &TuiSnapshot) -> bool {
        let Some(demand) = self.next_demand(state) else {
            if self.reads.requested.is_some() {
                self.reads.retire();
            }
            return false;
        };
        if self
            .reads
            .requested
            .as_ref()
            .is_some_and(|request| demand.matches(&request.demand, state))
        {
            return false;
        }
        let demand = demand.own(state);
        self.reads.submit(demand, state);
        if self.details_active {
            self.details.state = DetailState::Loading;
        }
        true
    }

    #[cfg(test)]
    pub(super) fn query_request(&mut self, state: &TuiSnapshot) -> Option<QueryRequest> {
        self.prepare_query_request(state);
        self.reads.requested.clone()
    }

    fn next_demand(&self, state: &TuiSnapshot) -> Option<ReadDemand<'_>> {
        if self.details_active {
            return self.details.demand(state);
        }
        if self.show_help {
            return None;
        }
        self.view
            .demand(state, &self.queue, &self.search.applied_query)
    }

    fn failure_slot(&mut self, kind: ReadKind<'_>) -> &mut Option<Arc<ReadSpec>> {
        if matches!(kind, ReadKind::Track(_)) {
            &mut self.details.failed_request
        } else {
            self.view.failure_slot(
                &mut self.queue,
                matches!(kind, ReadKind::PlaylistSummary(_)),
            )
        }
    }

    fn fail_query(&mut self, request: &QueryRequest, error: QueryError) -> bool {
        self.reads.requested = None;
        *self.failure_slot(request.demand.kind()) = Some(request.demand.clone());
        // An obsolete demand is parked quietly until its identity or freshness changes.
        if matches!(error, QueryError::RevisionChanged | QueryError::Cancelled) {
            if self.details_active {
                self.details.state = DetailState::Retired;
                self.mark_local_change();
            }
            return false;
        }
        let message = error.to_string();
        if self.details_active {
            self.details.stamp = Some(request.demand.stamp);
            self.details.error_scroll = 0;
            self.details.state = DetailState::Error(message.into());
            self.mark_local_change();
        } else {
            self.set_notice(message, NoticeKind::Error);
        }
        true
    }

    fn unexpected_query(&mut self, request: &QueryRequest) -> bool {
        self.fail_query(
            request,
            QueryError::Message(self.i18n.text("terminal-unexpected-response").into()),
        )
    }

    pub(super) fn apply_query(
        &mut self,
        state: &TuiSnapshot,
        request: &QueryRequest,
        result: Result<QueryResult, QueryError>,
    ) -> bool {
        if !self
            .next_demand(state)
            .is_some_and(|demand| demand.matches(&request.demand, state))
            || !self
                .reads
                .tracker
                .accepts_submission(request.submission, request.demand.revisions(state))
        {
            if self.reads.requested.as_ref() == Some(request) {
                self.reads.requested = None;
            }
            return false;
        }
        let result = match result {
            Ok(result) => result,
            Err(error) => return self.fail_query(request, error),
        };
        if !request.demand.accepts_view(&result.view) {
            return self.unexpected_query(request);
        }
        if !self
            .reads
            .tracker
            .accepts_submission(request.submission, result.revisions)
        {
            return self.fail_query(request, QueryError::RevisionChanged);
        }
        if self.details_active {
            if !self.details.accept(request.demand.stamp, result.view) {
                return self.unexpected_query(request);
            }
            self.reads.requested = None;
            self.mark_local_change();
            return true;
        }
        if self
            .view
            .accept(&mut self.queue, request, result.view)
            .is_err()
        {
            return self.unexpected_query(request);
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
            state: container.map_or(DetailState::Loading, |text| {
                DetailState::Container(text.into())
            }),
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
        if kind == NoticeKind::Success
            && self.message.as_ref().is_some_and(|notice| {
                notice.kind != NoticeKind::Success && Instant::now() < notice.expires_at
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
        if self.details.sync_snapshot(next) {
            self.mark_local_change();
        }
    }

    // Snapshot invalidation retires a submission, not failures of independent demands.
    // Their identity and selected freshness decide when they become eligible again.
    fn retire_query(&mut self) {
        self.reads.retire();
    }

    // Entry navigation retires submissions and retries entry reads, but cannot
    // reopen an unchanged failed playlist-title demand.
    fn cancel_query(&mut self) {
        self.retire_query();
        if self.details_active {
            self.details.failed_request = None;
        } else {
            *self.view.failure_slot(&mut self.queue, false) = None;
        }
    }

    fn reopen_view_queries(&mut self) {
        self.cancel_query();
        *self.view.failure_slot(&mut self.queue, true) = None;
    }

    pub(super) fn open_search(&mut self) {
        self.search.active = true;
        self.search.editing = true;
        self.search.query.clone_from(&self.search.applied_query);
        self.mark_local_change();
    }

    fn refilter_results(&mut self, state: &TuiSnapshot) {
        self.view
            .refilter(&mut self.queue, state, &self.search.applied_query);
        self.reconcile_read(state);
    }

    fn clear_search(&mut self, state: &TuiSnapshot) {
        if !self.search.active {
            return;
        }
        let had_query = !self.search.applied_query.is_empty();
        self.search = SearchState::default();
        if let View::LibraryResults(rows) = &self.view {
            // A global track result is not a root directory/OtherTracks row.
            // Returning to Tree therefore has no reachable matching identity.
            let mut tree = LibraryTree::new(&state.system.library_roots);
            tree.entries.viewport_rows = rows.viewport_rows;
            self.view = View::Tree(tree);
        }
        if had_query {
            self.refilter_results(state);
        }
        self.mark_local_change();
    }

    fn submit_search(&mut self, state: &TuiSnapshot) {
        let query = self.search.query.trim();
        let query_changed = self.search.applied_query != query;
        if query.is_empty() {
            self.clear_search(state);
            return;
        }
        if query_changed {
            self.search.applied_query.clear();
            self.search.applied_query.push_str(query);
        }
        self.search.editing = false;
        if let View::Tree(tree) = &self.view
            && !tree.other_tracks
            && tree.directory.as_os_str().is_empty()
        {
            self.view = View::LibraryResults(
                PagedList {
                    viewport_rows: tree.entries.viewport_rows,
                    ..PagedList::default()
                }
                .into(),
            );
        }
        if query_changed {
            self.refilter_results(state);
        }
        self.mark_local_change();
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
                recent.sync(state, "");
                View::Recent(recent)
            }),
        };
        self.startup_hint_deadline = None;
        self.reopen_view_queries();
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
            View::Tree(tree) if tree.other_tracks => KeyboardContext::TuiOther,
            View::Tree(tree)
                if matches!(tree.entries.selected(), Some(DirectoryRow::OtherTracks)) =>
            {
                KeyboardContext::TuiOtherTarget
            }
            View::Tree(_) => KeyboardContext::TuiTree,
            View::Playlists(_) => KeyboardContext::TuiPlaylists,
            View::PlaylistDetail { .. } => KeyboardContext::TuiPlaylistTracks,
        }
    }

    fn command_key(&mut self, key: KeyEvent) -> KeyAction {
        const MAX_COMMAND_BYTES: usize = 256;
        if key.modifiers.intersects(
            KeyModifiers::CONTROL
                | KeyModifiers::ALT
                | KeyModifiers::SUPER
                | KeyModifiers::HYPER
                | KeyModifiers::META,
        ) {
            return KeyAction::Ignored;
        }
        match key.code {
            KeyCode::Esc => self.command_input = None,
            KeyCode::Enter => {
                let input = self.command_input.take().unwrap_or_default();
                self.mark_local_change();
                return match input.trim() {
                    "" => KeyAction::Ignored,
                    "q" | "quit" => KeyAction::Quit,
                    "qa" | "qall" => KeyAction::Shutdown,
                    "help" => {
                        self.show_help = true;
                        self.help_scroll = 0;
                        KeyAction::Ignored
                    }
                    command => {
                        self.set_notice(
                            format!("{}: {command}", self.i18n.text("terminal-command-unknown")),
                            NoticeKind::Error,
                        );
                        KeyAction::Ignored
                    }
                };
            }
            KeyCode::Backspace => {
                if let Some(input) = &mut self.command_input {
                    input.pop();
                }
            }
            KeyCode::Char(character) if !character.is_control() => {
                if let Some(input) = &mut self.command_input
                    && input.len() + character.len_utf8() <= MAX_COMMAND_BYTES
                {
                    input.push(character);
                }
            }
            _ => return KeyAction::Ignored,
        }
        self.mark_local_change();
        KeyAction::Ignored
    }

    pub(super) fn key_action(&mut self, key: KeyEvent, state: &TuiSnapshot) -> KeyAction {
        // Global terminal exit wins over editors and modal input ownership.
        if resolve_key(key, KeyboardContext::TuiHelp) == Some(Action::Close) {
            self.queue_replace = None;
            return KeyAction::Quit;
        }
        if let Some(pending) = self.queue_replace.take() {
            self.mark_local_change();
            if !pending.valid(self, state) {
                return KeyAction::Ignored;
            }
            if key.modifiers.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER
                    | KeyModifiers::HYPER
                    | KeyModifiers::META,
            ) {
                self.queue_replace = Some(pending);
                return KeyAction::Ignored;
            }
            match key.code {
                KeyCode::Enter | KeyCode::Char('y' | 'Y') => {
                    return KeyAction::Command(Command::PlayPlaylist {
                        playlist_id: pending.playlist_id,
                    });
                }
                KeyCode::Esc | KeyCode::Char('n' | 'N') => return KeyAction::Ignored,
                KeyCode::Tab | KeyCode::BackTab => {}
                _ => {
                    self.queue_replace = Some(pending);
                    return KeyAction::Ignored;
                }
            }
        }
        if self.command_input.is_some() {
            return self.command_key(key);
        }
        if self.details_active {
            let action = resolve_key(key, KeyboardContext::TuiHelp);
            if action == Some(Action::Cancel) {
                self.details_active = false;
                self.cancel_query();
            } else {
                let details = &mut self.details;
                let (scroll, max_scroll) = match details.state {
                    DetailState::Loading => return KeyAction::Ignored,
                    DetailState::Error(_) => (&mut details.error_scroll, details.error_max_scroll),
                    _ => (&mut details.scroll, details.max_scroll),
                };
                match action {
                    Some(Action::Up) => *scroll = scroll.saturating_sub(1),
                    Some(Action::Down) => *scroll = scroll.saturating_add(1).min(max_scroll),
                    Some(Action::PageUp) => *scroll = scroll.saturating_sub(self.page_size),
                    Some(Action::PageDown) => {
                        *scroll = scroll.saturating_add(self.page_size).min(max_scroll)
                    }
                    Some(Action::First) => *scroll = 0,
                    Some(Action::Last) => *scroll = max_scroll,
                    _ => return KeyAction::Ignored,
                }
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
        let resolved = resolve_key(key, context);
        if resolved == Some(Action::CancelScan) {
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
        if key.code == KeyCode::Char(':')
            && !key.modifiers.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER
                    | KeyModifiers::HYPER
                    | KeyModifiers::META,
            )
        {
            self.command_input = Some(String::new());
            self.mark_local_change();
            return KeyAction::Ignored;
        }
        let Some(action) = resolved else {
            return KeyAction::Ignored;
        };
        if let View::Tree(tree) = &mut self.view
            && let Some(result) = tree.other_action(action)
        {
            if matches!(result, KeyAction::Ignored) {
                self.cancel_query();
                self.mark_local_change();
            }
            return result;
        }
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
            Action::Details => {
                self.open_details();
                return KeyAction::Ignored;
            }
            Action::Enqueue => {
                let track_id = match &self.view {
                    View::LibraryResults(results) => results.selected().map(|track| track.id),
                    View::PlaylistDetail { detail, .. } => {
                        detail.entries.selected().map(|entry| entry.track_id)
                    }
                    View::Tree(tree) => tree.entries.selected().and_then(|entry| match entry {
                        DirectoryRow::Track(track) => Some(track.id),
                        DirectoryRow::Directory { .. } | DirectoryRow::OtherTracks => None,
                    }),
                    _ => None,
                };
                return track_id.map_or(KeyAction::Ignored, |track_id| {
                    KeyAction::Command(Command::Enqueue {
                        track_ids: vec![track_id],
                    })
                });
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
        let mut cancelled_lookup = false;
        if matches!(
            action,
            Action::Up
                | Action::Down
                | Action::PageUp
                | Action::PageDown
                | Action::First
                | Action::Last
        ) {
            // Navigation owns the current selection even while a locator is
            // pending. Cancel that intent, then apply the actual action below.
            cancelled_lookup = match &mut self.view {
                View::Queue if self.queue.anchor.is_some() => {
                    self.queue.anchor = None;
                    self.queue.selected_id = None;
                    if !self.queue.loaded {
                        self.queue.selection.select(Some(0));
                    }
                    true
                }
                View::LibraryResults(rows) if rows.anchor.is_some() => {
                    rows.anchor = None;
                    rows.selected_id = None;
                    if !rows.loaded {
                        rows.selection.select(Some(0));
                    }
                    true
                }
                View::Tree(tree)
                    if tree.anchor.is_some() || tree.entries.identity_lookup.is_some() =>
                {
                    tree.anchor = None;
                    tree.selected_track_id = None;
                    tree.entries.identity_lookup = None;
                    if !tree.entries.loaded {
                        tree.entries.selection.select(Some(0));
                    }
                    true
                }
                View::PlaylistDetail { detail, .. } if detail.entry_anchor.is_some() => {
                    detail.entry_anchor = None;
                    detail.selected_entry_id = None;
                    if !detail.entries.loaded {
                        detail.entries.selection.select(Some(0));
                    }
                    true
                }
                View::Playlists(rows) if rows.identity_lookup.is_some() => {
                    rows.identity_lookup = None;
                    if !rows.loaded {
                        rows.selection.select(Some(0));
                    }
                    true
                }
                _ => false,
            };
            if cancelled_lookup {
                self.cancel_query();
            }
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
                    self.reopen_view_queries();
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
                    changed = results.navigate(action, page_size);
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
                    self.reopen_view_queries();
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
                                summary_dirty: false,
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
                            self.reopen_view_queries();
                            self.mark_local_change();
                            return KeyAction::Ignored;
                        }
                    }
                    Action::Activate => {
                        if let Some(playlist) = playlists.selected() {
                            if state.configuration.confirm_queue_replace && state.queue.total != 0 {
                                self.queue_replace = Some(QueueReplaceConfirmation {
                                    playlist_id: playlist.id,
                                    queue_revision: state.queue.revision,
                                    playlist_revision: state
                                        .library
                                        .playlist_entry_revisions
                                        .get(&playlist.id)
                                        .copied(),
                                    prompt: self.i18n.format(
                                        "terminal-confirm-queue-replace",
                                        &[("name", playlist.name.as_str().into())],
                                    ),
                                });
                                self.startup_hint_deadline = None;
                                changed = true;
                            } else {
                                command = Some(Command::PlayPlaylist {
                                    playlist_id: playlist.id,
                                });
                            }
                        }
                    }
                    _ => changed = playlists.navigate_action(action, page_size),
                }
                self.view = if close {
                    self.reopen_view_queries();
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
                        self.queue.anchor = None;
                        self.queue.selected_id = self.queue.selected().map(|row| row.id);
                    }
                }
            },
        }
        changed |= cancelled_lookup;
        if changed {
            if matches!(
                action,
                Action::Up
                    | Action::Down
                    | Action::PageUp
                    | Action::PageDown
                    | Action::First
                    | Action::Last
            ) && let View::LibraryResults(rows) = &mut self.view
            {
                rows.anchor = None;
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
    pub(super) summary_dirty: bool,
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
    fn other_query(&self) -> ReadKind<'_> {
        ReadKind::OtherTracks {
            favorite: self.favorite,
            missing: self.missing,
            sort: self.sort,
            anchor: self.anchor,
        }
    }
    fn other_action(&mut self, action: Action) -> Option<KeyAction> {
        if action == Action::EnqueueCollection {
            let (directories, other_tracks) = if self.other_tracks {
                (Vec::new(), true)
            } else {
                match self.entries.selected() {
                    Some(DirectoryRow::OtherTracks) => (Vec::new(), true),
                    Some(DirectoryRow::Directory { path }) => (vec![path.clone()], false),
                    _ => return Some(KeyAction::Ignored),
                }
            };
            return Some(KeyAction::Command(Command::EnqueueSources {
                directories,
                track_ids: Vec::new(),
                other_tracks,
            }));
        }
        if matches!(
            action,
            Action::CheckOtherTracks | Action::ForceCheckOtherTracks
        ) {
            return Some(KeyAction::Command(Command::CheckOtherTracks {
                force: action == Action::ForceCheckOtherTracks,
            }));
        }
        match action {
            Action::FilterFavorites => {
                self.favorite = if self.favorite.is_some() {
                    None
                } else {
                    Some(true)
                };
            }
            Action::FilterMissing => {
                self.missing = if self.missing.is_some() {
                    None
                } else {
                    Some(true)
                };
            }
            Action::CycleSort => {
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

fn directory_identity(row: &DirectoryRow) -> Option<ContainerIdentity> {
    match row {
        DirectoryRow::Directory { path } => Some(ContainerIdentity::Directory(path.clone())),
        DirectoryRow::OtherTracks => Some(ContainerIdentity::OtherTracks),
        DirectoryRow::Track(_) => None,
    }
}

fn matches_directory_identity(row: &DirectoryRow, identity: &ContainerIdentity) -> bool {
    match (row, identity) {
        (DirectoryRow::Directory { path }, ContainerIdentity::Directory(target)) => path == target,
        (DirectoryRow::OtherTracks, ContainerIdentity::OtherTracks) => true,
        _ => false,
    }
}

pub(super) enum KeyAction {
    Quit,
    Shutdown,
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
