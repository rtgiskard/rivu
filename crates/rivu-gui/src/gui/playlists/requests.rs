//! Playlist demand and physical gates; cancellation never releases a worker.
use super::{PlaylistBrowser, PlaylistModel, PlaylistRefresh};
use rivu_protocol::query_tracking::QueryTracker;
use rivu_protocol::{
    model::{Page, PlaylistSummary, Query},
    response::{QueryError, QueryResponse, QueryRevisions, ViewResponse},
};
use std::time::{Duration, Instant};
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
    retired: bool,
}

pub(in crate::gui) type SelectionKey = (u64, rivu_protocol::model::QueryStamp);

/// One lane owns logical demand and publication independently of physical occupancy.
#[derive(Default)]
struct ViewRequestSlot {
    projection: QueryTracker,
    publishing: bool,
    worker_active: bool,
    queued: bool,
    // Latest intent outlives logical cancellation, hiding and worker completion.
    demand: Option<ViewDemand>,
    immediate: bool,
}

enum ViewPublication {
    Obsolete,
    Cancelled,
    Retired,
    Publish,
}

impl ViewRequestSlot {
    fn pending_generation(&self) -> Option<u64> {
        self.publishing.then(|| self.projection.generation())
    }

    fn request(&self, kind: ViewRequestKind) -> ViewRequest {
        ViewRequest {
            kind,
            pending_generation: self.pending_generation(),
            read: None,
        }
    }

    fn prepare(
        &mut self,
        kind: ViewRequestKind,
        query: Query,
        revisions: QueryRevisions,
        selection_generation: u64,
        now: Instant,
        explicit: bool,
    ) -> ViewRequest {
        let stamp = query.stamp(revisions);
        let changed = self.demand.as_ref().is_none_or(|demand| {
            demand.query != query
                || demand.stamp != stamp
                || demand.selection_generation != selection_generation
        });
        if changed || explicit {
            self.demand = Some(ViewDemand {
                query,
                stamp,
                selection_generation,
                quiet_until: now + Duration::from_millis(80),
                retired: false,
            });
        }
        let demand = self.demand.as_mut().expect("prepared view demand");
        if std::mem::take(&mut self.immediate) {
            demand.quiet_until = now;
        }
        // Replaying an identical rejected snapshot cannot repair it.
        if demand.retired {
            return self.request(kind);
        }
        // Every new intent revokes the old ticket, including offset-only changes
        // and same-query retries, before waiting for the physical worker.
        if !self.projection.sync(&demand.query, revisions) {
            self.projection.cancel();
        }
        self.publishing = true;
        self.queued = true;
        self.take_read(kind, revisions, now)
    }

    fn take_read(
        &mut self,
        kind: ViewRequestKind,
        revisions: QueryRevisions,
        now: Instant,
    ) -> ViewRequest {
        if !self.queued || self.worker_active {
            return self.request(kind);
        }
        let demand = self.demand.as_mut().expect("queued view demand");
        let stamp = demand.query.stamp(revisions);
        if demand.stamp != stamp {
            demand.stamp = stamp;
            demand.quiet_until = now + Duration::from_millis(80);
            self.projection.sync(&demand.query, revisions);
        }
        self.queued = false;
        self.worker_active = true;
        let read = ViewRead {
            query: demand.query.clone(),
            kind,
            generation: self.projection.generation(),
            delay: demand.quiet_until.saturating_duration_since(now),
            selection_generation: demand.selection_generation,
        };
        ViewRequest {
            kind,
            pending_generation: self.pending_generation(),
            read: Some((read, self.projection.ticket())),
        }
    }

    fn dispatch_queued(
        &mut self,
        kind: ViewRequestKind,
        demanded: bool,
        revisions: QueryRevisions,
        now: Instant,
    ) -> (ViewRequest, bool) {
        if !self.queued {
            return (self.request(kind), false);
        }
        if !demanded {
            self.cancel();
            return (self.request(kind), true);
        }
        (self.take_read(kind, revisions, now), false)
    }

    fn is_current(&self, generation: u64) -> bool {
        self.pending_generation() == Some(generation)
    }

    fn accepts(
        &self,
        read: &ViewRead,
        revisions: QueryRevisions,
        selection_generation: u64,
    ) -> bool {
        self.is_current(read.generation)
            && (read.kind == ViewRequestKind::Summaries
                || read.selection_generation == selection_generation)
            && self
                .projection
                .accepts(&read.query, read.generation, revisions)
    }

