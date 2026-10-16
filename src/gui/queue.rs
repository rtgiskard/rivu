//! Queue pages and compact selections use Query-owned Q-only stamps. Page demand,
//! logical generations and physical worker gates have separate lifetimes: retiring
//! work never releases its gate, and only authoritative identity lookups delete a
//! selected occurrence. Hidden state sync does not demand metadata pages; failures
//! remain parked until explicit retry. Now-playing locate never enters selection
//! or metadata intent handling.

use super::metadata::{ManualAction, ManualIntentId};
use super::*;
use crate::{
    model::{Page, Query, QueryStamp, QueueRow},
    projection::QueryProjection,
    response::{QueryRevisions, ViewResponse},
};
use std::{ops::Range, time::Duration};

pub(in crate::gui) struct QueueCommit {
    stamp: QueryStamp,
    generation: u64,
    pub(in crate::gui) target: QueueIdentity,
    anchor: QueueIdentity,
    rows: Option<Vec<QueueIdentity>>,
    multi: bool,
    scroll: bool,
}

#[derive(Clone, Copy)]
enum QueueTarget {
    Row(QueueIdentity),
    Offset(usize),
}

struct QueueJob {
    intent: ManualIntentId,
    generation: u64,
    stamp: QueryStamp,
    target: QueueTarget,
    anchor: Option<QueueIdentity>,
    shift: bool,
    multi: bool,
    scroll: bool,
    total: usize,
}

