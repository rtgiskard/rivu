//! DuplicateReview owns synchronous transitions and physical query gates; GuiApp
//! runs effects, and Core retains relocation workers and task IDs (never UI joins).

use super::*;
use crate::gui::components::{
    ButtonTooltip, StatusKind, active_control, button, caption, column, icon_button, panel_header,
    panel_toolbar, row, status_message,
};
use crate::{
    model::{Page, RelocationPhase, RelocationTaskStatus},
    projection::QueryProjection,
    response::{Ack, QueryResponse, QueryRevisions, ViewResponse},
    store::{DuplicateGroup, DuplicateMember},
};

const GROUP_PAGE: usize = 12;
const MEMBER_PAGE: usize = 8;

struct Member {
    value: DuplicateMember,
    // Only NotFound is absence. Permission and other probe errors remain unknown.
    exists: Option<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Busy {
    Starting,
    Relocating(u64),
    Cancelling(u64),
    Keeping,
}

enum ReviewFeedback {
    Text(String, StatusKind),
    Localized(&'static str, StatusKind),
    ReadFailed(String),
}

enum TaskReadOutcome {
    Ignored,
    Finished,
    Failed,
}

struct ReviewRead {
    epoch: u64,
    query: Query,
    generation: u64,
    ticket: crate::core::QueryTicket,
}

type SelectedMembers = Option<(QueryResponse, Vec<Option<bool>>)>;

#[derive(Default)]
pub(super) struct DuplicateReview {
    pub(super) open: bool,
    epoch: u64,
    groups_projection: QueryProjection,
    members_projection: QueryProjection,
    groups_dirty: bool,
    members_dirty: bool,
    members_current: bool,
    groups_failed: bool,
    members_failed: bool,
    groups: Option<Page<DuplicateGroup>>,
    members: Vec<Member>,
    members_total: usize,
    groups_offset: usize,
    members_offset: usize,
    fingerprint: Option<String>,
    groups_loading: bool,
    members_loading: bool,
    source: Option<DuplicateMember>,
    target: Option<DuplicateMember>,
    confirming: bool,
    busy: Option<Busy>,
    task_query_pending: bool,
    feedback: Option<ReviewFeedback>,
}

impl DuplicateReview {
    fn open(&mut self, revisions: QueryRevisions) -> Option<u64> {
        let cancel = self.task_id();
        self.busy = None;
        self.epoch = self.epoch.wrapping_add(1);
        self.open = true;
        self.reset(revisions);
        cancel
    }

    fn close(&mut self) -> Option<u64> {
        let cancel = self.task_id();
        self.open = false;
        self.epoch = self.epoch.wrapping_add(1);
        self.invalidate();
        self.busy = None;
        cancel
    }

    fn reset(&mut self, revisions: QueryRevisions) -> bool {
        if self.busy.is_some() {
            return false;
        }
        self.invalidate();
        self.fingerprint = None;
        self.members_dirty = false;
        self.feedback = None;
        self.request_groups(0, revisions);
        true
    }

    fn task_id(&self) -> Option<u64> {
        match self.busy {
            Some(Busy::Relocating(id) | Busy::Cancelling(id)) => Some(id),
            _ => None,
        }
    }

    fn submit(&mut self, independent: bool) -> Option<(u64, Command)> {
        if !self.open || self.busy.is_some() || !self.members_current {
            return None;
        }
        let target = self.target.as_ref()?;
        let command = if independent {
            Command::KeepTrackIndependent {
                track_id: target.track.id,
            }
        } else {
            if !self.confirming || !self.can_relocate() {
                return None;
            }
            Command::RelocateTrack {
                source_id: self.source.as_ref()?.track.id,
                target_id: target.track.id,
            }
        };
        self.busy = Some(if independent {
            Busy::Keeping
        } else {
            Busy::Starting
        });
        self.confirming = false;
        self.feedback = None;
        Some((self.epoch, command))
    }

    // False leaves the caller responsible for cancelling a late relocation ack,
    // including when the window entity itself no longer exists.
    fn accept_ack(&mut self, epoch: u64, independent: bool, ack: Ack) -> bool {
        if !self.open || self.epoch != epoch {
            return false;
        }
        if !ack.ok {
            self.busy = None;
            self.feedback = Some(match ack.error {
                Some(error) => ReviewFeedback::Text(error, StatusKind::Error),
                None => ReviewFeedback::Localized("gl-review-action-failed", StatusKind::Error),
            });
        } else if independent {
            // Keep the gate until the host refreshes the post-command snapshot.
            self.feedback = Some(ReviewFeedback::Localized(
                "gl-kept-independent",
                StatusKind::Success,
            ));
        } else if let Some(id) = ack.relocation_id {
            self.busy = Some(Busy::Relocating(id));
        } else {
            self.busy = None;
            self.feedback = Some(ReviewFeedback::Localized(
                "gl-relocation-no-task",
                StatusKind::Error,
            ));
        }
        true
    }

    fn finish_ack_refresh(&mut self) {
        if self.busy == Some(Busy::Keeping) {
            self.busy = None;
        }
    }

    fn accept_task(&mut self, task: &RelocationTaskStatus) -> bool {
        if !self.open || self.task_id() != Some(task.id) || task.phase == RelocationPhase::Verifying
        {
            return false;
        }
        self.feedback = Some(match task.phase {
            RelocationPhase::Completed => {
                ReviewFeedback::Localized("gl-relocation-completed", StatusKind::Success)
            }
            RelocationPhase::Cancelled => {
                ReviewFeedback::Localized("gl-verification-cancelled", StatusKind::Info)
            }
            RelocationPhase::Failed => match &task.error {
                Some(error) => ReviewFeedback::Text(error.clone(), StatusKind::Error),
                None => ReviewFeedback::Localized("gl-relocation-failed", StatusKind::Error),
            },
            RelocationPhase::Verifying => unreachable!(),
        });
        self.busy = None;
        true
    }

    fn begin_task_read(&mut self, id: u64) -> Option<u64> {
        if !self.open || self.task_id() != Some(id) || self.task_query_pending {
            return None;
        }
        self.task_query_pending = true;
        Some(self.epoch)
    }

    fn accept_task_read(
        &mut self,
        epoch: u64,
        id: u64,
        response: QueryResponse,
    ) -> TaskReadOutcome {
        self.task_query_pending = false;
        if !self.open || self.epoch != epoch || self.task_id() != Some(id) {
            return TaskReadOutcome::Ignored;
        }
        match response.result {
            Ok(ViewResponse::RelocationTask(task)) if task.id == id => {
                if self.accept_task(&task) {
                    TaskReadOutcome::Finished
                } else {
                    TaskReadOutcome::Ignored
                }
            }
            result => {
                self.busy = None;
                self.feedback = Some(match result {
                    Err(error) => ReviewFeedback::ReadFailed(error.to_string()),
                    _ => ReviewFeedback::Localized("gl-verification-response", StatusKind::Error),
                });
                TaskReadOutcome::Failed
            }
        }
    }

    fn cancellable_task(&self) -> Option<u64> {
        match self.busy {
            Some(Busy::Relocating(id)) => Some(id),
            _ => None,
        }
    }

    fn accept_cancel(&mut self, id: u64, result: anyhow::Result<()>) {
        if self.cancellable_task() != Some(id) {
            return;
        }
        match result {
            Ok(()) => self.busy = Some(Busy::Cancelling(id)),
            Err(error) => {
                self.feedback = Some(ReviewFeedback::Text(
                    format!("{error:#}"),
                    StatusKind::Error,
                ))
            }
        }
    }

