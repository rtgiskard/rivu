//! Playlist occurrence selection is independent of bounded metadata pages.
//! Jobs retain compact identities and publish atomically for their original token.
//! Metadata-only changes retry pending navigation without invalidating occurrence identities.
//! The feature owns selection jobs; the window coordinates manual confirmation.
use super::{PlaylistEvent, PlaylistSelectionRequest, Playlists};
use crate::gui::coordination::manual_actions::ManualIntentId;
use gpui::Context;
use rivu_protocol::{
    model::{Command, Page, PlaylistEntryLocation, PlaylistEntryRow, Query},
    response::{PlaylistEntryRevision, QueryRevisions, ViewResponse},
};
use std::collections::HashMap;

pub(in crate::gui) struct EntryCommit {
    scope: EntryNavigationScope,
    generation: u64,
    pub(in crate::gui) target: EntryIdentity,
    anchor: Option<EntryIdentity>,
    rows: Option<Vec<EntryIdentity>>,
    multi: bool,
    scroll: bool,
}

enum EntryTarget {
    Row(EntryIdentity),
    Offset(usize),
}

/// Frozen before root cancellation: subsequent input builds on the desired row,
/// not the last committed row, even while range reads or confirmation are pending.
pub(in crate::gui) struct PreparedEntrySelection {
    scope: EntryNavigationScope,
    target: EntryTarget,
    shift: bool,
    multi: bool,
    all: bool,
}

struct EntryRangeRequest {
    intent: ManualIntentId,
    commit: EntryCommit,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) struct EntryIdentity {
    pub(in crate::gui) id: i64,
    pub(in crate::gui) track_id: i64,
    pub(in crate::gui) position: usize,
    pub(in crate::gui) offset: usize,
}
impl EntryIdentity {
    pub(in crate::gui) fn row(row: &PlaylistEntryRow, offset: usize) -> Self {
        Self {
            id: row.id,
            track_id: row.track_id,
            position: row.position,
            offset,
        }
    }
}
impl From<&PlaylistEntryLocation> for EntryIdentity {
    fn from(row: &PlaylistEntryLocation) -> Self {
        Self {
            id: row.id,
            track_id: row.track_id,
            position: row.position,
            offset: row.offset,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq)]
struct EntryNavigationScope {
    playlist_id: i64,
    revision: u64,
    metadata: u64,
    bitrate: u64,
}
impl EntryNavigationScope {
    fn revisions(self) -> QueryRevisions {
        QueryRevisions {
            bitrate: self.bitrate,
            playlist_metadata: Some(PlaylistEntryRevision {
                playlist_id: self.playlist_id,
                revision: self.metadata,
            }),
            playlist_entries: Some(PlaylistEntryRevision {
                playlist_id: self.playlist_id,
                revision: self.revision,
            }),
            ..Default::default()
        }
    }

