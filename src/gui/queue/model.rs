use crate::{
    gui::{coordination::manual_actions::ManualIntentId, paged},
    projection::QueryProjection,
};
use rivu_protocol::{
    model::{Query, QueryStamp, QueueRow},
    response::{QueryRevisions, ViewResponse},
};
use std::{collections::HashMap, ops::Range};

pub(in crate::gui) struct QueueCommit {
    pub(super) stamp: QueryStamp,
    pub(super) generation: u64,
    pub(in crate::gui) target: QueueIdentity,
    pub(super) anchor: QueueIdentity,
    pub(super) rows: Option<Vec<QueueIdentity>>,
    pub(super) multi: bool,
    pub(super) scroll: bool,
}

#[derive(Clone, Copy)]
pub(super) enum QueueTarget {
    Row(QueueIdentity),
    Offset(usize),
}

pub(super) struct QueueJob {
    pub(super) intent: ManualIntentId,
    pub(super) generation: u64,
    pub(super) stamp: QueryStamp,
    pub(super) target: QueueTarget,
    pub(super) anchor: Option<QueueIdentity>,
    pub(super) shift: bool,
    pub(super) all: bool,
    pub(super) multi: bool,
    pub(super) scroll: bool,
    pub(super) total: usize,
    pub(super) query: Option<String>,
}