    fn select_member(&mut self, id: i64, source: bool) -> bool {
        if self.busy.is_some() || !self.members_current || self.confirming {
            return false;
        }
        let value = self
            .members
            .iter()
            .find(|row| row.value.track.id == id)
            .map(|row| row.value.clone());
        if source {
            self.source = value;
        } else {
            self.target = value;
        }
        true
    }

    fn confirm(&mut self, confirming: bool) -> bool {
        if confirming && !self.can_relocate() {
            return false;
        }
        self.confirming = confirming;
        true
    }

    fn select_group(&mut self, fingerprint: &str, revisions: QueryRevisions) -> bool {
        if self.busy.is_some() || self.confirming {
            return false;
        }
        self.feedback = None;
        self.request_members(fingerprint.into(), 0, revisions);
        true
    }

    fn request_groups(&mut self, offset: usize, revisions: QueryRevisions) {
        if self.groups_offset != offset {
            self.groups = None;
        }
        self.groups_offset = offset;
        self.sync_queries(revisions);
        self.groups_projection.cancel();
        self.groups_dirty = true;
        self.groups_failed = false;
    }

    fn request_members(&mut self, fingerprint: String, offset: usize, revisions: QueryRevisions) {
        if self.fingerprint.as_ref() != Some(&fingerprint) {
            self.clear_selection();
            self.members_total = 0;
        }
        self.fingerprint = Some(fingerprint);
        self.members_offset = offset;
        self.sync_queries(revisions);
        self.members_projection.cancel();
        self.members.clear();
        self.members_current = false;
        self.members_dirty = true;
        self.members_failed = false;
        self.confirming = false;
    }

    fn begin_groups(&mut self) -> Option<ReviewRead> {
        if !self.open || self.busy.is_some() || self.groups_loading || !self.groups_dirty {
            return None;
        }
        self.groups_dirty = false;
        self.groups_loading = true;
        Some(ReviewRead {
            epoch: self.epoch,
            query: self.groups_query(),
            generation: self.groups_projection.generation(),
            ticket: self.groups_projection.ticket(),
        })
    }

    fn begin_members(&mut self) -> Option<(ReviewRead, Vec<i64>)> {
        if !self.open || self.busy.is_some() || self.members_loading || !self.members_dirty {
            return None;
        }
        let query = self.members_query()?;
        self.members_dirty = false;
        self.members_loading = true;
        let selected = self
            .source
            .iter()
            .chain(self.target.iter())
            .map(|member| member.track.id)
            .collect();
        Some((
            ReviewRead {
                epoch: self.epoch,
                query,
                generation: self.members_projection.generation(),
                ticket: self.members_projection.ticket(),
            },
            selected,
        ))
    }

    fn groups_worker_finished(&mut self) {
        self.groups_loading = false;
    }

    fn members_worker_finished(&mut self) {
        self.members_loading = false;
    }

    fn accept_groups(
        &mut self,
        epoch: u64,
        query: &Query,
        generation: u64,
        response: QueryResponse,
        revisions: QueryRevisions,
    ) -> bool {
        if !self.accepts_groups(epoch, query, generation, response.revisions) {
            if self.groups_projection.generation() == generation {
                self.groups_dirty = true;
            }
            return false;
        }
        match response.result {
            Ok(ViewResponse::DuplicateGroups(page)) => {
                if self.groups_offset > 0 && self.groups_offset >= page.total {
                    self.request_groups(0, revisions);
                    return false;
                }
                self.groups = Some(page);
            }
            result => {
                self.groups_failed = true;
                self.feedback = Some(match result {
                    Err(error) => ReviewFeedback::Text(error.to_string(), StatusKind::Error),
                    _ => ReviewFeedback::Localized("gl-duplicate-response", StatusKind::Error),
                });
            }
        }
        true
    }

    fn accept_members(
        &mut self,
        epoch: u64,
        query: &Query,
        generation: u64,
        result: (QueryResponse, Vec<Option<bool>>, SelectedMembers),
        revisions: QueryRevisions,
    ) -> bool {
        let (response, existence, selected) = result;
        let selected_current = selected.as_ref().is_none_or(|(selected, _)| {
            query.stamp(selected.revisions) == query.stamp(response.revisions)
        });
        if !selected_current || !self.accepts_members(epoch, query, generation, response.revisions)
        {
            if self.members_projection.generation() == generation {
                self.members_dirty = true;
            }
            return false;
        }
        let selected = match selected {
            Some((
                QueryResponse {
                    result: Ok(ViewResponse::DuplicateMembers(page)),
                    ..
                },
                exists,
            )) => page
                .rows
                .into_iter()
                .zip(exists)
                .map(|(value, exists)| Member { value, exists })
                .collect(),
            Some((response, _)) => {
                self.fail_members(response.result.err().map(|error| error.to_string()));
                return true;
            }
            None => Vec::new(),
        };
        match response.result {
            Ok(ViewResponse::DuplicateMembers(page)) => {
                if self.members_offset > 0 && self.members_offset >= page.total {
                    if let Some(hash) = self.fingerprint.clone() {
                        self.request_members(hash, 0, revisions);
                    }
                    return false;
                }
                self.members_total = page.total;
                self.members = page
                    .rows
                    .into_iter()
                    .zip(existence)
                    .map(|(value, exists)| Member { value, exists })
                    .collect();
                self.reconcile_selection(&selected);
            }
            result => self.fail_members(result.err().map(|error| error.to_string())),
        }
        true
    }

    fn fail_members(&mut self, message: Option<String>) {
        self.members_failed = true;
        self.members_current = false;
        self.feedback = Some(match message {
            Some(message) => ReviewFeedback::Text(message, StatusKind::Error),
            None => ReviewFeedback::Localized("gl-member-response", StatusKind::Error),
        });
    }

    fn groups_query(&self) -> Query {
        Query::DuplicateGroups {
            offset: self.groups_offset,
            limit: GROUP_PAGE,
        }
    }

    fn members_query(&self) -> Option<Query> {
        Some(Query::DuplicateMembers {
            fingerprint: self.fingerprint.clone()?,
            offset: self.members_offset,
            limit: MEMBER_PAGE,
        })
    }

    fn sync_queries(&mut self, revisions: QueryRevisions) -> bool {
        let groups_query = self.groups_query();
        let groups_changed = self.groups_projection.sync(&groups_query, revisions);
        if groups_changed {
            self.groups = None;
            self.groups_dirty = true;
            self.groups_failed = false;
        }
        let members_changed = self
            .members_query()
            .is_some_and(|query| self.members_projection.sync(&query, revisions));
        if members_changed {
            self.members.clear();
            self.members_current = false;
            self.members_dirty = true;
            self.members_failed = false;
            self.confirming = false;
        }
        groups_changed || members_changed
    }

    fn accepts_groups(
        &self,
        epoch: u64,
        query: &Query,
        generation: u64,
        revisions: QueryRevisions,
    ) -> bool {
        self.open
            && self.epoch == epoch
            && self.groups_projection.accepts(query, generation, revisions)
            && matches!(query, Query::DuplicateGroups { offset, .. } if *offset == self.groups_offset)
    }

    fn accepts_members(
        &self,
        epoch: u64,
        query: &Query,
        generation: u64,
        revisions: QueryRevisions,
    ) -> bool {
        self.open
            && self.epoch == epoch
            && self
                .members_projection
                .accepts(query, generation, revisions)
            && matches!(query, Query::DuplicateMembers { offset, .. } if *offset == self.members_offset)
    }

    fn clear_selection(&mut self) {
        self.source = None;
        self.target = None;
        self.confirming = false;
    }