    fn same_occurrences(self, other: Self) -> bool {
        let query = Query::PlaylistEntryLocations {
            query: None,
            playlist_id: self.playlist_id,
            entry_ids: Vec::new(),
        };
        query.stamp(self.revisions()) == query.stamp(other.revisions())
    }
}

impl PartialEq for EntryNavigationScope {
    fn eq(&self, other: &Self) -> bool {
        let query = Query::PlaylistEntries {
            query: None,
            playlist_id: self.playlist_id,
            offset: 0,
            limit: 1,
        };
        query.stamp(self.revisions()) == query.stamp(other.revisions())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EntryNavigationJob {
    scope: EntryNavigationScope,
    generation: u64,
    offset: usize,
    shift: bool,
    multi: bool,
    all: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct EntryNavigationRange {
    start: usize,
    end: usize,
    anchor: EntryIdentity,
    additive: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum EntryNavigationEffect {
    None,
    Reload,
    Failed(String),
    Selected {
        target: EntryIdentity,
        range: Option<EntryNavigationRange>,
    },
}

#[derive(Default)]
pub(in crate::gui) struct EntrySelection {
    selected: HashMap<i64, EntryIdentity>,
    pub(in crate::gui) query: Option<String>,
    pub(in crate::gui) active: Option<EntryIdentity>,
    anchor: Option<EntryIdentity>,
    generation: u64,
    projection: crate::query_tracking::QueryTracker,
    navigation_projection: crate::query_tracking::QueryTracker,
    pub(in crate::gui) busy: bool,
    pub(in crate::gui) error: Option<String>,
    navigation: Option<EntryNavigationJob>,
    navigation_target: Option<(EntryNavigationScope, usize)>,
    // Logical cancellation skips queued reads; dispatched reads retain the gate.
    navigation_in_flight: Option<EntryNavigationJob>,
    intent: Option<ManualIntentId>,
    range_request: Option<EntryRangeRequest>,
    range_in_flight: Option<u64>,
    refresh_in_flight: bool,
    refresh_pending: bool,
}
impl EntrySelection {
    fn reclaim_selection_capacity(&mut self, len: usize) {
        if self.selected.capacity() > 64.max(len.saturating_mul(4)) {
            self.selected.shrink_to(len.saturating_mul(2));
        }
    }
    pub(in crate::gui) fn sync_projection(
        &mut self,
        playlist_id: i64,
        query: Option<String>,
        revisions: QueryRevisions,
    ) -> bool {
        let changed = self.projection.sync(
            &Query::PlaylistEntryLocations {
                query: query.clone(),
                playlist_id,
                entry_ids: Vec::new(),
            },
            revisions,
        );
        if changed {
            self.request_refresh();
        }
        self.navigation_projection.sync(
            &Query::PlaylistEntries {
                query,
                playlist_id,
                offset: 0,
                limit: 1,
            },
            revisions,
        );
        changed
    }

    fn request_refresh(&mut self) {
        self.invalidate();
        self.refresh_pending = true;
    }

    fn prepare_refresh(&mut self) -> Option<(Vec<i64>, u64, crate::query_tracking::QueryTicket)> {
        if self.refresh_in_flight || !self.refresh_pending {
            return None;
        }
        self.refresh_pending = false;
        let ids = self.ids();
        if ids.is_empty() {
            return None;
        }
        self.refresh_in_flight = true;
        Some((ids, self.generation, self.projection.ticket()))
    }

    fn retire_refresh(&mut self) -> bool {
        self.refresh_in_flight = false;
        self.refresh_pending
    }

    pub(in crate::gui) fn len(&self) -> usize {
        self.selected.len()
    }
    pub(in crate::gui) fn contains(&self, id: i64) -> bool {
        self.selected.contains_key(&id)
    }
    pub(in crate::gui) fn ready(&self) -> bool {
        !self.busy && self.error.is_none()
    }
    pub(in crate::gui) fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.projection.cancel();
        self.navigation_projection.cancel();
        self.refresh_pending = false;
        self.busy = false;
        self.error = None;
        // The physical worker owns its gate until completion, including after a switch.
        self.navigation = None;
        self.navigation_target = None;
        self.intent = None;
        self.range_request = None;
    }
    pub(in crate::gui) fn cancel_manual(&mut self) {
        if self.intent.is_some() || self.range_request.is_some() {
            self.cancel();
        }
    }
    pub(in crate::gui) fn clear(&mut self) {
        self.cancel();
        self.selected.clear();
        self.reclaim_selection_capacity(0);
        self.active = None;
        self.anchor = None;
    }
    pub(in crate::gui) fn invalidate(&mut self) {
        self.cancel();
        self.busy = !self.selected.is_empty() || self.active.is_some() || self.anchor.is_some();
    }
    pub(in crate::gui) fn select(&mut self, row: EntryIdentity, multi: bool) {
        self.cancel();
        if !multi {
            self.selected.clear();
            self.reclaim_selection_capacity(1);
        }
        if !multi || self.selected.remove(&row.id).is_none() {
            self.selected.insert(row.id, row);
        }
        self.reclaim_selection_capacity(self.selected.len());
        self.active = Some(row);
        self.anchor = Some(row);
    }
    fn begin_selection(
        &mut self,
        intent: ManualIntentId,
        scope: EntryNavigationScope,
        row: EntryIdentity,
        shift: bool,
        multi: bool,
    ) -> Option<EntryCommit> {
        self.intent = Some(intent);
        self.navigation_target = Some((scope, row.offset));
        let commit = EntryCommit {
            scope,
            generation: self.generation,
            target: row,
            anchor: self.anchor,
            rows: None,
            multi,
            scroll: false,
        };
        if shift && let Some(anchor) = self.anchor {
            self.queue_range(
                intent,
                commit,
                anchor.offset.min(row.offset),
                anchor.offset.max(row.offset) + 1,
            );
            None
        } else {
            Some(commit)
        }
    }

    fn queue_range(
        &mut self,
        intent: ManualIntentId,
        commit: EntryCommit,
        start: usize,
        end: usize,
    ) {
        self.busy = true;
        self.intent = Some(intent);
        self.range_request = Some(EntryRangeRequest {
            intent,
            commit,
            start,
            end,
        });
    }

    fn finish_read(&mut self, intent: ManualIntentId, generation: u64) -> bool {
        if self.intent != Some(intent) || !self.current(generation) {
            return false;
        }
        self.busy = false;
        true
    }

    fn begin_navigation(
        &mut self,
        intent: ManualIntentId,
        scope: EntryNavigationScope,
        offset: usize,
        shift: bool,
        multi: bool,
        all: bool,
    ) {
        self.navigate_to(scope, offset, shift, multi, all);
        self.intent = Some(intent);
    }
    pub(in crate::gui) fn ids(&self) -> Vec<i64> {
        let mut ids: Vec<_> = self.selected.keys().copied().collect();
        for owner in [self.active, self.anchor].into_iter().flatten() {
            if !ids.contains(&owner.id) {
                ids.push(owner.id);
            }
        }
        ids
    }
    pub(in crate::gui) fn ordered(&self) -> Vec<EntryIdentity> {
        if !self.ready() {
            return Vec::new();
        }
        let mut rows: Vec<_> = self.selected.values().copied().collect();
        rows.sort_unstable_by_key(|row| row.position);
        rows
    }
    pub(in crate::gui) fn ordered_ids(&self) -> Vec<i64> {
        if !self.ready() {
            return Vec::new();
        }
        let mut ids: Vec<_> = self.selected.keys().copied().collect();
        ids.sort_unstable_by_key(|id| self.selected[id].position);
        ids
    }
    pub(in crate::gui) fn drag_ids(&self, source: i64) -> Vec<i64> {
        if !self.ready() {
            Vec::new()
        } else if self.contains(source) {
            self.ordered_ids()
        } else {
            vec![source]
        }
    }
    fn fail(&mut self, message: String) {
        self.busy = false;
        self.error = Some(message);
        self.navigation = None;
        self.navigation_target = None;
        self.intent = None;
    }
    fn timeout(&mut self, generation: u64, message: String) -> bool {
        if !self.current(generation) {
            return false;
        }
        // Retire publication rights, never the physical navigation/range/refresh gates.
        self.cancel();
        self.fail(message);
        true
    }
    #[cfg(test)]
    pub(in crate::gui) fn remove(&mut self, id: i64) {
        self.cancel();
        self.selected.remove(&id);
        self.reclaim_selection_capacity(self.selected.len());
        if self.active.is_some_and(|row| row.id == id) {
            self.active = None;
        }
        if self.anchor.is_some_and(|row| row.id == id) {
            self.anchor = None;
        }
    }
    fn current(&self, generation: u64) -> bool {
        self.generation == generation
    }
    pub(in crate::gui) fn reconcile(&mut self, rows: Vec<EntryIdentity>) {
        self.active = self
            .active
            .and_then(|old| rows.iter().find(|row| row.id == old.id).copied());
        self.anchor = self
            .anchor
            .and_then(|old| rows.iter().find(|row| row.id == old.id).copied());
        self.selected = rows
            .into_iter()
            .filter(|row| self.selected.contains_key(&row.id))
            .map(|row| (row.id, row))
            .collect();
        self.reclaim_selection_capacity(self.selected.len());
        self.busy = false;
        self.error = None;
    }
    fn complete_range(
        &mut self,
        generation: u64,
        anchor: Option<EntryIdentity>,
        target: EntryIdentity,
        rows: Vec<EntryIdentity>,
        additive: bool,
    ) -> bool {
        if !self.current(generation) {
            return false;
        }
        if !additive {
            self.selected.clear();
            self.reclaim_selection_capacity(rows.len());
        }
        self.selected
            .extend(rows.into_iter().map(|row| (row.id, row)));
        self.anchor = anchor.or(Some(target));
        self.active = Some(target);
        self.busy = false;
        self.navigation = None;
        self.navigation_target = None;
        self.intent = None;
        true
    }
    fn can_select_all(&self, total: usize) -> bool {
        total > 0 && (self.ready() || (self.busy && self.intent.is_some()))
    }

    fn prepare_request(
        &self,
        scope: EntryNavigationScope,
        total: usize,
        request: PlaylistSelectionRequest,
        row: Option<EntryIdentity>,
    ) -> Option<PreparedEntrySelection> {
        let (target, shift, multi, all) = match request {
            PlaylistSelectionRequest::Entry { shift, multi, .. } => {
                if (shift || multi) && !self.can_select_all(total) {
                    return None;
                }
                (EntryTarget::Row(row?), shift, multi, false)
            }
            PlaylistSelectionRequest::Navigate { down, shift, multi } => (
                EntryTarget::Offset(self.navigation_offset(scope, total, down)?),
                shift,
                multi,
                false,
            ),
            PlaylistSelectionRequest::Edge { last } => {
                if !self.can_select_all(total) {
                    return None;
                }
                (
                    EntryTarget::Offset(if last { total - 1 } else { 0 }),
                    false,
                    false,
                    false,
                )
            }
            PlaylistSelectionRequest::All => {
                if !self.can_select_all(total) {
                    return None;
                }
                (EntryTarget::Offset(0), false, false, true)
            }
        };
        Some(PreparedEntrySelection {
            scope,
            target,
            shift,
            multi,
            all,
        })
    }

    fn navigation_offset(
        &self,
        scope: EntryNavigationScope,
        total: usize,
        down: bool,
    ) -> Option<usize> {
        if !self.can_select_all(total) {
            return None;
        }
        let current = self
            .navigation_target
            .filter(|(target_scope, _)| target_scope.same_occurrences(scope))
            .map(|(_, offset)| offset)
            .or(self.active.map(|row| row.offset));
        let offset = match current {
            Some(index) if down => index.saturating_add(1).min(total - 1),
            Some(index) => index.saturating_sub(1),
            None if down => 0,
            None => total - 1,
        };
        (current != Some(offset)).then_some(offset)
    }
    fn navigate_to(
        &mut self,
        scope: EntryNavigationScope,
        offset: usize,
        shift: bool,
        multi: bool,
        all: bool,
    ) {
        self.cancel();
        self.busy = true;
        self.navigation_target = Some((scope, offset));
        self.navigation = Some(EntryNavigationJob {
            scope,
            generation: self.generation,
            offset,
            shift,
            multi,
            all,
        });
    }
    fn take_navigation(&mut self) -> Option<EntryNavigationJob> {
        if self.navigation_in_flight.is_some() || self.range_in_flight.is_some() {
            return None;
        }
        let job = self.navigation?;
        self.navigation_in_flight = Some(job);
        Some(job)
    }
    fn take_range(&mut self) -> Option<EntryRangeRequest> {
        if self.range_in_flight.is_some() || self.navigation_in_flight.is_some() {
            return None;
        }
        let job = self.range_request.take()?;
        self.range_in_flight = Some(job.commit.generation);
        Some(job)
    }

    fn release_range_worker(&mut self, generation: u64) {
        if self.range_in_flight == Some(generation) {
            self.range_in_flight = None;
        }
    }

    fn finish_navigation(
        &mut self,
        unexpected_response: &str,
        job: EntryNavigationJob,
        scope: Option<EntryNavigationScope>,
        response: rivu_protocol::response::QueryResponse,
    ) -> EntryNavigationEffect {
        if self.navigation_in_flight != Some(job) {
            return EntryNavigationEffect::None;
        }
        self.navigation_in_flight = None;
        if let Some(intent) = self.navigation
            && scope != Some(intent.scope)
        {
            if let Some(scope) = scope.filter(|scope| intent.scope.same_occurrences(*scope)) {
                // Occurrences remain valid; retry the latest intent with current metadata.
                self.navigation = Some(EntryNavigationJob { scope, ..intent });
            } else {
                self.invalidate();
            }
        }
        if scope != Some(job.scope) || !self.current(job.generation) || self.navigation != Some(job)
        {
            return EntryNavigationEffect::None;
        }
        self.navigation = None;
        let response = entry_query_result(response, job.scope, self.query.clone());
        match response {
            Ok(Some(ViewResponse::PlaylistEntries(page))) => {
                let Some(row) = page.rows.first() else {
                    return EntryNavigationEffect::Reload;
                };
                let target = EntryIdentity::row(row, job.offset);
                let range = if job.all {
                    Some(EntryNavigationRange {
                        start: 0,
                        end: page.total,
                        anchor: target,
                        additive: false,
                    })
                } else if job.shift
                    && let Some(anchor) = self.anchor
                {
                    Some(EntryNavigationRange {
                        start: anchor.offset.min(job.offset),
                        end: anchor.offset.max(job.offset) + 1,
                        anchor,
                        additive: job.multi,
                    })
                } else {
                    None
                };
                self.busy = range.is_some();
                EntryNavigationEffect::Selected { target, range }
            }
            // A stale payload cannot make retained identities ready. The window
            // must first accept the reload effect for this manual intent.
            Ok(None) => EntryNavigationEffect::Reload,
            result => {
                let message = match result {
                    Err(error) => error.to_string(),
                    _ => unexpected_response.into(),
                };
                self.fail(message.clone());
                EntryNavigationEffect::Failed(message)
            }
        }
    }
}

/// Only compact identities accumulate; at most one metadata batch is live.
struct EntryRangePages {
    offset: usize,
    end: usize,
    identities: Vec<EntryIdentity>,
}
impl EntryRangePages {
    fn new(start: usize, end: usize) -> Self {
        Self {
            offset: start,
            end,
            identities: Vec::with_capacity(end - start),
        }
    }
    fn next(&self) -> Option<(usize, usize)> {
        (self.offset < self.end).then(|| {
            (
                self.offset,
                (self.end - self.offset).min(rivu_protocol::model::MAX_QUERY_ROWS),
            )
        })
    }
    fn accept(&mut self, page: Page<PlaylistEntryRow>) -> bool {
        let Some((offset, limit)) = self.next() else {
            return false;
        };
        if page.total < self.end
            || page.rows.is_empty()
            || page.rows.len() > limit
            || page
                .rows
                .iter()
                .enumerate()
                .any(|(index, row)| row.offset != offset + index)
        {
            return false;
        }
        self.identities.extend(
            page.rows
                .iter()
                .enumerate()
                .map(|(index, row)| EntryIdentity::row(row, offset + index)),
        );
        self.offset += page.rows.len();
        true
    }
}

/// Transport failures have no playlist scope; only successful payloads can be revision-checked.
fn entry_query_result(
    response: rivu_protocol::response::QueryResponse,
    scope: EntryNavigationScope,
    filter: Option<String>,
) -> Result<Option<ViewResponse>, rivu_protocol::response::QueryError> {
    let result = response.result?;
    let query = match &result {
        ViewResponse::PlaylistEntryLocations(_) => Query::PlaylistEntryLocations {
            query: filter,
            playlist_id: scope.playlist_id,
            entry_ids: Vec::new(),
        },
        _ => Query::PlaylistEntries {
            query: filter,
            playlist_id: scope.playlist_id,
            offset: 0,
            limit: 1,
        },
    };
    let expected = scope.revisions();
    Ok((query.stamp(response.revisions) == query.stamp(expected)).then_some(result))
}

impl Playlists {
    pub(in crate::gui) fn prepare_selection_request(
        &mut self,
        request: PlaylistSelectionRequest,
        cx: &mut Context<Self>,
    ) -> Option<PreparedEntrySelection> {
        self.refresh(cx);
        let scope = self.playlist_navigation_scope()?;
        let entries = self.browser.playlists().entries();
        let row = match request {
            PlaylistSelectionRequest::Entry { id, index, .. } => entries
                .rows
                .get(index)
                .filter(|row| row.id == id)
                .map(|row| EntryIdentity::row(row, index)),
            _ => None,
        };
        self.browser.playlists().track_selection.prepare_request(
            scope,
            entries.rows.total()?,
            request,
            row,
        )
    }

    pub(in crate::gui) fn prepare_selection(
        &mut self,
        request: PreparedEntrySelection,
        intent: ManualIntentId,
        cx: &mut Context<Self>,
    ) {
        self.intent = Some(intent);
        match request.target {
            EntryTarget::Row(row) => {
                if let Some(commit) = self.browser.playlist_selection_mut().begin_selection(
                    intent,
                    request.scope,
                    row,
                    request.shift,
                    request.multi,
                ) {
                    self.ready_entry_selection(intent, commit, cx);
                } else {
                    self.dispatch_playlist_range(cx);
                }
            }
            EntryTarget::Offset(offset) => {
                self.browser.playlist_selection_mut().begin_navigation(
                    intent,
                    request.scope,
                    offset,
                    request.shift,
                    request.multi,
                    request.all,
                );
                self.dispatch_playlist_navigation(cx);
            }
        }
        cx.notify();
    }

    pub(in crate::gui) fn select_playlist_entry(
        &mut self,
        id: i64,
        index: usize,
        shift: bool,
        multi: bool,
        cx: &mut Context<Self>,
    ) {
        cx.emit(PlaylistEvent::Selection(PlaylistSelectionRequest::Entry {
            id,
            index,
            shift,
            multi,
        }));
    }

    pub(in crate::gui) fn select_all_playlist_entries(&mut self, cx: &mut Context<Self>) {
        cx.emit(PlaylistEvent::Selection(PlaylistSelectionRequest::All));
    }

    pub(in crate::gui) fn navigate_playlist(
        &mut self,
        down: bool,
        shift: bool,
        multi: bool,
        cx: &mut Context<Self>,
    ) {
        cx.emit(PlaylistEvent::Selection(
            PlaylistSelectionRequest::Navigate { down, shift, multi },
        ));
    }

    pub(in crate::gui) fn jump_playlist_edge(&mut self, last: bool, cx: &mut Context<Self>) {
        cx.emit(PlaylistEvent::Selection(PlaylistSelectionRequest::Edge {
            last,
        }));
    }
}

impl Playlists {
    pub(in crate::gui) fn playlist_occurrence_stamp(
        &self,
        playlist_id: i64,
    ) -> rivu_protocol::model::QueryStamp {
        Query::PlaylistEntryLocations {
            query: None,
            playlist_id,
            entry_ids: Vec::new(),
        }
        .stamp(self.state.query_revisions(Some(playlist_id)))
    }

    pub(in crate::gui) fn sync_entry_projection(&mut self, cx: &mut Context<Self>) {
        let revisions = self
            .state
            .query_revisions(self.browser.playlists().selected());
        if self.browser.sync_entry_projection(revisions) {
            self.dispatch_entry_reconcile(cx);
        }
    }

    pub(in crate::gui) fn playlist_revision(&self, id: i64) -> u64 {
        self.state
            .library
            .playlist_entry_revisions
            .get(&id)
            .copied()
            .unwrap_or(0)
    }
    fn entry_job_current(
        &self,
        playlist_id: i64,
        generation: u64,
        revision: u64,
        library: u64,
    ) -> bool {
        self.browser.playlists().selected() == Some(playlist_id)
            && self.browser.playlists().entries().playlist_id == Some(playlist_id)
            && self.browser.playlists().track_selection.current(generation)
            && {
                let query = Query::PlaylistEntryLocations {
                    query: self.browser.playlists().entries().query.clone(),
                    playlist_id,
                    entry_ids: Vec::new(),
                };
                let expected = QueryRevisions {
                    playlist_metadata: Some(PlaylistEntryRevision {
                        playlist_id,
                        revision: library,
                    }),
                    playlist_entries: Some(PlaylistEntryRevision {
                        playlist_id,
                        revision,
                    }),
                    ..Default::default()
                };
                query.stamp(self.state.query_revisions(Some(playlist_id))) == query.stamp(expected)
            }
    }
    fn entry_job_failed(
        &mut self,
        playlist_id: i64,
        generation: u64,
        message: String,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if self.browser.playlists().selected() == Some(playlist_id)
            && self.browser.playlists().track_selection.current(generation)
        {
            // Keep the pre-job selection intact; a failed partial range never becomes visible.
            self.browser.playlist_selection_mut().fail(message.clone());
            self.report_error(message, cx);
            cx.notify();
        }
    }

    fn entry_job_timeout(
        &mut self,
        playlist_id: i64,
        generation: u64,
        revision: u64,
        metadata: u64,
        response: rivu_protocol::response::QueryResponse,
        cx: &mut Context<Self>,
    ) -> bool {
        self.refresh(cx);
        if !self.entry_job_current(playlist_id, generation, revision, metadata) {
            return false;
        }
        let Err(error) = response.result else {
            return false;
        };
        let message = error.to_string();
        if self
            .browser
            .playlist_selection_mut()
            .timeout(generation, message.clone())
        {
            self.report_error(message, cx);
            cx.notify();
            return true;
        }
        false
    }
    pub(in crate::gui) fn refresh_entry_selection(&mut self, cx: &mut Context<Self>) {
        self.browser.playlist_selection_mut().request_refresh();
        self.dispatch_entry_reconcile(cx);
    }

    pub(in crate::gui) fn dispatch_entry_reconcile(&mut self, cx: &mut Context<Self>) {
        let Some(playlist_id) = self.browser.playlists().selected() else {
            return;
        };
        let Some((ids, generation, ticket)) =
            self.browser.playlist_selection_mut().prepare_refresh()
        else {
            return;
        };
        let revision = self.playlist_revision(playlist_id);
        let library = self
            .state
            .library
            .metadata_revisions
            .playlist_revision(playlist_id, self.state.library.revision);
        let handle = self.context.handle().clone();
        let i18n = std::sync::Arc::clone(&self.i18n);
        let filter = self.browser.playlists().entries().query.clone();
        cx.spawn(async move |this, cx| {
            async {
                let mut identities = Vec::with_capacity(ids.len());
                for chunk in ids.chunks(rivu_protocol::model::MAX_QUERY_ROWS) {
                    if !this
                        .update(cx, |this, cx| {
                            this.refresh(cx);
                            this.entry_job_current(playlist_id, generation, revision, library)
                        })
                        .unwrap_or(false)
                    {
                        return;
                    }
                    let handle = handle.clone();
                    let entry_ids = chunk.to_vec();
                    let ticket = ticket.clone();
                    let query = filter.clone();
                    let response = crate::gui::query::run(
                        handle,
                        cx,
                        move |handle| async move {
                            handle
                                .query_if_current(
                                    Query::PlaylistEntryLocations {
                                        query,
                                        playlist_id,
                                        entry_ids,
                                    },
                                    ticket,
                                )
                                .await
                        },
                        |response, cx| {
                            let _ = this.update(cx, |this, cx| {
                                this.entry_job_timeout(
                                    playlist_id,
                                    generation,
                                    revision,
                                    library,
                                    response,
                                    cx,
                                );
                            });
                        },
                    )
                    .await;
                    let result = match entry_query_result(
                        response,
                        EntryNavigationScope {
                            playlist_id,
                            revision,
                            metadata: library,
                            bitrate: 0,
                        },
                        filter.clone(),
                    ) {
                        Ok(result) => result,
                        Err(error) => {
                            let _ = this.update(cx, |this, cx| {
                                this.refresh(cx);
                                if this.entry_job_current(
                                    playlist_id,
                                    generation,
                                    revision,
                                    library,
                                ) {
                                    this.entry_job_failed(
                                        playlist_id,
                                        generation,
                                        error.to_string(),
                                        cx,
                                    );
                                }
                            });
                            return;
                        }
                    };
                    let Some(result) = result else {
                        let _ = this.update(cx, |this, cx| {
                            this.refresh(cx);
                            if this.entry_job_current(playlist_id, generation, revision, library) {
                                this.refresh_entry_selection(cx);
                            }
                        });
                        return;
                    };
                    match result {
                        ViewResponse::PlaylistEntryLocations(rows) => identities.extend(
                            rows.into_iter()
                                .filter(|row| row.playlist_id == playlist_id),
                        ),
                        _ => {
                            let message = i18n.text("gl-playlist-selection-response").into();
                            let _ = this.update(cx, |this, cx| {
                                this.entry_job_failed(playlist_id, generation, message, cx)
                            });
                            return;
                        }
                    }
                }
                let _ = this.update(cx, |this, cx| {
                    this.refresh(cx);
                    if !this.entry_job_current(playlist_id, generation, revision, library) {
                        return;
                    }
                    this.browser.accept_playlist_entry_locations(identities);
                    cx.notify();
                });
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                if this.browser.playlist_selection_mut().retire_refresh() {
                    this.dispatch_entry_reconcile(cx);
                }
            });
        })
        .detach();
    }
    pub(in crate::gui) fn select_playlist_container(
        &mut self,
        id: i64,
        name: String,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        self.cancel_selection(cx);
        cx.emit(PlaylistEvent::InvalidateManualIntent);
        if self.browser.select_playlist(id, name, index) {
            self.playlist_entries_search
                .update(cx, |search, cx| search.reset(cx));
            self.request_playlist_entries(id, 0, cx);
            self.sync_entry_projection(cx);
        }
    }

    pub(in crate::gui) fn entry_commit_valid(&self, commit: &EntryCommit) -> bool {
        self.entry_job_current(
            commit.scope.playlist_id,
            commit.generation,
            commit.scope.revision,
            commit.scope.metadata,
        )
    }

    pub(in crate::gui) fn commit_entry_selection(
        &mut self,
        commit: EntryCommit,
        cx: &mut Context<Self>,
    ) {
        if !self.entry_commit_valid(&commit) {
            return;
        }
        self.intent = None;
        let selection = self.browser.playlist_selection_mut();
        if let Some(rows) = commit.rows {
            selection.complete_range(
                commit.generation,
                commit.anchor,
                commit.target,
                rows,
                commit.multi,
            );
        } else {
            selection.select(commit.target, commit.multi);
        }
        self.browser.sync_active_playlist_entry();
        if self.panel_demand("playlists") {
            self.focus_list(cx);
        }
        cx.emit(PlaylistEvent::Metadata(commit.target.track_id));
        if commit.scroll {
            self.context
                .scroll
                .borrow_mut()
                .scroll_list_to("playlist-entries", commit.target.offset);
        }
        cx.notify();
    }

    fn queue_playlist_range(
        &mut self,
        intent: ManualIntentId,
        commit: EntryCommit,
        start: usize,
        end: usize,
        cx: &mut Context<Self>,
    ) {
        self.browser
            .playlist_selection_mut()
            .queue_range(intent, commit, start, end);
        self.dispatch_playlist_range(cx);
    }

    fn dispatch_playlist_range(&mut self, cx: &mut Context<Self>) {
        let Some(mut job) = self.browser.playlist_selection_mut().take_range() else {
            return;
        };
        let generation = job.commit.generation;
        let handle = self.context.handle().clone();
        let ticket = self
            .browser
            .playlist_selection_mut()
            .navigation_projection
            .ticket();
        let i18n = std::sync::Arc::clone(&self.i18n);
        let filter = self.browser.playlists().entries().query.clone();
        cx.spawn(async move |this, cx| {
            let result: Result<Option<Vec<EntryIdentity>>, String> = async {
                let mut pages = EntryRangePages::new(job.start, job.end);
                while let Some((offset, limit)) = pages.next() {
                    if !this
                        .update(cx, |this, cx| {
                            this.refresh(cx);
                            this.manual_intent_current(job.intent)
                                && this.entry_commit_valid(&job.commit)
                        })
                        .unwrap_or(false)
                    {
                        return Ok(None);
                    }
                    let handle = handle.clone();
                    let scope = job.commit.scope;
                    let ticket = ticket.clone();
                    let query = filter.clone();
                    let response = crate::gui::query::run(
                        handle,
                        cx,
                        move |handle| async move {
                            handle
                                .query_if_current(
                                    Query::PlaylistEntries {
                                        query,
                                        playlist_id: scope.playlist_id,
                                        offset,
                                        limit,
                                    },
                                    ticket,
                                )
                                .await
                        },
                        |response, cx| {
                            let _ = this.update(cx, |this, cx| {
                                this.refresh(cx);
                                if this.manual_intent_current(job.intent)
                                    && this.playlist_navigation_scope() == Some(scope)
                                    && this.entry_job_timeout(
                                        scope.playlist_id,
                                        generation,
                                        scope.revision,
                                        scope.metadata,
                                        response,
                                        cx,
                                    )
                                {
                                    this.fail_manual_intent(job.intent, cx);
                                }
                            });
                        },
                    )
                    .await;
                    let result = entry_query_result(response, scope, filter.clone())
                        .map_err(|error| error.to_string())?;
                    let Some(ViewResponse::PlaylistEntries(page)) = result else {
                        return Err(i18n.text("gl-playlist-range-changed").into());
                    };
                    if !pages.accept(page) {
                        return Err(i18n.text("gl-playlist-range-changed").into());
                    }
                }
                Ok(Some(pages.identities))
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                this.browser
                    .playlist_selection_mut()
                    .release_range_worker(generation);
                this.refresh(cx);
                if this.manual_intent_current(job.intent)
                    && this.entry_commit_valid(&job.commit)
                    && this
                        .browser
                        .playlist_selection_mut()
                        .finish_read(job.intent, generation)
                {
                    if job.commit.scope.bitrate != this.state.library.bitrate_revision {
                        job.commit.scope.bitrate = this.state.library.bitrate_revision;
                        this.browser
                            .playlist_selection_mut()
                            .queue_range(job.intent, job.commit, job.start, job.end);
                        this.dispatch_playlist_range(cx);
                        this.dispatch_playlist_navigation(cx);
                        cx.notify();
                        return;
                    }
                    match result {
                        Ok(Some(rows)) => {
                            job.commit.rows = Some(rows);
                            this.ready_entry_selection(job.intent, job.commit, cx);
                        }
                        Err(message) => {
                            this.entry_job_failed(
                                job.commit.scope.playlist_id,
                                generation,
                                message,
                                cx,
                            );
                            this.fail_manual_intent(job.intent, cx);
                        }
                        Ok(None) => {}
                    }
                }
                this.dispatch_playlist_range(cx);
                this.dispatch_playlist_navigation(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn playlist_navigation_scope(&self) -> Option<EntryNavigationScope> {
        let playlist_id = self.browser.playlists().selected()?;
        (self.browser.playlists().entries().playlist_id == Some(playlist_id)).then(|| {
            EntryNavigationScope {
                playlist_id,
                revision: self.playlist_revision(playlist_id),
                metadata: self
                    .state
                    .library
                    .metadata_revisions
                    .playlist_revision(playlist_id, self.state.library.revision),
                bitrate: self.state.library.bitrate_revision,
            }
        })
    }

    fn dispatch_playlist_navigation(&mut self, cx: &mut Context<Self>) {
        let Some(intent) = self.browser.playlists().track_selection.intent else {
            return;
        };
        if !self.manual_intent_current(intent) {
            return;
        }
        let Some(job) = self.browser.playlist_selection_mut().take_navigation() else {
            return;
        };
        let handle = self.context.handle().clone();
        let ticket = self
            .browser
            .playlist_selection_mut()
            .navigation_projection
            .ticket();
        let query = self.browser.playlists().entries().query.clone();
        cx.spawn(async move |this, cx| {
            let response = crate::gui::query::run(
                handle,
                cx,
                move |handle| async move {
                    handle
                        .query_if_current(
                            Query::PlaylistEntries {
                                query,
                                playlist_id: job.scope.playlist_id,
                                offset: job.offset,
                                limit: 1,
                            },
                            ticket,
                        )
                        .await
                },
                |response, cx| {
                    let _ = this.update(cx, |this, cx| {
                        this.refresh(cx);
                        if this.manual_intent_current(intent)
                            && this.playlist_navigation_scope() == Some(job.scope)
                            && this.entry_job_timeout(
                                job.scope.playlist_id,
                                job.generation,
                                job.scope.revision,
                                job.scope.metadata,
                                response,
                                cx,
                            )
                        {
                            this.fail_manual_intent(intent, cx);
                        }
                    });
                },
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                let scope = this.playlist_navigation_scope();
                let effect = this.browser.playlist_selection_mut().finish_navigation(
                    this.i18n.text("gl-playlist-navigation-response"),
                    job,
                    scope,
                    response,
                );
                if this.manual_intent_current(intent) {
                    match effect {
                        EntryNavigationEffect::None => {}
                        EntryNavigationEffect::Reload => {
                            this.browser
                                .playlist_selection_mut()
                                .finish_read(intent, job.generation);
                            this.fail_manual_intent(intent, cx);
                            this.report_error(
                                this.i18n.text("gl-playlist-changed-select").into(),
                                cx,
                            );
                        }
                        EntryNavigationEffect::Failed(message) => {
                            this.fail_manual_intent(intent, cx);
                            this.report_error(message, cx);
                        }
                        EntryNavigationEffect::Selected { target, range } => {
                            let commit = EntryCommit {
                                scope: job.scope,
                                generation: job.generation,
                                target,
                                anchor: range.as_ref().map(|range| range.anchor),
                                rows: None,
                                multi: range.as_ref().map_or(job.multi, |range| range.additive),
                                scroll: true,
                            };
                            if let Some(range) = range {
                                this.queue_playlist_range(
                                    intent,
                                    commit,
                                    range.start,
                                    range.end,
                                    cx,
                                );
                            } else {
                                this.ready_entry_selection(intent, commit, cx);
                            }
                        }
                    }
                }
                this.dispatch_playlist_range(cx);
                this.dispatch_playlist_navigation(cx);
                cx.notify();
            });
        })
        .detach();
    }
    pub(in crate::gui) fn remove_playlist_selection(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
        let Some(playlist_id) = self.browser.playlists().selected() else {
            return;
        };
        let entry_ids = self.browser.playlists().track_selection.ordered_ids();
        if entry_ids.is_empty() {
            return;
        }
        self.send(
            Command::RemovePlaylistEntries {
                playlist_id,
                entry_ids,
                expected_revision: self.playlist_revision(playlist_id),
            },
            cx,
        );
        // Keep identities until the authoritative refresh confirms removal, including command failure.
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn selection_releases_high_water_capacity_and_preserves_toggle_owners() {
        let mut selection = super::EntrySelection::default();
        for id in 0..4096 {
            selection.select(identity(id, id as usize), true);
        }
        let capacity = selection.selected.capacity();
        selection.select(identity(4096, 4096), true);
        assert_eq!(selection.selected.capacity(), capacity);
        selection.select(identity(9, 9), false);
        assert_eq!(selection.len(), 1);
        assert!(selection.selected.capacity() <= 64);
        selection.select(identity(9, 9), true);
        assert_eq!(selection.len(), 0);
        assert_eq!(selection.active, Some(identity(9, 9)));
        assert_eq!(selection.anchor, Some(identity(9, 9)));
        for id in 0..4096 {
            selection.select(identity(id, id as usize), true);
        }
        selection.reconcile(vec![identity(9, 9)]);
        assert_eq!(selection.len(), 1);
        assert!(selection.selected.capacity() <= 64);
        selection.clear();
        assert_eq!(selection.len(), 0);
        assert!(selection.selected.capacity() <= 64);
    }

    use super::{
        EntryIdentity, EntryNavigationEffect, EntryNavigationJob, EntryNavigationRange,
        EntryNavigationScope, EntryRangePages, EntrySelection, EntryTarget,
        PlaylistSelectionRequest,
    };
    use rivu_protocol::{
        model::{MAX_QUERY_ROWS, Page, PlaylistEntryRow},
        response::{QueryError, QueryResponse, QueryRevisions, ViewResponse},
    };

    #[test]
    fn repeated_tracks_remain_independent_ordered_occurrences() {
        let mut selection = EntrySelection::default();
        let first = EntryIdentity {
            id: 91,
            track_id: 7,
            position: 0,
            offset: 0,
        };
        let second = EntryIdentity {
            id: 12,
            track_id: 7,
            position: 1,
            offset: 1,
        };
        let third = EntryIdentity {
            id: 53,
            track_id: 3,
            position: 2,
            offset: 2,
        };
        selection.select(third, false);
        selection.select(first, true);
        selection.select(second, true);
        assert_eq!(selection.ordered_ids(), [91, 12, 53]);
        assert_eq!(
            selection
                .ordered()
                .iter()
                .map(|row| row.track_id)
                .collect::<Vec<_>>(),
            [7, 7, 3]
        );

        selection.select(first, true);
        assert!(!selection.contains(first.id));
        assert!(selection.contains(second.id));
        assert_eq!(selection.ordered_ids(), [12, 53]);

        selection.reconcile(vec![third, second]);
        assert_eq!(selection.ordered_ids(), [12, 53]);
        assert_eq!(selection.len(), 2);
    }

    fn navigate(
        selection: &mut EntrySelection,
        scope: EntryNavigationScope,
        total: usize,
        down: bool,
        shift: bool,
        multi: bool,
    ) -> bool {
        let Some(prepared) = selection.prepare_request(
            scope,
            total,
            PlaylistSelectionRequest::Navigate { down, shift, multi },
            None,
        ) else {
            return false;
        };
        let EntryTarget::Offset(offset) = prepared.target else {
            panic!("navigation prepares an offset")
        };
        let mut intents = crate::gui::coordination::manual_actions::ManualIntents::default();
        // Match the root boundary: freeze intent before cancellation, then dispatch.
        selection.cancel_manual();
        selection.begin_navigation(
            intents.begin(),
            prepared.scope,
            offset,
            prepared.shift,
            prepared.multi,
            prepared.all,
        );
        true
    }

    #[test]
    fn navigation_after_ready_or_range_uses_desired_row_before_root_cancellation() {
        let scope = navigation_scope();
        for shift in [false, true] {
            let mut selection = EntrySelection::default();
            selection.select(identity(0, 0), false);
            assert!(navigate(&mut selection, scope, 20, true, shift, false));
            let first = selection.take_navigation().unwrap();
            assert!(matches!(
                selection.finish_navigation(
                    "unexpected",
                    first,
                    Some(scope),
                    navigation_response(first, 20)
                ),
                EntryNavigationEffect::Selected { .. }
            ));
            // Ready is not committed yet; Shift also has range work outstanding.
            assert_eq!(selection.active, Some(identity(0, 0)));
            assert!(selection.navigation.is_none());
            assert!(navigate(&mut selection, scope, 20, true, shift, true));
            let latest = selection.take_navigation().unwrap();
            assert_eq!(latest.offset, 2);
            assert_eq!(latest.shift, shift);
            assert!(latest.multi);
            assert_eq!(selection.anchor, Some(identity(0, 0)));
            assert_eq!(selection.len(), 1);
        }
    }

    #[test]
    fn edges_and_select_all_supersede_pending_range_without_releasing_worker() {
        use crate::gui::coordination::manual_actions::ManualIntents;
        let scope = navigation_scope();
        for (request, expected, all) in [
            (PlaylistSelectionRequest::Edge { last: true }, 19, false),
            (PlaylistSelectionRequest::Edge { last: false }, 0, false),
            (PlaylistSelectionRequest::All, 0, true),
        ] {
            let mut intents = ManualIntents::default();
            let mut selection = EntrySelection::default();
            selection.select(identity(3, 3), false);
            let old = intents.begin();
            assert!(
                selection
                    .begin_selection(old, scope, identity(8, 8), true, false)
                    .is_none()
            );
            let running = selection.take_range().unwrap();
            let prepared = selection.prepare_request(scope, 20, request, None).unwrap();
            let EntryTarget::Offset(offset) = prepared.target else {
                panic!("edges and select-all prepare an offset")
            };
            assert_eq!(offset, expected);
            assert_eq!(prepared.all, all);
            selection.cancel_manual();
            let latest = intents.begin();
            selection.begin_navigation(
                latest,
                prepared.scope,
                offset,
                prepared.shift,
                prepared.multi,
                prepared.all,
            );
            assert!(selection.take_navigation().is_none());
            assert!(!selection.finish_read(old, running.commit.generation));
            assert_eq!(selection.anchor, Some(identity(3, 3)));
            selection.release_range_worker(running.commit.generation);
            let job = selection.take_navigation().unwrap();
            assert_eq!(job.offset, expected);
            assert_eq!(job.all, all);
            assert!(selection.take_navigation().is_none());
        }
    }

    #[test]
    fn navigation_boundary_does_not_cancel_the_current_prepared_target() {
        let scope = navigation_scope();
        let mut selection = EntrySelection::default();
        selection.select(identity(18, 18), false);
        assert!(navigate(&mut selection, scope, 20, true, true, false));
        let running = selection.take_navigation().unwrap();
        assert!(!navigate(&mut selection, scope, 20, true, false, false));
        assert_eq!(selection.navigation_in_flight, Some(running));
        assert_eq!(selection.navigation, Some(running));
        assert_eq!(selection.anchor, Some(identity(18, 18)));
    }

    #[test]
    fn superseded_range_cannot_finish_latest_selection_or_release_its_gate() {
        let mut selection = EntrySelection::default();
        let mut intents = crate::gui::coordination::manual_actions::ManualIntents::default();
        let scope = navigation_scope();
        selection.select(identity(1, 0), false);
        let old = intents.begin();
        assert!(
            selection
                .begin_selection(old, scope, identity(2, 1), true, false)
                .is_none()
        );
        let running = selection.take_range().unwrap();
        selection.cancel();
        let latest = intents.begin();
        selection.begin_navigation(latest, scope, 2, false, false, false);
        assert!(selection.take_navigation().is_none());
        assert!(!selection.finish_read(old, running.commit.generation));
        assert!(selection.busy);
        assert!(selection.contains(1));
        assert_eq!(selection.intent, Some(latest));
        selection.release_range_worker(running.commit.generation);
        assert_eq!(selection.take_navigation().unwrap().offset, 2);
    }

    #[test]
    fn navigation_timeout_stops_loading_but_holds_gate_until_final_completion() {
        let mut selection = EntrySelection::default();
        let scope = navigation_scope();
        assert!(navigate(&mut selection, scope, 10, true, false, false));
        let running = selection.take_navigation().unwrap();
        assert!(selection.timeout(running.generation, "timeout".into()));
        assert!(!selection.busy);
        assert_eq!(selection.error.as_deref(), Some("timeout"));
        assert_eq!(selection.navigation_in_flight, Some(running));
        selection.select(identity(1, 0), false);
        assert!(navigate(&mut selection, scope, 10, true, false, false));
        let latest = selection.navigation.unwrap();
        assert!(!selection.timeout(running.generation, "obsolete timeout".into()));
        assert!(selection.take_navigation().is_none());
        assert_eq!(
            selection.finish_navigation(
                "unexpected",
                running,
                Some(scope),
                QueryResponse {
                    revisions: scope.revisions(),
                    result: Err(QueryError::Message("timeout".into())),
                },
            ),
            EntryNavigationEffect::None
        );
        assert!(selection.busy);
        assert!(selection.error.is_none());
        assert_eq!(selection.take_navigation(), Some(latest));
    }

    #[test]
    fn range_timeout_preserves_selection_and_blocks_latest_until_retirement() {
        let mut selection = EntrySelection::default();
        let mut intents = crate::gui::coordination::manual_actions::ManualIntents::default();
        let scope = navigation_scope();
        selection.select(identity(1, 0), false);
        let intent = intents.begin();
        assert!(
            selection
                .begin_selection(intent, scope, identity(2, 1), true, false)
                .is_none()
        );
        let running = selection.take_range().unwrap();
        let generation = running.commit.generation;
        assert!(selection.timeout(generation, "timeout".into()));
        assert!(!selection.busy);
        assert!(selection.contains(1));
        assert_eq!(selection.error.as_deref(), Some("timeout"));
        assert_eq!(selection.range_in_flight, Some(generation));
        selection.begin_navigation(intents.begin(), scope, 3, false, false, false);
        assert!(!selection.timeout(generation, "obsolete".into()));
        assert!(!selection.finish_read(intent, generation));
        assert!(selection.take_navigation().is_none());
        selection.release_range_worker(generation);
        assert!(selection.busy);
        assert!(selection.error.is_none());
        assert_eq!(selection.take_navigation().unwrap().offset, 3);
    }

    #[test]
    fn refresh_timeout_retains_gate_and_coalesces_replacement_refresh() {
        let mut selection = EntrySelection::default();
        selection.select(identity(1, 0), false);
        selection.request_refresh();
        let (_, generation, _) = selection.prepare_refresh().unwrap();
        assert!(selection.timeout(generation, "timeout".into()));
        assert!(!selection.busy);
        assert!(selection.refresh_in_flight);
        assert_eq!(selection.error.as_deref(), Some("timeout"));
        selection.request_refresh();
        selection.request_refresh();
        assert!(!selection.timeout(generation, "obsolete".into()));
        assert!(selection.prepare_refresh().is_none());
        assert!(selection.retire_refresh());
        let (_, latest, _) = selection.prepare_refresh().unwrap();
        assert_ne!(generation, latest);
        assert!(selection.refresh_in_flight);
        assert!(selection.prepare_refresh().is_none());
    }

    fn identity(id: i64, position: usize) -> EntryIdentity {
        EntryIdentity {
            id,
            track_id: id + 100,
            position,
            offset: position,
        }
    }
    #[test]
    fn entry_projection_changes_queue_one_refresh_without_releasing_the_running_worker() {
        let mut selection = EntrySelection::default();
        let mut first = identity(1, 0);
        first.track_id = 100;
        let mut duplicate = identity(2, 1);
        duplicate.track_id = 100;
        selection.select(first, false);
        selection.select(duplicate, true);
        let revisions = QueryRevisions::default();
        assert!(selection.sync_projection(5, None, revisions));
        let (ids, old_generation, ticket) = selection.prepare_refresh().unwrap();
        assert_eq!(ids.len(), 2);
        for query in ["first", "second", "latest"] {
            assert!(selection.sync_projection(5, Some(query.into()), revisions));
            assert!(selection.prepare_refresh().is_none());
            assert!(selection.refresh_in_flight);
            assert!(!selection.current(old_generation));
        }
        assert!(!ticket.is_current());
        assert!(selection.retire_refresh());
        let (ids, generation, _) = selection.prepare_refresh().unwrap();
        assert_eq!(ids.len(), 2);
        assert_ne!(generation, old_generation);
        assert!(!selection.retire_refresh());
        assert!(selection.prepare_refresh().is_none());
        selection.request_refresh();
        selection.clear();
        assert!(selection.prepare_refresh().is_none());
    }

    fn page(offset: usize, count: usize, total: usize) -> Page<PlaylistEntryRow> {
        Page {
            total,
            rows: (offset..offset + count)
                .map(|position| PlaylistEntryRow {
                    id: position as i64,
                    track_id: position as i64 + 100,
                    position,
                    offset: position,
                    title: String::new(),
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
                })
                .collect(),
        }
    }
    fn navigation_scope() -> EntryNavigationScope {
        EntryNavigationScope {
            playlist_id: 9,
            revision: 12,
            metadata: 6,
            bitrate: 0,
        }
    }
    fn navigation_response(job: EntryNavigationJob, total: usize) -> QueryResponse {
        QueryResponse {
            revisions: job.scope.revisions(),
            result: Ok(ViewResponse::PlaylistEntries(page(job.offset, 1, total))),
        }
    }
    fn navigation_error(message: &str) -> QueryResponse {
        QueryResponse {
            revisions: QueryRevisions::default(),
            result: Err(QueryError::Message(message.into())),
        }
    }

    #[test]
    fn bitrate_navigation_retries_latest_payload_without_losing_selection() {
        let scope = navigation_scope();
        let current = EntryNavigationScope {
            bitrate: 1,
            ..scope
        };
        let mut selection = EntrySelection::default();
        selection.select(identity(0, 0), false);
        assert!(navigate(&mut selection, scope, 20, true, false, false));
        let old = selection.take_navigation().unwrap();
        assert!(selection.take_navigation().is_none());
        assert_eq!(
            selection.finish_navigation(
                "Unexpected playlist navigation response",
                old,
                Some(current),
                navigation_response(old, 20),
            ),
            EntryNavigationEffect::None,
        );
        assert_eq!(selection.active, Some(identity(0, 0)));
        assert!(selection.contains(0));
        let retry = selection.take_navigation().unwrap();
        assert_eq!(retry.scope, current);
        assert!(selection.take_navigation().is_none());
        assert!(matches!(
            selection.finish_navigation(
                "Unexpected playlist navigation response",
                retry,
                Some(current),
                navigation_response(retry, 20),
            ),
            EntryNavigationEffect::Selected { target, .. } if target == identity(1, 1)
        ));
    }

    #[test]
    fn filtered_range_keeps_original_positions_and_matching_offsets() {
        let mut result = page(0, 3, 3);
        for (row, position) in result.rows.iter_mut().zip([4, 40, 400]) {
            row.position = position;
        }
        let mut range = EntryRangePages::new(0, 3);
        assert!(range.accept(result));
        assert_eq!(
            range
                .identities
                .iter()
                .map(|row| (row.position, row.offset))
                .collect::<Vec<_>>(),
            [(4, 0), (40, 1), (400, 2)]
        );
    }

    #[test]
    fn filtered_navigation_advances_by_matching_offset_not_occurrence_position() {
        let scope = navigation_scope();
        let mut selection = EntrySelection {
            query: Some("needle".into()),
            ..Default::default()
        };
        selection.select(
            EntryIdentity {
                id: 5,
                track_id: 105,
                position: 400,
                offset: 1,
            },
            false,
        );
        assert!(navigate(&mut selection, scope, 3, true, false, false));
        assert_eq!(selection.navigation.unwrap().offset, 2);
    }

    #[test]
    fn metadata_only_navigation_change_retries_without_losing_selection_or_readiness() {
        use crate::gui::coordination::manual_actions::ManualIntents;
        let scope = navigation_scope();
        let changed = EntryNavigationScope {
            metadata: scope.metadata + 1,
            ..scope
        };
        for (shift, multi) in [(false, false), (false, true), (true, false), (true, true)] {
            let mut selection = EntrySelection::default();
            let mut intents = ManualIntents::default();
            selection.select(identity(9, 9), false);
            selection.select(identity(0, 0), true);
            assert!(navigate(&mut selection, scope, 20, true, shift, multi));
            let intent = intents.begin();
            selection.intent = Some(intent);
            let stale = selection.take_navigation().unwrap();
            assert_eq!(
                selection.finish_navigation(
                    "Unexpected playlist navigation response",
                    stale,
                    Some(changed),
                    navigation_response(stale, 20)
                ),
                EntryNavigationEffect::None
            );
            assert_eq!(selection.intent, Some(intent));
            assert!(intents.current(intent));
            assert!(selection.current(stale.generation));
            assert_eq!(selection.active, Some(identity(0, 0)));
            assert_eq!(selection.anchor, Some(identity(0, 0)));
            assert!(selection.contains(0) && selection.contains(9));
            assert!(selection.busy);
            assert!(selection.error.is_none());
            let retry = selection.take_navigation().unwrap();
            assert_eq!(
                retry,
                EntryNavigationJob {
                    scope: changed,
                    ..stale
                }
            );
            assert!(selection.take_navigation().is_none());
            let target = identity(1, 1);
            assert_eq!(
                selection.finish_navigation(
                    "Unexpected playlist navigation response",
                    retry,
                    Some(changed),
                    navigation_response(retry, 20)
                ),
                EntryNavigationEffect::Selected {
                    target,
                    range: shift.then_some(EntryNavigationRange {
                        start: 0,
                        end: 2,
                        anchor: identity(0, 0),
                        additive: multi,
                    }),
                }
            );
            if shift {
                assert!(selection.complete_range(
                    retry.generation,
                    Some(identity(0, 0)),
                    target,
                    vec![identity(0, 0), target],
                    multi,
                ));
            } else {
                selection.select(target, multi);
            }
            assert_eq!(
                selection.ordered_ids(),
                match (shift, multi) {
                    (false, false) => vec![1],
                    (true, false) => vec![0, 1],
                    (_, true) => vec![0, 1, 9],
                }
            );
            assert_eq!(selection.active, Some(target));
            assert!(selection.ready());
            assert!(navigate(&mut selection, changed, 20, true, false, false));
            assert_eq!(selection.take_navigation().unwrap().offset, 2);
        }
    }

    #[test]
    fn metadata_only_change_preserves_newer_navigation_target_intent_and_worker_gate() {
        use crate::gui::coordination::manual_actions::ManualIntents;
        let scope = navigation_scope();
        let changed = EntryNavigationScope {
            metadata: scope.metadata + 1,
            ..scope
        };
        let mut selection = EntrySelection::default();
        let mut intents = ManualIntents::default();
        selection.select(identity(0, 0), false);
        assert!(navigate(&mut selection, scope, 20, true, false, false));
        selection.intent = Some(intents.begin());
        let stale = selection.take_navigation().unwrap();
        assert!(navigate(&mut selection, changed, 20, true, true, true));
        let intent = intents.begin();
        selection.intent = Some(intent);
        let generation = selection.generation;
        assert_eq!(selection.navigation_in_flight, Some(stale));
        assert!(selection.take_navigation().is_none());
        assert_eq!(
            selection.finish_navigation(
                "Unexpected playlist navigation response",
                stale,
                Some(changed),
                navigation_error("obsolete worker")
            ),
            EntryNavigationEffect::None
        );
        assert_eq!(selection.intent, Some(intent));
        assert!(intents.current(intent));
        assert!(selection.current(generation));
        assert!(selection.error.is_none());
        let latest = selection.take_navigation().unwrap();
        assert_eq!(latest.scope, changed);
        assert_eq!(latest.offset, 2);
        assert!(latest.shift && latest.multi);
        assert_eq!(
            selection.finish_navigation(
                "Unexpected playlist navigation response",
                latest,
                Some(changed),
                navigation_response(latest, 20)
            ),
            EntryNavigationEffect::Selected {
                target: identity(2, 2),
                range: Some(EntryNavigationRange {
                    start: 0,
                    end: 3,
                    anchor: identity(0, 0),
                    additive: true,
                }),
            }
        );
    }

    #[test]
    fn navigation_burst_dispatches_only_first_and_latest_target() {
        let scope = navigation_scope();
        let mut selection = EntrySelection::default();
        selection.select(identity(0, 0), false);
        assert!(navigate(&mut selection, scope, 1_000, true, false, false));
        let first = selection.take_navigation().unwrap();
        assert_eq!(first.offset, 1);
        for _ in 0..254 {
            assert!(navigate(&mut selection, scope, 1_000, true, false, false));
            assert!(selection.take_navigation().is_none());
        }
        assert_eq!(selection.active, Some(identity(0, 0)));
        assert_eq!(selection.anchor, Some(identity(0, 0)));
        assert_eq!(selection.len(), 1);
        assert!(selection.ordered_ids().is_empty());
        assert_eq!(
            selection.finish_navigation(
                "Unexpected playlist navigation response",
                first,
                Some(scope),
                navigation_response(first, 1_000)
            ),
            EntryNavigationEffect::None
        );
        assert_eq!(selection.active, Some(identity(0, 0)));
        assert!(selection.error.is_none());
        let latest = selection.take_navigation().unwrap();
        assert_eq!(latest.offset, 255);
        assert!(selection.take_navigation().is_none());
        assert_eq!(
            selection.finish_navigation(
                "Unexpected playlist navigation response",
                latest,
                Some(scope),
                navigation_response(latest, 1_000)
            ),
            EntryNavigationEffect::Selected {
                target: identity(255, 255),
                range: None,
            }
        );
        assert_eq!(selection.active, Some(identity(0, 0)));
        assert_eq!(selection.anchor, Some(identity(0, 0)));
        assert!(selection.contains(0));
        assert!(!selection.contains(255));
        selection.cancel();
        assert_eq!(selection.ordered_ids(), vec![0]);
        selection.select(identity(255, 255), false);
        assert!(selection.ready());
        assert_eq!(selection.ordered_ids(), vec![255]);
        assert_eq!(selection.drag_ids(255), vec![255]);
        assert!(selection.take_navigation().is_none());
    }

    #[test]
    fn navigation_boundaries_do_not_dispatch_duplicate_queries() {
        let scope = navigation_scope();
        let mut selection = EntrySelection::default();
        assert!(!navigate(&mut selection, scope, 0, true, false, false));
        assert!(selection.take_navigation().is_none());
        assert!(navigate(&mut selection, scope, 3, true, false, false));
        let first = selection.take_navigation().unwrap();
        assert_eq!(first.offset, 0);
        selection.finish_navigation(
            "Unexpected playlist navigation response",
            first,
            Some(scope),
            navigation_response(first, 3),
        );
        selection.select(identity(0, 0), false);
        let generation = selection.generation;
        for _ in 0..100 {
            assert!(!navigate(&mut selection, scope, 3, false, true, true));
            assert!(selection.take_navigation().is_none());
        }
        assert_eq!(selection.generation, generation);
        assert_eq!(selection.ordered_ids(), vec![0]);
        assert!(navigate(&mut selection, scope, 3, true, false, false));
        let first = selection.take_navigation().unwrap();
        assert!(navigate(&mut selection, scope, 3, true, false, false));
        for _ in 0..100 {
            assert!(!navigate(&mut selection, scope, 3, true, true, true));
            assert!(selection.take_navigation().is_none());
        }
        assert_eq!(
            selection.finish_navigation(
                "Unexpected playlist navigation response",
                first,
                Some(scope),
                navigation_response(first, 3)
            ),
            EntryNavigationEffect::None
        );
        let last = selection.take_navigation().unwrap();
        assert_eq!(last.offset, 2);
        for _ in 0..100 {
            assert!(!navigate(&mut selection, scope, 3, true, false, false));
            assert!(selection.take_navigation().is_none());
        }
        selection.finish_navigation(
            "Unexpected playlist navigation response",
            last,
            Some(scope),
            navigation_response(last, 3),
        );
        selection.select(identity(2, 2), false);
        assert_eq!(selection.ordered_ids(), vec![2]);
        assert!(!navigate(&mut selection, scope, 3, true, false, false));
        assert!(selection.take_navigation().is_none());
        selection.clear();
        assert!(navigate(&mut selection, scope, 3, false, false, false));
        assert_eq!(selection.take_navigation().unwrap().offset, 2);
    }

    #[test]
    fn navigation_uses_latest_modifiers_and_preserves_shift_anchor_and_additive_selection() {
        let scope = navigation_scope();
        for (shift, multi) in [(false, false), (false, true), (true, false), (true, true)] {
            let mut selection = EntrySelection::default();
            selection.select(identity(9, 9), false);
            selection.select(identity(2, 2), true);
            assert!(navigate(&mut selection, scope, 20, true, !shift, !multi));
            let first = selection.take_navigation().unwrap();
            assert!(navigate(&mut selection, scope, 20, true, shift, multi));
            assert_eq!(
                selection.finish_navigation(
                    "Unexpected playlist navigation response",
                    first,
                    Some(scope),
                    navigation_error("superseded")
                ),
                EntryNavigationEffect::None
            );
            assert!(selection.error.is_none());
            let latest = selection.take_navigation().unwrap();
            let effect = selection.finish_navigation(
                "Unexpected playlist navigation response",
                latest,
                Some(scope),
                navigation_response(latest, 20),
            );
            let target = identity(4, 4);
            if shift {
                let range = EntryNavigationRange {
                    start: 2,
                    end: 5,
                    anchor: identity(2, 2),
                    additive: multi,
                };
                assert_eq!(
                    effect,
                    EntryNavigationEffect::Selected {
                        target,
                        range: Some(range),
                    }
                );
                assert_eq!(selection.active, Some(identity(2, 2)));
                assert_eq!(selection.len(), 2);
                assert!(selection.busy);
                assert!(selection.complete_range(
                    selection.generation,
                    Some(identity(2, 2)),
                    target,
                    vec![identity(2, 2), identity(3, 3), target],
                    multi,
                ));
                assert_eq!(
                    selection.ordered_ids(),
                    if multi {
                        vec![2, 3, 4, 9]
                    } else {
                        vec![2, 3, 4]
                    }
                );
                assert_eq!(selection.anchor, Some(identity(2, 2)));
            } else {
                assert_eq!(
                    effect,
                    EntryNavigationEffect::Selected {
                        target,
                        range: None
                    }
                );
                assert_eq!(selection.active, Some(identity(2, 2)));
                selection.select(target, false);
                assert_eq!(selection.ordered_ids(), vec![4]);
                assert_eq!(selection.anchor, Some(target));
            }
            assert_eq!(selection.active, Some(target));
            assert!(selection.ready());
            assert!(selection.take_navigation().is_none());
        }
    }

    #[test]
    fn cancelled_navigation_releases_worker_without_replacing_newer_selection() {
        let scope = navigation_scope();
        let mut selection = EntrySelection::default();
        selection.select(identity(0, 0), false);
        assert!(navigate(&mut selection, scope, 10, true, false, false));
        let old = selection.take_navigation().unwrap();
        selection.cancel();
        assert!(navigate(&mut selection, scope, 10, true, true, true));
        let generation = selection.generation;
        assert!(selection.take_navigation().is_none());
        assert_eq!(
            selection.finish_navigation(
                "Unexpected playlist navigation response",
                old,
                Some(scope),
                navigation_response(old, 10)
            ),
            EntryNavigationEffect::None
        );
        assert!(selection.current(generation));
        assert_eq!(selection.active, Some(identity(0, 0)));
        let next = selection.take_navigation().unwrap();
        assert!(next.shift && next.multi);
        assert_eq!(next.offset, 1);
    }

    #[test]
    fn range_and_navigation_share_one_worker_even_after_cancellation() {
        use super::{EntryCommit, EntryRangeRequest};
        use crate::gui::coordination::manual_actions::ManualIntents;
        let scope = navigation_scope();
        let mut intents = ManualIntents::default();
        let mut selection = EntrySelection::default();
        selection.select(identity(0, 0), false);
        assert!(navigate(&mut selection, scope, 10, true, false, false));
        let old = selection.take_navigation().unwrap();
        selection.cancel();
        let intent = intents.begin();
        let generation = selection.generation;
        selection.range_request = Some(EntryRangeRequest {
            intent,
            start: 0,
            end: 5,
            commit: EntryCommit {
                scope,
                generation,
                target: identity(4, 4),
                anchor: Some(identity(0, 0)),
                rows: None,
                multi: false,
                scroll: true,
            },
        });
        assert!(selection.take_range().is_none());
        assert_eq!(
            selection.finish_navigation(
                "Unexpected playlist navigation response",
                old,
                Some(scope),
                navigation_response(old, 10)
            ),
            EntryNavigationEffect::None
        );
        let range = selection.take_range().unwrap();
        assert_eq!(range.commit.target, identity(4, 4));
        selection.cancel();
        assert!(navigate(&mut selection, scope, 10, true, false, false));
        assert!(selection.take_navigation().is_none());
        assert_eq!(selection.active, Some(identity(0, 0)));
        selection.release_range_worker(generation);
        assert!(selection.take_navigation().is_some());
        assert_eq!(selection.anchor, Some(identity(0, 0)));
    }

    #[test]
    fn select_all_navigation_establishes_first_occurrence_before_atomic_range_publication() {
        let scope = navigation_scope();
        let mut selection = EntrySelection::default();
        let mut intents = crate::gui::coordination::manual_actions::ManualIntents::default();
        assert!(!selection.can_select_all(0));
        assert!(selection.can_select_all(5));
        selection.begin_navigation(intents.begin(), scope, 0, false, false, true);
        assert!(selection.can_select_all(5));
        let job = selection.take_navigation().unwrap();
        assert_eq!(job.offset, 0);
        assert_eq!(
            selection.finish_navigation(
                "Unexpected playlist navigation response",
                job,
                Some(scope),
                navigation_response(job, 5)
            ),
            EntryNavigationEffect::Selected {
                target: identity(0, 0),
                range: Some(EntryNavigationRange {
                    start: 0,
                    end: 5,
                    anchor: identity(0, 0),
                    additive: false,
                }),
            }
        );
        assert_eq!(selection.len(), 0);
        assert!(selection.busy);
        assert!(selection.complete_range(
            selection.generation,
            Some(identity(0, 0)),
            identity(0, 0),
            (0..5).map(|index| identity(index as i64, index)).collect(),
            false,
        ));
        assert_eq!(selection.ordered_ids(), vec![0, 1, 2, 3, 4]);
        assert_eq!(selection.active, Some(identity(0, 0)));
        assert_eq!(selection.anchor, Some(identity(0, 0)));
        assert!(selection.ready());
    }

    #[test]
    fn cancel_switch_back_and_revision_changes_keep_physical_navigation_gate_until_completion() {
        for transition in 0..5 {
            let mut scope = navigation_scope();
            let mut selection = EntrySelection::default();
            selection.select(identity(0, 0), false);
            assert!(navigate(&mut selection, scope, 10, true, false, false));
            let old = selection.take_navigation().unwrap();
            assert!(navigate(&mut selection, scope, 10, true, false, false));
            let base = match transition {
                0 => {
                    selection.cancel();
                    selection.select(identity(4, 4), false);
                    identity(4, 4)
                }
                1 => {
                    selection.clear();
                    let other = EntryNavigationScope {
                        playlist_id: 10,
                        ..scope
                    };
                    assert!(navigate(&mut selection, other, 10, true, false, false));
                    assert!(selection.take_navigation().is_none());
                    selection.clear();
                    selection.select(identity(4, 4), false);
                    identity(4, 4)
                }
                2..=3 => {
                    selection.invalidate();
                    assert!(selection.busy);
                    match transition {
                        2 => scope.revision += 1,
                        _ => scope.metadata += 1,
                    }
                    selection.reconcile(vec![identity(0, 3)]);
                    identity(0, 3)
                }
                _ => {
                    selection.clear();
                    scope.playlist_id = 10;
                    selection.select(identity(4, 4), false);
                    identity(4, 4)
                }
            };
            assert!(selection.navigation.is_none());
            assert_eq!(selection.navigation_in_flight, Some(old));
            assert!(navigate(&mut selection, scope, 10, true, false, false));
            assert!(selection.take_navigation().is_none());
            assert_eq!(
                selection.finish_navigation(
                    "Unexpected playlist navigation response",
                    old,
                    Some(scope),
                    navigation_error("old worker")
                ),
                EntryNavigationEffect::None
            );
            assert!(selection.error.is_none());
            assert_eq!(selection.active, Some(base));
            let latest = selection.take_navigation().unwrap();
            assert_eq!(latest.scope, scope);
            assert_eq!(latest.offset, base.position + 1);
            assert!(selection.take_navigation().is_none());
            selection.finish_navigation(
                "Unexpected playlist navigation response",
                latest,
                Some(scope),
                navigation_response(latest, 10),
            );
            selection.select(
                identity((base.position + 1) as i64, base.position + 1),
                false,
            );
            assert_eq!(selection.ordered_ids(), vec![(base.position + 1) as i64]);
            assert!(selection.ready());
            assert!(selection.navigation_in_flight.is_none());
        }
    }

    #[test]
    fn changed_navigation_scope_discards_error_and_invalidates_latest_intent() {
        let scope = navigation_scope();
        let mut selection = EntrySelection::default();
        selection.select(identity(0, 0), false);
        assert!(navigate(&mut selection, scope, 10, true, false, false));
        let job = selection.take_navigation().unwrap();
        let changed = EntryNavigationScope {
            revision: scope.revision + 1,
            ..scope
        };
        assert_eq!(
            selection.finish_navigation(
                "Unexpected playlist navigation response",
                job,
                Some(changed),
                navigation_error("stale")
            ),
            EntryNavigationEffect::None
        );
        assert!(selection.navigation.is_none());
        assert!(selection.take_navigation().is_none());
        assert!(selection.error.is_none());
        assert_eq!(selection.active, Some(identity(0, 0)));
        assert!(selection.busy);
        selection.reconcile(vec![identity(0, 2)]);
        assert_eq!(selection.ordered_ids(), vec![0]);
    }

    #[test]
    fn repeated_navigation_failures_require_explicit_retry_and_preserve_unselected_anchor() {
        let scope = navigation_scope();
        let mut selection = EntrySelection::default();
        selection.select(identity(7, 18), false);
        selection.select(identity(8, 19), true);
        selection.select(identity(8, 19), true);
        for scoped in [false, true, false] {
            assert!(navigate(&mut selection, scope, 30, true, false, false));
            let job = selection.take_navigation().unwrap();
            let mut response = if scoped {
                navigation_response(job, 30)
            } else {
                navigation_error("unavailable")
            };
            response.result = Err(QueryError::Message("unavailable".into()));
            assert_eq!(
                selection.finish_navigation(
                    "Unexpected playlist navigation response",
                    job,
                    Some(scope),
                    response
                ),
                EntryNavigationEffect::Failed("unavailable".into())
            );
            assert!(!selection.busy);
            assert!(!selection.ready());
            assert!(selection.navigation.is_none());
            assert!(selection.navigation_in_flight.is_none());
            assert_eq!(selection.active, Some(identity(8, 19)));
            assert_eq!(selection.anchor, Some(identity(8, 19)));
            assert!(selection.contains(7));
            assert!(!selection.contains(8));
            assert!(selection.ordered_ids().is_empty());
            assert!(selection.drag_ids(7).is_empty());
            for _ in 0..100 {
                assert!(!navigate(&mut selection, scope, 30, true, false, false));
                assert!(selection.take_navigation().is_none());
            }
            let mut ids = selection.ids();
            ids.sort_unstable();
            assert_eq!(ids, vec![7, 8]);
            selection.invalidate();
            assert_eq!(selection.active, Some(identity(8, 19)));
            selection.reconcile(vec![identity(7, 18), identity(8, 19)]);
            assert!(selection.ready());
            assert_eq!(selection.ordered_ids(), vec![7]);
        }
        assert!(navigate(&mut selection, scope, 30, true, false, false));
        let job = selection.take_navigation().unwrap();
        selection.finish_navigation(
            "Unexpected playlist navigation response",
            job,
            Some(scope),
            navigation_response(job, 30),
        );
        selection.select(identity(20, 20), false);
        assert_eq!(selection.ordered_ids(), vec![20]);
        assert!(selection.ready());
    }

    #[test]
    fn failed_navigation_without_identities_retries_to_ready_empty_and_can_navigate_again() {
        let scope = navigation_scope();
        let mut selection = EntrySelection::default();
        assert!(navigate(&mut selection, scope, 3, true, false, false));
        let job = selection.take_navigation().unwrap();
        assert_eq!(
            selection.finish_navigation(
                "Unexpected playlist navigation response",
                job,
                Some(scope),
                navigation_error("unavailable")
            ),
            EntryNavigationEffect::Failed("unavailable".into())
        );
        selection.invalidate();
        assert!(selection.ids().is_empty());
        assert!(selection.ready());
        assert!(navigate(&mut selection, scope, 3, true, false, false));
        let job = selection.take_navigation().unwrap();
        selection.finish_navigation(
            "Unexpected playlist navigation response",
            job,
            Some(scope),
            navigation_response(job, 3),
        );
        selection.select(identity(0, 0), false);
        assert_eq!(selection.ordered_ids(), vec![0]);
        assert!(selection.ready());
    }

    #[test]
    fn rejected_navigation_read_settles_only_its_current_intent() {
        let mut selection = EntrySelection::default();
        let mut intents = crate::gui::coordination::manual_actions::ManualIntents::default();
        let scope = navigation_scope();
        let old = intents.begin();
        selection.begin_navigation(old, scope, 0, false, false, false);
        let generation = selection.generation;
        let latest = intents.begin();
        selection.begin_navigation(latest, scope, 1, false, false, false);
        assert!(!selection.finish_read(old, generation));
        assert!(selection.busy);
        assert!(selection.finish_read(latest, selection.generation));
        assert!(!selection.busy);
    }

    #[test]
    fn navigation_success_with_stale_revisions_never_publishes_target() {
        let scope = navigation_scope();
        for stale_domain in 0..6 {
            let mut selection = EntrySelection::default();
            selection.select(identity(0, 0), false);
            assert!(navigate(&mut selection, scope, 10, true, false, false));
            let job = selection.take_navigation().unwrap();
            let mut response = navigation_response(job, 10);
            match stale_domain {
                0 => response.revisions.playlist_entries = None,
                1 => {
                    response
                        .revisions
                        .playlist_entries
                        .as_mut()
                        .unwrap()
                        .playlist_id += 1
                }
                2 => {
                    response
                        .revisions
                        .playlist_entries
                        .as_mut()
                        .unwrap()
                        .revision += 1
                }
                3 => response.revisions.playlist_metadata = None,
                4 => {
                    response
                        .revisions
                        .playlist_metadata
                        .as_mut()
                        .unwrap()
                        .playlist_id += 1
                }
                _ => {
                    response
                        .revisions
                        .playlist_metadata
                        .as_mut()
                        .unwrap()
                        .revision += 1
                }
            }
            assert_eq!(
                selection.finish_navigation(
                    "Unexpected playlist navigation response",
                    job,
                    Some(scope),
                    response
                ),
                EntryNavigationEffect::Reload
            );
            assert!(selection.error.is_none());
            assert_eq!(selection.active, Some(identity(0, 0)));
            assert_eq!(selection.len(), 1);
            assert!(selection.busy);
            assert!(selection.take_navigation().is_none());
            selection.invalidate();
            selection.reconcile(vec![identity(0, 2)]);
            assert_eq!(selection.ordered_ids(), vec![0]);
            assert!(selection.ready());
        }
    }

    #[test]
    fn revision_query_error_and_unexpected_navigation_payload_enable_retry_without_reloading() {
        let scope = navigation_scope();
        for unexpected_payload in [false, true] {
            let mut selection = EntrySelection::default();
            selection.select(identity(0, 0), false);
            assert!(navigate(&mut selection, scope, 10, true, false, false));
            let job = selection.take_navigation().unwrap();
            let mut response = navigation_response(job, 10);
            let message = if unexpected_payload {
                response.result = Ok(ViewResponse::Track(None));
                "Unexpected playlist navigation response".to_owned()
            } else {
                response.revisions = QueryRevisions::default();
                response.result = Err(QueryError::RevisionChanged);
                QueryError::RevisionChanged.to_string()
            };
            assert_eq!(
                selection.finish_navigation(
                    "Unexpected playlist navigation response",
                    job,
                    Some(scope),
                    response
                ),
                EntryNavigationEffect::Failed(message.clone())
            );
            assert_eq!(selection.error, Some(message));
            assert!(!selection.busy);
            assert!(selection.navigation.is_none());
            assert_eq!(selection.active, Some(identity(0, 0)));
            assert!(!navigate(&mut selection, scope, 10, true, false, false));
            assert!(selection.take_navigation().is_none());
        }
    }

    #[test]
    fn transport_failure_keeps_selection_until_explicit_retry() {
        use rivu_protocol::response::{QueryError, QueryResponse, QueryRevisions};
        let mut selection = EntrySelection::default();
        selection.select(identity(7, 20), false);
        selection.busy = true;
        let response = QueryResponse {
            revisions: QueryRevisions::default(),
            result: Err(QueryError::Message("core unavailable".into())),
        };
        let error = super::entry_query_result(response, navigation_scope(), None).unwrap_err();
        selection.fail(error.to_string());
        assert!(!selection.busy);
        assert!(selection.error.is_some());
        assert_eq!(selection.active, Some(identity(7, 20)));
        assert!(selection.contains(7));
        assert!(selection.ordered_ids().is_empty());
        selection.invalidate();
        selection.reconcile(vec![identity(7, 18)]);
        assert!(selection.ready());
        assert_eq!(selection.ordered_ids(), vec![7]);
    }

    #[test]
    fn full_rows_require_scoped_metadata_and_occurrences_not_catalog() {
        use rivu_protocol::response::{
            PlaylistEntryRevision, QueryResponse, QueryRevisions, ViewResponse,
        };
        let current = QueryRevisions {
            library: 6,
            playlist: 8,
            playlist_metadata: Some(PlaylistEntryRevision {
                playlist_id: 9,
                revision: 6,
            }),
            playlist_entries: Some(PlaylistEntryRevision {
                playlist_id: 9,
                revision: 12,
            }),
            ..Default::default()
        };
        for revisions in [
            QueryRevisions {
                playlist_metadata: Some(PlaylistEntryRevision {
                    playlist_id: 9,
                    revision: 7,
                }),
                ..current
            },
            QueryRevisions {
                playlist_entries: None,
                ..current
            },
            QueryRevisions {
                playlist_entries: Some(PlaylistEntryRevision {
                    playlist_id: 10,
                    revision: 12,
                }),
                ..current
            },
            QueryRevisions {
                playlist_entries: Some(PlaylistEntryRevision {
                    playlist_id: 9,
                    revision: 13,
                }),
                ..current
            },
        ] {
            let response = QueryResponse {
                revisions,
                result: Ok(ViewResponse::PlaylistEntries(page(0, 1, 1))),
            };
            assert!(
                super::entry_query_result(response, navigation_scope(), None)
                    .unwrap()
                    .is_none()
            );
        }
        let response = QueryResponse {
            revisions: current,
            result: Ok(ViewResponse::PlaylistEntries(page(0, 1, 1))),
        };
        let Some(ViewResponse::PlaylistEntries(page)) =
            super::entry_query_result(response, navigation_scope(), None).unwrap()
        else {
            panic!("current payload rejected");
        };
        assert_eq!(page.rows[0].id, 0);
        let renamed = QueryRevisions {
            playlist: 9,
            library: 7,
            ..current
        };
        assert!(
            super::entry_query_result(
                QueryResponse {
                    revisions: renamed,
                    result: Ok(ViewResponse::PlaylistEntries(super::Page {
                        total: 0,
                        rows: Vec::new()
                    })),
                },
                navigation_scope(),
                None
            )
            .unwrap()
            .is_some()
        );
        let metadata_only = QueryRevisions {
            library: 7,
            playlist: 9,
            ..current
        };
        assert!(
            super::entry_query_result(
                QueryResponse {
                    revisions: metadata_only,
                    result: Ok(ViewResponse::PlaylistEntryLocations(Vec::new())),
                },
                navigation_scope(),
                None
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn control_toggle_keeps_active_and_anchor_separate_from_group_actions() {
        let mut selection = EntrySelection::default();
        selection.select(identity(9, 8), false);
        selection.select(identity(2, 1), true);
        assert_eq!(selection.ordered_ids(), vec![2, 9]);
        selection.select(identity(2, 1), true);
        assert_eq!(selection.ordered_ids(), vec![9]);
        assert_eq!(selection.active, Some(identity(2, 1)));
        assert_eq!(selection.anchor, Some(identity(2, 1)));
        assert_eq!(selection.ids().len(), 2);
        assert_eq!(selection.drag_ids(9), vec![9]);
        assert_eq!(selection.drag_ids(2), vec![2]);
    }

    #[test]
    fn inclusive_crosspage_range_replaces_then_adds_without_moving_anchor() {
        let mut selection = EntrySelection::default();
        let anchor = identity(100, MAX_QUERY_ROWS - 1);
        let target = identity(102, MAX_QUERY_ROWS + 1);
        selection.select(anchor, false);
        let rows = vec![anchor, identity(101, MAX_QUERY_ROWS), target];
        assert!(selection.complete_range(selection.generation, Some(anchor), target, rows, false));
        assert_eq!(selection.ordered_ids(), vec![100, 101, 102]);
        assert_eq!(selection.active, Some(target));
        assert_eq!(selection.anchor, Some(anchor));
        let next = identity(1, 0);
        assert!(selection.complete_range(
            selection.generation,
            Some(anchor),
            next,
            vec![next],
            true
        ));
        assert_eq!(selection.ordered_ids(), vec![1, 100, 101, 102]);
        assert_eq!(selection.anchor, Some(anchor));
        assert_eq!(selection.active, Some(next));
    }

    #[test]
    fn range_pages_bound_metadata_and_publish_complete_absolute_identities() {
        let start = MAX_QUERY_ROWS - 1;
        let end = start + MAX_QUERY_ROWS * 2 + 1;
        let mut pages = EntryRangePages::new(start, end);
        let mut calls = 0;
        while let Some((offset, limit)) = pages.next() {
            assert!(limit <= MAX_QUERY_ROWS);
            assert!(pages.accept(page(offset, limit, end)));
            calls += 1;
        }
        assert_eq!(calls, 3);
        assert_eq!(pages.identities.len(), end - start);
        assert_eq!(pages.identities.first().unwrap().position, start);
        assert_eq!(pages.identities.last().unwrap().position, end - 1);
        let mut selection = EntrySelection::default();
        let active = *pages.identities.last().unwrap();
        assert!(selection.complete_range(
            selection.generation,
            None,
            active,
            pages.identities,
            false
        ));
        assert_eq!(selection.len(), end - start);
        assert_eq!(selection.drag_ids(active.id).len(), end - start);
    }

    #[test]
    fn partial_range_failure_rolls_back_and_retry_restores_group_consumers() {
        let mut selection = EntrySelection::default();
        selection.select(identity(7, 20), false);
        selection.busy = true;
        let mut pages = EntryRangePages::new(0, MAX_QUERY_ROWS + 1);
        assert!(pages.accept(page(0, MAX_QUERY_ROWS, MAX_QUERY_ROWS + 1)));
        assert!(!pages.accept(page(MAX_QUERY_ROWS, 0, MAX_QUERY_ROWS + 1)));
        selection.fail("range failed".into());
        assert_eq!(selection.len(), 1);
        assert!(selection.ordered_ids().is_empty());
        assert!(selection.drag_ids(7).is_empty());
        selection.invalidate();
        selection.reconcile(vec![identity(7, 18)]);
        assert_eq!(selection.ordered_ids(), vec![7]);
        assert_eq!(selection.active, Some(identity(7, 18)));
    }

    #[test]
    fn superseded_range_navigation_revision_and_switch_results_never_publish() {
        let mut selection = EntrySelection::default();
        selection.select(identity(7, 20), false);
        assert!(navigate(
            &mut selection,
            navigation_scope(),
            100,
            true,
            false,
            false
        ));
        let old = selection.generation;
        selection.select(identity(8, 21), false);
        assert!(!selection.complete_range(
            old,
            None,
            identity(9, 22),
            vec![identity(9, 22)],
            false
        ));
        assert_eq!(selection.ordered_ids(), vec![8]);
        let old = selection.generation;
        selection.invalidate();
        assert!(!selection.complete_range(
            old,
            None,
            identity(9, 22),
            vec![identity(9, 22)],
            false
        ));
        selection.reconcile(vec![identity(8, 3)]);
        let old = selection.generation;
        selection.clear();
        selection.select(identity(10, 0), false);
        assert!(!selection.complete_range(old, None, identity(8, 3), vec![identity(8, 3)], false));
        assert_eq!(selection.ordered_ids(), vec![10]);
        assert!(selection.navigation.is_none());
    }

    #[test]
    fn refresh_relocates_compact_ids_removes_missing_and_keeps_source_order() {
        let mut selection = EntrySelection::default();
        selection.select(identity(9, 200_000), false);
        selection.select(identity(2, 5), true);
        selection.select(identity(3, 4), true);
        selection.invalidate();
        assert!(selection.ordered_ids().is_empty());
        selection.reconcile(vec![identity(9, 0), identity(3, 10)]);
        assert_eq!(selection.ordered_ids(), vec![9, 3]);
        assert!(!selection.contains(2));
        assert_eq!(selection.drag_ids(3), vec![9, 3]);
        assert_eq!(selection.drag_ids(4), vec![4]);
        assert_eq!(selection.active, Some(identity(3, 10)));
        assert_eq!(selection.anchor, Some(identity(3, 10)));
    }

    #[test]
    fn truncated_or_oversized_range_page_does_not_advance_or_publish() {
        let mut pages = EntryRangePages::new(10, 20);
        assert!(!pages.accept(page(10, 5, 19)));
        assert!(!pages.accept(page(10, 11, 30)));
        assert_eq!(pages.next(), Some((10, 10)));
        assert!(pages.identities.is_empty());
    }
}
