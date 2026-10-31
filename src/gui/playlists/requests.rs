//! Playlist demand and physical gates; cancellation never releases a worker.
use super::{PlaylistBrowser, PlaylistModel, PlaylistRefresh};
use crate::query_tracking::QueryTracker;
use rivu_protocol::{
    model::{Page, PlaylistSummary, Query},
    response::{QueryResponse, QueryRevisions, ViewResponse},
};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::gui) enum ViewRequestKind {
    Summaries,
    Entries,
    Selection,
}

impl ViewRequestKind {
    fn for_query(query: &Query) -> Option<Self> {
        Some(match query {
            Query::PlaylistSummaries { .. } => Self::Summaries,
            Query::PlaylistEntries { .. } => Self::Entries,
            Query::PlaylistSummary { .. } => Self::Selection,
            _ => return None,
        })
    }
}
struct ViewDemand {
    query: Query,
    stamp: rivu_protocol::model::QueryStamp,
    selection_generation: u64,
    quiet_until: Instant,
}

pub(in crate::gui) type SelectionKey = (u64, rivu_protocol::model::QueryStamp);

#[derive(Default)]
pub(super) struct ViewRequests {
    generation: u64,
    generations: HashMap<ViewRequestKind, u64>,
    projections: HashMap<ViewRequestKind, QueryTracker>,
    workers: HashSet<ViewRequestKind>,
    queued: HashMap<ViewRequestKind, (Query, u64)>,
    // Latest intent outlives logical cancellation, hiding and worker completion.
    demands: HashMap<ViewRequestKind, ViewDemand>,
    immediate: HashSet<ViewRequestKind>,
    selection_baseline: Option<SelectionKey>,
    selection_failure: Option<(SelectionKey, String)>,
}

impl ViewRequests {
    fn prepare_view(
        &mut self,
        kind: ViewRequestKind,
        query: Query,
        generation: u64,
        revisions: rivu_protocol::response::QueryRevisions,
        selection_generation: u64,
        now: Instant,
    ) -> Option<(Query, Duration)> {
        let stamp = query.stamp(revisions);
        let selection_generation = if kind == ViewRequestKind::Summaries {
            0
        } else {
            selection_generation
        };
        let demand = self.demands.entry(kind).or_insert_with(|| ViewDemand {
            query: query.clone(),
            stamp,
            selection_generation,
            quiet_until: now + Duration::from_millis(80),
        });
        if demand.query != query
            || demand.stamp != stamp
            || demand.selection_generation != selection_generation
        {
            *demand = ViewDemand {
                query: query.clone(),
                stamp,
                selection_generation,
                quiet_until: now + Duration::from_millis(80),
            };
        }
        if self.immediate.remove(&kind) {
            demand.quiet_until = now;
        }
        let delay = demand.quiet_until.saturating_duration_since(now);
        let replaced = self
            .generations
            .get(&kind)
            .is_some_and(|current| *current != generation);
        self.begin(kind, generation);
        let owner = self.projections.entry(kind).or_default();
        if !owner.sync(&query, revisions) && replaced {
            owner.cancel();
        }
        if !self.workers.insert(kind) {
            self.queued.insert(kind, (query, generation));
            return None;
        }
        Some((query, delay))
    }

    pub(in crate::gui) fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    fn begin(&mut self, kind: ViewRequestKind, generation: u64) {
        self.generations.insert(kind, generation);
    }

    pub(in crate::gui) fn is_current(&self, kind: ViewRequestKind, generation: u64) -> bool {
        self.generations.get(&kind) == Some(&generation)
    }

    fn finish(&mut self, kind: ViewRequestKind, generation: u64) -> bool {
        if !self.is_current(kind, generation) {
            return false;
        }
        self.generations.remove(&kind);
        true
    }

    pub(in crate::gui) fn pending(&self, kind: ViewRequestKind) -> bool {
        self.generations.contains_key(&kind)
    }

    pub(in crate::gui) fn pending_generation(&self, kind: ViewRequestKind) -> Option<u64> {
        self.generations.get(&kind).copied()
    }

    fn cancel(&mut self, kind: ViewRequestKind) {
        self.generations.remove(&kind);
        self.queued.remove(&kind);
        if let Some(projection) = self.projections.get_mut(&kind) {
            projection.cancel();
        }
    }
}

/// The demand lane remains observable even while its physical read is queued.
/// Construction stays with the owner, so dispatch cannot supply a different kind.
pub(in crate::gui) struct ViewRequest {
    kind: ViewRequestKind,
    pending_generation: Option<u64>,
    read: Option<(ViewRead, crate::query_tracking::QueryTicket)>,
}

impl ViewRequest {
    pub(in crate::gui) fn kind(&self) -> ViewRequestKind {
        self.kind
    }

    pub(in crate::gui) fn pending_generation(&self) -> Option<u64> {
        self.pending_generation
    }

    pub(in crate::gui) fn into_read(
        self,
    ) -> Option<(ViewRead, crate::query_tracking::QueryTicket)> {
        self.read
    }
}

/// A reserved physical read. Only completion (including preflight abandonment)
/// releases its gate; logical cancellation only retires its publication rights.
pub(in crate::gui) struct ViewRead {
    pub(in crate::gui) query: Query,
    kind: ViewRequestKind,
    pub(in crate::gui) generation: u64,
    pub(in crate::gui) delay: Duration,
    owner_generation: u64,
    selection_generation: u64,
    selected_playlist: Option<i64>,
}

impl PlaylistModel {
    pub(in crate::gui::playlists) fn sync_entry_projection(
        &mut self,
        revisions: QueryRevisions,
    ) -> bool {
        let Some(id) = self.playlists.selected() else {
            return false;
        };
        let query = self.playlists.entries().query.clone();
        self.playlists
            .track_selection
            .sync_projection(id, query, revisions)
    }

    pub(in crate::gui::playlists) fn playlists(&self) -> &PlaylistBrowser {
        &self.playlists
    }

    /// Selection jobs may mutate identities, never the paired page lifecycle.
    pub(in crate::gui::playlists) fn playlist_selection_mut(
        &mut self,
    ) -> &mut super::super::selection::EntrySelection {
        &mut self.playlists.track_selection
    }

    pub(in crate::gui::playlists) fn view_pending(&self, kind: ViewRequestKind) -> bool {
        self.requests.pending(kind)
    }

    pub(in crate::gui::playlists) fn view_loading(
        &self,
        kind: ViewRequestKind,
        token: u64,
    ) -> bool {
        self.requests.is_current(kind, token)
    }

    pub(in crate::gui::playlists) fn sync_active_playlist_entry(&mut self) {
        self.playlists.sync_active_entry();
    }

    pub(in crate::gui::playlists) fn accept_playlist_entry_locations(
        &mut self,
        rows: Vec<rivu_protocol::model::PlaylistEntryLocation>,
    ) {
        self.playlists.accept_entry_locations(rows);
    }

    pub(in crate::gui::playlists) fn focus(&self) -> Option<crate::gui::BrowserFocus> {
        self.focus
    }

    pub(in crate::gui::playlists) fn focus_list(&mut self, focus: crate::gui::ListFocus) {
        self.focus = Some(crate::gui::BrowserFocus::List(focus));
    }

    pub(in crate::gui::playlists) fn focus_playlists(&mut self) {
        self.focus = Some(crate::gui::BrowserFocus::Playlists);
    }
    pub(in crate::gui::playlists) fn remove_playlist_selection(&mut self) {
        self.cancel_playlist_read(ViewRequestKind::Selection);
        self.cancel_playlist_read(ViewRequestKind::Entries);
        self.playlists.clear_selection();
    }