    fn invalidate(&mut self) {
        self.groups_projection.cancel();
        self.members_projection.cancel();
        self.groups_dirty = true;
        self.members_dirty = self.fingerprint.is_some();
        self.members_current = false;
        self.groups_failed = false;
        self.members_failed = false;
        self.groups = None;
        self.members.clear();
        self.members_total = 0;
        self.clear_selection();
        // Logical cancellation must not release either physical worker gate.
    }

    fn reconcile_selection(&mut self, selected: &[Member]) {
        let source = self.source.as_ref().and_then(|source| {
            self.members
                .iter()
                .chain(selected)
                .find(|member| member.value.track.id == source.track.id)
                .filter(|member| selectable_source(member))
                .map(|member| member.value.clone())
        });
        let target = self.target.as_ref().and_then(|target| {
            self.members
                .iter()
                .chain(selected)
                .find(|member| member.value.track.id == target.track.id)
                .filter(|member| selectable_target(member))
                .map(|member| member.value.clone())
        });
        self.source = source;
        self.target = target;
        if self.source.is_none() || self.target.as_ref().is_none_or(has_target_state) {
            self.confirming = false;
        }
        self.members_current = true;
    }

    fn can_relocate(&self) -> bool {
        self.members_current
            && self.source.is_some()
            && self
                .target
                .as_ref()
                .is_some_and(|target| !has_target_state(target))
    }
}

impl GuiApp {
    pub(super) fn open_duplicate_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_interaction(window, cx);
        if let Some(id) = self.duplicate_review.open(self.state.query_revisions(None)) {
            let _ = self.handle.send(Command::CancelRelocation { id });
        }
        self.panel_menu = None;
        self.app_menu_open = false;
        self.catalog_open = false;
        self.start_duplicate_groups(cx);
        cx.notify();
    }

    pub(super) fn close_duplicate_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.duplicate_review.close() {
            let _ = self.handle.send(Command::CancelRelocation { id });
        }
        if self.settings_open {
            self.settings.focus_handle(cx).focus(window, cx);
        } else {
            self.focus_layout(window, cx);
        }
        cx.notify();
    }

    pub(super) fn cancel_duplicate_task(&self) {
        if let Some(id) = self.duplicate_review.task_id() {
            let _ = self.handle.send(Command::CancelRelocation { id });
        }
    }

    fn request_duplicate_groups(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.duplicate_review
            .request_groups(offset, self.state.query_revisions(None));
        self.start_duplicate_groups(cx);
    }