    fn finish(&mut self, generation: u64) -> bool {
        if !self.is_current(generation) {
            return false;
        }
        self.cancel();
        true
    }

    fn cancel(&mut self) {
        self.publishing = false;
        self.queued = false;
        self.projection.cancel();
    }

    fn release_worker(&mut self) -> bool {
        std::mem::take(&mut self.worker_active)
    }

    fn publish(
        &mut self,
        read: &ViewRead,
        current: QueryRevisions,
        selection_generation: u64,
        response: &QueryResponse,
    ) -> ViewPublication {
        if !self.is_current(read.generation) {
            return ViewPublication::Obsolete;
        }
        let publication = if !self.accepts(read, current, selection_generation) {
            ViewPublication::Cancelled
        } else {
            let expected_payload = matches!(
                (&response.result, read.kind),
                (
                    Ok(ViewResponse::PlaylistSummaries(_)),
                    ViewRequestKind::Summaries
                ) | (
                    Ok(ViewResponse::PlaylistEntries(_)),
                    ViewRequestKind::Entries
                ) | (
                    Ok(ViewResponse::PlaylistSummary(_)),
                    ViewRequestKind::Selection
                )
            );
            if matches!(
                &response.result,
                Err(QueryError::RevisionChanged | QueryError::Cancelled)
            ) || (expected_payload
                && !self
                    .projection
                    .accepts(&read.query, read.generation, response.revisions))
            {
                self.demand.as_mut().expect("prepared view demand").retired = true;
                ViewPublication::Retired
            } else {
                ViewPublication::Publish
            }
        };
        // Timeout and final completion share this one-shot publication right;
        // neither consumption nor cancellation releases physical occupancy.
        self.finish(read.generation);
        publication
    }
}

#[derive(Default)]
pub(super) struct ViewRequests {
    summaries: ViewRequestSlot,
    entries: ViewRequestSlot,
    selection: ViewRequestSlot,
    selection_baseline: Option<SelectionKey>,
    selection_failure: Option<(SelectionKey, String)>,
}

impl ViewRequests {
    fn slot(&self, kind: ViewRequestKind) -> &ViewRequestSlot {
        match kind {
            ViewRequestKind::Summaries => &self.summaries,
            ViewRequestKind::Entries => &self.entries,
            ViewRequestKind::Selection => &self.selection,
        }
    }

    fn slot_mut(&mut self, kind: ViewRequestKind) -> &mut ViewRequestSlot {
        match kind {
            ViewRequestKind::Summaries => &mut self.summaries,
            ViewRequestKind::Entries => &mut self.entries,
            ViewRequestKind::Selection => &mut self.selection,
        }
    }

    fn prepare_view(
        &mut self,
        kind: ViewRequestKind,
        query: Query,
        explicit: bool,
        revisions: QueryRevisions,
        selection_generation: u64,
        now: Instant,
    ) -> ViewRequest {
        let selection_generation = if kind == ViewRequestKind::Summaries {
            0
        } else {
            selection_generation
        };
        self.slot_mut(kind)
            .prepare(kind, query, revisions, selection_generation, now, explicit)
    }
}

