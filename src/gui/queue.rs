//! Queue metadata lives in bounded viewport pages. Revision changes discard positional
//! payloads and relocate selected occurrence IDs through bounded worker lookups;
//! current-only playback updates do not invalidate pages. Range selection streams
//! pages into compact identities and commits only for its original selection token.

use super::metadata::{ManualAction, ManualIntentId};
use super::*;
use crate::{
    model::{Page, Query, QueueRow},
    response::ViewResponse,
};
use std::ops::Range;

pub(in crate::gui) struct QueueCommit {
    revision: u64,
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
    revision: u64,
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

#[derive(Default)]
pub(super) struct QueueSelection {
    pub(super) rows: paged::PagedRows<QueueRow>,
    selected: HashMap<u64, QueueIdentity>,
    anchor: Option<QueueIdentity>,
    revision: u64,
    page_generation: u64,
    generation: u64,
    pub(super) busy: bool,
    pub(super) selection_error: Option<String>,
    navigation: Option<usize>,
    active: Option<QueueIdentity>,
    pending: Option<QueueJob>,
    worker: Option<u64>,
    manual: Option<ManualIntentId>,
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
        self.generation = self.generation.wrapping_add(1);
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
    fn current(&self, generation: u64, revision: u64) -> bool {
        self.generation == generation && self.revision == revision
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
        self.worker = Some(job.generation);
        Some(job)
    }

    fn release_worker(&mut self, generation: u64) {
        if self.worker == Some(generation) {
            self.worker = None;
        }
    }

