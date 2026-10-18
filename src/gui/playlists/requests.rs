//! Playlist demand and physical gates; cancellation never releases a worker.
use super::model::{PlaylistBrowser, PlaylistRefresh};
use crate::{
    model::{Page, PlaylistSummary, Query},
    projection::QueryProjection,
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
            Query::PlaylistSummaryBatch { .. } => Self::Selection,
            _ => return None,
        })
    }
}
struct ViewDemand {
    query: Query,
    stamp: crate::model::QueryStamp,
    selection_generation: u64,
    quiet_until: Instant,
}

#[derive(Default)]
pub(in crate::gui) struct ViewRequests {
    generation: u64,
    generations: HashMap<ViewRequestKind, u64>,
    projections: HashMap<ViewRequestKind, QueryProjection>,
    workers: HashSet<ViewRequestKind>,
    queued: HashMap<ViewRequestKind, (Query, u64)>,
    // Latest intent outlives logical cancellation, hiding and worker completion.
    demands: HashMap<ViewRequestKind, ViewDemand>,
    immediate: HashSet<ViewRequestKind>,
    selection_baseline: Option<(u64, crate::model::QueryStamp)>,
}

impl ViewRequests {
    fn prepare_view(
        &mut self,
        kind: ViewRequestKind,
        query: Query,
        generation: u64,
        revisions: crate::response::QueryRevisions,
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
    read: Option<(ViewRead, crate::core::QueryTicket)>,
}

impl ViewRequest {
    pub(in crate::gui) fn kind(&self) -> ViewRequestKind {
        self.kind
    }

    pub(in crate::gui) fn pending_generation(&self) -> Option<u64> {
        self.pending_generation
    }