    fn accept_playlist_summaries(
        &mut self,
        page: Page<PlaylistSummary>,
        revisions: QueryRevisions,
    ) {
        if page.total == 0 && self.playlists.summaries().query.is_none() {
            self.remove_playlist_selection();
        }
        let contains_selected = self.playlists.accept_summaries(page);
        if contains_selected {
            let key = self.playlist_lookup(revisions).map(|(_, key)| key);
            if self
                .requests
                .selection_failure
                .as_ref()
                .map(|(key, _)| *key)
                == key
            {
                // A page can restore the fact, but cannot retire a newer lookup intent.
                self.requests.selection_failure = None;
            }
        }
    }

    /// Resolve selection by identity independently of catalog order and pagination.
    pub(in crate::gui::playlists) fn accept_playlist_lookup(
        &mut self,
        id: i64,
        first: Option<PlaylistSummary>,
    ) {
        if self.playlists.selected() != Some(id) {
            return;
        }
        if let Some(summary) = first.filter(|summary| summary.id == id) {
            self.playlists.accept_selected_summary(summary);
            self.requests.selection_failure = None;
        } else {
            self.remove_playlist_selection();
        }
    }
    fn playlist_request(&self, kind: ViewRequestKind) -> ViewRequest {
        ViewRequest {
            kind,
            pending_generation: self.requests.pending_generation(kind),
            read: None,
        }
    }

    pub(in crate::gui) fn select_playlist(&mut self, id: i64, name: String, index: usize) -> bool {
        if !self.playlists.select(id, name, index) {
            return false;
        }
        self.cancel_playlist_read(ViewRequestKind::Selection);
        self.cancel_playlist_read(ViewRequestKind::Entries);
        self.playlists.prepare_entries(id);
        true
    }

    pub(in crate::gui) fn request_playlist_summaries(
        &mut self,
        offset: usize,
        page_size: usize,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        self.prepare_playlist_page(false, offset, page_size, true, revisions)
    }

    pub(in crate::gui) fn request_playlist_entries(
        &mut self,
        id: i64,
        offset: usize,
        page_size: usize,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        if self.playlists.entries().playlist_id != Some(id) {
            self.cancel_playlist_read(ViewRequestKind::Entries);
            self.playlists.prepare_entries(id);
        }
        self.prepare_playlist_page(true, offset, page_size, true, revisions)
    }

    pub(in crate::gui) fn refresh_playlists(&mut self, refresh: PlaylistRefresh) {
        if refresh.summaries {
            self.cancel_playlist_read(ViewRequestKind::Summaries);
            self.playlists.invalidate_summaries();
            self.requests.selection_failure = None;
        }
        if refresh.entries {
            self.cancel_playlist_read(ViewRequestKind::Entries);
            if refresh.entry_payload_only {
                self.playlists.invalidate_entry_payload();
            } else if let Some(id) = self.playlists.entries().playlist_id {
                self.playlists.prepare_entries(id);
            }
        }
    }
    pub(in crate::gui) fn search_playlists(&mut self, query: Option<String>) {
        self.remove_playlist_selection();
        self.cancel_playlist_read(ViewRequestKind::Summaries);
        self.playlists.search_summaries(query);
        self.requests.immediate.insert(ViewRequestKind::Summaries);
    }

    pub(in crate::gui) fn search_playlist_entries(&mut self, query: Option<String>) {
        self.cancel_playlist_read(ViewRequestKind::Entries);
        self.playlists.search_entries(query);
        self.requests.immediate.insert(ViewRequestKind::Entries);
    }

    pub(in crate::gui) fn cancel_playlist_read(&mut self, kind: ViewRequestKind) {
        self.requests.cancel(kind);
        match kind {
            ViewRequestKind::Summaries => self.playlists.cancel_page(false),
            ViewRequestKind::Entries => self.playlists.cancel_page(true),
            ViewRequestKind::Selection => {
                self.requests.selection_baseline = None;
                self.requests.selection_failure = None;
            }
        }
    }

    pub(in crate::gui) fn cancel_window_reads(&mut self) {
        for kind in [
            ViewRequestKind::Summaries,
            ViewRequestKind::Entries,
            ViewRequestKind::Selection,
        ] {
            if self.requests.pending(kind) {
                self.cancel_playlist_read(kind);
            }
        }
    }
    pub(in crate::gui) fn configure_page_size(&mut self, page_size: usize) {
        if self.page_size == page_size {
            return;
        }
        self.page_size = page_size;
        // Lookup is not a page. Identity, pins and scroll survive size changes.
        self.cancel_playlist_read(ViewRequestKind::Summaries);
        self.cancel_playlist_read(ViewRequestKind::Entries);
        self.playlists.configure_page_size(page_size);
    }
    pub(in crate::gui) fn demand_playlist_summaries(
        &mut self,
        range: std::ops::Range<usize>,
        page_size: usize,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        self.demand_playlist_page(false, range, page_size, revisions)
    }

    pub(in crate::gui) fn demand_playlist_entries(
        &mut self,
        range: std::ops::Range<usize>,
        page_size: usize,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        self.demand_playlist_page(true, range, page_size, revisions)
    }

    pub(in crate::gui) fn retry_playlist_summaries(
        &mut self,
        page_size: usize,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        self.retry_playlist_page(false, page_size, revisions)
    }

    pub(in crate::gui) fn retry_playlist_entries(
        &mut self,
        page_size: usize,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        self.retry_playlist_page(true, page_size, revisions)
    }

    fn prepare_playlist_page(
        &mut self,
        entries: bool,
        offset: usize,
        page_size: usize,
        explicit: bool,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        let kind = if entries {
            ViewRequestKind::Entries
        } else {
            ViewRequestKind::Summaries
        };
        if explicit {
            self.requests.demands.remove(&kind);
        }
        let query = self.playlists.prepare_page(entries, offset, page_size);
        let generation = self.requests.next_generation();
        self.prepare_playlist_read(query, generation, revisions)
    }

    fn demand_playlist_page(
        &mut self,
        entries: bool,
        range: std::ops::Range<usize>,
        page_size: usize,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        let kind = if entries {
            ViewRequestKind::Entries
        } else {
            ViewRequestKind::Summaries
        };
        if entries && self.playlists.entries().playlist_id.is_none() {
            return self.playlist_request(kind);
        }
        let Some(offset) = self.playlists.demand_page(entries, range, page_size) else {
            return self.playlist_request(kind);
        };
        self.prepare_playlist_page(entries, offset, page_size, false, revisions)
    }

    fn retry_playlist_page(
        &mut self,
        entries: bool,
        page_size: usize,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        let kind = if entries {
            ViewRequestKind::Entries
        } else {
            ViewRequestKind::Summaries
        };
        if entries && self.playlists.entries().playlist_id.is_none() {
            return self.playlist_request(kind);
        }
        let Some(offset) = self.playlists.retry_page(entries, page_size) else {
            return self.playlist_request(kind);
        };
        self.prepare_playlist_page(entries, offset, page_size, false, revisions)
    }

    fn playlist_lookup(&self, revisions: QueryRevisions) -> Option<(Query, SelectionKey)> {
        let id = self.playlists.selected()?;
        let query = Query::PlaylistSummary { playlist_id: id };
        let key = (
            self.playlists.selection_generation(),
            query.stamp(revisions),
        );
        Some((query, key))
    }