    fn start_duplicate_groups(&mut self, cx: &mut Context<Self>) {
        let Some(ReviewRead {
            epoch,
            query,
            generation,
            ticket,
        }) = self.duplicate_review.begin_groups()
        else {
            return;
        };
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            if !this
                .update(cx, |this, cx| {
                    let current = this.duplicate_review.accepts_groups(
                        epoch,
                        &query,
                        generation,
                        this.state.query_revisions(None),
                    );
                    if !current {
                        this.duplicate_review.groups_worker_finished();
                        this.start_duplicate_groups(cx);
                    }
                    current
                })
                .unwrap_or(false)
            {
                return;
            }
            let worker_query = query.clone();
            let response = cx
                .background_executor()
                .spawn(async move { handle.query_if_current(worker_query, ticket) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.duplicate_review.groups_worker_finished();
                this.refresh(cx);
                if !this.duplicate_review.accept_groups(
                    epoch,
                    &query,
                    generation,
                    response,
                    this.state.query_revisions(None),
                ) {
                    this.start_duplicate_groups(cx);
                    return;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn request_duplicate_members(
        &mut self,
        fingerprint: String,
        offset: usize,
        cx: &mut Context<Self>,
    ) {
        self.duplicate_review.request_members(
            fingerprint,
            offset,
            self.state.query_revisions(None),
        );
        self.start_duplicate_members(cx);
    }

    fn start_duplicate_members(&mut self, cx: &mut Context<Self>) {
        let Some((
            ReviewRead {
                epoch,
                query,
                generation,
                ticket,
            },
            selected_ids,
        )) = self.duplicate_review.begin_members()
        else {
            return;
        };
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            if !this
                .update(cx, |this, cx| {
                    let current = this.duplicate_review.accepts_members(
                        epoch,
                        &query,
                        generation,
                        this.state.query_revisions(None),
                    );
                    if !current {
                        this.duplicate_review.members_worker_finished();
                        this.start_duplicate_members(cx);
                    }
                    current
                })
                .unwrap_or(false)
            {
                return;
            }
            let worker_query = query.clone();
            let (response, existence, selected) = cx
                .background_executor()
                .spawn(async move {
                    let response = handle.query_if_current(worker_query.clone(), ticket.clone());
                    let existence = member_existence(&response);
                    let selected = match (&worker_query, &response.result) {
                        (
                            Query::DuplicateMembers { fingerprint, .. },
                            Ok(ViewResponse::DuplicateMembers(page)),
                        ) => {
                            let track_ids = selected_ids
                                .into_iter()
                                .filter(|id| !page.rows.iter().any(|member| member.track.id == *id))
                                .collect::<Vec<_>>();
                            if track_ids.is_empty() {
                                None
                            } else {
                                let selected = handle.query_if_current(
                                    Query::DuplicateMemberSelection {
                                        fingerprint: fingerprint.clone(),
                                        track_ids,
                                    },
                                    ticket.clone(),
                                );
                                let exists = member_existence(&selected);
                                Some((selected, exists))
                            }
                        }
                        _ => None,
                    };
                    (response, existence, selected)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.duplicate_review.members_worker_finished();
                this.refresh(cx);
                if !this.duplicate_review.accept_members(
                    epoch,
                    &query,
                    generation,
                    (response, existence, selected),
                    this.state.query_revisions(None),
                ) {
                    this.start_duplicate_members(cx);
                    return;
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn sync_duplicate_review(&mut self, cx: &mut Context<Self>) {
        let data_changed = self
            .duplicate_review
            .sync_queries(self.state.query_revisions(None));
        if !self.duplicate_review.open {
            return;
        }
        let task_id = self.duplicate_review.task_id();
        let task = task_id.and_then(|id| {
            self.state
                .system
                .relocation_task
                .as_ref()
                .filter(|task| task.id == id)
        });
        let finished = task.is_some_and(|task| self.duplicate_review.accept_task(task));
        if task.is_none()
            && let Some(id) = task_id
        {
            self.request_retained_relocation(id, cx);
        }
        self.start_duplicate_groups(cx);
        self.start_duplicate_members(cx);
        if data_changed || finished {
            cx.notify();
        }
    }

    fn request_retained_relocation(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(epoch) = self.duplicate_review.begin_task_read(id) else {
            return;
        };
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let response = cx
                .background_executor()
                .spawn(async move { handle.query(Query::RelocationTask { id }) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match this.duplicate_review.accept_task_read(epoch, id, response) {
                    TaskReadOutcome::Finished => {
                        this.refresh(cx);
                        this.sync_duplicate_review(cx);
                    }
                    TaskReadOutcome::Failed => this.sync_duplicate_review(cx),
                    TaskReadOutcome::Ignored => return,
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn submit_duplicate_action(&mut self, independent: bool, cx: &mut Context<Self>) {
        self.refresh(cx);
        let Some((epoch, command)) = self.duplicate_review.submit(independent) else {
            return;
        };
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let ack_handle = handle.clone();
            let ack = cx
                .background_executor()
                .spawn(async move { ack_handle.request_ack(command) })
                .await;
            let relocation_id = ack.relocation_id;
            let accepted = this
                .update(cx, |this, cx| {
                    if !this.duplicate_review.accept_ack(epoch, independent, ack) {
                        return false;
                    }
                    this.refresh(cx);
                    this.duplicate_review.finish_ack_refresh();
                    this.sync_duplicate_review(cx);
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            // Closing/replacing the root can precede the start acknowledgement.
            if !accepted && let Some(id) = relocation_id {
                let _ = handle.send(Command::CancelRelocation { id });
            }
        })
        .detach();
        cx.notify();
    }

    fn cancel_review_verification(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.duplicate_review.cancellable_task() else {
            return;
        };
        let result = self.handle.send(Command::CancelRelocation { id });
        self.duplicate_review.accept_cancel(id, result);
        cx.notify();
    }

    pub(super) fn duplicate_review_dialog(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = crate::gui::theme::palette(cx);
        let review = &self.duplicate_review;
        let busy = review.busy.is_some();
        let mut groups = column().gap_2();
        if review.groups_loading {
            groups = groups.child(caption(theme, self.tr("gl-groups-loading")));
        } else if let Some(page) = &review.groups {
            if page.total == 0 {
                groups = groups.child(caption(theme, self.tr("gl-groups-empty")));
            }
            for (index, group) in page.rows.iter().enumerate() {
                let hash = group.fingerprint.clone();
                let selected = review.fingerprint.as_ref() == Some(&hash);
                groups = groups.child(active_control(
                    theme,
                    components::disabled_control(
                        theme,
                        button(("duplicate-group", index), "", cx, move |this, _, cx| {
                            if this
                                .duplicate_review
                                .select_group(&hash, this.state.query_revisions(None))
                            {
                                this.start_duplicate_members(cx);
                                cx.notify();
                            }
                        })
                        .w_full()
                        .justify_start()
                        .child(
                            column()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(
                                    div().text_sm().truncate().child(
                                        self.i18n.format(
                                            "gl-duplicate-group-count",
                                            &[
                                                (
                                                    "hash",
                                                    (&group.fingerprint
                                                        [..group.fingerprint.len().min(12)])
                                                        .into(),
                                                ),
                                                ("count", (group.total_members).into()),
                                            ],
                                        ),
                                    ),
                                )
                                .child(
                                    caption(
                                        theme,
                                        self.i18n.format(
                                            "gl-duplicate-presence-count",
                                            &[
                                                ("missing", (group.missing_members).into()),
                                                ("present", (group.present_members).into()),
                                            ],
                                        ),
                                    )
                                    .whitespace_normal(),
                                ),
                        ),
                        busy || review.confirming,
                    ),
                    selected,
                ));
            }
        }
        if review.groups_failed && !review.groups_loading {
            groups = groups.child(icon_button(
                "review-groups-retry",
                "󰑐",
                self.tr("gl-retry-loading"),
                cx,
                |this, _, cx| {
                    if this.duplicate_review.busy.is_none() {
                        let offset = this.duplicate_review.groups_offset;
                        this.request_duplicate_groups(offset, cx);
                        cx.notify();
                    }
                },
            ));
        }
        let group_total = review.groups.as_ref().map_or(0, |page| page.total);
        let group_offset = review.groups_offset;
        let group_pager = self.review_pager(
            true,
            group_offset,
            group_total,
            GROUP_PAGE,
            busy || review.groups_loading || review.confirming,
            cx,
        );

        let mut members = column().flex_shrink_0().gap_2();
        if let Some(fingerprint) = &review.fingerprint {
            let fingerprint = fingerprint.clone();
            members = members.child(
                caption(
                    theme,
                    self.i18n.format(
                        "gl-cached-fingerprint",
                        &[("fingerprint", (fingerprint.as_str()).into())],
                    ),
                )
                .id("review-fingerprint")
                .truncate()
                .tooltip({
                    let text: gpui::SharedString = self
                        .i18n
                        .format(
                            "gl-cached-fingerprint-note",
                            &[("fingerprint", (fingerprint.as_str()).into())],
                        )
                        .into();
                    move |_, cx| cx.new(|_| ButtonTooltip { text: text.clone() }).into()
                }),
            );
        }
        if review.members_loading {
            members = members.child(caption(theme, self.tr("gl-members-loading")));
        } else if review.fingerprint.is_none() {
            members = members.child(caption(theme, self.tr("gl-select-relocation")));
        } else if !review.members_failed && review.members.is_empty() {
            members = members.child(caption(theme, self.tr("gl-members-empty")));
        }
        if review.members_failed && !review.members_loading {
            members = members.child(icon_button(
                "review-members-retry",
                "󰑐",
                self.tr("gl-retry-loading"),
                cx,
                |this, _, cx| {
                    if this.duplicate_review.busy.is_none()
                        && let Some(hash) = this.duplicate_review.fingerprint.clone()
                    {
                        let offset = this.duplicate_review.members_offset;
                        this.request_duplicate_members(hash, offset, cx);
                        cx.notify();
                    }
                },
            ));
        }
        for member in &review.members {
            let value = &member.value;
            let track = &value.track;
            let source_selected = review
                .source
                .as_ref()
                .is_some_and(|row| row.track.id == track.id);
            let target_selected = review
                .target
                .as_ref()
                .is_some_and(|row| row.track.id == track.id);
            let selectable_source = selectable_source(member);
            let selectable_target = selectable_target(member);
            let id = track.id;
            let existence = match member.exists {
                Some(true) => self.i18n.text("gl-exists-now"),
                Some(false) => self.i18n.text("gl-absent-now"),
                None => self.i18n.text("gl-existence-unknown"),
            };
            let last_played = track
                .last_played
                .and_then(|timestamp| chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0))
                .map(|time| {
                    time.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string()
                })
                .unwrap_or_else(|| self.i18n.text("gl-never").into());
            let mut card = column()
                .flex_shrink_0()
                .min_w_0()
                .gap_1()
                .p_2()
                .border_1()
                .rounded_md()
                .border_color(rgb(if source_selected || target_selected {
                    theme.accent
                } else {
                    theme.border
                }))
                .child(
                    div()
                        .id(("review-path", id as u64))
                        .text_sm()
                        .truncate()
                        .child(track.path.display().to_string())
                        .tooltip({
                            let path = track.path.display().to_string();
                            move |_, cx| {
                                cx.new(|_| ButtonTooltip {
                                    text: path.clone().into(),
                                })
                                .into()
                            }
                        }),
                )
                .child(caption(
                    theme,
                    self.i18n.format(
                        "gl-member-presence",
                        &[
                            ("id", (id.to_string()).into()),
                            ("existence", (existence).into()),
                            (
                                "scan",
                                (self.i18n.text(if track.missing {
                                    "gl-scan-missing"
                                } else {
                                    "gl-scan-present"
                                }))
                                .into(),
                            ),
                        ],
                    ),
                ))
                .child(caption(
                    theme,
                    self.i18n.format(
                        "gl-member-file-data",
                        &[
                            ("size", (value.file_size.to_string()).into()),
                            ("mtime", (value.modified_ns.to_string()).into()),
                        ],
                    ),
                ))
                .child(caption(
                    theme,
                    self.i18n.format(
                        "gl-member-title",
                        &[("title", (track.title.as_str()).into())],
                    ),
                ))
                .child(caption(
                    theme,
                    self.i18n.format(
                        "gl-member-metadata",
                        &[
                            (
                                "artist",
                                (if track.artist.is_empty() {
                                    "-"
                                } else {
                                    &track.artist
                                })
                                .into(),
                            ),
                            (
                                "album",
                                (if track.album.is_empty() {
                                    "-"
                                } else {
                                    &track.album
                                })
                                .into(),
                            ),
                            (
                                "duration",
                                (track
                                    .duration
                                    .map(format_time)
                                    .unwrap_or_else(|| "-".into()))
                                .into(),
                            ),
                        ],
                    ),
                ))
                .child(caption(
                    theme,
                    self.i18n.format(
                        "gl-member-user-state",
                        &[
                            (
                                "favorite",
                                (self
                                    .i18n
                                    .text(if track.favorite { "gl-yes" } else { "gl-no" }))
                                .into(),
                            ),
                            (
                                "overrides",
                                (self.i18n.text(if value.has_overrides {
                                    "gl-yes"
                                } else {
                                    "gl-no"
                                }))
                                .into(),
                            ),
                            ("plays", (track.play_count).into()),
                            ("last_played", (last_played.as_str()).into()),
                        ],
                    ),
                ))
                .child(caption(
                    theme,
                    self.i18n.format(
                        "gl-member-references",
                        &[
                            ("playlists", (value.playlist_references).into()),
                            ("queue", (value.queue_references).into()),
                            (
                                "role",
                                (self.i18n.text(if value.independent {
                                    "gl-independent-role"
                                } else {
                                    "gl-eligible-role"
                                }))
                                .into(),
                            ),
                        ],
                    ),
                ));
            let mut actions = panel_toolbar().flex_wrap();
            if selectable_source {
                actions = actions.child(active_control(
                    theme,
                    components::disabled_control(
                        theme,
                        button(
                            ("review-source", id as u64),
                            self.tr("gl-use-source"),
                            cx,
                            move |this, _, cx| {
                                if this.duplicate_review.select_member(id, true) {
                                    cx.notify();
                                }
                            },
                        ),
                        busy || review.confirming,
                    ),
                    source_selected,
                ));
            }
            if selectable_target {
                actions = actions.child(active_control(
                    theme,
                    components::disabled_control(
                        theme,
                        button(
                            ("review-target", id as u64),
                            self.tr("gl-use-target"),
                            cx,
                            move |this, _, cx| {
                                if this.duplicate_review.select_member(id, false) {
                                    cx.notify();
                                }
                            },
                        ),
                        busy || review.confirming,
                    ),
                    target_selected,
                ));
            }
            if selectable_target && has_target_state(value) {
                card = card.child(status_message(
                    theme,
                    self.tr("gl-target-state-warning"),
                    StatusKind::Error,
                ));
            }
            if selectable_source || selectable_target {
                card = card.child(actions);
            }
            members = members.child(card);
        }
        let mut decisions = column().gap_2();
        if let Some(source) = &review.source {
            decisions = decisions.child(caption(
                theme,
                self.i18n.format(
                    "gl-source-identity",
                    &[
                        ("id", (source.track.id.to_string()).into()),
                        ("path", (source.track.path.display().to_string()).into()),
                    ],
                ),
            ));
        }
        if let Some(target) = &review.target {
            decisions = decisions.child(caption(
                theme,
                self.i18n.format(
                    "gl-target-identity",
                    &[
                        ("id", (target.track.id.to_string()).into()),
                        ("path", (target.track.path.display().to_string()).into()),
                    ],
                ),
            ));
        }
        if review.confirming {
            decisions = decisions
                .child(status_message(
                    theme,
                    self.tr("gl-confirm-relocation-note"),
                    StatusKind::Info,
                ))
                .child(
                    panel_toolbar()
                        .flex_wrap()
                        .child(components::danger_control(
                            theme,
                            button(
                                "review-confirm",
                                self.tr("gl-confirm-relocation"),
                                cx,
                                |this, _, cx| this.submit_duplicate_action(false, cx),
                            ),
                        ))
                        .child(icon_button(
                            "review-back",
                            "",
                            self.tr("gl-back-selection"),
                            cx,
                            |this, _, cx| {
                                this.duplicate_review.confirm(false);
                                cx.notify();
                            },
                        )),
                );
        } else if !busy {
            let can_relocate = review.can_relocate();
            decisions = decisions.child(
                panel_toolbar()
                    .flex_wrap()
                    .when(can_relocate, |view| {
                        view.child(icon_button(
                            "review-relocate",
                            "󰁔",
                            self.tr("gl-review-relocation"),
                            cx,
                            |this, _, cx| {
                                if this.duplicate_review.confirm(true) {
                                    cx.notify();
                                }
                            },
                        ))
                    })
                    .when(review.members_current && review.target.is_some(), |view| {
                        view.child(button(
                            "review-independent",
                            self.tr("gl-keep-independent"),
                            cx,
                            |this, _, cx| this.submit_duplicate_action(true, cx),
                        ))
                    }),
            );
        }
        if let Some(task) = review.busy {
            decisions = decisions.child(status_message(
                theme,
                match task {
                    Busy::Starting => self.tr("gl-verification-starting"),
                    Busy::Relocating(_) => self.tr("gl-verification-running"),
                    Busy::Cancelling(_) => self.tr("gl-verification-cancelling"),
                    Busy::Keeping => self.tr("gl-independent-saving"),
                },
                StatusKind::Info,
            ));
            if matches!(task, Busy::Relocating(_)) {
                decisions = decisions.child(icon_button(
                    "review-cancel-verification",
                    "",
                    self.tr("gl-cancel-verification"),
                    cx,
                    |this, _, cx| this.cancel_review_verification(cx),
                ));
            }
        }
        if let Some(feedback) = &review.feedback {
            let (message, kind) = match feedback {
                ReviewFeedback::Text(message, kind) => (message.clone(), *kind),
                ReviewFeedback::Localized(key, kind) => (self.i18n.text(key).into(), *kind),
                ReviewFeedback::ReadFailed(error) => (
                    self.i18n.format(
                        "gl-verification-read-failed",
                        &[("error", error.as_str().into())],
                    ),
                    StatusKind::Error,
                ),
            };
            decisions = decisions.child(status_message(theme, message, kind));
        }
        crate::gui::components::modal_backdrop("duplicate-review-modal")
            .map(|surface| self.dialog_surface(surface))
            .child(
                column()
                    .id("duplicate-review-card")
                    .w(relative(0.94))
                    .max_h(window.viewport_size().height * 0.92)
                    .min_w_0()
                    .min_h_0()
                    .p_3()
                    .gap_2()
                    .overflow_y_scroll()
                    .bg(rgb(theme.panel))
                    .border_1()
                    .border_color(rgb(theme.border))
                    .rounded_md()
                    .child(
                        row()
                            .flex_shrink_0()
                            .child(div().flex_1().min_w_0().child(panel_header(
                                theme,
                                self.tr("gl-same-files-title"),
                                self.tr("gl-same-files-note"),
                            )))
                            .child(components::disabled_control(
                                theme,
                                components::quiet_icon_button(
                                    theme,
                                    icon_button(
                                        "review-refresh",
                                        "󰑐",
                                        self.tr("gl-refresh-groups"),
                                        cx,
                                        |this, _, cx| {
                                            if this
                                                .duplicate_review
                                                .reset(this.state.query_revisions(None))
                                            {
                                                this.start_duplicate_groups(cx);
                                                cx.notify();
                                            }
                                        },
                                    ),
                                ),
                                busy,
                            ))
                            .child(components::quiet_icon_button(
                                theme,
                                icon_button(
                                    "review-close",
                                    "",
                                    self.tr("gl-close-review"),
                                    cx,
                                    |this, window, cx| this.close_duplicate_review(window, cx),
                                ),
                            )),
                    )
                    .child(
                        row()
                            .flex_shrink(1.)
                            .min_h_0()
                            .items_stretch()
                            .child(
                                column()
                                    .w(relative(0.30))
                                    .min_w_0()
                                    .min_h_0()
                                    .gap_2()
                                    .child(
                                        caption(theme, self.tr("gl-same-content-groups"))
                                            .flex_shrink_0(),
                                    )
                                    .child(
                                        column()
                                            .id("duplicate-groups-scroll")
                                            .flex_1()
                                            .min_h_0()
                                            .overflow_y_scroll()
                                            .child(groups),
                                    )
                                    .child(group_pager),
                            )
                            .child(
                                column()
                                    .flex_1()
                                    .min_w_0()
                                    .min_h_0()
                                    .gap_2()
                                    .child(caption(theme, self.tr("gl-file-roles")).flex_shrink_0())
                                    .child(
                                        column()
                                            .id("duplicate-members-scroll")
                                            .flex_1()
                                            .min_w_0()
                                            .min_h_0()
                                            .overflow_y_scroll()
                                            .child(members),
                                    )
                                    .when(review.fingerprint.is_some(), |pane| {
                                        pane.child(self.review_pager(
                                            false,
                                            review.members_offset,
                                            review.members_total,
                                            MEMBER_PAGE,
                                            busy || review.members_loading || review.confirming,
                                            cx,
                                        ))
                                    }),
                            ),
                    )
                    .child(
                        column()
                            .id("review-decisions")
                            .flex_shrink_0()
                            .max_h(
                                (window.rem_size() * 14.).min(window.viewport_size().height * 0.35),
                            )
                            .overflow_y_scroll()
                            .child(decisions.flex_shrink_0()),
                    )
                    .child(caption(theme, self.tr("gl-cached-hash-note")).flex_shrink_0()),
            )
    }

    fn review_pager(
        &self,
        groups: bool,
        offset: usize,
        total: usize,
        size: usize,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = crate::gui::theme::palette(cx);
        let mut pager = panel_toolbar().flex_shrink_0().child(caption(
            theme,
            self.i18n.format(
                "gl-page-range",
                &[
                    ("start", (if total == 0 { 0 } else { offset + 1 }).into()),
                    ("end", ((offset + size).min(total)).into()),
                    ("total", (total).into()),
                ],
            ),
        ));
        for (next, icon, hint, previous) in [
            (
                offset.saturating_sub(size),
                "󰁍",
                self.tr("gl-previous-page"),
                true,
            ),
            (offset + size, "󰁔", self.tr("gl-next-page"), false),
        ] {
            let enabled = !disabled && (if previous { offset > 0 } else { next < total });
            pager = pager.child(components::disabled_control(
                theme,
                icon_button(
                    (
                        if groups {
                            "review-group-page"
                        } else {
                            "review-member-page"
                        },
                        next,
                    ),
                    icon,
                    hint,
                    cx,
                    move |this, _, cx| {
                        if !enabled
                            || this.duplicate_review.busy.is_some()
                            || this.duplicate_review.confirming
                        {
                            return;
                        }
                        if groups {
                            this.request_duplicate_groups(next, cx);
                        } else if let Some(hash) = this.duplicate_review.fingerprint.clone() {
                            this.request_duplicate_members(hash, next, cx);
                        }
                        cx.notify();
                    },
                ),
                !enabled,
            ));
        }
        pager
    }
}

fn selectable_source(member: &Member) -> bool {
    member.value.track.missing && member.exists == Some(false) && !member.value.independent
}

fn selectable_target(member: &Member) -> bool {
    !member.value.track.missing && member.exists == Some(true) && !member.value.independent
}

fn member_existence(response: &QueryResponse) -> Vec<Option<bool>> {
    match &response.result {
        Ok(ViewResponse::DuplicateMembers(page)) => page
            .rows
            .iter()
            .map(|member| match std::fs::metadata(&member.track.path) {
                Ok(metadata) if metadata.is_file() => Some(true),
                Ok(_) => None,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
                Err(_) => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn has_target_state(member: &DuplicateMember) -> bool {
    member.has_overrides
        || member.track.favorite
        || member.track.play_count != 0
        || member.track.last_played.is_some()
        || member.playlist_references != 0
        || member.queue_references != 0
}

#[cfg(test)]
mod tests {
    use super::{Busy, DuplicateReview, Member, ReviewFeedback, TaskReadOutcome};
    use crate::{
        model::{Command, Page, RelocationPhase, RelocationTaskStatus, Track},
        response::{Ack, QueryResponse, QueryRevisions, ViewResponse},
        store::DuplicateMember,
    };

    fn review() -> DuplicateReview {
        let mut review = DuplicateReview {
            open: true,
            epoch: 4,
            fingerprint: Some("fingerprint".into()),
            ..Default::default()
        };
        review.sync_queries(QueryRevisions::default());
        review.groups_dirty = false;
        review.members_dirty = false;
        review
    }

    fn member(id: i64, missing: bool) -> Member {
        Member {
            value: DuplicateMember {
                track: Track {
                    id,
                    path: format!("/music/{id}.wav").into(),
                    fingerprint: Some("fingerprint".into()),
                    cue: None,
                    title: String::new(),
                    artist: String::new(),
                    album: String::new(),
                    duration: None,
                    codec: String::new(),
                    channels: 0,
                    sample_rate: 0,
                    bitrate_bps: None,
                    bitrate_exact: false,
                    track_number: None,
                    disc_number: None,
                    bits_per_sample: None,
                    release_date: None,
                    favorite: false,
                    decoder_hint: Default::default(),
                    missing,
                    play_count: 0,
                    last_played: None,
                },
                file_size: 100,
                modified_ns: 1,
                has_overrides: false,
                playlist_references: 0,
                queue_references: 0,
                independent: false,
            },
            exists: Some(!missing),
        }
    }

    #[test]
    fn refresh_and_close_reopen_reject_old_results_without_releasing_workers() {
        let mut review = review();
        let revisions = QueryRevisions::default();
        review.request_groups(0, revisions);
        review.request_members("fingerprint".into(), 0, revisions);
        let groups = review.begin_groups().unwrap();
        let (members, _) = review.begin_members().unwrap();
        assert!(review.accepts_groups(groups.epoch, &groups.query, groups.generation, revisions));
        assert!(review.accepts_members(
            members.epoch,
            &members.query,
            members.generation,
            revisions
        ));
        assert!(review.reset(revisions));
        assert!(review.groups_loading);
        assert!(review.members_loading);
        assert!(!review.accepts_groups(groups.epoch, &groups.query, groups.generation, revisions));
        assert!(!review.accepts_members(
            members.epoch,
            &members.query,
            members.generation,
            revisions
        ));
        assert_eq!(review.close(), None);
        assert_eq!(review.open(revisions), None);
        review.request_members("fingerprint".into(), 0, revisions);
        assert!(!review.accepts_groups(
            groups.epoch,
            &groups.query,
            review.groups_projection.generation(),
            revisions
        ));
        assert!(!review.accepts_members(
            members.epoch,
            &members.query,
            review.members_projection.generation(),
            revisions
        ));
        assert!(review.begin_groups().is_none());
        assert!(review.begin_members().is_none());
        review.groups_worker_finished();
        review.members_worker_finished();
        assert!(review.begin_groups().is_some());
        assert!(review.begin_members().is_some());
    }

    #[test]
    fn group_validity_is_independent_of_metadata_and_member_reference_refreshes() {
        for revisions in [
            QueryRevisions {
                library: 1,
                ..Default::default()
            },
            QueryRevisions {
                duplicate_members: 1,
                ..Default::default()
            },
            QueryRevisions {
                queue: 1,
                ..Default::default()
            },
        ] {
            let mut review = review();
            review.groups = Some(Page {
                total: 1,
                rows: Vec::new(),
            });
            let groups_generation = review.groups_projection.generation();
            let groups_query = review.groups_query();
            assert!(review.sync_queries(revisions));
            assert!(review.groups.is_some());
            assert!(!review.groups_dirty);
            assert!(review.members_dirty);
            assert!(review.accepts_groups(4, &groups_query, groups_generation, revisions));
        }
    }

    #[test]
    fn catalog_rename_and_structure_changes_do_not_schedule_either_query() {
        let mut review = review();
        assert!(!review.sync_queries(QueryRevisions {
            playlist: 1,
            structure: 1,
            ..Default::default()
        }));
        assert!(!review.groups_dirty);
        assert!(!review.members_dirty);
    }

    #[test]
    fn hidden_sync_invalidates_only_the_changed_projection() {
        let mut review = review();
        review.open = false;
        assert!(review.sync_queries(QueryRevisions {
            duplicate_groups: 1,
            ..Default::default()
        }));
        assert!(review.groups_dirty);
        assert!(!review.members_dirty);
        assert!(!review.groups_loading);
        assert!(!review.members_loading);
    }

    #[test]
    fn reference_refresh_preserves_choices_but_rechecks_target_state() {
        let mut review = review();
        let source = member(1, true);
        let mut target = member(2, false);
        review.source = Some(source.value.clone());
        review.target = Some(target.value.clone());
        review.confirming = true;
        review.members_current = true;
        assert!(review.can_relocate());
        assert!(review.sync_queries(QueryRevisions {
            duplicate_members: 1,
            queue: 1,
            ..Default::default()
        }));
        assert_eq!(review.source.as_ref().unwrap().track.id, 1);
        assert_eq!(review.target.as_ref().unwrap().track.id, 2);
        assert!(!review.can_relocate());
        review.members = vec![source];
        target.value.playlist_references = 1;
        target.value.queue_references = 2;
        review.reconcile_selection(&[target]);
        assert_eq!(review.source.as_ref().unwrap().track.id, 1);
        let target = review.target.as_ref().unwrap();
        assert_eq!(target.track.id, 2);
        assert_eq!(target.playlist_references, 1);
        assert_eq!(target.queue_references, 2);
        assert!(!review.can_relocate());
        assert!(!review.confirming);
        let target = member(2, false);
        review.reconcile_selection(&[target]);
        assert_eq!(review.target.as_ref().unwrap().track.id, 2);
        assert!(review.can_relocate());
    }

    #[test]
    fn accepted_facts_clear_ineligible_absent_or_unknown_selections() {
        for invalid in [
            None,
            Some(Member {
                exists: None,
                ..member(1, true)
            }),
            Some(Member {
                exists: Some(true),
                ..member(1, true)
            }),
            Some(member(1, false)),
            Some({
                let mut member = member(1, true);
                member.value.independent = true;
                member
            }),
        ] {
            let mut review = review();
            review.source = Some(member(1, true).value);
            review.target = Some(member(2, false).value);
            review.members = vec![member(2, false)];
            review.reconcile_selection(&invalid.into_iter().collect::<Vec<_>>());
            assert!(review.source.is_none());
            assert_eq!(review.target.as_ref().unwrap().track.id, 2);
        }
        let mut review = review();
        review.source = Some(member(1, true).value);
        review.target = Some(member(2, false).value);
        let mut target = member(2, false);
        target.value.independent = true;
        review.members = vec![member(1, true), target];
        review.reconcile_selection(&[]);
        assert!(review.source.is_some());
        assert!(review.target.is_none());
    }

    #[test]
    fn unchanged_reconciliation_does_not_retry_failed_pages() {
        let mut review = review();
        review.groups_failed = true;
        review.members_failed = true;
        assert!(!review.sync_queries(QueryRevisions::default()));
        assert!(review.groups_failed);
        assert!(review.members_failed);
        assert!(!review.groups_dirty);
        assert!(!review.members_dirty);
    }

    fn ready_review() -> DuplicateReview {
        let mut review = review();
        review.members = vec![member(1, true), member(2, false)];
        review.members_current = true;
        assert!(review.select_member(1, true));
        assert!(review.select_member(2, false));
        review
    }

    fn ack(ok: bool, relocation_id: Option<u64>) -> Ack {
        Ack {
            ok,
            error: None,
            revision: 0,
            scan_id: None,
            relocation_id,
        }
    }

    fn task(id: u64, phase: RelocationPhase) -> RelocationTaskStatus {
        RelocationTaskStatus {
            id,
            source_id: 1,
            target_id: 2,
            phase,
            error: None,
        }
    }

    fn task_response(id: u64, phase: RelocationPhase) -> QueryResponse {
        QueryResponse {
            revisions: QueryRevisions::default(),
            result: Ok(ViewResponse::RelocationTask(task(id, phase))),
        }
    }

    #[test]
    fn submission_requires_current_confirmed_choices_and_serializes_actions() {
        let mut review = ready_review();
        assert!(review.submit(false).is_none());
        assert!(review.confirm(true));
        review.sync_queries(QueryRevisions {
            duplicate_members: 1,
            ..Default::default()
        });
        assert!(review.submit(false).is_none());
        assert!(review.submit(true).is_none());
        review.members = vec![member(1, true), member(2, false)];
        review.reconcile_selection(&[]);
        assert!(review.confirm(true));
        let (epoch, command) = review.submit(false).unwrap();
        assert_eq!(epoch, review.epoch);
        assert!(matches!(
            command,
            Command::RelocateTrack {
                source_id: 1,
                target_id: 2
            }
        ));
        assert!(review.busy == Some(Busy::Starting));
        assert!(!review.confirming);
        assert!(review.submit(true).is_none());
        assert!(!review.select_member(2, false));
        assert!(!review.reset(QueryRevisions::default()));
    }

    #[test]
    fn late_start_ack_is_rejected_without_touching_the_reopened_review() {
        let mut review = ready_review();
        review.confirm(true);
        let (epoch, _) = review.submit(false).unwrap();
        // Starting has no Core task ID yet: the async host must cancel the ID
        // from an acknowledgement rejected below, even if the entity was dropped.
        assert_eq!(review.close(), None);
        review.open(QueryRevisions::default());
        let late_ack = ack(true, Some(7));
        let cancellation = late_ack.relocation_id;
        assert!(!review.accept_ack(epoch, false, late_ack));
        assert_eq!(cancellation, Some(7));
        assert!(review.busy.is_none());
        assert!(review.feedback.is_none());
        assert!(!review.accept_task(&task(7, RelocationPhase::Completed)));
    }

    #[test]
    fn accepted_ack_cancellation_and_terminal_status_keep_core_task_identity() {
        let mut review = ready_review();
        review.confirm(true);
        let (epoch, _) = review.submit(false).unwrap();
        assert!(review.accept_ack(epoch, false, ack(true, Some(7))));
        assert_eq!(review.cancellable_task(), Some(7));
        review.accept_cancel(7, Err(anyhow::anyhow!("send failed")));
        assert_eq!(review.cancellable_task(), Some(7));
        assert!(
            matches!(&review.feedback, Some(ReviewFeedback::Text(error, _)) if error == "send failed")
        );
        review.accept_cancel(7, Ok(()));
        assert!(review.busy == Some(Busy::Cancelling(7)));
        assert_eq!(review.cancellable_task(), None);
        assert!(!review.accept_task(&task(8, RelocationPhase::Completed)));
        assert!(!review.accept_task(&task(7, RelocationPhase::Verifying)));
        // Cancellation is an intent, not a terminal fact: Core may finish first.
        assert!(review.accept_task(&task(7, RelocationPhase::Completed)));
        assert!(review.busy.is_none());
        assert!(matches!(
            review.feedback,
            Some(ReviewFeedback::Localized("gl-relocation-completed", _))
        ));
        assert!(!review.accept_task(&task(7, RelocationPhase::Cancelled)));
    }

    #[test]
    fn close_and_replace_return_only_known_core_tasks_for_cancellation() {
        for replacing in [false, true] {
            for cancelling in [false, true] {
                let mut review = ready_review();
                review.confirm(true);
                let (epoch, _) = review.submit(false).unwrap();
                review.accept_ack(epoch, false, ack(true, Some(7)));
                if cancelling {
                    review.accept_cancel(7, Ok(()));
                }
                let cancel = if replacing {
                    review.open(QueryRevisions::default())
                } else {
                    review.close()
                };
                assert_eq!(cancel, Some(7));
                assert_ne!(review.epoch, epoch);
                assert!(review.busy.is_none());
                assert_eq!(review.open, replacing);
            }
        }
    }

    #[test]
    fn ack_failure_missing_task_and_keep_refresh_release_only_their_gates() {
        for (ok, message) in [
            (false, "gl-review-action-failed"),
            (true, "gl-relocation-no-task"),
        ] {
            let mut review = ready_review();
            review.confirm(true);
            let (epoch, _) = review.submit(false).unwrap();
            assert!(review.accept_ack(epoch, false, ack(ok, None)));
            assert!(review.busy.is_none());
            assert!(
                matches!(review.feedback, Some(ReviewFeedback::Localized(key, _)) if key == message)
            );
        }
        let mut review = ready_review();
        let (epoch, command) = review.submit(true).unwrap();
        assert!(matches!(
            command,
            Command::KeepTrackIndependent { track_id: 2 }
        ));
        assert!(review.accept_ack(epoch, true, ack(true, None)));
        assert!(review.busy == Some(Busy::Keeping));
        review.request_groups(0, QueryRevisions::default());
        assert!(review.begin_groups().is_none());
        review.finish_ack_refresh();
        assert!(review.busy.is_none());
        assert!(review.begin_groups().is_some());
    }

    #[test]
    fn retained_read_gate_survives_reopen_and_old_result_cannot_finish_new_task() {
        let mut review = ready_review();
        review.confirm(true);
        let (epoch, _) = review.submit(false).unwrap();
        review.accept_ack(epoch, false, ack(true, Some(7)));
        assert_eq!(review.begin_task_read(7), Some(epoch));
        assert!(review.begin_task_read(7).is_none());
        review.close();
        review.open(QueryRevisions::default());
        review.members = vec![member(1, true), member(2, false)];
        review.members_current = true;
        review.select_member(1, true);
        review.select_member(2, false);
        review.confirm(true);
        let (new_epoch, _) = review.submit(false).unwrap();
        review.accept_ack(new_epoch, false, ack(true, Some(8)));
        assert!(review.begin_task_read(8).is_none());
        assert!(matches!(
            review.accept_task_read(epoch, 7, task_response(7, RelocationPhase::Completed)),
            TaskReadOutcome::Ignored
        ));
        assert_eq!(review.task_id(), Some(8));
        assert_eq!(review.begin_task_read(8), Some(new_epoch));
        assert!(matches!(
            review.accept_task_read(new_epoch, 8, task_response(8, RelocationPhase::Cancelled)),
            TaskReadOutcome::Finished
        ));
        assert!(review.busy.is_none());
        assert!(!review.task_query_pending);
    }

    #[test]
    fn failed_and_mismatched_retained_status_release_busy_with_feedback() {
        let mut review = ready_review();
        review.confirm(true);
        let (epoch, _) = review.submit(false).unwrap();
        review.accept_ack(epoch, false, ack(true, Some(7)));
        review.begin_task_read(7);
        assert!(matches!(
            review.accept_task_read(epoch, 7, task_response(8, RelocationPhase::Completed)),
            TaskReadOutcome::Failed
        ));
        assert!(review.busy.is_none());
        assert!(!review.task_query_pending);
        assert!(matches!(
            review.feedback,
            Some(ReviewFeedback::Localized("gl-verification-response", _))
        ));
        review.confirm(true);
        let (epoch, _) = review.submit(false).unwrap();
        review.accept_ack(epoch, false, ack(true, Some(8)));
        let mut failed = task(8, RelocationPhase::Failed);
        failed.error = Some("verification failed".into());
        assert!(review.accept_task(&failed));
        assert!(
            matches!(&review.feedback, Some(ReviewFeedback::Text(error, _)) if error == "verification failed")
        );
    }

    #[test]
    fn member_selection_read_must_share_page_revision_before_acceptance() {
        let mut review = ready_review();
        let revisions = QueryRevisions::default();
        review.request_members("fingerprint".into(), 0, revisions);
        let (read, selected_ids) = review.begin_members().unwrap();
        assert_eq!(selected_ids, vec![1, 2]);
        review.members_worker_finished();
        let response = QueryResponse {
            revisions,
            result: Ok(ViewResponse::DuplicateMembers(Page {
                total: 2,
                rows: vec![member(1, true).value],
            })),
        };
        let selected = QueryResponse {
            revisions: QueryRevisions {
                duplicate_members: 1,
                ..revisions
            },
            result: Ok(ViewResponse::DuplicateMembers(Page {
                total: 1,
                rows: vec![member(2, false).value],
            })),
        };
        assert!(!review.accept_members(
            read.epoch,
            &read.query,
            read.generation,
            (
                response,
                vec![Some(false)],
                Some((selected, vec![Some(true)]))
            ),
            revisions
        ));
        assert!(!review.members_current);
        assert!(review.members.is_empty());
        assert!(review.members_dirty);
        assert!(review.begin_members().is_some());
    }

    #[test]
    fn accepted_pages_rewind_empty_offsets_and_reconcile_member_choices() {
        let mut review = ready_review();
        let revisions = QueryRevisions::default();
        review.request_groups(12, revisions);
        let read = review.begin_groups().unwrap();
        review.groups_worker_finished();
        let response = QueryResponse {
            revisions,
            result: Ok(ViewResponse::DuplicateGroups(Page {
                total: 1,
                rows: Vec::new(),
            })),
        };
        assert!(!review.accept_groups(
            read.epoch,
            &read.query,
            read.generation,
            response,
            revisions
        ));
        assert_eq!(review.groups_offset, 0);
        assert!(review.begin_groups().is_some());
        review.request_members("fingerprint".into(), 0, revisions);
        let (read, _) = review.begin_members().unwrap();
        review.members_worker_finished();
        let response = QueryResponse {
            revisions,
            result: Ok(ViewResponse::DuplicateMembers(Page {
                total: 2,
                rows: vec![member(1, true).value, member(2, false).value],
            })),
        };
        assert!(review.accept_members(
            read.epoch,
            &read.query,
            read.generation,
            (response, vec![Some(false), Some(true)], None),
            revisions
        ));
        assert_eq!(review.members_total, 2);
        assert!(review.can_relocate());
        assert!(!review.members_loading);
    }

    #[test]
    fn retained_read_error_is_terminal_for_review_but_does_not_release_page_workers() {
        let mut review = ready_review();
        review.request_groups(0, QueryRevisions::default());
        assert!(review.begin_groups().is_some());
        review.confirm(true);
        let (epoch, _) = review.submit(false).unwrap();
        review.accept_ack(epoch, false, ack(true, Some(7)));
        review.begin_task_read(7);
        let response = QueryResponse {
            revisions: QueryRevisions::default(),
            result: Err(crate::response::QueryError::Message("read failed".into())),
        };
        assert!(matches!(
            review.accept_task_read(epoch, 7, response),
            TaskReadOutcome::Failed
        ));
        assert!(review.busy.is_none());
        assert!(!review.task_query_pending);
        assert!(review.groups_loading);
        assert!(
            matches!(&review.feedback, Some(ReviewFeedback::ReadFailed(error)) if error == "read failed")
        );
    }
}