    fn complete_range(
        &mut self,
        generation: u64,
        revision: u64,
        anchor: QueueIdentity,
        target: QueueIdentity,
        rows: Vec<QueueIdentity>,
        additive: bool,
    ) -> bool {
        if !self.current(generation, revision) {
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
    pub(super) fn refresh_queue(&mut self, cx: &mut Context<Self>) {
        let queue = &mut self.browser.queue;
        queue.rows.invalidate();
        queue.rows.set_total(self.state.queue.total);
        queue.page_generation = queue.page_generation.wrapping_add(1);
        queue.cancel();
        queue.revision = self.state.queue.revision;
        let mut ids: Vec<_> = queue.selected.keys().copied().collect();
        if let Some(anchor) = queue.anchor
            && !queue.selected.contains_key(&anchor.id)
        {
            ids.push(anchor.id);
        }
        if ids.is_empty() {
            return;
        }
        queue.busy = true;
        let generation = queue.generation;
        let revision = queue.revision;
        let handle = self.handle.clone();
        let i18n = std::sync::Arc::clone(&self.i18n);
        cx.spawn(async move |this, cx| {
            let mut selected = Vec::with_capacity(ids.len());
            for chunk in ids.chunks(crate::model::MAX_QUERY_ROWS) {
                if !this
                    .update(cx, |this, _| {
                        this.browser.queue.current(generation, revision)
                    })
                    .unwrap_or(false)
                {
                    return;
                }
                let handle = handle.clone();
                let queue_ids = chunk.to_vec();
                let response = cx
                    .background_executor()
                    .spawn(async move { handle.query(Query::QueueEntries { queue_ids }) })
                    .await;
                if response.revisions.queue != revision {
                    let _ = this.update(cx, |this, cx| {
                        if this.browser.queue.current(generation, revision) {
                            this.refresh(cx);
                            this.refresh_queue(cx);
                            cx.notify();
                        }
                    });
                    return;
                }
                match response.result {
                    Ok(ViewResponse::QueueEntries(rows)) => {
                        selected.extend(rows.iter().map(QueueIdentity::from))
                    }
                    result => {
                        let error = match result {
                            Err(error) => error.to_string(),
                            _ => i18n.text("gl-queue-selection-response").to_owned(),
                        };
                        let _ = this.update(cx, |this, cx| {
                            if this.browser.queue.current(generation, revision) {
                                this.error = Some(error.clone());
                                this.browser.queue.selection_error = Some(error);
                                cx.notify();
                            }
                        });
                        return;
                    }
                }
            }
            let _ = this.update(cx, |this, cx| {
                if !this.browser.queue.current(generation, revision)
                    || this.state.queue.revision != revision
                {
                    return;
                }
                this.browser.queue.reconcile(selected);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn ensure_queue_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let limit = self.view_page_size();
        let Some(offset) = self.browser.queue.rows.demand(range, limit) else {
            return;
        };
        self.browser.queue.rows.begin(offset, limit);
        self.browser.queue.page_generation = self.browser.queue.page_generation.wrapping_add(1);
        let generation = self.browser.queue.page_generation;
        let revision = self.state.queue.revision;
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let response = cx
                .background_executor()
                .spawn(async move { handle.query(Query::QueuePage { offset, limit }) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.browser.queue.page_generation != generation {
                    return;
                }
                if revision != this.state.queue.revision || response.revisions.queue != revision {
                    this.refresh(cx);
                    this.browser.queue.rows.invalidate();
                } else {
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
        let generation = self.browser.queue.generation;
        let revision = self.state.queue.revision;
        if shift && anchor.is_some() {
            self.browser.queue.pending = Some(QueueJob {
                intent,
                generation,
                revision,
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
                    revision,
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
        self.state.queue.revision == commit.revision
            && self
                .browser
                .queue
                .current(commit.generation, commit.revision)
    }

    pub(in crate::gui) fn commit_queue_selection(
        &mut self,
        commit: QueueCommit,
        cx: &mut Context<Self>,
    ) {
        let queue = &mut self.browser.queue;
        if let Some(rows) = commit.rows {
            queue.complete_range(
                commit.generation,
                commit.revision,
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
        let queue = &mut self.browser.queue;
        queue.manual = Some(intent);
        queue.navigation = Some(offset);
        queue.busy = true;
        queue.pending = Some(QueueJob {
            intent,
            generation: queue.generation,
            revision: queue.revision,
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
            && self.state.queue.revision == job.revision
            && self.browser.queue.current(job.generation, job.revision)
    }

    fn dispatch_queue_selection(&mut self, cx: &mut Context<Self>) {
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
                        if response.revisions.queue != job.revision {
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
                        if response.revisions.queue != job.revision {
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
                    revision: job.revision,
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
    fn identity(id: u64, position: usize) -> QueueIdentity {
        QueueIdentity {
            id,
            track_id: 1,
            position,
        }
    }
    #[test]
    fn occurrence_selection_survives_eviction_and_relocates_without_track_deduplication() {
        let mut queue = QueueSelection::default();
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
        let mut queue = QueueSelection {
            revision: 7,
            ..QueueSelection::default()
        };
        let generation = queue.generation;
        queue.select(identity(1, 0), false);
        assert!(!queue.current(generation, 7));
        assert!(!queue.current(queue.generation, 8));
        queue.busy = true;
        queue.navigation = Some(199_999);
        queue.clear();
        assert!(queue.ready());
        assert_eq!(queue.navigation, None);
        assert_eq!(queue.len(), 0);
    }
    #[test]
    fn streamed_ranges_replace_or_extend_occurrences_and_reject_stale_completion() {
        let mut queue = QueueSelection::default();
        queue.select(identity(1, 0), false);
        let generation = queue.generation;
        let anchor = identity(2, 199_997);
        let range = vec![anchor, identity(3, 199_998), identity(4, 199_999)];
        let target = identity(4, 199_999);
        assert!(queue.complete_range(generation, 0, anchor, target, range.clone(), true));
        assert_eq!(queue.len(), 4);
        assert_eq!(queue.anchor, Some(anchor));
        assert_eq!(queue.active(), Some(target));
        assert!(queue.complete_range(generation, 0, anchor, target, range, false));
        assert_eq!(queue.len(), 3);
        assert!(!queue.contains(&1));
        queue.select(identity(3, 199_998), true);
        assert!(!queue.contains(&3));
        assert_eq!(queue.anchor, Some(identity(3, 199_998)));
        assert!(!queue.complete_range(generation, 0, anchor, target, vec![identity(1, 0)], false));
        assert!(!queue.complete_range(
            queue.generation,
            1,
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
        let mut queue = QueueSelection::default();
        queue.select(identity(1, 0), false);
        let old = intents.begin();
        queue.pending = Some(QueueJob {
            intent: old,
            generation: queue.generation,
            revision: 0,
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
            generation: queue.generation,
            revision: 0,
            target: QueueTarget::Offset(8),
            anchor: queue.anchor,
            shift: false,
            multi: false,
            scroll: true,
            total: 10,
        });
        assert!(queue.take_job().is_none());
        assert!(!queue.current(running.generation, running.revision));
        assert_eq!(queue.ordered_ids(), vec![1]);
        queue.release_worker(running.generation);
        let next = queue.take_job().unwrap();
        assert_eq!(next.intent, latest);
        assert!(matches!(next.target, QueueTarget::Offset(8)));
        assert!(queue.take_job().is_none());
        assert_eq!(queue.active(), Some(identity(1, 0)));
    }
}