    pub(in crate::gui) fn into_read(self) -> Option<(ViewRead, crate::core::QueryTicket)> {
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

#[derive(Default)]
pub(super) struct PlaylistState {
    pub(super) playlists: PlaylistBrowser,
    requests: ViewRequests,
    page_size: usize,
    focus: Option<crate::gui::BrowserFocus>,
}
impl PlaylistState {
    pub(super) fn sync_entry_projection(&mut self, revisions: QueryRevisions) -> bool {
        let Some(id) = self.playlists.selected() else {
            return false;
        };
        let query = self.playlists.entries().query.clone();
        self.playlists
            .track_selection
            .sync_projection(id, query, revisions)
    }

    pub(super) fn playlists(&self) -> &PlaylistBrowser {
        &self.playlists
    }

    /// Selection jobs may mutate identities, never the paired page lifecycle.
    pub(super) fn playlist_selection_mut(&mut self) -> &mut super::selection::EntrySelection {
        &mut self.playlists.track_selection
    }

    pub(super) fn view_pending(&self, kind: ViewRequestKind) -> bool {
        self.requests.pending(kind)
    }

    pub(super) fn view_loading(&self, kind: ViewRequestKind, token: u64) -> bool {
        self.requests.is_current(kind, token)
    }

    pub(super) fn sync_active_playlist_entry(&mut self) {
        self.playlists.sync_active_entry();
    }

    pub(super) fn accept_playlist_entry_locations(
        &mut self,
        rows: Vec<crate::model::PlaylistEntryLocation>,
    ) {
        self.playlists.accept_entry_locations(rows);
    }

    pub(super) fn focus(&self) -> Option<crate::gui::BrowserFocus> {
        self.focus
    }

    pub(super) fn focus_list(&mut self, focus: crate::gui::ListFocus) {
        self.focus = Some(crate::gui::BrowserFocus::List(focus));
    }

    pub(super) fn focus_playlists(&mut self) {
        self.focus = Some(crate::gui::BrowserFocus::Playlists);
    }
    pub(super) fn remove_playlist_selection(&mut self) {
        self.cancel_playlist_read(ViewRequestKind::Selection);
        self.cancel_playlist_read(ViewRequestKind::Entries);
        self.playlists.clear_selection();
    }

    fn accept_playlist_summaries(&mut self, page: Page<PlaylistSummary>) {
        if page.total == 0 && self.playlists.summaries().query.is_none() {
            self.remove_playlist_selection();
        }
        self.playlists.accept_summaries(page);
    }

    /// The first row after `id - 1` proves existence without scanning other pages.
    pub(super) fn accept_playlist_lookup(&mut self, id: i64, first: Option<PlaylistSummary>) {
        if self.playlists.selected() != Some(id) {
            return;
        }
        if let Some(summary) = first.filter(|summary| summary.id == id) {
            self.playlists.accept_selected_summary(summary);
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
        }
        if refresh.entries {
            self.cancel_playlist_read(ViewRequestKind::Entries);
            if let Some(id) = self.playlists.entries().playlist_id {
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
            ViewRequestKind::Selection => self.requests.selection_baseline = None,
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

    pub(in crate::gui) fn prepare_playlist_lookup(
        &mut self,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        let Some(id) = self.playlists.selected() else {
            return self.playlist_request(ViewRequestKind::Selection);
        };
        let query = Query::PlaylistSummaryBatch {
            query: None,
            after: id.checked_sub(1),
            limit: 1,
            expected: None,
        };
        let baseline = (
            self.playlists.selection_generation(),
            query.stamp(revisions),
        );
        if self.requests.selection_baseline == Some(baseline) {
            return self.playlist_request(ViewRequestKind::Selection);
        }
        self.requests.selection_baseline = Some(baseline);
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
                    _ => {}
                }
                Some(message)
            }
            Ok(ViewResponse::PlaylistSummaries(page))
                if read.kind == ViewRequestKind::Summaries =>
            {
                self.accept_playlist_summaries(page);
                None
            }
            Ok(ViewResponse::PlaylistEntries(page)) if read.kind == ViewRequestKind::Entries => {
                self.playlists.accept_entries(page);
                None
            }
            Ok(ViewResponse::PlaylistSummaryBatch(batch))
                if read.kind == ViewRequestKind::Selection =>
            {
                if let Some(id) = read.selected_playlist {
                    self.accept_playlist_lookup(id, batch.rows.into_iter().next());
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

    #[test]
    fn view_timeout_publishes_once_and_retains_every_lane_until_retirement() {
        for kind in [
            ViewRequestKind::Summaries,
            ViewRequestKind::Entries,
            ViewRequestKind::Selection,
        ] {
            let mut browser = PlaylistState::default();
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
                result: Err(crate::response::QueryError::Message("timeout".into())),
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
        let mut browser = PlaylistState::default();
        let revisions = QueryRevisions::default();
        browser.select_playlist(1, "one".into(), 0);
        let (read, _) = browser
            .request_playlist_entries(1, 0, 20, revisions)
            .into_read()
            .unwrap();
        let timeout = || QueryResponse {
            revisions,
            result: Err(crate::response::QueryError::Message("timeout".into())),
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
        let mut browser = PlaylistState::default();
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
                result: Ok(ViewResponse::PlaylistEntries(crate::model::Page {
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
                result: Ok(ViewResponse::PlaylistEntries(crate::model::Page {
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
        let mut browser = PlaylistState::default();
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
        let mut browser = PlaylistState::default();
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
                result: Ok(ViewResponse::PlaylistEntries(crate::model::Page {
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
        let query = |offset| crate::model::Query::PlaylistEntries {
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
        let query = |offset| crate::model::Query::PlaylistSummaries {
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
        let query = crate::model::Query::PlaylistEntries {
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
        let revisions = crate::response::QueryRevisions {
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
        use crate::model::Query;
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
        let mut browser = PlaylistState::default();
        let revisions = QueryRevisions::default();
        let kind = ViewRequestKind::Entries;
        let empty = || QueryResponse {
            revisions,
            result: Ok(ViewResponse::PlaylistEntries(crate::model::Page {
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
                    result: Err(crate::response::QueryError::Message("unavailable".into())),
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
        let mut browser = PlaylistState::default();
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
        browser.accept_playlist_summaries(crate::model::Page {
            total: 0,
            rows: Vec::new(),
        });
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
        let mut browser = PlaylistState::default();
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