    pub(in crate::gui::playlists) fn playlist_lookup_failure(
        &self,
    ) -> Option<(SelectionKey, &str)> {
        self.requests
            .selection_failure
            .as_ref()
            .map(|(key, message)| (*key, message.as_str()))
    }

    pub(in crate::gui) fn prepare_playlist_lookup(
        &mut self,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        let Some((query, key)) = self.playlist_lookup(revisions) else {
            return self.playlist_request(ViewRequestKind::Selection);
        };
        if self.requests.selection_baseline == Some(key) {
            return self.playlist_request(ViewRequestKind::Selection);
        }
        self.requests.selection_failure = None;
        self.requests.selection_baseline = Some(key);
        let generation = self.requests.next_generation();
        self.prepare_playlist_read(query, generation, revisions)
    }

    pub(in crate::gui::playlists) fn retry_playlist_lookup(
        &mut self,
        failed_key: SelectionKey,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        let Some((query, key)) = self.playlist_lookup(revisions) else {
            return self.playlist_request(ViewRequestKind::Selection);
        };
        if key != failed_key
            || self.playlist_lookup_failure().map(|(key, _)| key) != Some(failed_key)
            || self.requests.pending(ViewRequestKind::Selection)
        {
            return self.playlist_request(ViewRequestKind::Selection);
        }
        self.requests.selection_failure = None;
        self.requests.selection_baseline = Some(key);
        let generation = self.requests.next_generation();
        self.prepare_playlist_read(query, generation, revisions)
    }

    fn prepare_playlist_read(
        &mut self,
        query: Query,
        generation: u64,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        let kind = ViewRequestKind::for_query(&query).expect("playlist read query");
        let selection_generation = self.playlists.selection_generation();
        let read = self
            .requests
            .prepare_view(
                kind,
                query,
                generation,
                revisions,
                selection_generation,
                Instant::now(),
            )
            .map(|(query, delay)| {
                let owner = self
                    .requests
                    .projections
                    .get_mut(&kind)
                    .expect("prepared view owner");
                let read = ViewRead {
                    query,
                    kind,
                    generation,
                    delay,
                    owner_generation: owner.generation(),
                    selection_generation,
                    selected_playlist: self.playlists.selected(),
                };
                (read, owner.ticket())
            });
        ViewRequest {
            kind,
            pending_generation: self.requests.pending_generation(kind),
            read,
        }
    }

    pub(in crate::gui) fn queued_playlist_read(
        &mut self,
        kind: ViewRequestKind,
        demanded: bool,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        let Some((query, generation)) = self.requests.queued.remove(&kind) else {
            return self.playlist_request(kind);
        };
        if !demanded {
            self.cancel_playlist_read(kind);
            return self.playlist_request(kind);
        }
        self.prepare_playlist_read(query, generation, revisions)
    }

    pub(in crate::gui) fn playlist_read_current(
        &mut self,
        read: &ViewRead,
        revisions: QueryRevisions,
    ) -> bool {
        if !self.requests.is_current(read.kind, read.generation)
            || (read.kind != ViewRequestKind::Summaries
                && self.playlists.selection_generation() != read.selection_generation)
        {
            return false;
        }
        let owner = self
            .requests
            .projections
            .get_mut(&read.kind)
            .expect("prepared view owner");
        owner.sync(&read.query, revisions);
        owner.accepts(&read.query, read.owner_generation, revisions)
    }

    pub(in crate::gui) fn abandon_playlist_read(&mut self, read: &ViewRead) {
        self.requests.workers.remove(&read.kind);
        if self.requests.is_current(read.kind, read.generation) {
            self.cancel_playlist_read(read.kind);
        }
    }

    pub(in crate::gui) fn complete_playlist_read(
        &mut self,
        read: &ViewRead,
        current: QueryRevisions,
        response: QueryResponse,
    ) -> Option<String> {
        self.requests.workers.remove(&read.kind);
        self.publish_playlist_read(read, current, response)
    }

    /// Timeout ends publication/loading, but only final completion releases the worker.
    pub(in crate::gui) fn timeout_playlist_read(
        &mut self,
        read: &ViewRead,
        current: QueryRevisions,
        response: QueryResponse,
    ) -> Option<String> {
        self.publish_playlist_read(read, current, response)
    }

