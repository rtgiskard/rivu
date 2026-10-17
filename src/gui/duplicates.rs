//! Bounded ordinary-file review. Projections own data validity; Core owns task IDs.

use super::*;
use crate::{
    model::{Page, RelocationPhase, RelocationTaskStatus},
    projection::QueryProjection,
    response::{QueryResponse, QueryRevisions, ViewResponse},
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
    feedback: Option<(String, StatusKind)>,
}

impl DuplicateReview {
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
        self.cancel_duplicate_task();
        self.duplicate_review.busy = None;
        self.panel_menu = None;
        self.app_menu_open = false;
        self.catalog_open = false;
        self.duplicate_review.epoch = self.duplicate_review.epoch.wrapping_add(1);
        self.duplicate_review.invalidate();
        self.duplicate_review.open = true;
        self.duplicate_review.fingerprint = None;
        self.duplicate_review.members_dirty = false;
        self.duplicate_review.feedback = None;
        self.request_duplicate_groups(0, cx);
        cx.notify();
    }

    pub(super) fn close_duplicate_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_duplicate_task();
        self.duplicate_review.open = false;
        self.duplicate_review.epoch = self.duplicate_review.epoch.wrapping_add(1);
        self.duplicate_review.invalidate();
        self.duplicate_review.busy = None;
        if self.settings_open {
            self.settings.focus_handle(cx).focus(window, cx);
        } else {
            self.focus_layout(window, cx);
        }
        cx.notify();
    }

    pub(super) fn cancel_duplicate_task(&self) {
        if let Some(Busy::Relocating(id) | Busy::Cancelling(id)) = self.duplicate_review.busy {
            let _ = self.handle.send(Command::CancelRelocation { id });
        }
    }

    fn request_duplicate_groups(&mut self, offset: usize, cx: &mut Context<Self>) {
        let revisions = self.state.query_revisions(None);
        let review = &mut self.duplicate_review;
        if review.groups_offset != offset {
            review.groups = None;
        }
        review.groups_offset = offset;
        review.sync_queries(revisions);
        review.groups_projection.cancel();
        review.groups_dirty = true;
        review.groups_failed = false;
        self.start_duplicate_groups(cx);
    }

    fn start_duplicate_groups(&mut self, cx: &mut Context<Self>) {
        let review = &mut self.duplicate_review;
        if !review.open || review.busy.is_some() || review.groups_loading || !review.groups_dirty {
            return;
        }
        review.groups_dirty = false;
        review.groups_loading = true;
        let query = review.groups_query();
        let (epoch, generation) = (review.epoch, review.groups_projection.generation());
        let ticket = review.groups_projection.ticket();
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
                        this.duplicate_review.groups_loading = false;
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
                this.duplicate_review.groups_loading = false;
                this.refresh(cx);
                if !this.duplicate_review.accepts_groups(
                    epoch,
                    &query,
                    generation,
                    response.revisions,
                ) {
                    if this.duplicate_review.groups_projection.generation() == generation {
                        this.duplicate_review.groups_dirty = true;
                    }
                    this.start_duplicate_groups(cx);
                    return;
                }
                match response.result {
                    Ok(ViewResponse::DuplicateGroups(page)) => {
                        if this.duplicate_review.groups_offset > 0
                            && this.duplicate_review.groups_offset >= page.total
                        {
                            this.request_duplicate_groups(0, cx);
                            return;
                        }
                        this.duplicate_review.groups = Some(page);
                    }
                    Err(error) => {
                        this.duplicate_review.groups_failed = true;
                        this.duplicate_review.feedback =
                            Some((error.to_string(), StatusKind::Error));
                    }
                    _ => {
                        this.duplicate_review.groups_failed = true;
                        this.duplicate_review.feedback = Some((
                            this.i18n.text("gl-duplicate-response").into(),
                            StatusKind::Error,
                        ));
                    }
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
        let revisions = self.state.query_revisions(None);
        let review = &mut self.duplicate_review;
        if review.fingerprint.as_ref() != Some(&fingerprint) {
            review.clear_selection();
            review.members_total = 0;
        }
        review.fingerprint = Some(fingerprint);
        review.members_offset = offset;
        review.sync_queries(revisions);
        review.members_projection.cancel();
        review.members.clear();
        review.members_current = false;
        review.members_dirty = true;
        review.members_failed = false;
        review.confirming = false;
        self.start_duplicate_members(cx);
    }

    fn start_duplicate_members(&mut self, cx: &mut Context<Self>) {
        let review = &mut self.duplicate_review;
        if !review.open || review.busy.is_some() || review.members_loading || !review.members_dirty
        {
            return;
        }
        let Some(query) = review.members_query() else {
            return;
        };
        review.members_dirty = false;
        review.members_loading = true;
        let selected_ids = review
            .source
            .iter()
            .chain(review.target.iter())
            .map(|member| member.track.id)
            .collect::<Vec<_>>();
        let (epoch, generation) = (review.epoch, review.members_projection.generation());
        let ticket = review.members_projection.ticket();
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
                        this.duplicate_review.members_loading = false;
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
                this.duplicate_review.members_loading = false;
                this.refresh(cx);
                let selected_current = selected.as_ref().is_none_or(|(selected, _)| {
                    query.stamp(selected.revisions) == query.stamp(response.revisions)
                });
                if !selected_current
                    || !this.duplicate_review.accepts_members(
                        epoch,
                        &query,
                        generation,
                        response.revisions,
                    )
                {
                    if this.duplicate_review.members_projection.generation() == generation {
                        this.duplicate_review.members_dirty = true;
                    }
                    this.start_duplicate_members(cx);
                    return;
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
                    Some((
                        QueryResponse {
                            result: Err(error), ..
                        },
                        _,
                    )) => {
                        this.fail_duplicate_members(error.to_string(), cx);
                        return;
                    }
                    Some(_) => {
                        this.fail_duplicate_members(
                            this.i18n.text("gl-member-response").into(),
                            cx,
                        );
                        return;
                    }
                    None => Vec::new(),
                };
                match response.result {
                    Ok(ViewResponse::DuplicateMembers(page)) => {
                        if this.duplicate_review.members_offset > 0
                            && this.duplicate_review.members_offset >= page.total
                        {
                            if let Some(hash) = this.duplicate_review.fingerprint.clone() {
                                this.request_duplicate_members(hash, 0, cx);
                            }
                            return;
                        }
                        this.duplicate_review.members_total = page.total;
                        this.duplicate_review.members = page
                            .rows
                            .into_iter()
                            .zip(existence)
                            .map(|(value, exists)| Member { value, exists })
                            .collect();
                        this.duplicate_review.reconcile_selection(&selected);
                    }
                    Err(error) => this.fail_duplicate_members(error.to_string(), cx),
                    _ => {
                        this.fail_duplicate_members(this.i18n.text("gl-member-response").into(), cx)
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn fail_duplicate_members(&mut self, message: String, cx: &mut Context<Self>) {
        self.duplicate_review.members_failed = true;
        self.duplicate_review.members_current = false;
        self.duplicate_review.feedback = Some((message, StatusKind::Error));
        cx.notify();
    }

    pub(super) fn sync_duplicate_review(&mut self, cx: &mut Context<Self>) {
        let data_changed = self
            .duplicate_review
            .sync_queries(self.state.query_revisions(None));
        if !self.duplicate_review.open {
            return;
        }
        let task_id = match self.duplicate_review.busy {
            Some(Busy::Relocating(id) | Busy::Cancelling(id)) => Some(id),
            _ => None,
        };
        let task = task_id
            .and_then(|id| {
                self.state
                    .system
                    .relocation_task
                    .as_ref()
                    .filter(|task| task.id == id)
            })
            .cloned();
        let finished = task
            .as_ref()
            .is_some_and(|task| self.accept_duplicate_task(task));
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

    fn accept_duplicate_task(&mut self, task: &RelocationTaskStatus) -> bool {
        if task.phase == RelocationPhase::Verifying {
            return false;
        }
        self.duplicate_review.feedback = Some(match task.phase {
            RelocationPhase::Completed => (
                self.i18n.text("gl-relocation-completed").into(),
                StatusKind::Success,
            ),
            RelocationPhase::Cancelled => (
                self.i18n.text("gl-verification-cancelled").into(),
                StatusKind::Info,
            ),
            RelocationPhase::Failed => (
                task.error
                    .clone()
                    .unwrap_or_else(|| self.i18n.text("gl-relocation-failed").into()),
                StatusKind::Error,
            ),
            RelocationPhase::Verifying => unreachable!(),
        });
        self.duplicate_review.busy = None;
        true
    }

    fn request_retained_relocation(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.duplicate_review.task_query_pending {
            return;
        }
        self.duplicate_review.task_query_pending = true;
        let epoch = self.duplicate_review.epoch;
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let response = cx.background_executor().spawn(async move {
                handle.query(Query::RelocationTask { id })
            }).await;
            let _ = this.update(cx, |this, cx| {
                this.duplicate_review.task_query_pending = false;
                if !this.duplicate_review.open || this.duplicate_review.epoch != epoch { return; }
                if !matches!(this.duplicate_review.busy, Some(Busy::Relocating(current) | Busy::Cancelling(current)) if current == id) { return; }
                match response.result {
                    Ok(ViewResponse::RelocationTask(task)) if task.id == id => {
                        if this.accept_duplicate_task(&task) {
                            this.refresh(cx);
                            this.sync_duplicate_review(cx);
                        }
                    }
                    Err(error) => {
                        this.duplicate_review.busy = None;
                        this.duplicate_review.feedback = Some((this.i18n.format("gl-verification-read-failed", &[("error", (error.to_string()).into())]), StatusKind::Error));
                        this.sync_duplicate_review(cx);
                    }
                    _ => {
                        this.duplicate_review.busy = None;
                        this.duplicate_review.feedback = Some((this.i18n.text("gl-verification-response").into(), StatusKind::Error));
                        this.sync_duplicate_review(cx);
                    }
                }
                cx.notify();
            });
        }).detach();
    }

    fn submit_duplicate_action(&mut self, independent: bool, cx: &mut Context<Self>) {
        self.refresh(cx);
        if self.duplicate_review.busy.is_some() || !self.duplicate_review.members_current {
            return;
        }
        let Some(target) = self.duplicate_review.target.as_ref() else {
            return;
        };
        let command = if independent {
            Command::KeepTrackIndependent {
                track_id: target.track.id,
            }
        } else {
            if !self.duplicate_review.confirming || !self.duplicate_review.can_relocate() {
                return;
            }
            let Some(source) = self.duplicate_review.source.as_ref() else {
                return;
            };
            Command::RelocateTrack {
                source_id: source.track.id,
                target_id: target.track.id,
            }
        };
        let epoch = self.duplicate_review.epoch;
        self.duplicate_review.busy = Some(if independent {
            Busy::Keeping
        } else {
            Busy::Starting
        });
        self.duplicate_review.confirming = false;
        self.duplicate_review.feedback = None;
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
                    if !this.duplicate_review.open || this.duplicate_review.epoch != epoch {
                        return false;
                    }
                    if !ack.ok {
                        this.duplicate_review.busy = None;
                        this.duplicate_review.feedback = Some((
                            ack.error.unwrap_or_else(|| {
                                this.i18n.text("gl-review-action-failed").into()
                            }),
                            StatusKind::Error,
                        ));
                    } else if independent {
                        this.duplicate_review.feedback = Some((
                            this.i18n.text("gl-kept-independent").into(),
                            StatusKind::Success,
                        ));
                    } else if let Some(id) = relocation_id {
                        this.duplicate_review.busy = Some(Busy::Relocating(id));
                    } else {
                        this.duplicate_review.busy = None;
                        this.duplicate_review.feedback = Some((
                            this.i18n.text("gl-relocation-no-task").into(),
                            StatusKind::Error,
                        ));
                    }
                    this.refresh(cx);
                    if independent && ack.ok {
                        this.duplicate_review.busy = None;
                    }
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
        let Some(Busy::Relocating(id)) = self.duplicate_review.busy else {
            return;
        };
        match self.handle.send(Command::CancelRelocation { id }) {
            Ok(()) => self.duplicate_review.busy = Some(Busy::Cancelling(id)),
            Err(error) => {
                self.duplicate_review.feedback = Some((format!("{error:#}"), StatusKind::Error))
            }
        }
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
                            if this.duplicate_review.busy.is_none()
                                && !this.duplicate_review.confirming
                            {
                                this.duplicate_review.feedback = None;
                                this.request_duplicate_members(hash.clone(), 0, cx);
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
                                if this.duplicate_review.busy.is_none()
                                    && this.duplicate_review.members_current
                                    && !this.duplicate_review.confirming
                                {
                                    this.duplicate_review.source = this
                                        .duplicate_review
                                        .members
                                        .iter()
                                        .find(|row| row.value.track.id == id)
                                        .map(|row| row.value.clone());
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
                                if this.duplicate_review.busy.is_none()
                                    && this.duplicate_review.members_current
                                    && !this.duplicate_review.confirming
                                {
                                    this.duplicate_review.target = this
                                        .duplicate_review
                                        .members
                                        .iter()
                                        .find(|row| row.value.track.id == id)
                                        .map(|row| row.value.clone());
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
                                this.duplicate_review.confirming = false;
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
                                if this.duplicate_review.can_relocate() {
                                    this.duplicate_review.confirming = true;
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
        if let Some((message, kind)) = &review.feedback {
            decisions = decisions.child(status_message(theme, message.clone(), *kind));
        }
        div()
            .id("duplicate-review-modal")
            .map(|surface| self.dialog_surface(surface))
            .occlude()
            .absolute()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x000000).alpha(0.58))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.stop_propagation()))
            .child(
                column()
                    .id("duplicate-review-card")
                    .w(relative(0.94))
                    .h(relative(0.92))
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
                                            if this.duplicate_review.busy.is_none() {
                                                this.duplicate_review.invalidate();
                                                this.duplicate_review.fingerprint = None;
                                                this.duplicate_review.members_dirty = false;
                                                this.duplicate_review.feedback = None;
                                                this.request_duplicate_groups(0, cx);
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
                            .flex_1()
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

impl Drop for GuiApp {
    fn drop(&mut self) {
        self.cancel_duplicate_task();
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
    use super::{DuplicateReview, Member};
    use crate::{
        model::{Page, Track},
        response::QueryRevisions,
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
        let groups = review.groups_query();
        let members = review.members_query().unwrap();
        let groups_generation = review.groups_projection.generation();
        let members_generation = review.members_projection.generation();
        review.groups_loading = true;
        review.members_loading = true;
        assert!(review.accepts_groups(4, &groups, groups_generation, revisions));
        assert!(review.accepts_members(4, &members, members_generation, revisions));
        review.invalidate();
        assert!(review.groups_loading);
        assert!(review.members_loading);
        assert!(!review.accepts_groups(4, &groups, groups_generation, revisions));
        assert!(!review.accepts_members(4, &members, members_generation, revisions));
        review.open = false;
        review.epoch += 1;
        review.open = true;
        assert!(!review.accepts_groups(
            4,
            &groups,
            review.groups_projection.generation(),
            revisions
        ));
        assert!(!review.accepts_members(
            4,
            &members,
            review.members_projection.generation(),
            revisions
        ));
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
}