/// Selection retains only occurrence identity and absolute position, never track metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) struct QueueIdentity {
    pub(in crate::gui) id: u64,
    pub(in crate::gui) track_id: i64,
    pub(in crate::gui) position: usize,
    pub(in crate::gui) offset: usize,
}
impl From<&QueueRow> for QueueIdentity {
    fn from(row: &QueueRow) -> Self {
        Self {
            id: row.id,
            track_id: row.track_id,
            position: row.position,
            offset: row.offset,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct QueueLocateRequest {
    pub(super) generation: u64,
    pub(super) stamp: QueryStamp,
    pub(super) id: u64,
}

#[derive(Clone, Copy)]
pub(super) struct QueueLocateReveal {
    pub(super) request: QueueLocateRequest,
    pub(super) position: usize,
    pub(super) scroll: bool,
}

#[derive(Default)]
pub(super) struct QueueLocate {
    pub(super) projection: QueryProjection,
    pub(super) query: Option<Query>,
    pub(super) pending: Option<QueueLocateRequest>,
    pub(super) worker: Option<QueueLocateRequest>,
    pub(super) reveal: Option<QueueLocateReveal>,
    pub(super) emphasized: Option<QueueLocateRequest>,
    pub(super) timer: Option<gpui::Task<()>>,
}

impl QueueLocate {
    pub(super) fn invalidate(&mut self) {
        self.projection.cancel();
        self.query = None;
        self.pending = None;
        self.reveal = None;
        self.emphasized = None;
        self.timer = None;
        // A cancelled lookup still owns the worker gate until its response arrives.
    }

    pub(super) fn begin(
        &mut self,
        id: u64,
        filter: Option<String>,
        revisions: QueryRevisions,
    ) -> QueueLocateRequest {
        self.invalidate();
        let query = Query::QueueEntries {
            query: filter,
            queue_ids: vec![id],
        };
        self.projection.sync(&query, revisions);
        let request = QueueLocateRequest {
            generation: self.projection.generation(),
            stamp: query.stamp(revisions),
            id,
        };
        self.query = Some(query);
        self.pending = Some(request);
        request
    }

    pub(super) fn current(
        &self,
        request: QueueLocateRequest,
        revisions: QueryRevisions,
        current_id: Option<u64>,
    ) -> bool {
        current_id == Some(request.id)
            && self.query.as_ref().is_some_and(|query| {
                self.projection
                    .accepts(query, request.generation, revisions)
                    && query.stamp(revisions) == request.stamp
            })
    }

    pub(super) fn take_job(&mut self) -> Option<QueueLocateRequest> {
        if self.worker.is_some() {
            return None;
        }
        let request = self.pending.take()?;
        self.worker = Some(request);
        Some(request)
    }

    pub(super) fn release_worker(&mut self, request: QueueLocateRequest) {
        if self.worker == Some(request) {
            self.worker = None;
        }
    }

    pub(super) fn resolve(&mut self, request: QueueLocateRequest, position: usize) {
        if !self.current(request, request.stamp.revisions(), Some(request.id)) {
            return;
        }
        self.pending = None;
        self.reveal = Some(QueueLocateReveal {
            request,
            position,
            scroll: true,
        });
    }
}

pub(super) struct QueueReconcileJob {
    pub(super) ids: Vec<u64>,
    pub(super) generation: u64,
    pub(super) stamp: QueryStamp,
    pub(super) query: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum QueueWorker {
    Selection(u64),
    Reconcile(u64),
}

#[derive(Default)]
pub(super) struct QueueSelection {
    pub(super) rows: paged::PagedRows<QueueRow>,
    pub(super) query: Option<String>,
    pub(super) locate_filtered: Option<u64>,
    pub(super) selected: HashMap<u64, QueueIdentity>,
    pub(super) anchor: Option<QueueIdentity>,
    pub(super) page_projection: QueryProjection,
    pub(super) page_worker: bool,
    pub(super) page_size: usize,
    pub(super) projection: QueryProjection,
    pub(super) reconcile_projection: QueryProjection,
    pub(super) reconcile_pending: Option<QueueReconcileJob>,
    pub(super) busy: bool,
    // Retained identities need relocation before their positions can authorize actions.
    positions_stale: bool,
    pub(super) selection_error: Option<String>,
    pub(super) navigation: Option<usize>,
    pub(super) active: Option<QueueIdentity>,
    pub(super) pending: Option<QueueJob>,
    pub(super) worker: Option<QueueWorker>,
    pub(super) manual: Option<ManualIntentId>,
    pub(super) locate: QueueLocate,
}
impl QueueSelection {
    pub(super) fn len(&self) -> usize {
        self.selected.len()
    }
    pub(super) fn contains(&self, id: &u64) -> bool {
        self.selected.contains_key(id)
    }
    pub(super) fn ready(&self) -> bool {
        !self.busy && !self.positions_stale
    }
    pub(super) fn identity(&self, id: u64) -> Option<QueueIdentity> {
        self.ready()
            .then(|| self.selected.get(&id).copied())
            .flatten()
    }
    pub(super) fn ordered(&self) -> Vec<QueueIdentity> {
        if !self.ready() {
            return Vec::new();
        }
        let mut rows: Vec<_> = self.selected.values().copied().collect();
        rows.sort_unstable_by_key(|row| row.position);
        rows
    }
    pub(super) fn ordered_ids(&self) -> Vec<u64> {
        if !self.ready() {
            return Vec::new();
        }
        let mut ids: Vec<_> = self.selected.keys().copied().collect();
        ids.sort_unstable_by_key(|id| self.selected[id].position);
        ids
    }
    pub(super) fn active(&self) -> Option<QueueIdentity> {
        self.active
            .or(self.anchor)
            .and_then(|row| self.identity(row.id))
            .or_else(|| {
                self.ready()
                    .then(|| {
                        self.selected
                            .values()
                            .min_by_key(|row| row.position)
                            .copied()
                    })
                    .flatten()
            })
    }
    pub(in crate::gui) fn cancel(&mut self) {
        self.projection.cancel();
        self.reconcile_projection.cancel();
        self.reconcile_pending = None;
        self.busy = false;
        self.navigation = None;
        self.pending = None;
        self.manual = None;
        self.selection_error = None;
    }
    pub(in crate::gui) fn cancel_manual(&mut self) {
        if self.manual.is_some() {
            self.projection.cancel();
            self.navigation = None;
            self.pending = None;
            self.manual = None;
            // A manual action never owns reconciliation or its failure.
            self.busy = self.positions_stale && self.selection_error.is_none();
            if !self.positions_stale {
                self.selection_error = None;
            }
        }
    }
    pub(super) fn clear(&mut self) {
        self.cancel();
        self.selected.clear();
        self.anchor = None;
        self.active = None;
        self.positions_stale = false;
        self.rows.pin(None);
    }
    pub(super) fn change_search(&mut self, query: Option<String>) {
        self.clear();
        self.query = query;
        self.locate_filtered = None;
        self.rows.clear();
    }
    pub(super) fn select(&mut self, row: QueueIdentity, multi: bool) {
        self.cancel();
        self.positions_stale = false;
        if !multi {
            self.selected.clear();
        }
        if multi && self.selected.remove(&row.id).is_some() {
        } else {
            self.selected.insert(row.id, row);
        }
        self.anchor = Some(row);
        self.active = Some(row);
        self.rows.pin(Some(row.offset));
    }
    pub(super) fn begin_selection(
        &mut self,
        intent: ManualIntentId,
        revisions: QueryRevisions,
        row: QueueIdentity,
        shift: bool,
        multi: bool,
    ) -> Option<QueueCommit> {
        if !self.ready() && (shift || multi) {
            return None;
        }
        self.manual = Some(intent);
        self.navigation = Some(row.offset);
        let stamp = self.query().stamp(revisions);
        let generation = self.projection.generation();
        if shift && self.anchor.is_some() {
            self.busy = true;
            self.pending = Some(QueueJob {
                intent,
                generation,
                stamp,
                target: QueueTarget::Row(row),
                anchor: self.anchor,
                shift,
                all: false,
                multi,
                scroll: false,
                total: self.rows.len(),
                query: self.query.clone(),
            });
            None
        } else {
            Some(QueueCommit {
                stamp,
                generation,
                target: row,
                anchor: row,
                rows: None,
                multi,
                scroll: false,
            })
        }
    }

    pub(super) fn begin_navigation(
        &mut self,
        intent: ManualIntentId,
        revisions: QueryRevisions,
        offset: usize,
        all: bool,
    ) {
        self.manual = Some(intent);
        self.navigation = Some(offset);
        self.busy = true;
        self.pending = Some(QueueJob {
            intent,
            generation: self.projection.generation(),
            stamp: self.query().stamp(revisions),
            target: QueueTarget::Offset(offset),
            anchor: self.anchor,
            shift: false,
            all,
            multi: false,
            scroll: true,
            total: self.rows.len(),
            query: self.query.clone(),
        });
    }

    pub(super) fn finish_selection(&mut self, job: &QueueJob, error: Option<&String>) -> bool {
        if self.manual != Some(job.intent) || !self.current(job.generation, job.stamp) {
            return false;
        }
        self.busy = false;
        if error.is_some() {
            self.navigation = None;
            self.manual = None;
        }
        self.selection_error = error.cloned();
        true
    }
    pub(super) fn query(&self) -> Query {
        Query::QueuePage {
            query: self.query.clone(),
            offset: 0,
            limit: 1,
        }
    }
    pub(super) fn current(&self, generation: u64, stamp: QueryStamp) -> bool {
        self.projection
            .accepts(&self.query(), generation, stamp.revisions())
    }
    pub(super) fn sync(&mut self, revisions: QueryRevisions, total: usize) -> bool {
        let query = self.query();
        let identity_changed = !self.page_projection.matches_identity(&query);
        let content_changed = self.page_projection.stamp().is_none_or(|stamp| {
            let previous = stamp.revisions();
            previous.queue != revisions.queue
                || (self.query.is_some() && previous.library != revisions.library)
        });
        let changed = self.page_projection.sync(&query, revisions);
        self.projection.sync(&query, revisions);
        if changed {
            self.locate.invalidate();
            self.locate_filtered = None;
            if identity_changed {
                self.rows.clear();
            } else if content_changed {
                self.rows.invalidate();
            } else {
                self.rows.invalidate_payload();
            }
            if content_changed || identity_changed {
                self.cancel();
                self.schedule_reconcile(revisions);
            } else {
                self.cancel_manual();
                if self.positions_stale && self.selection_error.is_none() {
                    self.reconcile_projection.cancel();
                    self.reconcile_pending = None;
                    self.schedule_reconcile(revisions);
                }
            }
        }
        if self.query.is_none() {
            // The snapshot count is authoritative even when DATA invalidation retired the pages.
            self.rows.set_total(total);
        }
        changed
    }
    pub(super) fn retry_selection(&mut self, revisions: QueryRevisions, total: usize) -> bool {
        if self.sync(revisions, total) {
            return true;
        }
        if self.selection_error.is_none() {
            return false;
        }
        self.cancel();
        self.schedule_reconcile(revisions);
        true
    }

    pub(super) fn retry_page(&mut self, revisions: QueryRevisions, total: usize) -> Range<usize> {
        self.rows.retry();
        if !self.ready() {
            self.retry_selection(revisions, total);
        }
        self.rows.visible()
    }

    // Must precede host refresh, which can submit replacement demand.
    pub(super) fn retire_page_worker(&mut self) {
        self.page_worker = false;
    }

    pub(super) fn accept_page(
        &mut self,
        query: &Query,
        generation: u64,
        response: rivu_protocol::response::QueryResponse,
        unexpected: impl FnOnce() -> String,
    ) {
        if !self
            .page_projection
            .accepts(query, generation, response.revisions)
        {
            return;
        }
        match response.result {
            Ok(ViewResponse::QueuePage(page)) => self.rows.accept(page),
            Err(error) => self.rows.fail(error.to_string()),
            _ => self.rows.fail(unexpected()),
        }
    }
    fn reconcile_query(&self) -> Query {
        Query::QueueEntries {
            query: self.query.clone(),
            queue_ids: Vec::new(),
        }
    }
    fn schedule_reconcile(&mut self, revisions: QueryRevisions) {
        let mut ids: Vec<_> = self.selected.keys().copied().collect();
        if let Some(anchor) = self.anchor
            && !self.selected.contains_key(&anchor.id)
        {
            ids.push(anchor.id);
        }
        if ids.is_empty() {
            return;
        }
        let query = self.reconcile_query();
        self.reconcile_projection.sync(&query, revisions);
        self.positions_stale = true;
        self.busy = true;
        self.reconcile_pending = Some(QueueReconcileJob {
            ids,
            generation: self.reconcile_projection.generation(),
            stamp: query.stamp(revisions),
            query: self.query.clone(),
        });
    }
    pub(super) fn reconcile_current(
        &self,
        job: &QueueReconcileJob,
        revisions: QueryRevisions,
    ) -> bool {
        let query = self.reconcile_query();
        self.reconcile_projection
            .accepts(&query, job.generation, revisions)
            && query.stamp(revisions) == job.stamp
    }
    pub(super) fn finish_reconcile(
        &mut self,
        job: &QueueReconcileJob,
        revisions: QueryRevisions,
        result: Result<Vec<QueueIdentity>, String>,
    ) -> bool {
        if !self.reconcile_current(job, revisions) {
            return false;
        }
        match result {
            Ok(rows) => self.reconcile(rows),
            Err(error) => {
                // Identity survives, but its old positions cannot authorize selection or ranges.
                self.busy = false;
                self.selection_error = Some(error);
                self.reconcile_projection.cancel();
                self.reconcile_pending = None;
                // Only physical completion releases the worker gate.
            }
        }
        true
    }
    pub(super) fn take_reconcile_job(&mut self) -> Option<QueueReconcileJob> {
        if self.worker.is_some() {
            return None;
        }
        let job = self.reconcile_pending.take()?;
        self.worker = Some(QueueWorker::Reconcile(job.generation));
        Some(job)
    }
    pub(super) fn release_reconcile_worker(&mut self, generation: u64) {
        if self.worker == Some(QueueWorker::Reconcile(generation)) {
            self.worker = None;
        }
    }
    pub(super) fn page_loading(&self, generation: u64) -> bool {
        self.page_worker && self.page_projection.generation() == generation
    }

    pub(super) fn cancel_page_demand(&mut self) {
        if self.page_worker {
            self.page_projection.cancel();
            self.rows.cancel_pending();
        }
        if self.locate.worker.is_some() {
            self.locate.invalidate();
        }
    }

    pub(super) fn demand_page(
        &mut self,
        range: Range<usize>,
        limit: usize,
    ) -> Option<(Query, u64)> {
        if self.page_size != limit {
            self.page_size = limit;
            self.page_projection.cancel();
            self.rows.configure_page_size(limit);
        }
        let offset = self.rows.demand(range, limit)?;
        if self.page_worker {
            return None;
        }
        self.rows.begin(offset, limit);
        self.page_worker = true;
        Some((
            Query::QueuePage {
                query: self.query.clone(),
                offset,
                limit,
            },
            self.page_projection.generation(),
        ))
    }
    pub(super) fn reconcile(&mut self, rows: Vec<QueueIdentity>) {
        self.active = self
            .active
            .and_then(|old| rows.iter().find(|row| row.id == old.id).copied());
        self.anchor = self
            .anchor
            .and_then(|anchor| rows.iter().find(|row| row.id == anchor.id).copied());
        self.selected = rows
            .into_iter()
            .filter(|row| self.selected.contains_key(&row.id))
            .map(|row| (row.id, row))
            .collect();
        self.busy = false;
        self.positions_stale = false;
        self.selection_error = None;
        self.rows.pin(self.active().map(|row| row.offset));
    }
    pub(super) fn take_job(&mut self) -> Option<QueueJob> {
        if self.worker.is_some() {
            return None;
        }
        let job = self.pending.take()?;
        self.worker = Some(QueueWorker::Selection(job.generation));
        Some(job)
    }

    pub(super) fn release_worker(&mut self, generation: u64) {
        if self.worker == Some(QueueWorker::Selection(generation)) {
            self.worker = None;
        }
    }

    pub(super) fn complete_range(
        &mut self,
        generation: u64,
        stamp: QueryStamp,
        anchor: QueueIdentity,
        target: QueueIdentity,
        rows: Vec<QueueIdentity>,
        additive: bool,
    ) -> bool {
        if !self.current(generation, stamp) {
            return false;
        }
        if !additive {
            self.selected.clear();
        }
        self.selected
            .extend(rows.into_iter().map(|row| (row.id, row)));
        self.anchor = Some(anchor);
        self.active = Some(target);
        self.busy = false;
        self.positions_stale = false;
        self.navigation = None;
        self.manual = None;
        self.rows.pin(Some(target.offset));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{QueueIdentity, QueueSelection};
    use rivu_protocol::{
        model::{LibraryRow, Page, Query, QueueRow},
        response::QueryRevisions,
    };

    fn revisions(queue: u64) -> QueryRevisions {
        QueryRevisions {
            queue,
            ..QueryRevisions::default()
        }
    }
    fn queue(revision: u64) -> QueueSelection {
        let mut queue = QueueSelection::default();
        queue.sync(revisions(revision), 200_000);
        queue
    }
    fn row(id: u64, position: usize) -> QueueRow {
        QueueRow {
            id,
            track_id: 1,
            position,
            offset: position,
            track: LibraryRow {
                id: 1,
                path: Default::default(),
                title: String::new(),
                artist: String::new(),
                album: String::new(),
                duration: None,
                codec: String::new(),
                bitrate_bps: None,
                bitrate_exact: false,
                channels: 0,
                sample_rate: 0,
                bits_per_sample: None,
                cue_number: None,
                favorite: false,
                missing: false,
                play_count: 0,
            },
        }
    }
    fn identity(id: u64, position: usize) -> QueueIdentity {
        QueueIdentity {
            id,
            track_id: 1,
            position,
            offset: position,
        }
    }
    #[test]
    fn bitrate_revision_retires_payload_preserving_identity_geometry_and_worker() {
        let mut queue = queue(7);
        queue.rows.begin(0, 20);
        queue.rows.accept(Page {
            total: 200_000,
            rows: vec![row(10, 0), row(11, 1)],
        });
        queue.select(identity(11, 1), false);
        let (old_query, old_generation) = queue.demand_page(40..50, 20).unwrap();
        let mut latest = revisions(7);
        latest.bitrate = 1;
        assert!(queue.sync(latest, 200_000));
        assert_eq!(queue.rows.total(), Some(200_000));
        assert_eq!(queue.rows.visible(), 40..50);
        assert_eq!(queue.ordered_ids(), vec![11]);
        assert_eq!(queue.anchor, Some(identity(11, 1)));
        assert!(queue.ready());
        assert!(queue.rows.get(0).is_none());
        assert!(queue.page_worker);
        assert!(queue.demand_page(40..50, 20).is_none());
        queue.retire_page_worker();
        queue.accept_page(
            &old_query,
            old_generation,
            rivu_protocol::response::QueryResponse {
                revisions: revisions(7),
                result: Ok(rivu_protocol::response::ViewResponse::QueuePage(Page {
                    total: 200_000,
                    rows: vec![row(12, 40)],
                })),
            },
            || unreachable!(),
        );
        assert!(queue.rows.get(40).is_none());
        assert!(queue.demand_page(40..50, 20).is_some());
    }

    #[test]
    fn queue_page_rejects_stale_bitrate_and_accepts_current_payload_unchanged() {
        let mut queue = queue(7);
        let mut latest = revisions(7);
        latest.bitrate = 1;
        queue.sync(latest, 200_000);
        let (query, generation) = queue.demand_page(0..2, 20).unwrap();
        let response = |revisions, row| rivu_protocol::response::QueryResponse {
            revisions,
            result: Ok(rivu_protocol::response::ViewResponse::QueuePage(Page {
                total: 200_000,
                rows: vec![row],
            })),
        };
        queue.accept_page(
            &query,
            generation,
            response(revisions(7), row(10, 0)),
            || unreachable!(),
        );
        assert!(queue.rows.get(0).is_none());
        let mut current = row(10, 0);
        current.track.bitrate_bps = Some(192_000);
        current.track.bitrate_exact = true;
        queue.retire_page_worker();
        queue.accept_page(
            &query,
            generation,
            response(latest, current),
            || unreachable!(),
        );
        let loaded = queue.rows.get(0).unwrap();
        assert_eq!(loaded.track.bitrate_bps, Some(192_000));
        assert!(loaded.track.bitrate_exact);
    }

    #[test]
    fn unfiltered_queue_keeps_payload_across_unrelated_library_revisions() {
        let mut queue = queue(7);
        queue.rows.begin(0, 20);
        queue.rows.accept(Page {
            total: 200_000,
            rows: vec![row(10, 0)],
        });
        queue.select(identity(10, 0), false);
        let generation = queue.page_projection.generation();
        let mut latest = revisions(7);
        latest.library = 1;
        assert!(!queue.sync(latest, 200_000));
        assert_eq!(queue.page_projection.generation(), generation);
        assert!(queue.rows.get(0).is_some());
        assert_eq!(queue.ordered_ids(), vec![10]);
    }

    #[test]
    fn hidden_filtered_bitrate_reload_keeps_count_and_demand_without_starting_worker() {
        let mut queue = queue(7);
        queue.change_search(Some("needle".into()));
        queue.sync(revisions(7), 200_000);
        queue.rows.demand(40..50, 20);
        queue.rows.begin(40, 20);
        queue.rows.accept(Page {
            total: 100,
            rows: vec![row(10, 40)],
        });
        queue.select(identity(10, 40), false);
        let mut latest = revisions(7);
        latest.bitrate = 1;
        queue.sync(latest, 200_000);
        assert_eq!(queue.rows.total(), Some(100));
        assert_eq!(queue.rows.visible(), 40..50);
        assert!(queue.rows.get(40).is_none());
        assert_eq!(queue.ordered_ids(), vec![10]);
        assert!(queue.ready());
        assert!(!queue.page_worker);
    }

    #[test]
    fn timeout_page_publishes_failure_without_releasing_replacement_gate() {
        let mut queue = queue(7);
        let (request, generation) = queue.demand_page(0..10, 20).unwrap();
        queue.accept_page(
            &request,
            generation,
            rivu_protocol::response::QueryResponse {
                revisions: revisions(7),
                result: Err(rivu_protocol::response::QueryError::Message(
                    "timeout".into(),
                )),
            },
            || unreachable!(),
        );
        assert!(queue.page_worker);
        assert_eq!(queue.rows.error(0), Some("timeout"));
        queue.change_search(Some("one".into()));
        queue.sync(revisions(7), 200_000);
        assert!(queue.demand_page(0..10, 20).is_none());
        queue.change_search(Some("latest".into()));
        queue.sync(revisions(7), 200_000);
        assert!(queue.demand_page(0..10, 20).is_none());
        queue.retire_page_worker();
        let (latest, _) = queue.demand_page(0..10, 20).unwrap();
        assert!(matches!(latest, Query::QueuePage { query: Some(query), .. } if query == "latest"));
        queue.accept_page(
            &request,
            generation,
            rivu_protocol::response::QueryResponse {
                revisions: revisions(7),
                result: Err(rivu_protocol::response::QueryError::Message(
                    "obsolete".into(),
                )),
            },
            || unreachable!(),
        );
        assert!(queue.rows.error(0).is_none());
        assert!(queue.page_worker);
    }

    #[test]
    fn filtered_queue_demands_matching_pages_and_keeps_occurrence_identity() {
        let mut queue = queue(7);
        let mut source = row(20, 400);
        source.offset = 1;
        let identity = QueueIdentity::from(&source);
        assert_eq!((identity.position, identity.offset), (400, 1));
        queue.query = Some("needle".into());
        assert!(queue.sync(revisions(7), 200_000));
        assert_eq!(queue.rows.total(), None);
        let (query, _) = queue.demand_page(0..3, 20).unwrap();
        assert!(
            matches!(query, rivu_protocol::model::Query::QueuePage { query: Some(query), offset: 0, .. } if query == "needle")
        );
        queue.select(identity, false);
        assert_eq!(queue.ordered_ids(), [20]);
        assert_eq!(queue.active().unwrap().position, 400);
        assert_eq!(queue.active().unwrap().offset, 1);
    }

    #[test]
    fn playing_locate_reveals_exact_duplicate_occurrence_without_selecting() {
        let mut queue = queue(7);
        queue.select(identity(10, 4), false);
        let selected = queue.ordered();
        let anchor = queue.anchor;
        let active = queue.active;
        let generation = queue.projection.generation();
        let request = queue.locate.begin(11, None, revisions(7));
        assert!(queue.locate.current(request, revisions(7), Some(11)));
        queue.locate.resolve(request, 199_999);
        let reveal = queue.locate.reveal.unwrap();
        assert_eq!(reveal.request.id, 11);
        assert_eq!(reveal.position, 199_999);
        assert!(reveal.scroll);
        assert_eq!(queue.ordered(), selected);
        assert_eq!(queue.anchor, anchor);
        assert_eq!(queue.active, active);
        assert_eq!(queue.projection.generation(), generation);
        assert!(queue.ready());
    }

    #[test]
    fn playing_locate_rejects_replaced_targets_and_revisions_with_one_worker() {
        let mut queue = queue(7);
        let old = queue.locate.begin(10, None, revisions(7));
        assert_eq!(queue.locate.take_job(), Some(old));
        let latest = queue.locate.begin(11, None, revisions(7));
        assert!(queue.locate.take_job().is_none());
        assert!(!queue.locate.current(old, revisions(7), Some(10)));
        assert!(!queue.locate.current(latest, revisions(7), Some(10)));
        assert!(!queue.locate.current(latest, revisions(7), None));
        assert!(!queue.locate.current(latest, revisions(8), Some(11)));
        queue.locate.resolve(old, 3);
        assert!(queue.locate.reveal.is_none());
        queue.locate.release_worker(old);
        assert_eq!(queue.locate.take_job(), Some(latest));
        assert!(queue.locate.take_job().is_none());
        queue.locate.invalidate();
        assert!(!queue.locate.current(latest, revisions(7), Some(11)));
        assert_eq!(queue.locate.worker, Some(latest));
        queue.locate.release_worker(latest);
        assert!(queue.locate.take_job().is_none());
    }

    #[test]
    fn occurrence_selection_survives_eviction_and_relocates_without_track_deduplication() {
        let mut queue = queue(0);
        queue.select(identity(10, 199_999), false);
        queue.select(identity(11, 3), true);
        queue.rows.invalidate();
        assert_eq!(
            queue.ordered().iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![11, 10]
        );
        queue.busy = true;
        assert!(queue.ordered().is_empty());
        queue.reconcile(vec![identity(10, 0), identity(11, 1)]);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.active(), Some(identity(11, 1)));
        queue.reconcile(vec![identity(10, 0)]);
        assert_eq!(queue.anchor, None);
        assert_eq!(queue.active(), Some(identity(10, 0)));
    }
    #[test]
    fn selection_changes_cancel_pending_navigation_and_range_results() {
        let mut queue = queue(7);
        let stamp = queue.query().stamp(revisions(7));
        let generation = queue.projection.generation();
        queue.select(identity(1, 0), false);
        assert!(!queue.current(generation, stamp));
        assert!(!queue.current(
            queue.projection.generation(),
            queue.query().stamp(revisions(8))
        ));
        queue.busy = true;
        queue.navigation = Some(199_999);
        queue.clear();
        assert!(queue.ready());
        assert_eq!(queue.navigation, None);
        assert_eq!(queue.len(), 0);
    }
    #[test]
    fn streamed_ranges_replace_or_extend_occurrences_and_reject_stale_completion() {
        let mut queue = queue(0);
        queue.select(identity(1, 0), false);
        let generation = queue.projection.generation();
        let stamp = queue.query().stamp(revisions(0));
        let anchor = identity(2, 199_997);
        let range = vec![anchor, identity(3, 199_998), identity(4, 199_999)];
        let target = identity(4, 199_999);
        assert!(queue.complete_range(generation, stamp, anchor, target, range.clone(), true));
        assert_eq!(queue.len(), 4);
        assert_eq!(queue.anchor, Some(anchor));
        assert_eq!(queue.active(), Some(target));
        assert!(queue.complete_range(generation, stamp, anchor, target, range, false));
        assert_eq!(queue.len(), 3);
        assert!(!queue.contains(&1));
        queue.select(identity(3, 199_998), true);
        assert!(!queue.contains(&3));
        assert_eq!(queue.anchor, Some(identity(3, 199_998)));
        assert!(!queue.complete_range(
            generation,
            stamp,
            anchor,
            target,
            vec![identity(1, 0)],
            false
        ));
        assert!(!queue.complete_range(
            queue.projection.generation(),
            queue.query().stamp(revisions(1)),
            anchor,
            target,
            vec![identity(1, 0)],
            false
        ));
        assert_eq!(queue.len(), 2);
        // Relocating the deselected anchor must not reselect it.
        queue.reconcile(vec![identity(2, 0), identity(3, 1), identity(4, 2)]);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.anchor, Some(identity(3, 1)));
        assert!(!queue.contains(&3));
    }

    #[test]
    fn cancelled_worker_retains_gate_and_selection_until_latest_job_can_start() {
        use super::QueueTarget;
        use crate::gui::coordination::manual_actions::ManualIntents;
        let mut intents = ManualIntents::default();
        let mut queue = queue(0);
        queue.select(identity(1, 0), false);
        let old = intents.begin();
        queue.begin_navigation(old, revisions(0), 1, false);
        let running = queue.take_job().unwrap();
        queue.cancel();
        let latest = intents.begin();
        queue.begin_navigation(latest, revisions(0), 8, false);
        assert!(queue.take_job().is_none());
        assert!(!queue.current(running.generation, running.stamp));
        assert!(!queue.finish_selection(&running, Some(&"obsolete".to_owned())));
        assert!(queue.busy);
        assert!(queue.selection_error.is_none());
        assert!(queue.contains(&1));
        queue.release_worker(running.generation);
        let next = queue.take_job().unwrap();
        assert_eq!(next.intent, latest);
        assert!(matches!(next.target, QueueTarget::Offset(8)));
        assert!(queue.take_job().is_none());
        assert!(queue.finish_selection(&next, None));
        assert_eq!(queue.active(), Some(identity(1, 0)));
    }

    #[test]
    fn ready_queue_navigation_keeps_desired_offset_until_confirmation_or_cancellation() {
        use crate::gui::coordination::manual_actions::ManualIntents;
        let mut intents = ManualIntents::default();
        let mut queue = queue(0);
        queue.select(identity(1, 0), false);
        queue.begin_navigation(intents.begin(), revisions(0), 8, false);
        let job = queue.take_job().unwrap();
        queue.release_worker(job.generation);
        assert!(queue.finish_selection(&job, None));
        assert_eq!(queue.active(), Some(identity(1, 0)));
        assert_eq!(queue.navigation, Some(8));
        let next = queue.navigation.unwrap() + 1;
        queue.cancel_manual();
        queue.begin_navigation(intents.begin(), revisions(0), next, false);
        let next = queue.take_job().unwrap();
        assert!(matches!(next.target, super::QueueTarget::Offset(9)));
        assert!(!queue.finish_selection(&job, Some(&"late failure".to_owned())));
        assert_eq!(queue.navigation, Some(9));
        assert!(queue.finish_selection(&next, Some(&"offline".to_owned())));
        assert_eq!(queue.navigation, None);
        assert_eq!(queue.active(), Some(identity(1, 0)));
    }
    #[test]
    fn unrelated_publications_preserve_pages_failures_selection_and_playing_locate() {
        let mut queue = queue(7);
        queue.rows.begin(0, 20);
        queue.rows.accept(Page {
            total: 200_000,
            rows: (0..20)
                .map(|position| row(position as u64, position))
                .collect(),
        });
        queue.select(identity(10, 10), false);
        let selected_generation = queue.projection.generation();
        let page_generation = queue.page_projection.generation();
        let locate = queue.locate.begin(11, None, revisions(7));
        queue.rows.begin(20, 20);
        queue.rows.fail("offline".into());
        let mut unrelated = revisions(7);
        unrelated.library = 11;
        unrelated.structure = 12;
        unrelated.playlist = 13;
        unrelated.duplicate_groups = 14;
        unrelated.duplicate_members = 15;
        assert!(!queue.sync(unrelated, 200_000));
        assert_eq!(queue.rows.get(10).unwrap().id, 10);
        assert_eq!(queue.rows.error(20), Some("offline"));
        assert_eq!(queue.ordered_ids(), vec![10]);
        assert!(queue.current(selected_generation, queue.query().stamp(unrelated)));
        assert!(
            queue
                .page_projection
                .accepts(&queue.query(), page_generation, unrelated)
        );
        assert!(queue.locate.current(locate, unrelated, Some(11)));
        assert!(queue.reconcile_pending.is_none());
    }

    #[test]
    fn unfiltered_snapshot_total_survives_invalidation_and_boundary_changes() {
        let mut queue = queue(7);
        assert!(queue.sync(revisions(8), 0));
        assert_eq!(queue.rows.total(), Some(0));
        assert_eq!(queue.rows.len(), 0);
        assert!(queue.demand_page(0..1, 20).is_none());
        assert!(!queue.page_worker);
        assert!(!queue.sync(revisions(8), 0));
        queue.cancel_page_demand();
        assert!(queue.demand_page(0..1, 40).is_none());
        assert_eq!(queue.rows.total(), Some(0));
        assert!(!queue.page_worker);

        assert!(queue.sync(revisions(9), 1));
        assert_eq!(queue.rows.total(), Some(1));
        let (query, _) = queue.demand_page(0..1, 40).unwrap();
        assert!(matches!(
            query,
            Query::QueuePage {
                query: None,
                offset: 0,
                limit: 40
            }
        ));
    }

    #[test]
    fn filtered_empty_survives_hide_and_resize_without_using_snapshot_total() {
        let mut queue = queue(7);
        queue.query = Some("needle".into());
        queue.sync(revisions(7), 200_000);
        queue.demand_page(0..1, 20).unwrap();
        queue.page_worker = false;
        queue.rows.accept(Page {
            total: 0,
            rows: vec![],
        });
        queue.cancel_page_demand();
        assert!(!queue.sync(revisions(7), 200_000));
        assert!(queue.demand_page(0..1, 40).is_none());
        assert_eq!(queue.rows.total(), Some(0));
        assert!(!queue.page_worker);
    }

    #[test]
    fn filtered_refresh_preserves_viewport_and_reloads_empty_results() {
        for refreshed in [
            QueryRevisions {
                library: 1,
                ..revisions(7)
            },
            revisions(8),
        ] {
            for total in [0, 100] {
                let mut queue = queue(7);
                queue.query = Some("Track".into());
                queue.sync(revisions(7), 200_000);
                queue.rows.begin(80, 20);
                queue.rows.accept(Page {
                    total,
                    rows: if total == 0 {
                        vec![]
                    } else {
                        (80..100)
                            .map(|position| row(position as u64, position))
                            .collect()
                    },
                });
                if total > 0 {
                    queue.rows.demand(80..90, 20);
                }
                assert!(queue.sync(refreshed, 200_000));
                assert_eq!(queue.rows.total(), None);
                if total == 0 {
                    let (query, generation) = queue.demand_page(queue.rows.visible(), 20).unwrap();
                    assert!(queue.page_projection.accepts(&query, generation, refreshed));
                    queue.page_worker = false;
                    queue.rows.accept(Page {
                        total: 1,
                        rows: vec![row(10, 0)],
                    });
                    assert_eq!(queue.rows.total(), Some(1));
                    assert_eq!(queue.rows.get(0).unwrap().id, 10);
                } else {
                    assert_eq!(queue.rows.len(), 100);
                    assert_eq!(queue.rows.visible(), 80..90);
                    assert!(queue.rows.get(80).is_none());
                    assert_eq!(queue.rows.demand(queue.rows.visible(), 20), Some(80));
                }
            }
        }
    }

    #[test]
    fn page_cancellation_and_boundary_changes_keep_the_physical_gate() {
        let mut queue = queue(7);
        let (old_query, old_generation) = queue.demand_page(0..10, 20).unwrap();
        assert!(queue.page_worker);
        queue.cancel();
        assert!(queue.page_worker);
        assert!(queue.sync(revisions(8), 200_000));
        assert!(queue.demand_page(0..10, 40).is_none());
        assert!(queue.page_worker);
        assert!(
            !queue
                .page_projection
                .accepts(&old_query, old_generation, revisions(7))
        );
        queue.page_worker = false; // Physical completion, not logical cancellation.
        let (latest, generation) = queue.demand_page(0..10, 40).unwrap();
        assert!(
            queue
                .page_projection
                .accepts(&latest, generation, revisions(8))
        );
        assert!(queue.demand_page(40..50, 40).is_none());
    }

    #[test]
    fn page_failure_waits_for_explicit_retry() {
        let mut queue = queue(7);
        queue.rows.set_total(20);
        let (query, generation) = queue.demand_page(0..10, 20).unwrap();
        assert!(
            queue
                .page_projection
                .accepts(&query, generation, revisions(7))
        );
        queue.retire_page_worker();
        queue.accept_page(
            &query,
            generation,
            rivu_protocol::response::QueryResponse {
                revisions: revisions(7),
                result: Err(rivu_protocol::response::QueryError::Message(
                    "offline".into(),
                )),
            },
            || unreachable!(),
        );
        for _ in 0..3 {
            assert!(!queue.sync(revisions(7), 20));
            assert!(queue.demand_page(0..10, 20).is_none());
        }
        assert_eq!(queue.rows.error(0), Some("offline"));
        assert_eq!(queue.retry_page(revisions(7), 20), queue.rows.visible());
        assert!(queue.demand_page(0..10, 20).is_some());
    }

    #[test]
    fn hidden_queue_changes_coalesce_relocation_without_releasing_its_worker() {
        let mut queue = queue(7);
        queue.select(identity(10, 199_999), false);
        queue.select(identity(11, 3), true);
        assert!(queue.sync(revisions(8), 10));
        assert!(!queue.page_worker);
        assert!(queue.busy);
        let old = queue.take_reconcile_job().unwrap();
        assert!(queue.sync(revisions(9), 10));
        assert!(!queue.reconcile_current(&old, revisions(8)));
        assert!(queue.take_reconcile_job().is_none());
        assert!(queue.take_job().is_none());
        assert_eq!(queue.selected.len(), 2);
        queue.release_reconcile_worker(old.generation);
        let latest = queue.take_reconcile_job().unwrap();
        assert!(queue.reconcile_current(&latest, revisions(9)));
        assert_eq!(latest.ids.len(), 2);
        queue.release_reconcile_worker(latest.generation);
        assert!(queue.finish_reconcile(&latest, revisions(9), Err("offline".into())));
        assert!(!queue.sync(revisions(9), 10));
        assert!(queue.take_reconcile_job().is_none());
        assert_eq!(queue.selected.len(), 2);
        assert!(queue.retry_selection(revisions(9), 10));
        let retry = queue.take_reconcile_job().unwrap();
        assert!(queue.finish_reconcile(
            &retry,
            revisions(9),
            Ok(vec![identity(10, 0), identity(11, 1)]),
        ));
        queue.release_reconcile_worker(retry.generation);
        assert_eq!(queue.ordered_ids(), vec![10, 11]);
    }
    #[test]
    fn page_size_is_demand_not_selection_or_locate_identity() {
        let mut queue = queue(7);
        queue.select(identity(10, 199_999), false);
        let selection_generation = queue.projection.generation();
        let locate = queue.locate.begin(11, None, revisions(7));
        let (old, generation) = queue.demand_page(0..10, 20).unwrap();
        assert!(queue.demand_page(0..10, 40).is_none());
        assert!(
            !queue
                .page_projection
                .accepts(&old, generation, revisions(7))
        );
        assert!(queue.current(selection_generation, queue.query().stamp(revisions(7))));
        assert_eq!(queue.ordered_ids(), vec![10]);
        assert!(queue.locate.current(locate, revisions(7), Some(11)));
        assert!(queue.reconcile_pending.is_none());
        queue.page_worker = false;
        assert!(queue.demand_page(0..10, 40).is_some());
    }

    #[test]
    fn reconcile_failure_parks_positions_and_retry_rejects_late_completion() {
        for error in ["offline", "timeout"] {
            let mut queue = queue(7);
            queue.select(identity(10, 100), false);
            queue.sync(revisions(8), 200);
            let old = queue.take_reconcile_job().unwrap();
            assert!(queue.finish_reconcile(&old, revisions(8), Err(error.into())));
            assert_eq!(queue.selection_error.as_deref(), Some(error));
            assert!(queue.contains(&10));
            assert_eq!(queue.anchor, Some(identity(10, 100)));
            queue.cancel_manual();
            assert!(!queue.ready());
            assert!(queue.identity(10).is_none());
            assert!(queue.active().is_none());
            assert!(queue.ordered().is_empty());
            let intent = crate::gui::coordination::manual_actions::ManualIntents::default().begin();
            assert!(
                queue
                    .begin_selection(intent, revisions(8), identity(11, 4), true, false)
                    .is_none()
            );
            assert!(queue.pending.is_none());
            assert!(!queue.sync(revisions(8), 200));
            assert!(queue.take_reconcile_job().is_none());
            assert!(queue.retry_selection(revisions(8), 200));
            assert!(queue.take_reconcile_job().is_none());
            assert!(!queue.finish_reconcile(&old, revisions(8), Ok(vec![identity(10, 100)])));
            assert!(!queue.ready());
            queue.release_reconcile_worker(old.generation);
            let retry = queue.take_reconcile_job().unwrap();
            assert!(queue.finish_reconcile(&retry, revisions(8), Ok(vec![identity(10, 2)])));
            assert_eq!(queue.active(), Some(identity(10, 2)));
            assert!(queue.selection_error.is_none());
            queue.release_reconcile_worker(retry.generation);
            assert!(!queue.finish_reconcile(&old, revisions(8), Ok(vec![identity(10, 100)])));
            assert_eq!(queue.active(), Some(identity(10, 2)));
            assert!(
                queue
                    .begin_selection(intent, revisions(8), identity(11, 4), true, false)
                    .is_none()
            );
            let range = queue.take_job().unwrap();
            assert_eq!(range.anchor, Some(identity(10, 2)));
        }
    }

    #[test]
    fn manual_range_failure_restores_valid_old_selection() {
        let mut queue = queue(7);
        queue.select(identity(10, 2), false);
        let intent = crate::gui::coordination::manual_actions::ManualIntents::default().begin();
        assert!(
            queue
                .begin_selection(intent, revisions(7), identity(11, 4), true, false)
                .is_none()
        );
        let job = queue.take_job().unwrap();
        assert!(queue.finish_selection(&job, Some(&"offline".to_owned())));
        queue.cancel_manual();
        assert!(queue.ready());
        assert_eq!(queue.active(), Some(identity(10, 2)));
        queue.release_worker(job.generation);
    }
    #[test]
    fn cancelling_replacement_does_not_validate_retained_positions() {
        let mut queue = queue(7);
        queue.select(identity(10, 100), false);
        queue.sync(revisions(8), 200);
        let old = queue.take_reconcile_job().unwrap();
        assert!(queue.finish_reconcile(&old, revisions(8), Err("offline".into())));
        let intent = crate::gui::coordination::manual_actions::ManualIntents::default().begin();
        let replacement = queue
            .begin_selection(intent, revisions(8), identity(11, 4), false, false)
            .unwrap();
        queue.cancel_manual();
        assert!(!queue.ready());
        assert!(queue.active().is_none());
        assert!(queue.ordered_ids().is_empty());
        assert_eq!(queue.selection_error.as_deref(), Some("offline"));
        assert!(!queue.current(replacement.generation, replacement.stamp));
        assert!(
            queue
                .begin_selection(intent, revisions(8), identity(12, 6), true, false)
                .is_none()
        );
        queue.release_reconcile_worker(old.generation);
        assert!(queue.retry_selection(revisions(8), 200));
        let retry = queue.take_reconcile_job().unwrap();
        assert!(queue.finish_reconcile(&retry, revisions(8), Ok(vec![identity(10, 2)])));
        queue.release_reconcile_worker(retry.generation);
        assert_eq!(queue.active(), Some(identity(10, 2)));
    }
}