    fn publish_playlist_read(
        &mut self,
        read: &ViewRead,
        current: QueryRevisions,
        response: QueryResponse,
    ) -> Option<String> {
        if !self.requests.is_current(read.kind, read.generation) {
            return None;
        }
        if !self.playlist_read_current(read, current)
            || (response.result.is_ok()
                && !self.requests.projections[&read.kind].accepts(
                    &read.query,
                    read.owner_generation,
                    response.revisions,
                ))
        {
            self.cancel_playlist_read(read.kind);
            return None;
        }
        self.requests.finish(read.kind, read.generation);
        match response.result {
            Err(error) => {
                let message = error.to_string();
                match read.kind {
                    ViewRequestKind::Summaries => self.playlists.fail_page(false, message.clone()),
                    ViewRequestKind::Entries => self.playlists.fail_page(true, message.clone()),
                    ViewRequestKind::Selection => {
                        let key = (read.selection_generation, read.query.stamp(current));
                        self.requests.selection_failure = Some((key, message.clone()));
                    }
                }
                Some(message)
            }
            Ok(ViewResponse::PlaylistSummaries(page))
                if read.kind == ViewRequestKind::Summaries =>
            {
                self.accept_playlist_summaries(page, current);
                None
            }
            Ok(ViewResponse::PlaylistEntries(page)) if read.kind == ViewRequestKind::Entries => {
                self.playlists.accept_entries(page);
                None
            }
            Ok(ViewResponse::PlaylistSummary(summary))
                if read.kind == ViewRequestKind::Selection =>
            {
                if let Some(id) = read.selected_playlist {
                    self.accept_playlist_lookup(id, summary);
                }
                None
            }
            _ => {
                self.cancel_playlist_read(read.kind);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::GuiLibrarySnapshot;
    use std::sync::Arc;

    fn summary(id: i64, entry_count: usize) -> PlaylistSummary {
        PlaylistSummary {
            id,
            name: format!("playlist {id}"),
            entry_count,
        }
    }

    fn entry(id: i64, position: usize) -> rivu_protocol::model::PlaylistEntryRow {
        rivu_protocol::model::PlaylistEntryRow {
            id,
            track_id: 1,
            position,
            offset: position,
            title: format!("entry {id}"),
            path: Default::default(),
            channels: 0,
            sample_rate: 0,
            bits_per_sample: None,
            bitrate_bps: None,
            bitrate_exact: false,
            cue_number: None,
            artist: String::new(),
            album: String::new(),
            duration: None,
            codec: String::new(),
            missing: false,
            favorite: false,
        }
    }

    #[test]
    fn bitrate_revision_retains_identity_counts_viewport_catalog_and_worker() {
        let mut browser = PlaylistModel::default();
        let previous = GuiLibrarySnapshot::default();
        let latest = GuiLibrarySnapshot {
            bitrate_revision: 1,
            ..previous.clone()
        };
        let revisions = previous.query_revisions(Some(1));
        let current = latest.query_revisions(Some(1));
        browser.select_playlist(1, "one".into(), 0);
        browser.playlists.prepare_page(false, 0, 20);
        browser.playlists.accept_summaries(Page {
            total: 1,
            rows: vec![summary(1, 100)],
        });
        browser.playlists.prepare_page(true, 0, 20);
        browser.playlists.accept_entries(Page {
            total: 100,
            rows: vec![entry(10, 0), entry(11, 1)],
        });
        browser.sync_entry_projection(revisions);
        browser.playlists.select_entry(10, 0);
        let (old, _) = browser
            .demand_playlist_entries(40..50, 20, revisions)
            .into_read()
            .unwrap();
        let generation = browser.playlists.selection_generation();
        let changes = browser.playlists.refresh_changes(&previous, &latest);
        assert!(!changes.summaries);
        assert!(changes.entries);
        assert!(changes.entry_payload_only);
        assert!(!browser.sync_entry_projection(current));
        browser.refresh_playlists(changes);
        assert_eq!(browser.playlists.selected_entry(), Some(10));
        assert_eq!(browser.playlists.selection_generation(), generation);
        assert!(browser.playlists.entry_ready());
        assert_eq!(browser.playlists.entries().rows.total(), Some(100));
        assert_eq!(browser.playlists.entries().rows.visible(), 40..50);
        assert_eq!(browser.playlists.selected_total(), Some(100));
        assert!(browser.playlists.summaries().rows.get(0).is_some());
        assert!(browser.playlists.entries().rows.get(0).is_none());
        assert!(browser.requests.workers.contains(&ViewRequestKind::Entries));
        assert!(
            !browser
                .requests
                .queued
                .contains_key(&ViewRequestKind::Entries)
        );
        assert!(
            browser
                .demand_playlist_entries(40..50, 20, current)
                .into_read()
                .is_none()
        );
        browser.complete_playlist_read(
            &old,
            current,
            QueryResponse {
                revisions,
                result: Ok(ViewResponse::PlaylistEntries(Page {
                    total: 100,
                    rows: vec![entry(12, 40)],
                })),
            },
        );
        assert!(browser.playlists.entries().rows.get(40).is_none());
        let (replacement, _) = browser
            .queued_playlist_read(ViewRequestKind::Entries, true, current)
            .into_read()
            .unwrap();
        assert_ne!(replacement.generation, old.generation);
    }

    #[test]
    fn playlist_payload_rejects_stale_bitrate_and_accepts_current_reply() {
        let mut browser = PlaylistModel::default();
        browser.select_playlist(1, "one".into(), 0);
        let revisions = QueryRevisions {
            bitrate: 1,
            ..QueryRevisions::default()
        };
        let (read, _) = browser
            .request_playlist_entries(1, 0, 20, revisions)
            .into_read()
            .unwrap();
        browser.complete_playlist_read(
            &read,
            revisions,
            QueryResponse {
                revisions: QueryRevisions::default(),
                result: Ok(ViewResponse::PlaylistEntries(Page {
                    total: 100,
                    rows: vec![entry(10, 0)],
                })),
            },
        );
        assert!(browser.playlists.entries().rows.get(0).is_none());
        assert!(!browser.requests.workers.contains(&ViewRequestKind::Entries));
        let (read, _) = browser
            .demand_playlist_entries(0..1, 20, revisions)
            .into_read()
            .unwrap();
        let mut row = entry(10, 0);
        row.bitrate_bps = Some(192_000);
        row.bitrate_exact = true;
        browser.complete_playlist_read(
            &read,
            revisions,
            QueryResponse {
                revisions,
                result: Ok(ViewResponse::PlaylistEntries(Page {
                    total: 100,
                    rows: vec![row],
                })),
            },
        );
        let loaded = browser.playlists.entries().rows.get(0).unwrap();
        assert_eq!(loaded.bitrate_bps, Some(192_000));
        assert!(loaded.bitrate_exact);
    }

    #[test]
    fn stale_playlist_source_completion_is_not_published() {
        let mut browser = PlaylistModel::default();
        let revisions = QueryRevisions::default();
        browser.select_playlist(1, "one".into(), 0);
        let (read, _) = browser
            .request_playlist_entries(1, 0, 20, revisions)
            .into_read()
            .unwrap();
        let current = QueryRevisions {
            library: 1,
            bitrate: 1,
            ..revisions
        };
        browser.complete_playlist_read(
            &read,
            current,
            QueryResponse {
                revisions,
                result: Ok(ViewResponse::PlaylistEntries(Page {
                    total: 100,
                    rows: vec![entry(10, 0)],
                })),
            },
        );
        assert!(browser.playlists.entries().rows.get(0).is_none());
        assert!(!browser.requests.workers.contains(&ViewRequestKind::Entries));
    }

    fn lookup_error(revisions: QueryRevisions, message: &str) -> QueryResponse {
        QueryResponse {
            revisions,
            result: Err(rivu_protocol::response::QueryError::Message(message.into())),
        }
    }

    fn lookup_success(revisions: QueryRevisions, id: i64, count: usize) -> QueryResponse {
        QueryResponse {
            revisions,
            result: Ok(ViewResponse::PlaylistSummary(Some(summary(id, count)))),
        }
    }

    #[test]
    fn failed_summary_parks_until_explicit_retry_and_recovers_in_place() {
        let mut browser = PlaylistModel::default();
        let revisions = QueryRevisions::default();
        browser.select_playlist(41, "one".into(), 0);
        let (read, _) = browser
            .prepare_playlist_lookup(revisions)
            .into_read()
            .unwrap();
        assert_eq!(
            browser.complete_playlist_read(&read, revisions, lookup_error(revisions, "offline")),
            Some("offline".into())
        );
        let (key, error) = browser.playlist_lookup_failure().unwrap();
        assert_eq!(error, "offline");
        for _ in 0..3 {
            assert!(
                browser
                    .prepare_playlist_lookup(revisions)
                    .into_read()
                    .is_none()
            );
            assert!(!browser.view_pending(ViewRequestKind::Selection));
        }
        browser.search_playlist_entries(Some("song".into()));
        browser.playlist_selection_mut().cancel_manual();
        browser.configure_page_size(40);
        browser.cancel_window_reads();
        assert_eq!(browser.playlist_lookup_failure(), Some((key, "offline")));
        let (retry, _) = browser
            .retry_playlist_lookup(key, revisions)
            .into_read()
            .unwrap();
        assert_ne!(retry.generation, read.generation);
        assert_eq!(browser.playlist_lookup_failure(), None);
        assert!(
            browser
                .prepare_playlist_lookup(revisions)
                .into_read()
                .is_none()
        );
        assert!(
            browser
                .retry_playlist_lookup(key, revisions)
                .into_read()
                .is_none()
        );
        browser.complete_playlist_read(&retry, revisions, lookup_success(revisions, 41, 7));
        assert_eq!(browser.playlists.selected(), Some(41));
        assert_eq!(browser.playlists.selected_total(), Some(7));
        assert_eq!(browser.playlist_lookup_failure(), None);
        assert!(!browser.view_pending(ViewRequestKind::Selection));
    }

    #[test]
    fn timed_out_summary_retry_waits_for_original_worker_and_ignores_late_results() {
        let mut browser = PlaylistModel::default();
        let revisions = QueryRevisions::default();
        browser.select_playlist(41, "one".into(), 0);
        let (old, _) = browser
            .prepare_playlist_lookup(revisions)
            .into_read()
            .unwrap();
        browser.timeout_playlist_read(&old, revisions, lookup_error(revisions, "timeout"));
        let key = browser.playlist_lookup_failure().unwrap().0;
        assert!(
            browser
                .prepare_playlist_lookup(revisions)
                .into_read()
                .is_none()
        );
        let queued = browser.retry_playlist_lookup(key, revisions);
        let generation = queued.pending_generation().unwrap();
        assert!(queued.into_read().is_none());
        assert_ne!(generation, old.generation);
        assert_eq!(browser.playlist_lookup_failure(), None);
        assert_eq!(
            browser.timeout_playlist_read(&old, revisions, lookup_error(revisions, "late timeout")),
            None
        );
        assert!(browser.view_loading(ViewRequestKind::Selection, generation));
        assert_eq!(
            browser.complete_playlist_read(&old, revisions, lookup_success(revisions, 41, 99)),
            None
        );
        assert_eq!(browser.playlists.selected_total(), None);
        assert!(browser.view_loading(ViewRequestKind::Selection, generation));
        let (retry, _) = browser
            .queued_playlist_read(ViewRequestKind::Selection, true, revisions)
            .into_read()
            .unwrap();
        assert_eq!(retry.generation, generation);
        browser.complete_playlist_read(&retry, revisions, lookup_success(revisions, 41, 3));
        assert_eq!(browser.playlists.selected_total(), Some(3));
        assert!(!browser.view_pending(ViewRequestKind::Selection));
    }

    #[test]
    fn summary_revision_rejects_old_retry_and_old_timeout_cannot_replace_new_failure() {
        let mut browser = PlaylistModel::default();
        let revisions = QueryRevisions::default();
        browser.select_playlist(41, "one".into(), 0);
        let (old, _) = browser
            .prepare_playlist_lookup(revisions)
            .into_read()
            .unwrap();
        browser.timeout_playlist_read(&old, revisions, lookup_error(revisions, "old timeout"));
        let old_key = browser.playlist_lookup_failure().unwrap().0;
        let current = QueryRevisions {
            playlist: 1,
            ..revisions
        };
        browser.refresh_playlists(PlaylistRefresh {
            summaries: true,
            entries: false,
            entry_payload_only: false,
        });
        assert_eq!(browser.playlist_lookup_failure(), None);
        let obsolete_retry = browser.retry_playlist_lookup(old_key, current);
        assert!(obsolete_retry.into_read().is_none());
        let queued = browser.prepare_playlist_lookup(current);
        let generation = queued.pending_generation().unwrap();
        assert!(queued.into_read().is_none());
        browser.complete_playlist_read(&old, current, lookup_error(revisions, "old final"));
        assert!(browser.view_loading(ViewRequestKind::Selection, generation));
        let (new, _) = browser
            .queued_playlist_read(ViewRequestKind::Selection, true, current)
            .into_read()
            .unwrap();
        browser.timeout_playlist_read(&new, current, lookup_error(current, "new timeout"));
        let new_key = browser.playlist_lookup_failure().unwrap().0;
        assert_ne!(old_key, new_key);
        browser.timeout_playlist_read(&old, current, lookup_error(revisions, "obsolete"));
        assert_eq!(
            browser.playlist_lookup_failure(),
            Some((new_key, "new timeout"))
        );
        assert!(
            browser
                .retry_playlist_lookup(old_key, current)
                .into_read()
                .is_none()
        );
        assert!(!browser.view_pending(ViewRequestKind::Selection));
        assert!(
            browser
                .prepare_playlist_lookup(current)
                .into_read()
                .is_none()
        );
    }

    #[test]
    fn current_catalog_page_restores_summary_without_cancelling_retry() {
        let mut browser = PlaylistModel::default();
        let revisions = QueryRevisions::default();
        browser.select_playlist(41, "one".into(), 0);
        let (read, _) = browser
            .prepare_playlist_lookup(revisions)
            .into_read()
            .unwrap();
        browser.complete_playlist_read(&read, revisions, lookup_error(revisions, "offline"));
        browser.playlists.prepare_page(false, 0, 20);
        browser.accept_playlist_summaries(
            Page {
                total: 1,
                rows: vec![summary(41, 5)],
            },
            revisions,
        );
        assert_eq!(browser.playlists.selected_total(), Some(5));
        assert_eq!(browser.playlist_lookup_failure(), None);
        assert!(
            browser
                .prepare_playlist_lookup(revisions)
                .into_read()
                .is_none()
        );

        browser.cancel_playlist_read(ViewRequestKind::Selection);
        let (read, _) = browser
            .prepare_playlist_lookup(revisions)
            .into_read()
            .unwrap();
        browser.complete_playlist_read(&read, revisions, lookup_error(revisions, "offline"));
        let key = browser.playlist_lookup_failure().unwrap().0;
        let (retry, _) = browser
            .retry_playlist_lookup(key, revisions)
            .into_read()
            .unwrap();
        browser.playlists.prepare_page(false, 0, 20);
        browser.accept_playlist_summaries(
            Page {
                total: 1,
                rows: vec![summary(41, 8)],
            },
            revisions,
        );
        assert_eq!(browser.playlists.selected_total(), Some(8));
        assert!(browser.playlist_read_current(&retry, revisions));
        browser.complete_playlist_read(&retry, revisions, lookup_success(revisions, 41, 8));
        assert_eq!(browser.playlists.selected_total(), Some(8));
    }

    #[test]
    fn catalog_revision_makes_count_unknown_until_selected_summary_is_current() {
        let mut browser = PlaylistModel::default();
        let revisions = QueryRevisions::default();
        browser.select_playlist(41, "one".into(), 0);
        browser.accept_playlist_lookup(41, Some(summary(41, 0)));
        assert_eq!(browser.playlists.selected_total(), Some(0));
        browser.refresh_playlists(PlaylistRefresh {
            summaries: true,
            entries: false,
            entry_payload_only: false,
        });
        assert_eq!(browser.playlists.selected_total(), None);
        browser.playlists.prepare_page(false, 0, 20);
        browser.accept_playlist_summaries(
            Page {
                total: 2,
                rows: vec![summary(1, 4)],
            },
            revisions,
        );
        assert_eq!(browser.playlists.selected_total(), None);
        let (read, _) = browser
            .prepare_playlist_lookup(revisions)
            .into_read()
            .unwrap();
        browser.complete_playlist_read(&read, revisions, lookup_error(revisions, "offline"));
        assert_eq!(browser.playlists.selected_total(), None);
        let failure = browser.playlist_lookup_failure().unwrap().0;
        browser.playlists.prepare_page(false, 0, 20);
        browser.accept_playlist_summaries(
            Page {
                total: 2,
                rows: vec![summary(1, 4)],
            },
            revisions,
        );
        assert_eq!(browser.playlists.selected_total(), None);
        assert_eq!(
            browser.playlist_lookup_failure(),
            Some((failure, "offline"))
        );
        browser.playlists.prepare_page(false, 0, 20);
        browser.accept_playlist_summaries(
            Page {
                total: 2,
                rows: vec![summary(41, 9)],
            },
            revisions,
        );
        assert_eq!(browser.playlists.selected_total(), Some(9));
        assert_eq!(browser.playlist_lookup_failure(), None);
    }

    #[test]
    fn selection_cancellation_change_and_removal_clear_summary_failure() {
        for action in 0..3 {
            let mut browser = PlaylistModel::default();
            let revisions = QueryRevisions::default();
            browser.select_playlist(41, "one".into(), 0);
            let (read, _) = browser
                .prepare_playlist_lookup(revisions)
                .into_read()
                .unwrap();
            browser.complete_playlist_read(&read, revisions, lookup_error(revisions, "offline"));
            let key = browser.playlist_lookup_failure().unwrap().0;
            match action {
                0 => browser.cancel_playlist_read(ViewRequestKind::Selection),
                1 => {
                    browser.select_playlist(42, "two".into(), 1);
                }
                _ => browser.remove_playlist_selection(),
            }
            assert_eq!(browser.playlist_lookup_failure(), None);
            assert!(
                browser
                    .retry_playlist_lookup(key, revisions)
                    .into_read()
                    .is_none()
            );
            if action != 2 {
                assert!(
                    browser
                        .prepare_playlist_lookup(revisions)
                        .into_read()
                        .is_some()
                );
            }
        }
    }

    #[test]
    fn view_timeout_publishes_once_and_retains_every_lane_until_retirement() {
        for kind in [
            ViewRequestKind::Summaries,
            ViewRequestKind::Entries,
            ViewRequestKind::Selection,
        ] {
            let mut browser = PlaylistModel::default();
            let revisions = QueryRevisions::default();
            browser.select_playlist(1, "one".into(), 0);
            let prepared = match kind {
                ViewRequestKind::Summaries => browser.request_playlist_summaries(0, 20, revisions),
                ViewRequestKind::Entries => browser.request_playlist_entries(1, 0, 20, revisions),
                ViewRequestKind::Selection => browser.prepare_playlist_lookup(revisions),
            };
            let (read, _) = prepared.into_read().unwrap();
            let timeout = || QueryResponse {
                revisions,
                result: Err(rivu_protocol::response::QueryError::Message(
                    "timeout".into(),
                )),
            };
            assert_eq!(
                browser.timeout_playlist_read(&read, revisions, timeout()),
                Some("timeout".into())
            );
            assert!(!browser.view_pending(kind));
            assert!(browser.requests.workers.contains(&kind));
            assert_eq!(
                browser.complete_playlist_read(&read, revisions, timeout()),
                None
            );
            assert!(!browser.requests.workers.contains(&kind));
            assert!(!browser.view_pending(kind));
            match kind {
                ViewRequestKind::Summaries => {
                    assert_eq!(browser.playlists.summaries().rows.error(0), Some("timeout"))
                }
                ViewRequestKind::Entries => {
                    assert_eq!(browser.playlists.entries().rows.error(0), Some("timeout"))
                }
                ViewRequestKind::Selection => assert_eq!(browser.playlists.selected(), Some(1)),
            }
        }
    }

    #[test]
    fn obsolete_view_timeout_and_final_return_preserve_latest_queued_demand() {
        let mut browser = PlaylistModel::default();
        let revisions = QueryRevisions::default();
        browser.select_playlist(1, "one".into(), 0);
        let (read, _) = browser
            .request_playlist_entries(1, 0, 20, revisions)
            .into_read()
            .unwrap();
        let timeout = || QueryResponse {
            revisions,
            result: Err(rivu_protocol::response::QueryError::Message(
                "timeout".into(),
            )),
        };
        assert!(
            browser
                .timeout_playlist_read(&read, revisions, timeout())
                .is_some()
        );
        browser.search_playlist_entries(Some("latest".into()));
        let queued = browser.request_playlist_entries(1, 0, 20, revisions);
        let generation = queued.pending_generation().unwrap();
        assert!(queued.into_read().is_none());
        assert_eq!(
            browser.timeout_playlist_read(&read, revisions, timeout()),
            None
        );
        assert!(browser.view_loading(ViewRequestKind::Entries, generation));
        assert_eq!(browser.playlists.entries().rows.error(0), None);
        assert_eq!(
            browser.complete_playlist_read(&read, revisions, timeout()),
            None
        );
        assert!(browser.view_loading(ViewRequestKind::Entries, generation));
        assert_eq!(browser.playlists.entries().rows.error(0), None);
        let (latest, _) = browser
            .queued_playlist_read(ViewRequestKind::Entries, true, revisions)
            .into_read()
            .unwrap();
        assert_eq!(latest.generation, generation);
        assert_eq!(browser.playlists.entries().query.as_deref(), Some("latest"));
    }

    #[test]
    fn search_replacement_keeps_worker_and_rejects_old_completion() {
        let mut browser = PlaylistModel::default();
        let revisions = QueryRevisions::default();
        browser.select_playlist(1, "one".into(), 0);
        let (old, _) = browser
            .request_playlist_entries(1, 0, 20, revisions)
            .into_read()
            .unwrap();
        browser.search_playlist_entries(Some("new".into()));
        assert!(browser.requests.workers.contains(&ViewRequestKind::Entries));
        let queued = browser.request_playlist_entries(1, 0, 20, revisions);
        assert_eq!(queued.kind(), ViewRequestKind::Entries);
        let queued_generation = queued
            .pending_generation()
            .expect("queued loading generation");
        assert!(queued.into_read().is_none());
        assert_ne!(queued_generation, old.generation);
        let still_queued = browser.demand_playlist_entries(0..1, 20, revisions);
        assert_eq!(still_queued.pending_generation(), Some(queued_generation));
        assert!(still_queued.into_read().is_none());
        browser.complete_playlist_read(
            &old,
            revisions,
            QueryResponse {
                revisions,
                result: Ok(ViewResponse::PlaylistEntries(rivu_protocol::model::Page {
                    total: 0,
                    rows: Vec::new(),
                })),
            },
        );
        assert!(browser.requests.pending(ViewRequestKind::Entries));
        assert_eq!(browser.playlists.entries().rows.total(), None);
        let (replacement, _) = browser
            .queued_playlist_read(ViewRequestKind::Entries, true, revisions)
            .into_read()
            .unwrap();
        assert_eq!(replacement.generation, queued_generation);
        browser.complete_playlist_read(
            &replacement,
            revisions,
            QueryResponse {
                revisions,
                result: Ok(ViewResponse::PlaylistEntries(rivu_protocol::model::Page {
                    total: 0,
                    rows: Vec::new(),
                })),
            },
        );
        assert!(!browser.requests.pending(ViewRequestKind::Entries));
        assert_eq!(browser.playlists.entries().rows.total(), Some(0));
        assert_eq!(browser.playlists.selected(), Some(1));
    }

    #[test]
    fn catalog_demand_survives_entry_selection_removal() {
        let mut browser = PlaylistModel::default();
        let revisions = QueryRevisions::default();
        let absent = browser.demand_playlist_entries(0..1, 20, revisions);
        assert_eq!(absent.pending_generation(), None);
        assert!(absent.into_read().is_none());

        let catalog = browser.demand_playlist_summaries(0..1, 20, revisions);
        let catalog_generation = catalog.pending_generation().unwrap();
        let (catalog, _) = catalog.into_read().unwrap();
        browser.select_playlist(1, "one".into(), 0);
        let (entries, _) = browser
            .request_playlist_entries(1, 0, 20, revisions)
            .into_read()
            .unwrap();
        browser.remove_playlist_selection();

        let retained = browser.demand_playlist_summaries(0..1, 20, revisions);
        assert_eq!(retained.kind(), ViewRequestKind::Summaries);
        assert_eq!(retained.pending_generation(), Some(catalog_generation));
        assert!(retained.into_read().is_none());
        assert!(browser.playlist_read_current(&catalog, revisions));
        assert!(!browser.playlist_read_current(&entries, revisions));
        assert!(browser.requests.workers.contains(&ViewRequestKind::Entries));
        assert!(!browser.requests.pending(ViewRequestKind::Entries));
    }

    #[test]
    fn page_resize_preserves_lookup_and_stale_completion_releases_page_reservation() {
        let mut browser = PlaylistModel::default();
        let revisions = QueryRevisions::default();
        browser.select_playlist(1, "one".into(), 0);
        let (lookup, _) = browser
            .prepare_playlist_lookup(revisions)
            .into_read()
            .unwrap();
        let (page, _) = browser
            .request_playlist_entries(1, 0, 20, revisions)
            .into_read()
            .unwrap();
        browser.configure_page_size(40);
        assert!(browser.playlist_read_current(&lookup, revisions));
        assert!(browser.requests.workers.contains(&ViewRequestKind::Entries));
        browser.abandon_playlist_read(&page);
        assert_eq!(browser.playlists.selected(), Some(1));
        let (page, _) = browser
            .demand_playlist_entries(0..1, 40, revisions)
            .into_read()
            .unwrap();
        let changed = QueryRevisions {
            library: revisions.library + 1,
            ..revisions
        };
        browser.complete_playlist_read(
            &page,
            changed,
            QueryResponse {
                revisions,
                result: Ok(ViewResponse::PlaylistEntries(rivu_protocol::model::Page {
                    total: 0,
                    rows: Vec::new(),
                })),
            },
        );
        assert!(!browser.requests.pending(ViewRequestKind::Entries));
        assert!(
            browser
                .demand_playlist_entries(0..1, 40, changed)
                .into_read()
                .is_some()
        );
        assert!(browser.playlist_read_current(&lookup, changed));
    }
    #[test]
    fn queued_playlist_demand_uses_elapsed_latest_deadline() {
        let start = Instant::now();
        let kind = ViewRequestKind::Entries;
        let query = |offset| rivu_protocol::model::Query::PlaylistEntries {
            query: None,
            playlist_id: 1,
            offset,
            limit: 20,
        };
        let revisions = Default::default();
        let mut requests = ViewRequests::default();
        let (_, delay) = requests
            .prepare_view(kind, query(0), 1, revisions, 1, start)
            .unwrap();
        assert_eq!(delay, Duration::from_millis(80));
        assert!(
            requests
                .prepare_view(
                    kind,
                    query(20),
                    2,
                    revisions,
                    1,
                    start + Duration::from_millis(10)
                )
                .is_none()
        );
        // Neither replacement nor an elapsed deadline releases the physical gate.
        assert!(
            requests
                .prepare_view(
                    kind,
                    query(20),
                    3,
                    revisions,
                    1,
                    start + Duration::from_millis(100)
                )
                .is_none()
        );
        assert_eq!(requests.workers.len(), 1);
        assert!(requests.workers.remove(&kind));
        let (queued, generation) = requests.queued.remove(&kind).unwrap();
        let (_, delay) = requests
            .prepare_view(
                kind,
                queued,
                generation,
                revisions,
                1,
                start + Duration::from_millis(100),
            )
            .unwrap();
        assert_eq!(delay, Duration::ZERO);
        assert_eq!(requests.workers.len(), 1);
    }

    #[test]
    fn playlist_replacement_and_hidden_reopen_preserve_remaining_quiet_interval() {
        let start = Instant::now();
        let kind = ViewRequestKind::Summaries;
        let query = |offset| rivu_protocol::model::Query::PlaylistSummaries {
            query: None,
            offset,
            limit: 20,
        };
        let revisions = Default::default();
        let mut requests = ViewRequests::default();
        requests
            .prepare_view(kind, query(0), 1, revisions, 0, start)
            .unwrap();
        let retired = requests.projections.get_mut(&kind).unwrap().ticket();
        assert!(
            requests
                .prepare_view(
                    kind,
                    query(20),
                    2,
                    revisions,
                    0,
                    start + Duration::from_millis(20)
                )
                .is_none()
        );
        assert!(!retired.is_current());
        assert!(requests.workers.remove(&kind));
        let (queued, generation) = requests.queued.remove(&kind).unwrap();
        let (_, delay) = requests
            .prepare_view(
                kind,
                queued,
                generation,
                revisions,
                0,
                start + Duration::from_millis(50),
            )
            .unwrap();
        assert_eq!(delay, Duration::from_millis(50));
        // Hiding cancels logical demand/reservations, never the occupied gate or deadline.
        requests.cancel(kind);
        assert!(requests.workers.contains(&kind));
        assert!(requests.workers.remove(&kind));
        let (_, delay) = requests
            .prepare_view(
                kind,
                query(20),
                3,
                revisions,
                0,
                start + Duration::from_millis(60),
            )
            .unwrap();
        assert_eq!(delay, Duration::from_millis(40));
        requests.cancel(kind);
        assert!(requests.workers.remove(&kind));
        let (_, delay) = requests
            .prepare_view(
                kind,
                query(20),
                4,
                revisions,
                0,
                start + Duration::from_millis(150),
            )
            .unwrap();
        assert_eq!(delay, Duration::ZERO);
    }

    #[test]
    fn playlist_freshness_and_identity_changes_establish_new_quiet_deadlines() {
        let start = Instant::now();
        let kind = ViewRequestKind::Entries;
        let query = rivu_protocol::model::Query::PlaylistEntries {
            query: None,
            playlist_id: 1,
            offset: 0,
            limit: 20,
        };
        let mut requests = ViewRequests::default();
        let revisions = Default::default();
        requests
            .prepare_view(kind, query.clone(), 1, revisions, 1, start)
            .unwrap();
        assert!(requests.workers.remove(&kind));
        let revisions = rivu_protocol::response::QueryRevisions {
            library: 1,
            ..revisions
        };
        let (_, delay) = requests
            .prepare_view(
                kind,
                query.clone(),
                2,
                revisions,
                1,
                start + Duration::from_millis(100),
            )
            .unwrap();
        assert_eq!(delay, Duration::from_millis(80));
        assert!(requests.workers.remove(&kind));
        let (_, delay) = requests
            .prepare_view(
                kind,
                query,
                3,
                revisions,
                2,
                start + Duration::from_millis(200),
            )
            .unwrap();
        assert_eq!(delay, Duration::from_millis(80));
    }

    #[test]
    fn playlist_query_owners_separate_metadata_catalog_and_occurrences() {
        use rivu_protocol::model::Query;
        let mut library = GuiLibrarySnapshot::default();
        Arc::make_mut(&mut library.playlist_entry_revisions).insert(1, 0);
        let summaries = Query::PlaylistSummaries {
            query: None,
            offset: 0,
            limit: 20,
        };
        let entries = Query::PlaylistEntries {
            query: None,
            playlist_id: 1,
            offset: 0,
            limit: 20,
        };
        let compact = Query::PlaylistEntryLocations {
            query: None,
            playlist_id: 1,
            entry_ids: vec![3],
        };
        let before = library.query_revisions(Some(1));
        library.revision += 1;
        let metadata = library.query_revisions(Some(1));
        assert_eq!(summaries.stamp(before), summaries.stamp(metadata));
        assert_ne!(entries.stamp(before), entries.stamp(metadata));
        assert_eq!(compact.stamp(before), compact.stamp(metadata));
        library.playlist_revision += 1;
        let renamed = library.query_revisions(Some(1));
        assert_ne!(summaries.stamp(metadata), summaries.stamp(renamed));
        assert_eq!(entries.stamp(metadata), entries.stamp(renamed));
        assert_eq!(compact.stamp(metadata), compact.stamp(renamed));
        Arc::make_mut(&mut library.playlist_entry_revisions).remove(&1);
        let deleted = library.query_revisions(Some(1));
        assert_ne!(compact.stamp(renamed), compact.stamp(deleted));
    }

    #[test]
    fn cancelling_a_view_retains_its_physical_worker_gate() {
        let mut requests = ViewRequests::default();
        requests.begin(ViewRequestKind::Entries, 1);
        requests.workers.insert(ViewRequestKind::Entries);
        requests.cancel(ViewRequestKind::Entries);
        assert!(!requests.pending(ViewRequestKind::Entries));
        assert!(requests.workers.contains(&ViewRequestKind::Entries));
        assert!(!requests.workers.insert(ViewRequestKind::Entries));
        assert!(requests.workers.remove(&ViewRequestKind::Entries));
        assert!(requests.workers.insert(ViewRequestKind::Entries));
    }
    #[test]
    fn entry_refresh_supersedes_entry_results_without_cancelling_catalog_requests() {
        let mut requests = ViewRequests::default();
        for kind in [
            ViewRequestKind::Summaries,
            ViewRequestKind::Selection,
            ViewRequestKind::Entries,
        ] {
            requests.begin(kind, 1);
        }
        requests.begin(ViewRequestKind::Entries, 2);
        assert!(requests.is_current(ViewRequestKind::Summaries, 1));
        assert!(requests.is_current(ViewRequestKind::Selection, 1));
        assert!(!requests.is_current(ViewRequestKind::Entries, 1));
        assert!(requests.is_current(ViewRequestKind::Entries, 2));
    }

    #[test]
    fn empty_playlist_completion_ends_loading_and_failed_refresh_stays_retryable() {
        let mut browser = PlaylistModel::default();
        let revisions = QueryRevisions::default();
        let kind = ViewRequestKind::Entries;
        let empty = || QueryResponse {
            revisions,
            result: Ok(ViewResponse::PlaylistEntries(rivu_protocol::model::Page {
                total: 0,
                rows: Vec::new(),
            })),
        };
        browser.select_playlist(41, "empty".into(), 0);
        let (read, _) = browser
            .request_playlist_entries(41, 0, 20, revisions)
            .into_read()
            .unwrap();
        assert_eq!(browser.playlists.entries().rows.len(), 1);
        assert!(browser.requests.pending(kind));

        assert_eq!(
            browser.complete_playlist_read(&read, revisions, empty()),
            None
        );
        assert_eq!(browser.playlists.selected(), Some(41));
        assert!(browser.playlists.entries().rows.is_empty());
        assert!(!browser.requests.pending(kind));
        let loaded = browser.demand_playlist_entries(0..1, 20, revisions);
        assert_eq!(loaded.pending_generation(), None);
        assert!(loaded.into_read().is_none());

        browser.refresh_playlists(PlaylistRefresh {
            summaries: false,
            entries: true,
            entry_payload_only: false,
        });
        let (read, _) = browser
            .demand_playlist_entries(0..1, 20, revisions)
            .into_read()
            .unwrap();
        assert!(browser.requests.pending(kind));
        assert_eq!(
            browser.complete_playlist_read(
                &read,
                revisions,
                QueryResponse {
                    // Transport failures lack the playlist-local revision scope.
                    revisions: QueryRevisions::default(),
                    result: Err(rivu_protocol::response::QueryError::Message(
                        "unavailable".into()
                    )),
                }
            ),
            Some("unavailable".into())
        );
        assert_eq!(browser.playlists.entries().rows.total(), None);
        assert_eq!(browser.playlists.entries().rows.len(), 1);
        assert!(!browser.requests.pending(kind));
        assert_eq!(
            browser.playlists.entries().rows.error(0),
            Some("unavailable")
        );

        let (read, _) = browser
            .retry_playlist_entries(20, revisions)
            .into_read()
            .unwrap();
        assert!(browser.requests.pending(kind));
        assert_eq!(browser.playlists.entries().rows.error(0), None);
        assert_eq!(
            browser.complete_playlist_read(&read, revisions, empty()),
            None
        );
        assert!(browser.playlists.entries().rows.is_empty());
        assert!(!browser.requests.pending(kind));
    }

    #[test]
    fn finishing_a_superseded_page_cannot_end_its_replacement() {
        let mut requests = ViewRequests::default();
        requests.begin(ViewRequestKind::Entries, 1);
        requests.begin(ViewRequestKind::Entries, 2);
        assert!(!requests.finish(ViewRequestKind::Entries, 1));
        assert!(requests.pending(ViewRequestKind::Entries));
        assert!(requests.is_current(ViewRequestKind::Entries, 2));
        assert!(requests.finish(ViewRequestKind::Entries, 2));
        assert!(!requests.pending(ViewRequestKind::Entries));
        assert!(!requests.finish(ViewRequestKind::Entries, 2));
    }

    #[test]
    fn empty_catalog_releases_selected_playlist_and_pending_entry_requests() {
        let mut browser = PlaylistModel::default();
        assert!(browser.playlists.entries().rows.is_empty());
        browser.playlists.select(41, "deleted".into(), 0);
        browser.playlists.prepare_entries(41);
        browser.playlists.prepare_page(true, 0, 20);
        browser.playlists.prepare_page(false, 0, 20);
        for kind in [
            ViewRequestKind::Summaries,
            ViewRequestKind::Selection,
            ViewRequestKind::Entries,
        ] {
            browser.requests.begin(kind, 1);
        }

        assert!(browser.requests.finish(ViewRequestKind::Summaries, 1));
        browser.playlists.fail_page(false, "unavailable".into());
        assert!(!browser.requests.pending(ViewRequestKind::Summaries));
        assert_eq!(
            browser.playlists.summaries().rows.error(0),
            Some("unavailable")
        );
        assert_eq!(browser.playlists.selected(), Some(41));
        assert_eq!(browser.playlists.retry_page(false, 20), Some(0));
        browser.playlists.prepare_page(false, 0, 20);
        browser.requests.begin(ViewRequestKind::Summaries, 2);
        assert!(browser.requests.finish(ViewRequestKind::Summaries, 2));
        browser.accept_playlist_summaries(
            rivu_protocol::model::Page {
                total: 0,
                rows: Vec::new(),
            },
            QueryRevisions::default(),
        );
        assert_eq!(browser.playlists.selected(), None);
        assert_eq!(browser.playlists.entries().playlist_id, None);
        assert!(browser.playlists.entries().rows.is_empty());
        assert!(browser.playlists.summaries().rows.is_empty());
        for kind in [
            ViewRequestKind::Summaries,
            ViewRequestKind::Selection,
            ViewRequestKind::Entries,
        ] {
            assert!(!browser.requests.pending(kind));
            assert!(!browser.requests.finish(kind, 1));
        }
        assert_eq!(browser.playlists.demand_page(true, 0..1, 20), None);
    }

    #[test]
    fn clearing_selection_releases_entry_owner_and_outstanding_requests() {
        let mut browser = PlaylistModel::default();
        browser.playlists.select(41, "selected".into(), 0);
        browser.playlists.prepare_entries(41);
        browser.playlists.prepare_page(true, 0, 20);
        for kind in [ViewRequestKind::Selection, ViewRequestKind::Entries] {
            browser.requests.begin(kind, 1);
        }

        browser.remove_playlist_selection();
        assert_eq!(browser.playlists.selected(), None);
        assert_eq!(browser.playlists.entries().playlist_id, None);
        assert!(browser.playlists.entries().rows.is_empty());
        for kind in [ViewRequestKind::Selection, ViewRequestKind::Entries] {
            assert!(!browser.requests.pending(kind));
            assert!(!browser.requests.finish(kind, 1));
        }
    }
}