/// The demand lane remains observable even while its physical read is queued.
/// Construction stays with the owner, so dispatch cannot supply a different kind.
pub(in crate::gui) struct ViewRequest {
    kind: ViewRequestKind,
    pending_generation: Option<u64>,
    read: Option<(ViewRead, rivu_protocol::query_tracking::QueryTicket)>,
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
    ) -> Option<(ViewRead, rivu_protocol::query_tracking::QueryTicket)> {
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
    selection_generation: u64,
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
        self.requests.slot(kind).pending_generation().is_some()
    }

    pub(in crate::gui::playlists) fn view_loading(
        &self,
        kind: ViewRequestKind,
        token: u64,
    ) -> bool {
        self.requests.slot(kind).is_current(token)
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
        self.requests.slot(kind).request(kind)
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
        self.requests.slot_mut(ViewRequestKind::Summaries).immediate = true;
    }

    pub(in crate::gui) fn search_playlist_entries(&mut self, query: Option<String>) {
        self.cancel_playlist_read(ViewRequestKind::Entries);
        self.playlists.search_entries(query);
        self.requests.slot_mut(ViewRequestKind::Entries).immediate = true;
    }

    pub(in crate::gui) fn cancel_playlist_read(&mut self, kind: ViewRequestKind) {
        self.requests.slot_mut(kind).cancel();
        self.cancel_playlist_state(kind);
    }

    fn cancel_playlist_state(&mut self, kind: ViewRequestKind) {
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
            if self.view_pending(kind) {
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
        let query = self.playlists.prepare_page(entries, offset, page_size);
        let request = self.prepare_playlist_read(query, explicit, revisions);
        if request.pending_generation().is_none() {
            self.playlists.cancel_page(entries);
        }
        request
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
        self.prepare_playlist_read(query, false, revisions)
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
            || self
                .requests
                .slot(ViewRequestKind::Selection)
                .pending_generation()
                .is_some()
        {
            return self.playlist_request(ViewRequestKind::Selection);
        }
        self.requests.selection_failure = None;
        self.requests.selection_baseline = Some(key);
        self.prepare_playlist_read(query, false, revisions)
    }

    fn prepare_playlist_read(
        &mut self,
        query: Query,
        explicit: bool,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        let kind = ViewRequestKind::for_query(&query).expect("playlist read query");
        let selection_generation = self.playlists.selection_generation();
        self.requests.prepare_view(
            kind,
            query,
            explicit,
            revisions,
            selection_generation,
            Instant::now(),
        )
    }

    pub(in crate::gui) fn queued_playlist_read(
        &mut self,
        kind: ViewRequestKind,
        demanded: bool,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        let (request, cancelled) =
            self.requests
                .slot_mut(kind)
                .dispatch_queued(kind, demanded, revisions, Instant::now());
        if cancelled {
            self.cancel_playlist_state(kind);
        }
        if kind == ViewRequestKind::Selection && request.pending_generation().is_some() {
            self.requests.selection_baseline = self.playlist_lookup(revisions).map(|(_, key)| key);
        }
        request
    }

    pub(in crate::gui) fn playlist_read_current(
        &mut self,
        read: &ViewRead,
        revisions: QueryRevisions,
    ) -> bool {
        self.requests.slot(read.kind).accepts(
            read,
            revisions,
            self.playlists.selection_generation(),
        )
    }

    pub(in crate::gui) fn abandon_playlist_read(&mut self, read: &ViewRead) {
        self.requests.slot_mut(read.kind).release_worker();
        if self.requests.slot_mut(read.kind).finish(read.generation) {
            self.cancel_playlist_state(read.kind);
        }
    }

    pub(in crate::gui) fn complete_playlist_read(
        &mut self,
        read: &ViewRead,
        current: QueryRevisions,
        response: QueryResponse,
    ) -> Option<String> {
        self.requests.slot_mut(read.kind).release_worker();
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
        match self.requests.slot_mut(read.kind).publish(
            read,
            current,
            self.playlists.selection_generation(),
            &response,
        ) {
            ViewPublication::Obsolete => return None,
            ViewPublication::Cancelled => {
                self.cancel_playlist_state(read.kind);
                return None;
            }
            ViewPublication::Retired => {
                match read.kind {
                    ViewRequestKind::Summaries => self.playlists.cancel_page(false),
                    ViewRequestKind::Entries => self.playlists.cancel_page(true),
                    // Preserve the baseline so render cannot resubmit this lookup.
                    ViewRequestKind::Selection => {}
                }
                return None;
            }
            ViewPublication::Publish => {}
        }
        match response.result {
            Err(error) => self.fail_playlist_read(read, current, error.to_string()),
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
                if let Query::PlaylistSummary { playlist_id: id } = &read.query {
                    self.accept_playlist_lookup(*id, summary);
                }
                None
            }
            _ => {
                self.fail_playlist_read(read, current, "Unexpected playlist query response".into())
            }
        }
    }

    fn fail_playlist_read(
        &mut self,
        read: &ViewRead,
        current: QueryRevisions,
        message: String,
    ) -> Option<String> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivu_protocol::model::LibrarySnapshot;
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
        let previous = LibrarySnapshot::default();
        let latest = LibrarySnapshot {
            bitrate_revision: 1,
            ..previous.clone()
        };
        let revisions = previous.query_revisions(0, Some(1));
        let current = latest.query_revisions(0, Some(1));
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
        assert!(
            browser
                .requests
                .slot(ViewRequestKind::Entries)
                .worker_active
        );
        assert!(!browser.requests.slot(ViewRequestKind::Entries).queued);
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
        assert!(
            !browser
                .requests
                .slot(ViewRequestKind::Entries)
                .worker_active
        );
        assert!(
            browser
                .demand_playlist_entries(0..1, 20, revisions)
                .into_read()
                .is_none()
        );
        let (read, _) = browser
            .request_playlist_entries(1, 0, 20, revisions)
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
        assert!(
            !browser
                .requests
                .slot(ViewRequestKind::Entries)
                .worker_active
        );
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
        assert!(
            browser
                .complete_playlist_read(&read, revisions, lookup_error(revisions, "offline"))
                .is_some()
        );
        let (key, _) = browser.playlist_lookup_failure().unwrap();
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
        assert_eq!(
            browser.playlist_lookup_failure().map(|(key, _)| key),
            Some(key)
        );
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
            assert!(
                browser
                    .timeout_playlist_read(&read, revisions, timeout())
                    .is_some()
            );
            assert!(!browser.view_pending(kind));
            assert!(browser.requests.slot(kind).worker_active);
            let late = match kind {
                ViewRequestKind::Summaries => ViewResponse::PlaylistSummaries(Page {
                    total: 1,
                    rows: vec![summary(1, 99)],
                }),
                ViewRequestKind::Entries => ViewResponse::PlaylistEntries(Page {
                    total: 1,
                    rows: vec![entry(10, 0)],
                }),
                ViewRequestKind::Selection => ViewResponse::PlaylistSummary(Some(summary(1, 99))),
            };
            assert_eq!(
                browser.complete_playlist_read(
                    &read,
                    revisions,
                    QueryResponse {
                        revisions,
                        result: Ok(late),
                    }
                ),
                None
            );
            assert!(!browser.requests.slot(kind).worker_active);
            assert!(!browser.view_pending(kind));
            match kind {
                ViewRequestKind::Summaries => {
                    assert!(browser.playlists.summaries().rows.error(0).is_some())
                }
                ViewRequestKind::Entries => {
                    assert!(browser.playlists.entries().rows.error(0).is_some())
                }
                ViewRequestKind::Selection => {
                    assert_eq!(browser.playlists.selected(), Some(1));
                    assert_eq!(browser.playlists.selected_total(), None);
                    assert!(browser.playlist_lookup_failure().is_some());
                }
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
        assert!(
            browser
                .requests
                .slot(ViewRequestKind::Entries)
                .worker_active
        );
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
        assert!(browser.view_pending(ViewRequestKind::Entries));
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
        assert!(!browser.view_pending(ViewRequestKind::Entries));
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
        assert!(
            browser
                .requests
                .slot(ViewRequestKind::Entries)
                .worker_active
        );
        assert!(!browser.view_pending(ViewRequestKind::Entries));
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
        assert!(
            browser
                .requests
                .slot(ViewRequestKind::Entries)
                .worker_active
        );
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
        assert!(!browser.view_pending(ViewRequestKind::Entries));
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
        let query = |offset| Query::PlaylistEntries {
            query: None,
            playlist_id: 1,
            offset,
            limit: 20,
        };
        let revisions = Default::default();
        let mut requests = ViewRequests::default();
        let (first, ticket) = requests
            .prepare_view(kind, query(0), false, revisions, 1, start)
            .into_read()
            .unwrap();
        assert_eq!(first.delay, Duration::from_millis(80));
        let queued = requests.prepare_view(
            kind,
            query(20),
            false,
            revisions,
            1,
            start + Duration::from_millis(10),
        );
        let generation = queued.pending_generation().unwrap();
        assert!(queued.into_read().is_none());
        assert!(!ticket.is_current());
        // An elapsed deadline cannot release the physical gate or replace intent.
        assert!(
            requests
                .slot_mut(kind)
                .take_read(kind, revisions, start + Duration::from_millis(100))
                .into_read()
                .is_none()
        );
        assert!(requests.slot_mut(kind).release_worker());
        let (read, ticket) = requests
            .slot_mut(kind)
            .take_read(kind, revisions, start + Duration::from_millis(100))
            .into_read()
            .unwrap();
        assert_eq!(read.query, query(20));
        assert_eq!(read.generation, generation);
        assert!(ticket.is_current());
        assert_eq!(read.delay, Duration::ZERO);
    }

    #[test]
    fn playlist_replacement_and_hidden_reopen_preserve_remaining_quiet_interval() {
        let start = Instant::now();
        let kind = ViewRequestKind::Summaries;
        let query = |offset| Query::PlaylistSummaries {
            query: None,
            offset,
            limit: 20,
        };
        let revisions = Default::default();
        let mut requests = ViewRequests::default();
        let (_, retired) = requests
            .prepare_view(kind, query(0), false, revisions, 0, start)
            .into_read()
            .unwrap();
        assert!(
            requests
                .prepare_view(
                    kind,
                    query(20),
                    false,
                    revisions,
                    0,
                    start + Duration::from_millis(20),
                )
                .into_read()
                .is_none()
        );
        assert!(!retired.is_current());
        assert!(requests.slot_mut(kind).release_worker());
        let (read, _) = requests
            .slot_mut(kind)
            .take_read(kind, revisions, start + Duration::from_millis(50))
            .into_read()
            .unwrap();
        assert_eq!(read.delay, Duration::from_millis(50));
        // Hiding cancels publication, never the occupied gate or deadline.
        requests.slot_mut(kind).cancel();
        assert!(requests.slot(kind).worker_active);
        assert!(requests.slot_mut(kind).release_worker());
        let (read, _) = requests
            .prepare_view(
                kind,
                query(20),
                false,
                revisions,
                0,
                start + Duration::from_millis(60),
            )
            .into_read()
            .unwrap();
        assert_eq!(read.delay, Duration::from_millis(40));
        requests.slot_mut(kind).cancel();
        assert!(requests.slot_mut(kind).release_worker());
        let (read, _) = requests
            .prepare_view(
                kind,
                query(20),
                false,
                revisions,
                0,
                start + Duration::from_millis(150),
            )
            .into_read()
            .unwrap();
        assert_eq!(read.delay, Duration::ZERO);
    }

    #[test]
    fn playlist_freshness_and_identity_changes_establish_new_quiet_deadlines() {
        let start = Instant::now();
        let kind = ViewRequestKind::Entries;
        let query = Query::PlaylistEntries {
            query: None,
            playlist_id: 1,
            offset: 0,
            limit: 20,
        };
        let mut requests = ViewRequests::default();
        let revisions = Default::default();
        requests
            .prepare_view(kind, query.clone(), false, revisions, 1, start)
            .into_read()
            .unwrap();
        assert!(requests.slot_mut(kind).release_worker());
        let revisions = QueryRevisions {
            library: 1,
            ..revisions
        };
        let (read, _) = requests
            .prepare_view(
                kind,
                query.clone(),
                false,
                revisions,
                1,
                start + Duration::from_millis(100),
            )
            .into_read()
            .unwrap();
        assert_eq!(read.delay, Duration::from_millis(80));
        assert!(requests.slot_mut(kind).release_worker());
        let (read, _) = requests
            .prepare_view(
                kind,
                query,
                false,
                revisions,
                2,
                start + Duration::from_millis(200),
            )
            .into_read()
            .unwrap();
        assert_eq!(read.delay, Duration::from_millis(80));
    }

    #[test]
    fn same_query_offset_and_aba_intents_revoke_busy_worker_tickets() {
        let kind = ViewRequestKind::Summaries;
        let start = Instant::now();
        let revisions = QueryRevisions::default();
        let query = |offset| Query::PlaylistSummaries {
            query: None,
            offset,
            limit: 20,
        };
        let mut requests = ViewRequests::default();
        let (old, ticket) = requests
            .prepare_view(kind, query(0), false, revisions, 0, start)
            .into_read()
            .unwrap();
        let mut previous = old.generation;
        for offset in [0, 20, 0] {
            let queued = requests.prepare_view(kind, query(offset), false, revisions, 0, start);
            let generation = queued.pending_generation().unwrap();
            assert_ne!(generation, previous);
            assert!(queued.into_read().is_none());
            assert!(!ticket.is_current());
            assert!(!requests.slot(kind).accepts(&old, revisions, 0));
            previous = generation;
        }
        assert!(requests.slot_mut(kind).release_worker());
        let (latest, _) = requests
            .slot_mut(kind)
            .take_read(kind, revisions, start)
            .into_read()
            .unwrap();
        assert_eq!(latest.generation, previous);
        assert_eq!(latest.query, query(0));
    }

    #[test]
    fn queued_dispatch_rechecks_freshness_without_renewing_unchanged_intent() {
        let kind = ViewRequestKind::Entries;
        let start = Instant::now();
        let revisions = QueryRevisions::default();
        let query = |offset| Query::PlaylistEntries {
            playlist_id: 1,
            query: None,
            offset,
            limit: 20,
        };
        let mut requests = ViewRequests::default();
        requests
            .prepare_view(kind, query(0), false, revisions, 1, start)
            .into_read()
            .unwrap();
        let queued = requests.prepare_view(kind, query(20), false, revisions, 1, start);
        let generation = queued.pending_generation().unwrap();
        assert!(queued.into_read().is_none());
        assert!(requests.slot_mut(kind).release_worker());
        let current = QueryRevisions {
            library: 1,
            ..revisions
        };
        let (read, ticket) = requests
            .slot_mut(kind)
            .take_read(kind, current, start + Duration::from_millis(100))
            .into_read()
            .unwrap();
        assert_ne!(read.generation, generation);
        assert_eq!(read.delay, Duration::from_millis(80));
        assert!(ticket.is_current());
        assert!(requests.slot(kind).accepts(&read, current, 1));
        assert!(!requests.slot(kind).accepts(&read, revisions, 1));

        // The selection baseline follows the same refreshed intent, so render
        // cannot replace a just-dispatched lookup with another logical request.
        let mut browser = PlaylistModel::default();
        browser.select_playlist(1, "one".into(), 0);
        let (old, _) = browser
            .prepare_playlist_lookup(revisions)
            .into_read()
            .unwrap();
        browser.cancel_playlist_read(ViewRequestKind::Selection);
        let queued = browser.prepare_playlist_lookup(revisions);
        assert!(queued.into_read().is_none());
        browser.complete_playlist_read(&old, revisions, lookup_success(revisions, 1, 99));
        let current = QueryRevisions {
            playlist: 1,
            ..revisions
        };
        let (lookup, ticket) = browser
            .queued_playlist_read(ViewRequestKind::Selection, true, current)
            .into_read()
            .unwrap();
        let retained = browser.prepare_playlist_lookup(current);
        assert_eq!(retained.pending_generation(), Some(lookup.generation));
        assert!(retained.into_read().is_none());
        assert!(ticket.is_current());
        assert!(browser.playlist_read_current(&lookup, current));
    }

    #[test]
    fn debounced_search_does_not_add_a_second_quiet_interval() {
        for kind in [ViewRequestKind::Summaries, ViewRequestKind::Entries] {
            let mut model = PlaylistModel::default();
            let query = Some("needle".to_owned());
            let request = if kind == ViewRequestKind::Summaries {
                model.search_playlists(query.clone());
                Query::PlaylistSummaries {
                    query,
                    offset: 0,
                    limit: 20,
                }
            } else {
                model.select_playlist(1, "one".into(), 0);
                model.search_playlist_entries(query.clone());
                Query::PlaylistEntries {
                    playlist_id: 1,
                    query,
                    offset: 0,
                    limit: 20,
                }
            };
            let (read, _) = model
                .requests
                .prepare_view(
                    kind,
                    request,
                    false,
                    QueryRevisions::default(),
                    0,
                    Instant::now(),
                )
                .into_read()
                .unwrap();
            assert_eq!(read.delay, Duration::ZERO);
        }
    }

    #[test]
    fn playlist_query_owners_separate_metadata_catalog_and_occurrences() {
        use rivu_protocol::model::Query;
        let mut library = LibrarySnapshot::default();
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
        let before = library.query_revisions(0, Some(1));
        library.revision += 1;
        let metadata = library.query_revisions(0, Some(1));
        assert_eq!(summaries.stamp(before), summaries.stamp(metadata));
        assert_ne!(entries.stamp(before), entries.stamp(metadata));
        assert_eq!(compact.stamp(before), compact.stamp(metadata));
        library.playlist_revision += 1;
        let renamed = library.query_revisions(0, Some(1));
        assert_ne!(summaries.stamp(metadata), summaries.stamp(renamed));
        assert_eq!(entries.stamp(metadata), entries.stamp(renamed));
        assert_eq!(compact.stamp(metadata), compact.stamp(renamed));
        Arc::make_mut(&mut library.playlist_entry_revisions).remove(&1);
        let deleted = library.query_revisions(0, Some(1));
        assert_ne!(compact.stamp(renamed), compact.stamp(deleted));
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
        assert!(browser.view_pending(kind));

        assert_eq!(
            browser.complete_playlist_read(&read, revisions, empty()),
            None
        );
        assert_eq!(browser.playlists.selected(), Some(41));
        assert!(browser.playlists.entries().rows.is_empty());
        assert!(!browser.view_pending(kind));
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
        assert!(browser.view_pending(kind));
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
        assert!(!browser.view_pending(kind));
        assert_eq!(
            browser.playlists.entries().rows.error(0),
            Some("unavailable")
        );

        let (read, _) = browser
            .retry_playlist_entries(20, revisions)
            .into_read()
            .unwrap();
        assert!(browser.view_pending(kind));
        assert_eq!(browser.playlists.entries().rows.error(0), None);
        assert_eq!(
            browser.complete_playlist_read(&read, revisions, empty()),
            None
        );
        assert!(browser.playlists.entries().rows.is_empty());
        assert!(!browser.view_pending(kind));
    }

    #[test]
    fn empty_catalog_releases_selected_playlist_and_pending_entry_requests() {
        let mut browser = PlaylistModel::default();
        let revisions = QueryRevisions::default();
        browser.select_playlist(41, "deleted".into(), 0);
        let (lookup, _) = browser
            .prepare_playlist_lookup(revisions)
            .into_read()
            .unwrap();
        let (entries, _) = browser
            .request_playlist_entries(41, 0, 20, revisions)
            .into_read()
            .unwrap();
        let (catalog, _) = browser
            .request_playlist_summaries(0, 20, revisions)
            .into_read()
            .unwrap();
        browser.complete_playlist_read(&catalog, revisions, lookup_error(revisions, "unavailable"));
        assert!(!browser.view_pending(ViewRequestKind::Summaries));
        assert_eq!(
            browser.playlists.summaries().rows.error(0),
            Some("unavailable")
        );
        assert_eq!(browser.playlists.selected(), Some(41));
        let (retry, _) = browser
            .retry_playlist_summaries(20, revisions)
            .into_read()
            .unwrap();
        browser.complete_playlist_read(
            &retry,
            revisions,
            QueryResponse {
                revisions,
                result: Ok(ViewResponse::PlaylistSummaries(Page {
                    total: 0,
                    rows: Vec::new(),
                })),
            },
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
            assert!(!browser.view_pending(kind));
        }
        assert!(!browser.playlist_read_current(&lookup, revisions));
        assert!(!browser.playlist_read_current(&entries, revisions));
        assert!(
            browser
                .requests
                .slot(ViewRequestKind::Selection)
                .worker_active
        );
        assert!(
            browser
                .requests
                .slot(ViewRequestKind::Entries)
                .worker_active
        );
        assert_eq!(browser.playlists.demand_page(true, 0..1, 20), None);
    }

    #[test]
    fn clearing_selection_releases_entry_owner_and_outstanding_requests() {
        let mut browser = PlaylistModel::default();
        let revisions = QueryRevisions::default();
        browser.select_playlist(41, "selected".into(), 0);
        let (lookup, lookup_ticket) = browser
            .prepare_playlist_lookup(revisions)
            .into_read()
            .unwrap();
        let (entries, entries_ticket) = browser
            .request_playlist_entries(41, 0, 20, revisions)
            .into_read()
            .unwrap();
        browser.remove_playlist_selection();
        assert_eq!(browser.playlists.selected(), None);
        assert_eq!(browser.playlists.entries().playlist_id, None);
        assert!(browser.playlists.entries().rows.is_empty());
        for kind in [ViewRequestKind::Selection, ViewRequestKind::Entries] {
            assert!(!browser.view_pending(kind));
            assert!(browser.requests.slot(kind).worker_active);
        }
        assert!(!lookup_ticket.is_current());
        assert!(!entries_ticket.is_current());
        assert!(!browser.playlist_read_current(&lookup, revisions));
        assert!(!browser.playlist_read_current(&entries, revisions));
    }

    fn automatic_request(
        browser: &mut PlaylistModel,
        kind: ViewRequestKind,
        revisions: QueryRevisions,
    ) -> ViewRequest {
        match kind {
            ViewRequestKind::Summaries => browser.demand_playlist_summaries(0..1, 20, revisions),
            ViewRequestKind::Entries => browser.demand_playlist_entries(0..1, 20, revisions),
            ViewRequestKind::Selection => browser.prepare_playlist_lookup(revisions),
        }
    }

    #[test]
    fn structured_retirement_parks_only_rejected_demand_until_its_stamp_changes() {
        for kind in [
            ViewRequestKind::Summaries,
            ViewRequestKind::Entries,
            ViewRequestKind::Selection,
        ] {
            for error in [QueryError::RevisionChanged, QueryError::Cancelled] {
                let mut browser = PlaylistModel::default();
                let revisions = QueryRevisions::default();
                browser.select_playlist(1, "one".into(), 0);
                let (read, ticket) = automatic_request(&mut browser, kind, revisions)
                    .into_read()
                    .unwrap();
                let other = if kind == ViewRequestKind::Summaries {
                    ViewRequestKind::Entries
                } else {
                    ViewRequestKind::Summaries
                };
                let (other_read, other_ticket) = automatic_request(&mut browser, other, revisions)
                    .into_read()
                    .unwrap();
                assert_eq!(
                    browser.complete_playlist_read(
                        &read,
                        revisions,
                        QueryResponse {
                            revisions,
                            result: Err(error)
                        },
                    ),
                    None
                );
                assert!(!ticket.is_current());
                assert!(!browser.view_pending(kind));
                assert_eq!(browser.playlist_lookup_failure(), None);
                assert_eq!(browser.playlists.summaries().rows.error(0), None);
                assert_eq!(browser.playlists.entries().rows.error(0), None);
                for _ in 0..3 {
                    let parked = automatic_request(&mut browser, kind, revisions);
                    assert_eq!(parked.pending_generation(), None);
                    assert!(parked.into_read().is_none());
                }
                assert!(other_ticket.is_current());
                assert!(browser.playlist_read_current(&other_read, revisions));
                let current = QueryRevisions {
                    library: 1,
                    playlist: 1,
                    ..revisions
                };
                assert!(
                    automatic_request(&mut browser, kind, current)
                        .into_read()
                        .is_some()
                );
            }
        }
    }

    #[test]
    fn unexpected_payload_is_terminal_for_each_kind_and_existing_retry_recovers() {
        for kind in [
            ViewRequestKind::Summaries,
            ViewRequestKind::Entries,
            ViewRequestKind::Selection,
        ] {
            let mut browser = PlaylistModel::default();
            let revisions = QueryRevisions {
                library: 7,
                playlist: 7,
                ..Default::default()
            };
            browser.select_playlist(1, "one".into(), 0);
            let (read, _) = automatic_request(&mut browser, kind, revisions)
                .into_read()
                .unwrap();
            let message = browser
                .complete_playlist_read(
                    &read,
                    revisions,
                    QueryResponse {
                        // An abnormal variant has no usable payload stamp.
                        revisions: QueryRevisions::default(),
                        result: Ok(ViewResponse::Track(None)),
                    },
                )
                .expect("unexpected payload must fail");
            assert!(!browser.view_pending(kind));
            assert!(
                automatic_request(&mut browser, kind, revisions)
                    .into_read()
                    .is_none()
            );
            let retry = match kind {
                ViewRequestKind::Summaries => {
                    assert_eq!(
                        browser.playlists.summaries().rows.error(0),
                        Some(message.as_str())
                    );
                    browser.retry_playlist_summaries(20, revisions)
                }
                ViewRequestKind::Entries => {
                    assert_eq!(
                        browser.playlists.entries().rows.error(0),
                        Some(message.as_str())
                    );
                    browser.retry_playlist_entries(20, revisions)
                }
                ViewRequestKind::Selection => {
                    let (key, error) = browser.playlist_lookup_failure().unwrap();
                    assert_eq!(error, message);
                    browser.retry_playlist_lookup(key, revisions)
                }
            };
            let (retry, _) = retry.into_read().unwrap();
            let result = match kind {
                ViewRequestKind::Summaries => ViewResponse::PlaylistSummaries(Page {
                    total: 1,
                    rows: vec![summary(1, 0)],
                }),
                ViewRequestKind::Entries => ViewResponse::PlaylistEntries(Page {
                    total: 0,
                    rows: Vec::new(),
                }),
                ViewRequestKind::Selection => ViewResponse::PlaylistSummary(Some(summary(1, 0))),
            };
            assert_eq!(
                browser.complete_playlist_read(
                    &retry,
                    revisions,
                    QueryResponse {
                        revisions,
                        result: Ok(result)
                    },
                ),
                None
            );
            assert!(!browser.view_pending(kind));
            assert_eq!(browser.playlist_lookup_failure(), None);
        }
    }
}