/// Selection retains only occurrence identity and absolute position, never track metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct QueueIdentity {
    pub(super) id: u64,
    pub(super) track_id: i64,
    pub(super) position: usize,
}
impl From<&QueueRow> for QueueIdentity {
    fn from(row: &QueueRow) -> Self {
        Self {
            id: row.id,
            track_id: row.track_id,
            position: row.position,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QueueLocateRequest {
    generation: u64,
    stamp: QueryStamp,
    id: u64,
}

#[derive(Clone, Copy)]
struct QueueLocateReveal {
    request: QueueLocateRequest,
    position: usize,
    scroll: bool,
}

#[derive(Default)]
struct QueueLocate {
    projection: QueryProjection,
    query: Option<Query>,
    pending: Option<QueueLocateRequest>,
    worker: Option<QueueLocateRequest>,
    reveal: Option<QueueLocateReveal>,
    emphasized: Option<QueueLocateRequest>,
    timer: Option<gpui::Task<()>>,
}

impl QueueLocate {
    fn invalidate(&mut self) {
        self.projection.cancel();
        self.query = None;
        self.pending = None;
        self.reveal = None;
        self.emphasized = None;
        self.timer = None;
        // A cancelled lookup still owns the worker gate until its response arrives.
    }

    fn query(id: u64) -> Query {
        Query::QueueEntries {
            queue_ids: vec![id],
        }
    }

    fn begin(&mut self, id: u64, revisions: QueryRevisions) -> QueueLocateRequest {
        self.invalidate();
        let query = Self::query(id);
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

    fn current(
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

    fn take_job(&mut self) -> Option<QueueLocateRequest> {
        if self.worker.is_some() {
            return None;
        }
        let request = self.pending.take()?;
        self.worker = Some(request);
        Some(request)
    }

    fn release_worker(&mut self, request: QueueLocateRequest) {
        if self.worker == Some(request) {
            self.worker = None;
        }
    }

    fn resolve(&mut self, request: QueueLocateRequest, position: usize) {
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

struct QueueReconcileJob {
    ids: Vec<u64>,
    generation: u64,
    stamp: QueryStamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueueWorker {
    Selection(u64),
    Reconcile(u64),
}

#[derive(Default)]
pub(super) struct QueueSelection {
    pub(super) rows: paged::PagedRows<QueueRow>,
    selected: HashMap<u64, QueueIdentity>,
    anchor: Option<QueueIdentity>,
    page_projection: QueryProjection,
    page_worker: bool,
    page_size: usize,
    projection: QueryProjection,
    reconcile_projection: QueryProjection,
    reconcile_pending: Option<QueueReconcileJob>,
    pub(super) busy: bool,
    pub(super) selection_error: Option<String>,
    navigation: Option<usize>,
    active: Option<QueueIdentity>,
    pending: Option<QueueJob>,
    worker: Option<QueueWorker>,
    manual: Option<ManualIntentId>,
    locate: QueueLocate,
}
impl QueueSelection {
    pub(super) fn len(&self) -> usize {
        self.selected.len()
    }
    pub(super) fn contains(&self, id: &u64) -> bool {
        self.selected.contains_key(id)
    }
    pub(super) fn ready(&self) -> bool {
        !self.busy
    }
    pub(super) fn identity(&self, id: u64) -> Option<QueueIdentity> {
        self.ready()
            .then(|| self.selected.get(&id).copied())
            .flatten()
    }
    pub(super) fn ordered(&self) -> Vec<QueueIdentity> {
        if self.busy {
            return Vec::new();
        }
        let mut rows: Vec<_> = self.selected.values().copied().collect();
        rows.sort_unstable_by_key(|row| row.position);
        rows
    }
    pub(super) fn ordered_ids(&self) -> Vec<u64> {
        if self.busy {
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
            self.cancel();
        }
    }
    pub(super) fn clear(&mut self) {
        self.cancel();
        self.selected.clear();
        self.anchor = None;
        self.active = None;
        self.rows.pin(None);
    }
    fn select(&mut self, row: QueueIdentity, multi: bool) {
        self.cancel();
        if !multi {
            self.selected.clear();
        }
        if multi && self.selected.remove(&row.id).is_some() {
        } else {
            self.selected.insert(row.id, row);
        }
        self.anchor = Some(row);
        self.active = Some(row);
        self.rows.pin(Some(row.position));
    }
    fn query() -> Query {
        Query::QueuePage {
            offset: 0,
            limit: 1,
        }
    }
    fn current(&self, generation: u64, stamp: QueryStamp) -> bool {
        self.projection
            .accepts(&Self::query(), generation, stamp.revisions())
    }
    fn sync(&mut self, revisions: QueryRevisions, total: usize) -> bool {
        let query = Self::query();
        let changed = self.page_projection.sync(&query, revisions);
        self.projection.sync(&query, revisions);
        self.rows.set_total(total);
        if changed {
            self.locate.invalidate();
            self.rows.invalidate();
            self.cancel();
        }
        changed
    }
    fn reconcile_query() -> Query {
        // Chunk boundaries are demand; selection cancellation owns the chosen ID set.
        Query::QueueEntries {
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
        let query = Self::reconcile_query();
        self.reconcile_projection.sync(&query, revisions);
        self.busy = true;
        self.reconcile_pending = Some(QueueReconcileJob {
            ids,
            generation: self.reconcile_projection.generation(),
            stamp: query.stamp(revisions),
        });
    }
    fn reconcile_current(&self, job: &QueueReconcileJob, revisions: QueryRevisions) -> bool {
        let query = Self::reconcile_query();
        self.reconcile_projection
            .accepts(&query, job.generation, revisions)
            && query.stamp(revisions) == job.stamp
    }
    fn take_reconcile_job(&mut self) -> Option<QueueReconcileJob> {
        if self.worker.is_some() {
            return None;
        }
        let job = self.reconcile_pending.take()?;
        self.worker = Some(QueueWorker::Reconcile(job.generation));
        Some(job)
    }
    fn release_reconcile_worker(&mut self, generation: u64) {
        if self.worker == Some(QueueWorker::Reconcile(generation)) {
            self.worker = None;
        }
    }
    pub(super) fn page_loading(&self, generation: u64) -> bool {
        self.page_worker && self.page_projection.generation() == generation
    }

    fn demand_page(&mut self, range: Range<usize>, limit: usize) -> Option<(Query, u64)> {
        if self.page_size != limit {
            self.page_size = limit;
            self.page_projection.cancel();
        }
        let offset = self.rows.demand(range, limit)?;
        if self.page_worker {
            return None;
        }
        self.rows.begin(offset, limit);
        self.page_worker = true;
        Some((
            Query::QueuePage { offset, limit },
            self.page_projection.generation(),
        ))
    }
    fn reconcile(&mut self, rows: Vec<QueueIdentity>) {
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
        self.rows.pin(self.active().map(|row| row.position));
    }
    fn take_job(&mut self) -> Option<QueueJob> {
        if self.worker.is_some() {
            return None;
        }
        let job = self.pending.take()?;
        self.worker = Some(QueueWorker::Selection(job.generation));
        Some(job)
    }

    fn release_worker(&mut self, generation: u64) {
        if self.worker == Some(QueueWorker::Selection(generation)) {
            self.worker = None;
        }
    }

    fn complete_range(
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
        self.rows.pin(Some(target.position));
        true
    }
}

impl GuiApp {
    pub(super) fn locate_playing_queue(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.refresh(cx);
        let Some(id) = self.state.queue.current_id else {
            return;
        };
        let request = self
            .browser
            .queue
            .locate
            .begin(id, self.state.query_revisions(None));
        let position = self.state.queue.current_position.or_else(|| {
            self.browser
                .queue
                .rows
                .iter()
                .find(|row| row.id == id)
                .map(|row| row.position)
        });
        if let Some(position) = position {
            self.browser.queue.locate.resolve(request, position);
            self.ensure_queue_range(position..position.saturating_add(1), cx);
        } else {
            self.dispatch_queue_locate(cx);
        }
        cx.notify();
    }

    fn queue_locate_current(&self, request: QueueLocateRequest) -> bool {
        self.browser.queue.locate.current(
            request,
            self.state.query_revisions(None),
            self.state.queue.current_id,
        )
    }

    fn dispatch_queue_locate(&mut self, cx: &mut Context<Self>) {
        let Some(request) = self.browser.queue.locate.take_job() else {
            return;
        };
        if !self.queue_locate_current(request) {
            self.browser.queue.locate.release_worker(request);
            return;
        }
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let response = cx
                .background_executor()
                .spawn(async move {
                    handle.query(Query::QueueEntries {
                        queue_ids: vec![request.id],
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.browser.queue.locate.release_worker(request);
                this.refresh(cx);
                if this.queue_locate_current(request)
                    && this.browser.queue.locate.current(
                        request,
                        response.revisions,
                        this.state.queue.current_id,
                    )
                {
                    match response.result {
                        Ok(ViewResponse::QueueEntries(rows)) => {
                            if let Some(row) = rows.into_iter().find(|row| row.id == request.id) {
                                this.browser.queue.locate.resolve(request, row.position);
                                this.ensure_queue_range(
                                    row.position..row.position.saturating_add(1),
                                    cx,
                                );
                            } else {
                                this.error =
                                    Some(this.i18n.text("gl-queue-locate-missing").to_owned());
                            }
                        }
                        Err(error) => this.error = Some(error.to_string()),
                        _ => {
                            this.error =
                                Some(this.i18n.text("gl-queue-locate-response").to_owned());
                        }
                    }
                }
                this.dispatch_queue_locate(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn reveal_queue_locate(
        &mut self,
        scroll: &gpui::UniformListScrollHandle,
        cx: &mut Context<Self>,
    ) {
        let Some(reveal) = self.browser.queue.locate.reveal else {
            return;
        };
        if !self.queue_locate_current(reveal.request) {
            self.browser.queue.locate.invalidate();
            return;
        }
        if reveal.scroll {
            scroll.scroll_to_item(reveal.position, gpui::ScrollStrategy::Nearest);
            self.browser.queue.locate.reveal.as_mut().unwrap().scroll = false;
        }
        let Some(row) = self.browser.queue.rows.get(reveal.position) else {
            return;
        };
        let id = row.id;
        self.browser.queue.locate.reveal = None;
        if id != reveal.request.id {
            return;
        }
        let request = reveal.request;
        self.browser.queue.locate.emphasized = Some(request);
        let timer = cx.background_executor().timer(Duration::from_millis(1500));
        self.browser.queue.locate.timer = Some(cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                if this.browser.queue.locate.emphasized == Some(request) {
                    this.browser.queue.locate.emphasized = None;
                    cx.notify();
                }
            });
        }));
    }

    pub(super) fn queue_locate_emphasized(&self, id: u64) -> bool {
        self.browser
            .queue
            .locate
            .emphasized
            .is_some_and(|request| request.id == id && self.queue_locate_current(request))
    }

    /// Reconcile authoritative queue state even while its pane is hidden. No page demand.
    pub(super) fn sync_queue(&mut self, cx: &mut Context<Self>) -> bool {
        let revisions = self.state.query_revisions(None);
        if !self.browser.queue.sync(revisions, self.state.queue.total) {
            return false;
        }
        self.browser.queue.schedule_reconcile(revisions);
        self.dispatch_queue_selection(cx);
        true
    }

    /// Explicit retry of failed identity relocation; unchanged pages and locate survive.
    pub(super) fn refresh_queue(&mut self, cx: &mut Context<Self>) {
        if self.sync_queue(cx) {
            return;
        }
        if self.browser.queue.selection_error.is_some() {
            self.browser.queue.cancel();
            self.browser
                .queue
                .schedule_reconcile(self.state.query_revisions(None));
            self.dispatch_queue_selection(cx);
        }
    }

    fn dispatch_queue_reconcile(&mut self, job: QueueReconcileJob, cx: &mut Context<Self>) {
        let handle = self.handle.clone();
        let i18n = std::sync::Arc::clone(&self.i18n);
        cx.spawn(async move |this, cx| {
            let result: Result<Option<Vec<QueueIdentity>>, String> = async {
                let mut selected = Vec::with_capacity(job.ids.len());
                for chunk in job.ids.chunks(crate::model::MAX_QUERY_ROWS) {
                    if !this
                        .update(cx, |this, cx| {
                            this.refresh(cx);
                            this.browser
                                .queue
                                .reconcile_current(&job, this.state.query_revisions(None))
                        })
                        .unwrap_or(false)
                    {
                        return Ok(None);
                    }
                    let handle = handle.clone();
                    let queue_ids = chunk.to_vec();
                    let response = cx
                        .background_executor()
                        .spawn(async move { handle.query(Query::QueueEntries { queue_ids }) })
                        .await;
                    if QueueSelection::reconcile_query().stamp(response.revisions) != job.stamp {
                        return Ok(None);
                    }
                    match response.result {
                        Ok(ViewResponse::QueueEntries(rows)) => {
                            selected.extend(rows.iter().map(QueueIdentity::from));
                        }
                        Err(error) => return Err(error.to_string()),
                        _ => return Err(i18n.text("gl-queue-selection-response").to_owned()),
                    }
                }
                Ok(Some(selected))
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                this.browser.queue.release_reconcile_worker(job.generation);
                this.refresh(cx);
                if this
                    .browser
                    .queue
                    .reconcile_current(&job, this.state.query_revisions(None))
                {
                    match result {
                        Ok(Some(selected)) => this.browser.queue.reconcile(selected),
                        Err(error) => {
                            this.error = Some(error.clone());
                            this.browser.queue.selection_error = Some(error);
                        }
                        Ok(None) => {
                            // A newer snapshot will schedule fresh reconciliation.
                        }
                    }
                }
                this.dispatch_queue_selection(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn ensure_queue_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        self.sync_queue(cx);
        if !self.panel_demand("queue") {
            return;
        }
        let limit = self.view_page_size();
        let Some((query, generation)) = self.browser.queue.demand_page(range, limit) else {
            return;
        };
        self.start_loading_text(
            super::loading::LoadingOwner::View(ViewRequestKind::Queue),
            generation,
            cx,
        );
        let request = query.clone();
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let response = cx
                .background_executor()
                .spawn(async move { handle.query(query) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.finish_loading_text(
                    &super::loading::LoadingOwner::View(ViewRequestKind::Queue),
                    generation,
                    cx,
                );
                // Only physical completion releases this gate, including obsolete responses.
                this.browser.queue.page_worker = false;
                this.refresh(cx);
                this.sync_queue(cx);
                if this.browser.queue.page_projection.accepts(
                    &request,
                    generation,
                    response.revisions,
                ) {
                    match response.result {
                        Ok(ViewResponse::QueuePage(page)) => this.browser.queue.rows.accept(page),
                        Err(error) => this.browser.queue.rows.fail(error.to_string()),
                        _ => this
                            .browser
                            .queue
                            .rows
                            .fail(this.i18n.text("gl-queue-page-response").to_owned()),
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn select_queue_entry(
        &mut self,
        id: u64,
        shift: bool,
        multi: bool,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if self.browser.queue.busy && (shift || multi) {
            return;
        }
        let Some(row) = self
            .browser
            .queue
            .rows
            .iter()
            .find(|row| row.id == id)
            .map(QueueIdentity::from)
        else {
            return;
        };
        let anchor = self.browser.queue.anchor;
        let intent = self.begin_manual_intent();
        self.browser.queue.manual = Some(intent);
        let generation = self.browser.queue.projection.generation();
        let stamp = QueueSelection::query().stamp(self.state.query_revisions(None));
        if shift && anchor.is_some() {
            self.browser.queue.pending = Some(QueueJob {
                intent,
                generation,
                stamp,
                target: QueueTarget::Row(row),
                anchor,
                shift,
                multi,
                scroll: false,
                total: self.state.queue.total,
            });
            self.browser.queue.busy = true;
            self.dispatch_queue_selection(cx);
        } else {
            self.ready_manual_action(
                intent,
                ManualAction::Queue(QueueCommit {
                    stamp,
                    generation,
                    target: row,
                    anchor: row,
                    rows: None,
                    multi,
                    scroll: false,
                }),
                cx,
            );
        }
    }

    pub(in crate::gui) fn queue_commit_valid(&self, commit: &QueueCommit) -> bool {
        QueueSelection::query().stamp(self.state.query_revisions(None)) == commit.stamp
            && self.browser.queue.current(commit.generation, commit.stamp)
    }

    pub(in crate::gui) fn commit_queue_selection(
        &mut self,
        commit: QueueCommit,
        cx: &mut Context<Self>,
    ) {
        if !self.queue_commit_valid(&commit) {
            return;
        }
        let queue = &mut self.browser.queue;
        if let Some(rows) = commit.rows {
            queue.complete_range(
                commit.generation,
                commit.stamp,
                commit.anchor,
                commit.target,
                rows,
                commit.multi,
            );
        } else {
            queue.select(commit.target, commit.multi);
        }
        queue.navigation = None;
        self.browser.focus_list(ListFocus::Queue);
        self.request_track(commit.target.track_id, cx);
        if commit.scroll {
            self.scroll_views
                .scroll_list_to("queue", commit.target.position);
        }
        self.sync_waveform(cx);
        cx.notify();
    }

    pub(super) fn navigate_queue(&mut self, down: bool, cx: &mut Context<Self>) {
        self.refresh(cx);
        let total = self.state.queue.total;
        if total == 0 || (self.browser.queue.busy && self.browser.queue.navigation.is_none()) {
            return;
        }
        let current = self
            .browser
            .queue
            .navigation
            .or_else(|| self.browser.queue.active().map(|row| row.position));
        let offset = match current {
            Some(index) if down => index.saturating_add(1).min(total - 1),
            Some(index) => index.saturating_sub(1),
            None if down => 0,
            None => total - 1,
        };
        if current == Some(offset) {
            return;
        }
        self.start_queue_navigation(offset, cx);
    }

    fn start_queue_navigation(&mut self, offset: usize, cx: &mut Context<Self>) {
        let anchor = self.browser.queue.anchor;
        let intent = self.begin_manual_intent();
        let stamp = QueueSelection::query().stamp(self.state.query_revisions(None));
        let queue = &mut self.browser.queue;
        queue.manual = Some(intent);
        queue.navigation = Some(offset);
        queue.busy = true;
        queue.pending = Some(QueueJob {
            intent,
            generation: queue.projection.generation(),
            stamp,
            target: QueueTarget::Offset(offset),
            anchor,
            shift: false,
            multi: false,
            scroll: true,
            total: self.state.queue.total,
        });
        self.dispatch_queue_selection(cx);
        cx.notify();
    }

    fn queue_job_current(&self, job: &QueueJob) -> bool {
        self.manual_intent_current(job.intent)
            && QueueSelection::query().stamp(self.state.query_revisions(None)) == job.stamp
            && self.browser.queue.current(job.generation, job.stamp)
    }

    fn dispatch_queue_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = self.browser.queue.take_reconcile_job() {
            self.dispatch_queue_reconcile(job, cx);
            return;
        }
        let Some(job) = self.browser.queue.take_job() else {
            return;
        };
        let handle = self.handle.clone();
        let i18n = std::sync::Arc::clone(&self.i18n);
        cx.spawn(async move |this, cx| {
            let result: Result<Option<QueueCommit>, String> = async {
                if !this
                    .update(cx, |this, cx| {
                        this.refresh(cx);
                        this.queue_job_current(&job)
                    })
                    .unwrap_or(false)
                {
                    return Ok(None);
                }
                let target = match job.target {
                    QueueTarget::Row(row) => row,
                    QueueTarget::Offset(offset) => {
                        let handle = handle.clone();
                        let response = cx
                            .background_executor()
                            .spawn(
                                async move { handle.query(Query::QueuePage { offset, limit: 1 }) },
                            )
                            .await;
                        if QueueSelection::query().stamp(response.revisions) != job.stamp {
                            return Err(i18n.text("gl-queue-changed").into());
                        }
                        let ViewResponse::QueuePage(page) =
                            response.result.map_err(|error| error.to_string())?
                        else {
                            return Err(i18n.text("gl-queue-selection-response").into());
                        };
                        let Some(row) = page.rows.first().filter(|row| row.position == offset)
                        else {
                            return Err(i18n.text("gl-queue-changed").into());
                        };
                        QueueIdentity::from(row)
                    }
                };
                let anchor = job.anchor.unwrap_or(target);
                let mut rows = None;
                if job.shift && job.anchor.is_some() {
                    let (start, end) = (
                        anchor.position.min(target.position),
                        anchor.position.max(target.position) + 1,
                    );
                    let mut identities = Vec::with_capacity(end - start);
                    let mut offset = start;
                    while offset < end {
                        if !this
                            .update(cx, |this, cx| {
                                this.refresh(cx);
                                this.queue_job_current(&job)
                            })
                            .unwrap_or(false)
                        {
                            return Ok(None);
                        }
                        let limit = (end - offset).min(crate::model::MAX_QUERY_ROWS);
                        let handle = handle.clone();
                        let response = cx
                            .background_executor()
                            .spawn(async move { handle.query(Query::QueuePage { offset, limit }) })
                            .await;
                        if QueueSelection::query().stamp(response.revisions) != job.stamp {
                            return Err(i18n.text("gl-queue-changed").into());
                        }
                        let ViewResponse::QueuePage(Page { rows: page, total }) =
                            response.result.map_err(|error| error.to_string())?
                        else {
                            return Err(i18n.text("gl-queue-range-response").into());
                        };
                        if total != job.total
                            || page.len() != limit
                            || page
                                .iter()
                                .enumerate()
                                .any(|(index, row)| row.position != offset + index)
                        {
                            return Err(i18n.text("gl-queue-changed").into());
                        }
                        offset += page.len();
                        identities.extend(page.iter().map(QueueIdentity::from));
                    }
                    rows = Some(identities);
                }
                Ok(Some(QueueCommit {
                    stamp: job.stamp,
                    generation: job.generation,
                    target,
                    anchor,
                    rows,
                    multi: job.multi,
                    scroll: job.scroll,
                }))
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                this.browser.queue.release_worker(job.generation);
                this.refresh(cx);
                if this.queue_job_current(&job) {
                    this.browser.queue.busy = false;
                    this.browser.queue.navigation = None;
                    match result {
                        Ok(Some(commit)) => {
                            this.ready_manual_action(job.intent, ManualAction::Queue(commit), cx)
                        }
                        Err(message) => {
                            this.fail_manual_intent(job.intent);
                            this.browser.queue.selection_error = Some(message.clone());
                            this.error = Some(message);
                        }
                        Ok(None) => {}
                    }
                }
                this.dispatch_queue_selection(cx);
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::{QueueIdentity, QueueSelection};
    use crate::{
        model::{LibraryRow, Page, QueueRow},
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
            track: LibraryRow {
                id: 1,
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
            },
        }
    }
    fn identity(id: u64, position: usize) -> QueueIdentity {
        QueueIdentity {
            id,
            track_id: 1,
            position,
        }
    }
    #[test]
    fn playing_locate_reveals_exact_duplicate_occurrence_without_selecting() {
        let mut queue = queue(7);
        queue.select(identity(10, 4), false);
        let selected = queue.ordered();
        let anchor = queue.anchor;
        let active = queue.active;
        let generation = queue.projection.generation();
        let request = queue.locate.begin(11, revisions(7));
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
        let old = queue.locate.begin(10, revisions(7));
        assert_eq!(queue.locate.take_job(), Some(old));
        let latest = queue.locate.begin(11, revisions(7));
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
        let stamp = QueueSelection::query().stamp(revisions(7));
        let generation = queue.projection.generation();
        queue.select(identity(1, 0), false);
        assert!(!queue.current(generation, stamp));
        assert!(!queue.current(
            queue.projection.generation(),
            QueueSelection::query().stamp(revisions(8))
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
        let stamp = QueueSelection::query().stamp(revisions(0));
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
            QueueSelection::query().stamp(revisions(1)),
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
        use super::{QueueJob, QueueTarget};
        use crate::gui::metadata::ManualIntents;
        let mut intents = ManualIntents::default();
        let mut queue = queue(0);
        queue.select(identity(1, 0), false);
        let old = intents.begin();
        queue.pending = Some(QueueJob {
            intent: old,
            generation: queue.projection.generation(),
            stamp: QueueSelection::query().stamp(revisions(0)),
            target: QueueTarget::Offset(1),
            anchor: queue.anchor,
            shift: false,
            multi: false,
            scroll: true,
            total: 10,
        });
        let running = queue.take_job().unwrap();
        queue.cancel();
        let latest = intents.begin();
        queue.pending = Some(QueueJob {
            intent: latest,
            generation: queue.projection.generation(),
            stamp: QueueSelection::query().stamp(revisions(0)),
            target: QueueTarget::Offset(8),
            anchor: queue.anchor,
            shift: false,
            multi: false,
            scroll: true,
            total: 10,
        });
        assert!(queue.take_job().is_none());
        assert!(!queue.current(running.generation, running.stamp));
        assert_eq!(queue.ordered_ids(), vec![1]);
        queue.release_worker(running.generation);
        let next = queue.take_job().unwrap();
        assert_eq!(next.intent, latest);
        assert!(matches!(next.target, QueueTarget::Offset(8)));
        assert!(queue.take_job().is_none());
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
        let locate = queue.locate.begin(11, revisions(7));
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
        assert!(queue.current(
            selected_generation,
            QueueSelection::query().stamp(unrelated)
        ));
        assert!(queue.page_projection.accepts(
            &QueueSelection::query(),
            page_generation,
            unrelated
        ));
        assert!(queue.locate.current(locate, unrelated, Some(11)));
        assert!(queue.reconcile_pending.is_none());
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
        queue.page_worker = false;
        queue.rows.fail("offline".into());
        for _ in 0..3 {
            assert!(!queue.sync(revisions(7), 20));
            assert!(queue.demand_page(0..10, 20).is_none());
        }
        assert_eq!(queue.rows.error(0), Some("offline"));
        queue.rows.retry();
        assert!(queue.demand_page(0..10, 20).is_some());
    }

    #[test]
    fn hidden_queue_changes_coalesce_relocation_without_releasing_its_worker() {
        let mut queue = queue(7);
        queue.select(identity(10, 199_999), false);
        queue.select(identity(11, 3), true);
        assert!(queue.sync(revisions(8), 10));
        queue.schedule_reconcile(revisions(8));
        assert!(!queue.page_worker);
        assert!(queue.busy);
        let old = queue.take_reconcile_job().unwrap();
        assert!(queue.sync(revisions(9), 10));
        queue.schedule_reconcile(revisions(9));
        assert!(!queue.reconcile_current(&old, revisions(8)));
        assert!(queue.take_reconcile_job().is_none());
        assert!(queue.take_job().is_none());
        assert_eq!(queue.selected.len(), 2);
        queue.release_reconcile_worker(old.generation);
        let latest = queue.take_reconcile_job().unwrap();
        assert!(queue.reconcile_current(&latest, revisions(9)));
        assert_eq!(latest.ids.len(), 2);
        queue.release_reconcile_worker(latest.generation);
        queue.selection_error = Some("offline".into());
        assert!(!queue.sync(revisions(9), 10));
        assert!(queue.take_reconcile_job().is_none());
        assert_eq!(queue.selected.len(), 2);
        queue.cancel(); // Explicit retry retires the failed logical operation.
        queue.schedule_reconcile(revisions(9));
        assert!(queue.take_reconcile_job().is_some());
        queue.reconcile(vec![identity(10, 0), identity(11, 1)]);
        assert_eq!(queue.ordered_ids(), vec![10, 11]);
    }
    #[test]
    fn page_size_is_demand_not_selection_or_locate_identity() {
        let mut queue = queue(7);
        queue.select(identity(10, 199_999), false);
        let selection_generation = queue.projection.generation();
        let locate = queue.locate.begin(11, revisions(7));
        let (old, generation) = queue.demand_page(0..10, 20).unwrap();
        assert!(queue.demand_page(0..10, 40).is_none());
        assert!(
            !queue
                .page_projection
                .accepts(&old, generation, revisions(7))
        );
        assert!(queue.current(
            selection_generation,
            QueueSelection::query().stamp(revisions(7))
        ));
        assert_eq!(queue.ordered_ids(), vec![10]);
        assert!(queue.locate.current(locate, revisions(7), Some(11)));
        assert!(queue.reconcile_pending.is_none());
        queue.page_worker = false;
        assert!(queue.demand_page(0..10, 40).is_some());
    }
}
