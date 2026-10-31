use super::model::{QueueJob, QueueLocateRequest, QueueReconcileJob, QueueTarget};
use super::{Queue, QueueCommit, QueueEvent, QueueIdentity, QueueSelectionRequest};
use gpui::{Context, Window};
use rivu_protocol::{
    model::{Page, Query},
    response::ViewResponse,
};
use std::{ops::Range, time::Duration};

impl Queue {
    pub(in crate::gui) fn apply_queue_search(
        &mut self,
        query: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.model.query == query {
            return;
        }
        cx.emit(QueueEvent::SearchChanged);
        self.model.change_search(query);
        self.context.scroll.borrow_mut().scroll_list_to("queue", 0);
        self.sync_queue(cx);
        self.retain_loading(cx);
        self.ensure_queue_range(0..1, cx);
        cx.notify();
    }
    pub(in crate::gui) fn locate_playing_queue(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        let Some(id) = self.state.queue.current_id else {
            return;
        };
        self.model.locate_filtered = None;
        let filter = self.model.query.clone();
        let request = self
            .model
            .locate
            .begin(id, filter.clone(), self.state.query_revisions(None));
        let position = self
            .model
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
            self.model.locate.resolve(request, position);
            self.ensure_queue_range(position..position.saturating_add(1), cx);
        } else {
            self.dispatch_queue_locate(cx);
        }
        cx.notify();
    }

    fn queue_locate_current(&self, request: QueueLocateRequest) -> bool {
        self.model.locate.current(
            request,
            self.state.query_revisions(None),
            self.state.queue.current_id,
        )
    }

    fn dispatch_queue_locate(&mut self, cx: &mut Context<Self>) {
        let Some(request) = self.model.locate.take_job() else {
            return;
        };
        if !self.queue_locate_current(request) {
            self.model.locate.release_worker(request);
            return;
        }
        let handle = self.context.handle().clone();
        let ticket = self.model.locate.projection.ticket();
        let query = self
            .model
            .locate
            .query
            .clone()
            .expect("active locate query");
        cx.spawn(async move |this, cx| {
            let mut timed_out = false;
            let response = crate::gui::query::run(
                handle,
                cx,
                move |handle| async move { handle.query_if_current(query, ticket).await },
                |response, cx| {
                    timed_out = true;
                    let _ = this.update(cx, |this, cx| {
                        this.refresh(cx);
                        if this.queue_locate_current(request)
                            && this.model.locate.current(
                                request,
                                response.revisions,
                                this.state.queue.current_id,
                            )
                        {
                            this.model.locate.invalidate();
                            if let Err(error) = response.result {
                                cx.emit(QueueEvent::Error(error.to_string()));
                            }
                            cx.notify();
                        }
                    });
                },
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                this.model.locate.release_worker(request);
                this.refresh(cx);
                if !timed_out
                    && this.queue_locate_current(request)
                    && this.model.locate.current(
                        request,
                        response.revisions,
                        this.state.queue.current_id,
                    )
                {
                    match response.result {
                        Ok(ViewResponse::QueueEntries(rows)) => {
                            if let Some(row) = rows.into_iter().find(|row| row.id == request.id) {
                                this.model.locate.resolve(request, row.offset);
                                this.ensure_queue_range(
                                    row.offset..row.offset.saturating_add(1),
                                    cx,
                                );
                            } else if this.model.query.is_some() {
                                this.model.locate_filtered = Some(request.id);
                            } else {
                                cx.emit(QueueEvent::Error(
                                    this.context.i18n.text("gl-queue-locate-missing").to_owned(),
                                ));
                            }
                        }
                        Err(error) => cx.emit(QueueEvent::Error(error.to_string())),
                        _ => {
                            cx.emit(QueueEvent::Error(
                                this.context
                                    .i18n
                                    .text("gl-queue-locate-response")
                                    .to_owned(),
                            ));
                        }
                    }
                }
                this.dispatch_queue_locate(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::gui) fn reveal_queue_locate(
        &mut self,
        scroll: &gpui::UniformListScrollHandle,
        cx: &mut Context<Self>,
    ) {
        let Some(reveal) = self.model.locate.reveal else {
            return;
        };
        if !self.queue_locate_current(reveal.request) {
            self.model.locate.invalidate();
            return;
        }
        if reveal.scroll {
            scroll.scroll_to_item(reveal.position, gpui::ScrollStrategy::Nearest);
            self.model.locate.reveal.as_mut().unwrap().scroll = false;
        }
        let Some(row) = self.model.rows.get(reveal.position) else {
            return;
        };
        let id = row.id;
        self.model.locate.reveal = None;
        if id != reveal.request.id {
            return;
        }
        let request = reveal.request;
        self.model.locate.emphasized = Some(request);
        let timer = cx.background_executor().timer(Duration::from_millis(1500));
        self.model.locate.timer = Some(cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                if this.model.locate.emphasized == Some(request) {
                    this.model.locate.emphasized = None;
                    cx.notify();
                }
            });
        }));
    }

    pub(in crate::gui) fn queue_locate_emphasized(&self, id: u64) -> bool {
        self.model
            .locate
            .emphasized
            .is_some_and(|request| request.id == id && self.queue_locate_current(request))
    }

    /// Reconcile authoritative queue state even while its pane is hidden. No page demand.
    pub(in crate::gui) fn sync_queue(&mut self, cx: &mut Context<Self>) -> bool {
        let revisions = self.state.query_revisions(None);
        let intent = self.model.manual;
        if !self.model.sync(revisions, self.state.queue.total) {
            return false;
        }
        if let Some(intent) = intent {
            cx.emit(QueueEvent::SelectionFailed(intent));
        }
        self.dispatch_queue_selection(cx);
        true
    }

    /// Explicit retry of failed identity relocation; unchanged pages and locate survive.
    pub(in crate::gui) fn refresh_queue(&mut self, cx: &mut Context<Self>) {
        if self
            .model
            .retry_selection(self.state.query_revisions(None), self.state.queue.total)
        {
            self.dispatch_queue_selection(cx);
        }
    }

    pub(in crate::gui) fn retry_queue_page(&mut self, cx: &mut Context<Self>) {
        let range = self
            .model
            .retry_page(self.state.query_revisions(None), self.state.queue.total);
        self.dispatch_queue_selection(cx);
        self.ensure_queue_range(range, cx);
    }

    fn dispatch_queue_reconcile(&mut self, job: QueueReconcileJob, cx: &mut Context<Self>) {
        let handle = self.context.handle().clone();
        let i18n = std::sync::Arc::clone(&self.context.i18n);
        let ticket = self.model.reconcile_projection.ticket();
        cx.spawn(async move |this, cx| {
            let result: Result<Option<Vec<QueueIdentity>>, String> = async {
                let mut selected = Vec::with_capacity(job.ids.len());
                for chunk in job.ids.chunks(rivu_protocol::model::MAX_QUERY_ROWS) {
                    if !this
                        .update(cx, |this, cx| {
                            this.refresh(cx);
                            this.model
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
                    let response = crate::gui::query::run(
                        handle,
                        cx,
                        move |handle| async move {
                            handle
                                .query_if_current(Query::QueueEntries { query, queue_ids }, ticket)
                                .await
                        },
                        |response, cx| {
                            let _ = this.update(cx, |this, cx| {
                                this.refresh(cx);
                                if this
                                    .model
                                    .reconcile_current(&job, this.state.query_revisions(None))
                                    && this.model.reconcile_current(&job, response.revisions)
                                    && let Err(error) = response.result
                                {
                                    let error = error.to_string();
                                    this.model.finish_reconcile(
                                        &job,
                                        response.revisions,
                                        Err(error.clone()),
                                    );
                                    cx.emit(QueueEvent::Error(error));
                                    cx.notify();
                                }
                            });
                        },
                    )
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
                this.model.release_reconcile_worker(job.generation);
                this.refresh(cx);
                if this
                    .model
                    .reconcile_current(&job, this.state.query_revisions(None))
                {
                    match result {
                        Ok(Some(selected)) => {
                            this.model.finish_reconcile(
                                &job,
                                this.state.query_revisions(None),
                                Ok(selected),
                            );
                        }
                        Err(error) => {
                            this.model.finish_reconcile(
                                &job,
                                this.state.query_revisions(None),
                                Err(error.clone()),
                            );
                            cx.emit(QueueEvent::Error(error));
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

    pub(in crate::gui) fn ensure_queue_range(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) {
        self.sync_queue(cx);
        if !self.visible {
            return;
        }
        let limit = self.state.system.config.query_batch_size as usize;
        let Some((query, generation)) = self.model.demand_page(range, limit) else {
            return;
        };
        self.loading
            .begin((), generation, std::time::Instant::now());
        self.loading.schedule(cx, |this| &mut this.loading);
        let request = query.clone();
        let handle = self.context.handle().clone();
        let ticket = self.model.page_projection.ticket();
        cx.spawn(async move |this, cx| {
            let mut timed_out = false;
            let response = crate::gui::query::run(
                handle,
                cx,
                move |handle| async move { handle.query_if_current(query, ticket).await },
                |response, cx| {
                    timed_out = true;
                    let _ = this.update(cx, |this, cx| {
                        this.refresh(cx);
                        this.sync_queue(cx);
                        this.model.accept_page(&request, generation, response, || {
                            this.context.i18n.text("gl-queue-page-response").to_owned()
                        });
                        this.loading.finish(&(), generation);
                        this.loading.schedule(cx, |this| &mut this.loading);
                        cx.notify();
                    });
                },
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                this.loading.finish(&(), generation);
                this.loading.schedule(cx, |this| &mut this.loading);
                this.model.retire_page_worker();
                this.refresh(cx);
                this.sync_queue(cx);
                if !timed_out {
                    this.model.accept_page(&request, generation, response, || {
                        this.context.i18n.text("gl-queue-page-response").to_owned()
                    });
                }
                this.demand_visible_page(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::gui) fn select_queue_entry(
        &mut self,
        id: u64,
        shift: bool,
        multi: bool,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if !self.model.ready() && (shift || multi) {
            return;
        }
        let Some(row) = self
            .model
            .rows
            .iter()
            .find(|row| row.id == id)
            .map(QueueIdentity::from)
        else {
            return;
        };
        cx.emit(QueueEvent::SelectionRequested(QueueSelectionRequest::Row {
            id: row.id,
            shift,
            multi,
        }));
    }

    pub(in crate::gui) fn queue_commit_valid(&self, commit: &QueueCommit) -> bool {
        self.model.query().stamp(self.state.query_revisions(None)) == commit.stamp
            && self.model.current(commit.generation, commit.stamp)
    }

    pub(in crate::gui) fn commit_queue_selection(
        &mut self,
        commit: QueueCommit,
        cx: &mut Context<Self>,
    ) {
        if !self.queue_commit_valid(&commit) {
            return;
        }
        let queue = &mut self.model;
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
        cx.emit(QueueEvent::SelectionCommitted {
            track_id: commit.target.track_id,
        });
        if commit.scroll {
            self.context
                .scroll
                .borrow_mut()
                .scroll_list_to("queue", commit.target.offset);
        }
        cx.notify();
    }

    pub(in crate::gui) fn navigate_queue(&mut self, down: bool, cx: &mut Context<Self>) {
        self.refresh(cx);
        let total = self.model.rows.len();
        if total == 0 || (!self.model.ready() && self.model.navigation.is_none()) {
            return;
        }
        let current = self
            .model
            .navigation
            .or_else(|| self.model.active().map(|row| row.offset));
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

    pub(in crate::gui) fn jump_queue_edge(&mut self, last: bool, cx: &mut Context<Self>) {
        self.refresh(cx);
        let total = self.model.rows.len();
        if total == 0 || (!self.model.ready() && self.model.navigation.is_none()) {
            return;
        }
        self.start_queue_navigation(if last { total - 1 } else { 0 }, false, cx);
    }

    pub(in crate::gui) fn select_all_queue_entries(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
        if self.model.rows.total().is_some_and(|total| total > 0)
            && (self.model.ready() || self.model.manual.is_some())
        {
            self.start_queue_navigation(0, true, cx);
        }
    }

    fn start_queue_navigation(&mut self, offset: usize, all: bool, cx: &mut Context<Self>) {
        cx.emit(QueueEvent::SelectionRequested(
            QueueSelectionRequest::Offset { offset, all },
        ));
    }

    fn timeout_queue_selection(
        &mut self,
        job: &QueueJob,
        response: rivu_protocol::response::QueryResponse,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if self.queue_job_current(job)
            && self.model.query().stamp(response.revisions) == job.stamp
            && let Err(error) = response.result
        {
            let error = error.to_string();
            self.model.finish_selection(job, Some(&error));
            cx.emit(QueueEvent::SelectionFailed(job.intent));
            cx.emit(QueueEvent::Error(error));
            cx.notify();
        }
    }

    fn queue_job_current(&self, job: &QueueJob) -> bool {
        self.model.manual == Some(job.intent)
            && self.model.query().stamp(self.state.query_revisions(None)) == job.stamp
            && self.model.current(job.generation, job.stamp)
    }

    pub(in crate::gui) fn dispatch_queue_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = self.model.take_reconcile_job() {
            self.dispatch_queue_reconcile(job, cx);
            return;
        }
        let Some(job) = self.model.take_job() else {
            return;
        };
        let handle = self.context.handle().clone();
        let i18n = std::sync::Arc::clone(&self.context.i18n);
        let ticket = self.model.projection.ticket();
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
                        let response = crate::gui::query::run(
                            handle,
                            cx,
                            move |handle| async move {
                                handle
                                    .query_if_current(
                                        Query::QueuePage {
                                            query,
                                            offset,
                                            limit: 1,
                                        },
                                        ticket,
                                    )
                                    .await
                            },
                            |response, cx| {
                                let _ = this.update(cx, |this, cx| {
                                    this.timeout_queue_selection(&job, response, cx);
                                });
                            },
                        )
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
                        let limit = (end - offset).min(rivu_protocol::model::MAX_QUERY_ROWS);
                        let handle = handle.clone();
                        let ticket = ticket.clone();
                        let query = job.query.clone();
                        let response = crate::gui::query::run(
                            handle,
                            cx,
                            move |handle| async move {
                                handle
                                    .query_if_current(
                                        Query::QueuePage {
                                            query,
                                            offset,
                                            limit,
                                        },
                                        ticket,
                                    )
                                    .await
                            },
                            |response, cx| {
                                let _ = this.update(cx, |this, cx| {
                                    this.timeout_queue_selection(&job, response, cx);
                                });
                            },
                        )
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
                this.model.release_worker(job.generation);
                this.refresh(cx);
                if this.queue_job_current(&job)
                    && this.model.finish_selection(&job, result.as_ref().err())
                {
                    match result {
                        Ok(Some(commit)) => {
                            cx.emit(QueueEvent::SelectionReady {
                                intent: job.intent,
                                commit: std::cell::RefCell::new(Some(commit)),
                            });
                        }
                        Err(message) => {
                            cx.emit(QueueEvent::SelectionFailed(job.intent));
                            cx.emit(QueueEvent::Error(message));
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
