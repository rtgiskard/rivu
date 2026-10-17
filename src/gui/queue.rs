//! Queue pages and compact selections use Query-owned stamps. Filtered views also
//! depend on metadata and retain separate matching offsets and occurrence positions.
//! Page demand, logical generations and physical worker gates have separate lifetimes: retiring
//! work never releases its gate, and only authoritative identity lookups delete a
//! selected occurrence. Hidden state sync does not demand metadata pages; failures
//! remain parked until explicit retry. Now-playing locate never enters selection
//! or metadata intent handling.

use super::metadata::{ManualAction, ManualIntentId};
use super::*;
use super::{
    components::{
        ButtonTooltip, LIST_ROW_REMS, StatusKind, button, caption, compact_icon_button,
        disabled_control, hover_actions, icon_button, list_panel_header, list_viewport,
        panel_surface, panel_toolbar, row, scrollable_list, status_message, track_list_row,
    },
    library::LibraryDrag,
    playlists::PlaylistDrag,
};
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
    all: bool,
    multi: bool,
    scroll: bool,
    total: usize,
    query: Option<String>,
}

/// Selection retains only occurrence identity and absolute position, never track metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct QueueIdentity {
    pub(super) id: u64,
    pub(super) track_id: i64,
    pub(super) position: usize,
    pub(super) offset: usize,
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

    fn begin(
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
    query: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueueWorker {
    Selection(u64),
    Reconcile(u64),
}

#[derive(Default)]
pub(super) struct QueueSelection {
    pub(super) rows: paged::PagedRows<QueueRow>,
    pub(super) query: Option<String>,
    pub(super) locate_filtered: Option<u64>,
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
        self.rows.pin(Some(row.offset));
    }
    fn query(&self) -> Query {
        Query::QueuePage {
            query: self.query.clone(),
            offset: 0,
            limit: 1,
        }
    }
    fn current(&self, generation: u64, stamp: QueryStamp) -> bool {
        self.projection
            .accepts(&self.query(), generation, stamp.revisions())
    }
    fn sync(&mut self, revisions: QueryRevisions, total: usize) -> bool {
        let query = self.query();
        let identity_changed = !self.page_projection.matches_identity(&query);
        let changed = self.page_projection.sync(&query, revisions);
        self.projection.sync(&query, revisions);
        if changed {
            self.locate.invalidate();
            self.locate_filtered = None;
            if identity_changed {
                self.rows.clear();
            } else {
                self.rows.invalidate();
            }
            self.cancel();
        }
        if self.query.is_none() {
            // The snapshot count is authoritative even when DATA invalidation retired the pages.
            self.rows.set_total(total);
        }
        changed
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
        self.busy = true;
        self.reconcile_pending = Some(QueueReconcileJob {
            ids,
            generation: self.reconcile_projection.generation(),
            stamp: query.stamp(revisions),
            query: self.query.clone(),
        });
    }
    fn reconcile_current(&self, job: &QueueReconcileJob, revisions: QueryRevisions) -> bool {
        let query = self.reconcile_query();
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

    pub(super) fn cancel_page_demand(&mut self) {
        if self.page_worker {
            self.page_projection.cancel();
            self.rows.cancel_pending();
        }
        if self.locate.worker.is_some() {
            self.locate.invalidate();
        }
    }

    fn demand_page(&mut self, range: Range<usize>, limit: usize) -> Option<(Query, u64)> {
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
        self.rows.pin(self.active().map(|row| row.offset));
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
        self.rows.pin(Some(target.offset));
        true
    }
}

impl GuiApp {
    pub(super) fn apply_queue_search(&mut self, query: Option<String>, cx: &mut Context<Self>) {
        if self.browser.queue.query == query {
            return;
        }
        self.begin_manual_intent();
        if !self.metadata_protected(cx) {
            self.clear_metadata_selection(cx);
        }
        self.content_menu = None;
        self.browser.queue.clear();
        self.browser.queue.query = query;
        self.browser.queue.locate_filtered = None;
        self.browser.queue.rows.clear();
        self.scroll_views.scroll_list_to("queue", 0);
        self.sync_queue(cx);
        self.ensure_queue_range(0..1, cx);
        cx.notify();
    }
    pub(super) fn locate_playing_queue(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.refresh(cx);
        let Some(id) = self.state.queue.current_id else {
            return;
        };
        self.browser.queue.locate_filtered = None;
        let filter = self.browser.queue.query.clone();
        let request =
            self.browser
                .queue
                .locate
                .begin(id, filter.clone(), self.state.query_revisions(None));
        let position = self
            .browser
            .queue
            .rows
            .iter()
            .find(|row| row.id == id)
            .map(|row| row.offset)
            .or_else(|| {
                filter
                    .is_none()
                    .then_some(self.state.queue.current_position)
                    .flatten()
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
        let ticket = self.browser.queue.locate.projection.ticket();
        let query = self
            .browser
            .queue
            .locate
            .query
            .clone()
            .expect("active locate query");
        cx.spawn(async move |this, cx| {
            let response = cx
                .background_executor()
                .spawn(async move { handle.query_if_current(query, ticket) })
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
                                this.browser.queue.locate.resolve(request, row.offset);
                                this.ensure_queue_range(
                                    row.offset..row.offset.saturating_add(1),
                                    cx,
                                );
                            } else if this.browser.queue.query.is_some() {
                                this.browser.queue.locate_filtered = Some(request.id);
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
        let ticket = self.browser.queue.reconcile_projection.ticket();
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
                    let ticket = ticket.clone();
                    let query = job.query.clone();
                    let response = cx
                        .background_executor()
                        .spawn(async move {
                            handle
                                .query_if_current(Query::QueueEntries { query, queue_ids }, ticket)
                        })
                        .await;
                    if (Query::QueueEntries {
                        query: job.query.clone(),
                        queue_ids: Vec::new(),
                    })
                    .stamp(response.revisions)
                        != job.stamp
                    {
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
        let ticket = self.browser.queue.page_projection.ticket();
        cx.spawn(async move |this, cx| {
            let response = cx
                .background_executor()
                .spawn(async move { handle.query_if_current(query, ticket) })
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
        let stamp = self
            .browser
            .queue
            .query()
            .stamp(self.state.query_revisions(None));
        if shift && anchor.is_some() {
            self.browser.queue.pending = Some(QueueJob {
                intent,
                generation,
                stamp,
                target: QueueTarget::Row(row),
                anchor,
                shift,
                all: false,
                multi,
                scroll: false,
                total: self.browser.queue.rows.len(),
                query: self.browser.queue.query.clone(),
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
        self.browser
            .queue
            .query()
            .stamp(self.state.query_revisions(None))
            == commit.stamp
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
                .scroll_list_to("queue", commit.target.offset);
        }
        self.sync_waveform(cx);
        cx.notify();
    }

    pub(super) fn navigate_queue(&mut self, down: bool, cx: &mut Context<Self>) {
        self.refresh(cx);
        let total = self.browser.queue.rows.len();
        if total == 0 || (self.browser.queue.busy && self.browser.queue.navigation.is_none()) {
            return;
        }
        let current = self
            .browser
            .queue
            .navigation
            .or_else(|| self.browser.queue.active().map(|row| row.offset));
        let offset = match current {
            Some(index) if down => index.saturating_add(1).min(total - 1),
            Some(index) => index.saturating_sub(1),
            None if down => 0,
            None => total - 1,
        };
        if current == Some(offset) {
            return;
        }
        self.start_queue_navigation(offset, false, cx);
    }

    pub(super) fn jump_queue_edge(&mut self, last: bool, cx: &mut Context<Self>) {
        self.refresh(cx);
        let total = self.browser.queue.rows.len();
        if total == 0 || (self.browser.queue.busy && self.browser.queue.navigation.is_none()) {
            return;
        }
        self.start_queue_navigation(if last { total - 1 } else { 0 }, false, cx);
    }

    pub(super) fn select_all_queue_entries(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
        if self
            .browser
            .queue
            .rows
            .total()
            .is_some_and(|total| total > 0)
            && self.browser.queue.ready()
        {
            self.start_queue_navigation(0, true, cx);
        }
    }

    fn start_queue_navigation(&mut self, offset: usize, all: bool, cx: &mut Context<Self>) {
        let anchor = self.browser.queue.anchor;
        let intent = self.begin_manual_intent();
        let stamp = self
            .browser
            .queue
            .query()
            .stamp(self.state.query_revisions(None));
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
            all,
            multi: false,
            scroll: true,
            total: queue.rows.len(),
            query: queue.query.clone(),
        });
        self.dispatch_queue_selection(cx);
        cx.notify();
    }

    fn queue_job_current(&self, job: &QueueJob) -> bool {
        self.manual_intent_current(job.intent)
            && self
                .browser
                .queue
                .query()
                .stamp(self.state.query_revisions(None))
                == job.stamp
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
        let ticket = self.browser.queue.projection.ticket();
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
                        let ticket = ticket.clone();
                        let query = job.query.clone();
                        let response = cx
                            .background_executor()
                            .spawn(async move {
                                handle.query_if_current(
                                    Query::QueuePage {
                                        query,
                                        offset,
                                        limit: 1,
                                    },
                                    ticket,
                                )
                            })
                            .await;
                        if (Query::QueuePage {
                            query: job.query.clone(),
                            offset: 0,
                            limit: 1,
                        })
                        .stamp(response.revisions)
                            != job.stamp
                        {
                            return Err(i18n.text("gl-queue-changed").into());
                        }
                        let ViewResponse::QueuePage(page) =
                            response.result.map_err(|error| error.to_string())?
                        else {
                            return Err(i18n.text("gl-queue-selection-response").into());
                        };
                        let Some(row) = page.rows.first().filter(|row| row.offset == offset) else {
                            return Err(i18n.text("gl-queue-changed").into());
                        };
                        QueueIdentity::from(row)
                    }
                };
                let anchor = if job.all {
                    target
                } else {
                    job.anchor.unwrap_or(target)
                };
                let mut rows = None;
                if job.all || (job.shift && job.anchor.is_some()) {
                    let (start, end) = if job.all {
                        (0, job.total)
                    } else {
                        (
                            anchor.offset.min(target.offset),
                            anchor.offset.max(target.offset) + 1,
                        )
                    };
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
                        let ticket = ticket.clone();
                        let query = job.query.clone();
                        let response = cx
                            .background_executor()
                            .spawn(async move {
                                handle.query_if_current(
                                    Query::QueuePage {
                                        query,
                                        offset,
                                        limit,
                                    },
                                    ticket,
                                )
                            })
                            .await;
                        if (Query::QueuePage {
                            query: job.query.clone(),
                            offset: 0,
                            limit: 1,
                        })
                        .stamp(response.revisions)
                            != job.stamp
                        {
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
                                .any(|(index, row)| row.offset != offset + index)
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
            offset: position,
        }
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
            matches!(query, crate::model::Query::QueuePage { query: Some(query), offset: 0, .. } if query == "needle")
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
        use super::{QueueJob, QueueTarget};
        use crate::gui::metadata::ManualIntents;
        let mut intents = ManualIntents::default();
        let mut queue = queue(0);
        queue.select(identity(1, 0), false);
        let old = intents.begin();
        queue.pending = Some(QueueJob {
            intent: old,
            generation: queue.projection.generation(),
            stamp: queue.query().stamp(revisions(0)),
            target: QueueTarget::Offset(1),
            anchor: queue.anchor,
            shift: false,
            all: false,
            query: None,
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
            stamp: queue.query().stamp(revisions(0)),
            target: QueueTarget::Offset(8),
            anchor: queue.anchor,
            shift: false,
            all: false,
            query: None,
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
}

pub(super) struct QueueDrag {
    owner: EntityId,
    source: queue::QueueIdentity,
    // GPUI retains this payload; the drag constructor only creates its preview.
    entries: std::cell::OnceCell<Vec<queue::QueueIdentity>>,
    revision: u64,
}

impl QueueDrag {
    pub(super) fn can_transfer(&self, app: &GuiApp, destination: DragDestination) -> bool {
        matches!(destination, DragDestination::Playlist(_))
            && self.revision == app.state.queue.revision
            && self.entries.get().is_some_and(|rows| !rows.is_empty())
    }

    pub(super) fn track_ids(&self) -> Vec<i64> {
        self.entries
            .get()
            .unwrap()
            .iter()
            .map(|row| row.track_id)
            .collect()
    }

    fn insertion_index(&self, target: Option<queue::QueueIdentity>, total: usize) -> Option<usize> {
        let entries = self.entries.get()?;
        if entries.is_empty()
            || target.is_some_and(|target| entries.iter().any(|row| row.id == target.id))
        {
            return None;
        }
        let position = target.map_or(total, |target| target.position);
        position.checked_sub(entries.iter().filter(|row| row.position < position).count())
    }
}

impl GuiApp {
    fn send_queue_row_action(&mut self, revision: u64, command: Command, cx: &mut Context<Self>) {
        self.refresh(cx);
        if self.state.queue.revision == revision {
            self.send(command, cx);
        }
    }

    pub(super) fn selected_queue_ids_in_order(&self) -> Vec<u64> {
        self.browser.queue.ordered_ids()
    }

    fn queue_drop_index(
        &self,
        drag: &QueueDrag,
        target_id: Option<u64>,
        owner: gpui::EntityId,
    ) -> Option<usize> {
        if self.browser.queue.query.is_some()
            || drag.owner != owner
            || drag.revision != self.state.queue.revision
        {
            return None;
        }
        let target = match target_id {
            Some(id) => Some(
                self.browser
                    .queue
                    .rows
                    .iter()
                    .find(|row| row.id == id)
                    .map(super::queue::QueueIdentity::from)?,
            ),
            None => None,
        };
        drag.insertion_index(target, self.state.queue.total)
    }

    fn move_queue_entry(
        &mut self,
        drag: &QueueDrag,
        target_id: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        let Some(index) = self.queue_drop_index(drag, target_id, cx.entity().entity_id()) else {
            return;
        };
        self.send(
            Command::MoveQueueEntries {
                queue_ids: drag
                    .entries
                    .get()
                    .unwrap()
                    .iter()
                    .map(|row| row.id)
                    .collect(),
                index,
            },
            cx,
        );
    }

    pub(super) fn queue_panel(
        &mut self,
        panel_id: u64,
        _tab_bar_visible: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = crate::gui::theme::palette(cx);
        let matched = self.browser.queue.rows.total();
        let total = self.state.queue.total;
        self.queue_search.update(cx, |search, cx| {
            if let Some(matched) = matched {
                search.set_counts(matched, total, cx);
            } else {
                search.clear_counts(cx);
            }
        });
        let selection_count = self.browser.queue.len();
        let header = list_panel_header(
            theme,
            self.tr("gl-queue"),
            self.i18n.format(
                "gl-queue-count",
                &[
                    ("total", (self.state.queue.total).into()),
                    ("selected", (selection_count).into()),
                ],
            ),
        )
        .flex_1()
        .min_w(gpui::rems(3.))
        .id(("queue-count", panel_id))
        .tooltip({
            let text = self.tr("gl-queue-drag-hint");
            move |_, cx| cx.new(|_| ButtonTooltip { text: text.clone() }).into()
        });
        let tools = panel_toolbar()
            .min_h(px(32.))
            .child(self.queue_search.clone())
            .child(disabled_control(
                theme,
                icon_button(
                    ("queue-locate-playing", panel_id),
                    "",
                    self.tr("gl-locate-playing-entry"),
                    cx,
                    |this, window, cx| this.locate_playing_queue(window, cx),
                ),
                self.state.queue.current_id.is_none(),
            ))
            .min_w_0()
            .max_w(gpui::rems(20.5))
            .flex_shrink(1.);
        let (selection_message, selection_kind): (SharedString, _) =
            if let Some(error) = &self.browser.queue.selection_error {
                (error.clone().into(), StatusKind::Error)
            } else if self.browser.queue.busy {
                (
                    self.i18n.text("gl-selection-loading").into(),
                    StatusKind::Info,
                )
            } else {
                ("".into(), StatusKind::Info)
            };
        let status = row()
            .h(gpui::rems(2.))
            .flex_shrink_0()
            .overflow_hidden()
            .child(
                status_message(theme, selection_message.clone(), selection_kind)
                    .id(("queue-selection-status", panel_id))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .tooltip(move |_, cx| {
                        cx.new(|_| ButtonTooltip {
                            text: selection_message.clone(),
                        })
                        .into()
                    }),
            )
            .when(self.browser.queue.selection_error.is_some(), |status| {
                status.child(icon_button(
                    ("queue-selection-retry", panel_id),
                    "󰑐",
                    self.tr("gl-retry-selection"),
                    cx,
                    |this, _, cx| {
                        if this.browser.queue.selection_error.is_some() {
                            this.refresh_queue(cx);
                            cx.notify();
                        }
                    },
                ))
            });
        let scroll = self.scroll_views.list(("queue", panel_id));
        self.reveal_queue_locate(&scroll, cx);
        let panel_drop_owner = cx.entity().downgrade();
        let list_drop_owner = panel_drop_owner.clone();
        panel_surface(("queue-panel", panel_id))
            .on_drop(cx.listener(|this, drag: &QueueDrag, window, cx| {
                window.prevent_default();
                this.move_queue_entry(drag, None, cx);
            }))
            .can_drop(move |value, _, cx| {
                panel_drop_owner.upgrade().is_some_and(|owner| {
                    let this = owner.read(cx);
                    if let Some(drag) = value.downcast_ref::<QueueDrag>() {
                        this.queue_drop_index(drag, None, owner.entity_id())
                            .is_some()
                    } else if let Some(drag) = value.downcast_ref::<LibraryDrag>() {
                        this.can_resolve_library_drag(drag)
                    } else {
                        this.can_transfer_content(value, super::DragDestination::Queue)
                    }
                })
            })
            .drag_over::<QueueDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .on_drop(cx.listener(|this, drag: &LibraryDrag, window, cx| {
                window.prevent_default();
                this.resolve_library_drag(drag.nodes(), super::DragDestination::Queue, cx);
            }))
            .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .on_drop(cx.listener(|this, drag: &super::ContentDrag, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                this.drop_content(drag, super::DragDestination::Queue, cx);
            }))
            .on_drop(cx.listener(|this, drag: &PlaylistDrag, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                this.transfer_playlist_drag(drag, super::DragDestination::Queue, cx);
            }))
            .drag_over::<super::ContentDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .drag_over::<PlaylistDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
            .child(row().items_end().child(header).child(tools))
            .when(
                self.browser
                    .queue
                    .locate_filtered
                    .is_some_and(|id| Some(id) == self.state.queue.current_id),
                |panel| {
                    panel.child(button(
                        ("queue-clear-and-locate", panel_id),
                        self.tr("search-clear-and-locate"),
                        cx,
                        |this, window, cx| {
                            this.queue_search.update(cx, |search, cx| search.clear(cx));
                            this.apply_queue_search(None, cx);
                            this.locate_playing_queue(window, cx);
                        },
                    ))
                },
            )
            .when(matched == Some(0), |panel| {
                panel.child(caption(
                    theme,
                    self.tr(if total == 0 {
                        "search-empty"
                    } else {
                        "search-no-matches"
                    }),
                ))
            })
            .when(
                self.browser.queue.busy || self.browser.queue.selection_error.is_some(),
                |panel| panel.child(status),
            )
            .child(list_viewport(scrollable_list(
                ("queue-scrollbar", panel_id),
                uniform_list(
                    ("queue-rows", panel_id),
                    self.browser.queue.rows.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                        range
                            .map(|index| {
                                let Some(entry) = this.browser.queue.rows.get(index) else {
                                    return this
                                        .paged_loading_row(ViewRequestKind::Queue, index, cx)
                                        .h(gpui::rems(LIST_ROW_REMS));
                                };
                                let id = entry.id;
                                let track_id = entry.track_id;
                                let favorite = entry.track.favorite;
                                let position = entry.position;
                                let can_move_down = position + 1 < this.state.queue.total;
                                let revision = this.state.queue.revision;
                                let (title, detail) = this.panel_track_text(&entry.track);
                                let playing = this.state.queue.current_id == Some(id);
                                let drag_owner = cx.entity().downgrade();
                                let drop_owner = drag_owner.clone();
                                let drag_i18n = std::sync::Arc::clone(&this.i18n);
                                let drag = QueueDrag {
                                    owner: cx.entity().entity_id(),
                                    source: super::queue::QueueIdentity::from(entry),
                                    entries: Default::default(),
                                    revision: this.state.queue.revision,
                                };
                                track_list_row(
                                    theme,
                                    ("queue-entry", id),
                                    ("queue-entry-text", id),
                                    (title, detail),
                                    index,
                                    playing,
                                    this.browser.queue.contains(&id),
                                )
                                .when(this.queue_locate_emphasized(id), |row| {
                                    row.border_1()
                                        .border_color(rgb(theme.accent))
                                        .bg(rgb(theme.highlight))
                                })
                                .when(
                                    this.layout_focus.is_focused(window)
                                        && this.browser.focus()
                                            == Some(super::BrowserFocus::List(ListFocus::Queue))
                                        && this
                                            .browser
                                            .queue
                                            .active()
                                            .is_some_and(|row| row.id == id),
                                    |row| row.border_color(rgb(theme.accent)),
                                )
                                .group("queue-track-row")
                                .line_height(gpui::rems(1.125))
                                .relative()
                                .child(
                                    hover_actions(
                                        theme,
                                        "queue-track-row",
                                        this.browser.queue.contains(&id),
                                    )
                                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation();
                                    })
                                    .child(compact_icon_button(
                                        theme,
                                        icon_button(
                                            ("queue-entry-play", id),
                                            "",
                                            this.tr("gl-play-queue-entry"),
                                            cx,
                                            move |this, _, cx| {
                                                this.send_queue_row_action(
                                                    revision,
                                                    Command::PlayQueue { queue_id: id },
                                                    cx,
                                                )
                                            },
                                        ),
                                    ))
                                    .child(this.track_favorite_toggle(
                                        ("queue-entry-favorite", id),
                                        "queue-track-row",
                                        track_id,
                                        favorite,
                                        cx,
                                    ))
                                    .child(compact_icon_button(
                                        theme,
                                        icon_button(
                                            ("queue-entry-remove", id),
                                            "",
                                            this.tr("gl-remove-queue"),
                                            cx,
                                            move |this, _, cx| {
                                                this.send_queue_row_action(
                                                    revision,
                                                    Command::RemoveQueueEntries {
                                                        queue_ids: vec![id],
                                                    },
                                                    cx,
                                                )
                                            },
                                        ),
                                    ))
                                    .child(disabled_control(
                                        theme,
                                        compact_icon_button(
                                            theme,
                                            icon_button(
                                                ("queue-entry-up", id),
                                                "",
                                                this.tr("gl-entry-up"),
                                                cx,
                                                move |this, _, cx| {
                                                    if position > 0 {
                                                        this.send_queue_row_action(
                                                            revision,
                                                            Command::MoveQueue {
                                                                queue_id: id,
                                                                index: position - 1,
                                                            },
                                                            cx,
                                                        );
                                                    }
                                                },
                                            ),
                                        ),
                                        position == 0,
                                    ))
                                    .child(disabled_control(
                                        theme,
                                        compact_icon_button(
                                            theme,
                                            icon_button(
                                                ("queue-entry-down", id),
                                                "",
                                                this.tr("gl-entry-down"),
                                                cx,
                                                move |this, _, cx| {
                                                    if can_move_down {
                                                        this.send_queue_row_action(
                                                            revision,
                                                            Command::MoveQueue {
                                                                queue_id: id,
                                                                index: position + 1,
                                                            },
                                                            cx,
                                                        );
                                                    }
                                                },
                                            ),
                                        ),
                                        !can_move_down,
                                    )),
                                )
                                .on_mouse_down(
                                    gpui::MouseButton::Right,
                                    cx.listener(
                                        move |this, event: &gpui::MouseDownEvent, window, cx| {
                                            this.focus_layout(window, cx);
                                            this.browser.focus_list(ListFocus::Queue);
                                            if this.browser.queue.contains(&id)
                                                && !this.browser.queue.ready()
                                            {
                                                cx.stop_propagation();
                                                return;
                                            }
                                            if !this.browser.queue.contains(&id) {
                                                this.select_queue_entry(id, false, false, cx);
                                                cx.notify();
                                            }
                                            let rows = this.browser.queue.ordered();
                                            let track_ids = if rows.iter().any(|row| row.id == id) {
                                                rows.iter().map(|row| row.track_id).collect()
                                            } else {
                                                vec![track_id]
                                            };
                                            let queue_ids = if rows.iter().any(|row| row.id == id) {
                                                rows.iter().map(|row| row.id).collect()
                                            } else {
                                                vec![id]
                                            };
                                            this.open_object_menu(
                                                super::ContentMenuRequest {
                                                    source: super::TrackSource::Tracks(track_ids),
                                                    position: event.position,
                                                    play: Some(Command::PlayQueue { queue_id: id }),
                                                },
                                                super::list_interaction::ContentGuard::Queue(
                                                    revision,
                                                ),
                                                Some(Command::RemoveQueueEntries { queue_ids }),
                                                None,
                                                None,
                                                cx,
                                            );
                                            cx.stop_propagation();
                                        },
                                    ),
                                )
                                .on_click(cx.listener(
                                    move |this, event: &gpui::ClickEvent, window, cx| {
                                        this.focus_layout(window, cx);
                                        this.browser.focus_list(ListFocus::Queue);
                                        let modifiers = event.modifiers();
                                        this.select_queue_entry(
                                            id,
                                            modifiers.shift,
                                            modifiers.control,
                                            cx,
                                        );
                                        if event.click_count() == 2 {
                                            this.send(Command::PlayQueue { queue_id: id }, cx);
                                        }
                                    },
                                ))
                                .when(
                                    this.browser.queue.ready()
                                        && this.browser.queue.query.is_none()
                                        && this.browser.queue.selection_error.is_none(),
                                    |row| {
                                        row.on_drag(drag, move |drag, _, window, cx| {
                                            super::components::begin_list_drag(window, cx);
                                            drag.entries.get_or_init(|| {
                                                drag_owner
                                                    .update(cx, |this, cx| {
                                                        this.refresh(cx);
                                                        if drag.revision
                                                            != this.state.queue.revision
                                                            || !this.browser.queue.ready()
                                                            || this.browser.queue.query.is_some()
                                                            || this
                                                                .browser
                                                                .queue
                                                                .selection_error
                                                                .is_some()
                                                        {
                                                            return Vec::new();
                                                        }
                                                        if this
                                                            .browser
                                                            .queue
                                                            .contains(&drag.source.id)
                                                        {
                                                            this.browser.queue.ordered()
                                                        } else {
                                                            vec![drag.source]
                                                        }
                                                    })
                                                    .unwrap_or_default()
                                            });
                                            let label = drag_i18n
                                                .format(
                                                    "gl-move-queue-entries",
                                                    &[(
                                                        "count",
                                                        drag.entries
                                                            .get()
                                                            .map_or(0, Vec::len)
                                                            .into(),
                                                    )],
                                                )
                                                .into();
                                            cx.new(|_| super::components::ListDragPreview { label })
                                        })
                                    },
                                )
                                .on_drop(cx.listener(move |this, drag: &QueueDrag, window, cx| {
                                    window.prevent_default();
                                    cx.stop_propagation();
                                    this.move_queue_entry(drag, Some(id), cx);
                                }))
                                .can_drop(move |value, _, cx| {
                                    let Some(drag) = value.downcast_ref::<QueueDrag>() else {
                                        return false;
                                    };
                                    drop_owner.upgrade().is_some_and(|owner| {
                                        owner
                                            .read(cx)
                                            .queue_drop_index(drag, Some(id), owner.entity_id())
                                            .is_some()
                                    })
                                })
                                .drag_over::<QueueDrag>(
                                    |style, _, _, _| style.bg(rgb(theme.highlight)),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .with_decoration(paged::viewport_demand(
                    cx.processor(|this, range, _, cx| this.ensure_queue_range(range, cx)),
                ))
                .on_drop(cx.listener(|this, drag: &QueueDrag, window, cx| {
                    window.prevent_default();
                    this.move_queue_entry(drag, None, cx);
                }))
                .can_drop(move |value, _, cx| {
                    list_drop_owner.upgrade().is_some_and(|owner| {
                        let this = owner.read(cx);
                        if let Some(drag) = value.downcast_ref::<QueueDrag>() {
                            this.queue_drop_index(drag, None, owner.entity_id())
                                .is_some()
                        } else if let Some(drag) = value.downcast_ref::<LibraryDrag>() {
                            this.can_resolve_library_drag(drag)
                        } else {
                            this.can_transfer_content(value, super::DragDestination::Queue)
                        }
                    })
                })
                .drag_over::<QueueDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
                .on_drop(cx.listener(|this, drag: &LibraryDrag, window, cx| {
                    window.prevent_default();
                    this.resolve_library_drag(drag.nodes(), super::DragDestination::Queue, cx);
                }))
                .drag_over::<LibraryDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
                .on_drop(cx.listener(|this, drag: &super::ContentDrag, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                    this.drop_content(drag, super::DragDestination::Queue, cx);
                }))
                .on_drop(cx.listener(|this, drag: &PlaylistDrag, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                    this.transfer_playlist_drag(drag, super::DragDestination::Queue, cx);
                }))
                .drag_over::<super::ContentDrag>(|style, _, _, _| {
                    style.border_color(rgb(theme.accent))
                })
                .drag_over::<PlaylistDrag>(|style, _, _, _| style.border_color(rgb(theme.accent)))
                .h_full()
                .min_h_0()
                .w_full()
                .track_scroll(&scroll),
                &scroll,
            )))
            .into_any_element()
    }
}
#[cfg(test)]
mod queue_drag_tests {
    use super::{QueueDrag, QueueIdentity};

    fn identity(position: usize) -> QueueIdentity {
        QueueIdentity {
            id: position as u64 + 10,
            track_id: 7,
            position,
            offset: position,
        }
    }

    fn drag(positions: &[usize]) -> QueueDrag {
        QueueDrag {
            owner: 1_u64.into(),
            source: identity(positions[0]),
            entries: std::cell::OnceCell::from(
                positions.iter().copied().map(identity).collect::<Vec<_>>(),
            ),
            revision: 4,
        }
    }

    fn reordered(drag: &QueueDrag, target: Option<usize>) -> Vec<u64> {
        let index = drag.insertion_index(target.map(identity), 5).unwrap();
        let selected: Vec<_> = drag
            .entries
            .get()
            .unwrap()
            .iter()
            .map(|row| row.id)
            .collect();
        let mut remaining: Vec<_> = (0..5)
            .map(identity)
            .map(|row| row.id)
            .filter(|id| !selected.contains(id))
            .collect();
        remaining.splice(index..index, selected);
        remaining
    }

    #[test]
    fn queue_drop_inserts_before_target_or_appends_in_source_order() {
        assert_eq!(reordered(&drag(&[1]), Some(4)), [10, 12, 13, 11, 14]);
        assert_eq!(reordered(&drag(&[3]), Some(1)), [10, 13, 11, 12, 14]);
        assert_eq!(reordered(&drag(&[1]), None), [10, 12, 13, 14, 11]);
        // All rows share a track ID; distinct queue occurrences must survive.
        let group = drag(&[1, 3]);
        assert_eq!(reordered(&group, Some(4)), [10, 12, 11, 13, 14]);
        assert_eq!(reordered(&group, Some(0)), [11, 13, 10, 12, 14]);
        assert_eq!(reordered(&group, None), [10, 12, 14, 11, 13]);
        assert_eq!(group.insertion_index(Some(identity(1)), 5), None);
        assert_eq!(group.insertion_index(Some(identity(3)), 5), None);
    }
}
