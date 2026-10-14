//! Bounded ordinary-file review. Query epochs belong to the dialog; task IDs belong to Core.

use super::*;
use crate::{
    model::{Page, RelocationPhase, RelocationTaskStatus},
    response::{QueryRevisions, ViewResponse},
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
    groups_request: u64,
    members_request: u64,
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
    fn accepts_groups(&self, epoch: u64, request: u64) -> bool {
        self.open && self.epoch == epoch && self.groups_request == request
    }

    fn accepts_members(&self, epoch: u64, request: u64) -> bool {
        self.open && self.epoch == epoch && self.members_request == request
    }

    fn clear_selection(&mut self) {
        self.source = None;
        self.target = None;
        self.confirming = false;
    }

    fn invalidate(&mut self) {
        self.groups_request = self.groups_request.wrapping_add(1);
        self.members_request = self.members_request.wrapping_add(1);
        self.groups_loading = false;
        self.members_loading = false;
        self.groups = None;
        self.members.clear();
        self.members_total = 0;
        self.clear_selection();
    }
}

impl GuiApp {
    pub(super) fn open_duplicate_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_interaction(window, cx);
        self.panel_menu = None;
        self.app_menu_open = false;
        self.catalog_open = false;
        let epoch = self.duplicate_review.epoch.wrapping_add(1);
        self.duplicate_review = DuplicateReview {
            open: true,
            epoch,
            ..Default::default()
        };
        self.duplicate_focus.focus(window, cx);
        self.request_duplicate_groups(0, cx);
        cx.notify();
    }

    pub(super) fn close_duplicate_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_duplicate_task();
        self.duplicate_review.open = false;
        self.duplicate_review.epoch = self.duplicate_review.epoch.wrapping_add(1);
        self.duplicate_review.invalidate();
        self.duplicate_review.busy = None;
        self.focus_layout(window, cx);
        cx.notify();
    }

    pub(super) fn cancel_duplicate_task(&self) {
        if let Some(Busy::Relocating(id) | Busy::Cancelling(id)) = self.duplicate_review.busy {
            let _ = self.handle.send(Command::CancelRelocation { id });
        }
    }

    fn review_revisions_match(&self, revisions: QueryRevisions) -> bool {
        revisions.library == self.state.library.revision
            && revisions.structure == self.state.library.structure_revision
            && revisions.playlist == self.state.library.playlist_revision
            && revisions.queue == self.state.queue.revision
    }

    fn request_duplicate_groups(&mut self, offset: usize, cx: &mut Context<Self>) {
        let review = &mut self.duplicate_review;
        review.groups_offset = offset;
        review.groups_loading = true;
        review.groups_request = review.groups_request.wrapping_add(1);
        let (epoch, request) = (review.epoch, review.groups_request);
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            if !this
                .update(cx, |this, _| {
                    this.duplicate_review.accepts_groups(epoch, request)
                })
                .unwrap_or(false)
            {
                return;
            }
            let response = cx
                .background_executor()
                .spawn(async move {
                    handle.query(Query::DuplicateGroups {
                        offset,
                        limit: GROUP_PAGE,
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                if !this.duplicate_review.accepts_groups(epoch, request) {
                    return;
                }
                if !this.review_revisions_match(response.revisions) {
                    this.request_duplicate_groups(offset, cx);
                    return;
                }
                this.duplicate_review.groups_loading = false;
                match response.result {
                    Ok(ViewResponse::DuplicateGroups(page)) => {
                        if offset > 0 && offset >= page.total {
                            this.request_duplicate_groups(0, cx);
                            return;
                        }
                        this.duplicate_review.groups = Some(page);
                    }
                    Err(error) => {
                        this.duplicate_review.feedback =
                            Some((error.to_string(), StatusKind::Error))
                    }
                    _ => {
                        this.duplicate_review.feedback =
                            Some(("Unexpected same-files response".into(), StatusKind::Error))
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
        let review = &mut self.duplicate_review;
        if review.fingerprint.as_ref() != Some(&fingerprint) {
            review.clear_selection();
        }
        review.fingerprint = Some(fingerprint.clone());
        review.members_offset = offset;
        review.members.clear();
        review.members_loading = true;
        review.members_request = review.members_request.wrapping_add(1);
        let (epoch, request) = (review.epoch, review.members_request);
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            if !this
                .update(cx, |this, _| {
                    this.duplicate_review.accepts_members(epoch, request)
                })
                .unwrap_or(false)
            {
                return;
            }
            let (response, existence) = cx
                .background_executor()
                .spawn(async move {
                    let response = handle.query(Query::DuplicateMembers {
                        fingerprint,
                        offset,
                        limit: MEMBER_PAGE,
                    });
                    let existence = match &response.result {
                        Ok(ViewResponse::DuplicateMembers(page)) => page
                            .rows
                            .iter()
                            .map(|member| match std::fs::metadata(&member.track.path) {
                                Ok(metadata) if metadata.is_file() => Some(true),
                                Ok(_) => None,
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                    Some(false)
                                }
                                Err(_) => None,
                            })
                            .collect::<Vec<_>>(),
                        _ => Vec::new(),
                    };
                    (response, existence)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                if !this.duplicate_review.accepts_members(epoch, request) {
                    return;
                }
                if !this.review_revisions_match(response.revisions) {
                    if let Some(hash) = this.duplicate_review.fingerprint.clone() {
                        this.request_duplicate_members(hash, offset, cx);
                    }
                    return;
                }
                this.duplicate_review.members_loading = false;
                match response.result {
                    Ok(ViewResponse::DuplicateMembers(page)) => {
                        if offset > 0 && offset >= page.total {
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
                    }
                    Err(error) => {
                        this.duplicate_review.feedback =
                            Some((error.to_string(), StatusKind::Error))
                    }
                    _ => {
                        this.duplicate_review.feedback =
                            Some(("Unexpected member response".into(), StatusKind::Error))
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn sync_duplicate_review(&mut self, changed: bool, cx: &mut Context<Self>) {
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
        if (changed || finished) && self.duplicate_review.busy.is_none() {
            self.reload_duplicate_review(cx);
        }
    }

    fn accept_duplicate_task(&mut self, task: &RelocationTaskStatus) -> bool {
        if task.phase == RelocationPhase::Verifying {
            return false;
        }
        self.duplicate_review.feedback = Some(match task.phase {
            RelocationPhase::Completed => ("Relocation completed. The old identity and user state were preserved; destination freshly verified before commit. No audio files were deleted.".into(), StatusKind::Success),
            RelocationPhase::Cancelled => ("Verification cancelled; no relocation committed.".into(), StatusKind::Info),
            RelocationPhase::Failed => (task.error.clone().unwrap_or_else(|| "Relocation failed; no state was merged. Refresh and choose again.".into()), StatusKind::Error),
            RelocationPhase::Verifying => unreachable!(),
        });
        self.duplicate_review.busy = None;
        true
    }

    fn reload_duplicate_review(&mut self, cx: &mut Context<Self>) {
        let offset = self.duplicate_review.groups_offset;
        let fingerprint = self.duplicate_review.fingerprint.clone();
        self.duplicate_review.invalidate();
        self.request_duplicate_groups(offset, cx);
        if let Some(hash) = fingerprint {
            self.request_duplicate_members(hash, 0, cx);
        }
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
                if !this.duplicate_review.open || this.duplicate_review.epoch != epoch { return; }
                this.duplicate_review.task_query_pending = false;
                if !matches!(this.duplicate_review.busy, Some(Busy::Relocating(current) | Busy::Cancelling(current)) if current == id) { return; }
                match response.result {
                    Ok(ViewResponse::RelocationTask(task)) if task.id == id => {
                        if this.accept_duplicate_task(&task) {
                            this.refresh(cx);
                            this.reload_duplicate_review(cx);
                        }
                    }
                    Err(error) => {
                        this.duplicate_review.busy = None;
                        this.duplicate_review.feedback = Some((format!("Could not read verification result: {error}. Refresh the library before deciding again."), StatusKind::Error));
                        this.reload_duplicate_review(cx);
                    }
                    _ => {
                        this.duplicate_review.busy = None;
                        this.duplicate_review.feedback = Some(("Unexpected verification result; refresh before deciding again.".into(), StatusKind::Error));
                        this.reload_duplicate_review(cx);
                    }
                }
                cx.notify();
            });
        }).detach();
    }

    fn submit_duplicate_action(&mut self, independent: bool, cx: &mut Context<Self>) {
        if self.duplicate_review.busy.is_some() {
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
            if !self.duplicate_review.confirming {
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
            let ack = cx.background_executor().spawn(async move { ack_handle.request_ack(command) }).await;
            let relocation_id = ack.relocation_id;
            let accepted = this.update(cx, |this, cx| {
                if !this.duplicate_review.open || this.duplicate_review.epoch != epoch { return false; }
                if !ack.ok {
                    this.duplicate_review.busy = None;
                    this.duplicate_review.feedback = Some((ack.error.unwrap_or_else(|| "Could not complete review action".into()), StatusKind::Error));
                } else if independent {
                    this.duplicate_review.feedback = Some(("Destination kept independent. Future scans will not relocate it into another identity.".into(), StatusKind::Success));
                } else if let Some(id) = relocation_id {
                    this.duplicate_review.busy = Some(Busy::Relocating(id));
                } else {
                    this.duplicate_review.busy = None;
                    this.duplicate_review.feedback = Some(("Relocation acknowledgement did not identify a verification task".into(), StatusKind::Error));
                }
                this.refresh(cx);
                if independent && ack.ok {
                    this.duplicate_review.busy = None;
                    this.reload_duplicate_review(cx);
                } else {
                    this.sync_duplicate_review(false, cx);
                }
                cx.notify();
                true
            }).unwrap_or(false);
            // Closing/replacing the root can precede the start acknowledgement.
            if !accepted && let Some(id) = relocation_id {
                let _ = handle.send(Command::CancelRelocation { id });
            }
        }).detach();
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

    pub(super) fn duplicate_review_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let review = &self.duplicate_review;
        let busy = review.busy.is_some();
        let mut groups = column().gap_2();
        if review.groups_loading {
            groups = groups.child(caption("Loading same-content groups…"));
        } else if let Some(page) = &review.groups {
            if page.total == 0 {
                groups = groups.child(caption("No ordinary-file same-content groups."));
            }
            for (index, group) in page.rows.iter().enumerate() {
                let hash = group.fingerprint.clone();
                let selected = review.fingerprint.as_ref() == Some(&hash);
                groups = groups.child(active_control(
                    button(
                        ("duplicate-group", index),
                        format!(
                            "{} · {} files · {} missing / {} present (last scan)",
                            &hash[..hash.len().min(12)],
                            group.total_members,
                            group.missing_members,
                            group.present_members
                        ),
                        cx,
                        move |this, _, cx| {
                            if this.duplicate_review.busy.is_none() {
                                this.duplicate_review.feedback = None;
                                this.request_duplicate_members(hash.clone(), 0, cx);
                                cx.notify();
                            }
                        },
                    )
                    .w_full()
                    .justify_start()
                    .when(busy, |view| view.opacity(0.5)),
                    selected,
                ));
            }
        }
        let group_total = review.groups.as_ref().map_or(0, |page| page.total);
        let group_offset = review.groups_offset;
        groups = groups.child(self.review_pager(
            true,
            group_offset,
            group_total,
            GROUP_PAGE,
            busy || review.groups_loading,
            cx,
        ));

        let mut members = column().flex_shrink_0().gap_2();
        if let Some(fingerprint) = &review.fingerprint {
            members = members.child(caption(format!("Historical cached BLAKE3: {fingerprint}")));
        }
        if review.members_loading {
            members = members.child(caption("Loading file metadata and checking paths…"));
        } else if review.fingerprint.is_none() {
            members = members.child(caption(
                "Select a group to inspect its paths. This list is read-only.",
            ));
        } else if review.members.is_empty() {
            members = members.child(caption(
                "This group no longer has members. Refresh to review current groups.",
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
            let selectable_source =
                track.missing && member.exists == Some(false) && !value.independent;
            let selectable_target =
                !track.missing && member.exists == Some(true) && !value.independent;
            let id = track.id;
            let existence = match member.exists {
                Some(true) => "file exists now",
                Some(false) => "file absent now",
                None => "existence unknown (path check failed / not a regular file)",
            };
            let last_played = track
                .last_played
                .and_then(|timestamp| chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0))
                .map(|time| {
                    time.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string()
                })
                .unwrap_or_else(|| "Never".into());
            let mut card = column()
                .flex_shrink_0()
                .min_w_0()
                .gap_1()
                .p_2()
                .border_1()
                .rounded_md()
                .border_color(rgb(if source_selected || target_selected {
                    ACCENT
                } else {
                    BORDER
                }))
                .child(div().text_sm().child(track.path.display().to_string()))
                .child(caption(format!(
                    "ID {id} · {existence} · last scan: {} · {} bytes · cached mtime(ns): {}",
                    if track.missing { "missing" } else { "present" },
                    value.file_size,
                    value.modified_ns
                )))
                .child(caption(format!(
                    "Title: {} · Artist: {} · Album: {} · Duration: {}",
                    track.title,
                    track.artist,
                    track.album,
                    track
                        .duration
                        .map(format_time)
                        .unwrap_or_else(|| "Unknown".into())
                )))
                .child(caption(format!(
                    "Favorite: {} · Overrides: {} · Plays: {} · Last played: {last_played}",
                    track.favorite, value.has_overrides, track.play_count
                )))
                .child(caption(format!(
                    "Playlist references: {} · Queue references: {} · {}",
                    value.playlist_references,
                    value.queue_references,
                    if value.independent {
                        "Kept independent (not eligible for relocation)"
                    } else {
                        "Relocation eligible"
                    }
                )))
                .child(caption(
                    "Hash: historical cached BLAKE3 — NOT freshly verified",
                ));
            let mut actions = panel_toolbar();
            if selectable_source {
                actions = actions.child(active_control(
                    button(
                        ("review-source", id as u64),
                        "Select missing old identity",
                        cx,
                        move |this, _, cx| {
                            if this.duplicate_review.busy.is_none()
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
                    )
                    .when(busy || review.confirming, |view| view.opacity(0.5)),
                    source_selected,
                ));
            }
            if selectable_target {
                actions = actions.child(active_control(
                    button(
                        ("review-target", id as u64),
                        "Select present destination",
                        cx,
                        move |this, _, cx| {
                            if this.duplicate_review.busy.is_none()
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
                    )
                    .when(busy || review.confirming, |view| view.opacity(0.5)),
                    target_selected,
                ));
            }
            if has_target_state(value) {
                card = card.child(status_message("Destination has user state/references: relocation into this row will be rejected, not merged.", StatusKind::Error));
            }
            members = members.child(card.child(actions));
        }
        if review.fingerprint.is_some() {
            members = members.child(self.review_pager(
                false,
                review.members_offset,
                review.members_total,
                MEMBER_PAGE,
                busy || review.members_loading || review.confirming,
                cx,
            ));
        }
        let mut decisions = column().gap_2();
        if let Some(source) = &review.source {
            decisions = decisions.child(caption(format!(
                "Old identity #{}: {}",
                source.track.id,
                source.track.path.display()
            )));
        }
        if let Some(target) = &review.target {
            decisions = decisions.child(caption(format!(
                "Destination #{}: {}",
                target.track.id,
                target.track.path.display()
            )));
        }
        if review.confirming {
            decisions = decisions.child(status_message("Confirm relocation? Preserve the old ID, favorite, overrides, play history and references at the selected destination. The destination row must have no user state/references. Rivu rehashes its full bytes and revalidates the database before commit. No audio files are deleted.", StatusKind::Info))
                .child(panel_toolbar()
                    .child(button("review-confirm", "Confirm and verify destination", cx, |this, _, cx| this.submit_duplicate_action(false, cx)))
                    .child(button("review-back", "Back", cx, |this, _, cx| { this.duplicate_review.confirming = false; cx.notify(); })));
        } else if !busy {
            let can_relocate = review.source.is_some()
                && review
                    .target
                    .as_ref()
                    .is_some_and(|target| !has_target_state(target));
            decisions = decisions.child(
                panel_toolbar()
                    .when(can_relocate, |view| {
                        view.child(button(
                            "review-relocate",
                            "Review relocation…",
                            cx,
                            |this, _, cx| {
                                this.duplicate_review.confirming = true;
                                cx.notify();
                            },
                        ))
                    })
                    .when(review.target.is_some(), |view| {
                        view.child(button(
                            "review-independent",
                            "Keep destination independent",
                            cx,
                            |this, _, cx| this.submit_duplicate_action(true, cx),
                        ))
                    }),
            );
        }
        if let Some(task) = review.busy {
            decisions = decisions.child(status_message(match task {
                Busy::Starting => "Starting fresh destination verification…",
                Busy::Relocating(_) => "Verifying full destination bytes in background; cached hash is not fresh evidence…",
                Busy::Cancelling(_) => "Cancelling verification… (a completed commit cannot be undone)",
                Busy::Keeping => "Saving independent-file decision…",
            }, StatusKind::Info));
            if matches!(task, Busy::Relocating(_)) {
                decisions = decisions.child(button(
                    "review-cancel-verification",
                    "Cancel verification",
                    cx,
                    |this, _, cx| this.cancel_review_verification(cx),
                ));
            }
        }
        if let Some((message, kind)) = &review.feedback {
            decisions = decisions.child(status_message(message.clone(), *kind));
        }
        div().id("duplicate-review-modal").track_focus(&self.duplicate_focus).occlude().absolute().size_full()
            .flex().items_center().justify_center().bg(rgb(0x000000).alpha(0.58))
            .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| cx.stop_propagation()))
            .on_mouse_down(MouseButton::Right, cx.listener(|_, _, _, cx| cx.stop_propagation()))
            .on_mouse_down(MouseButton::Middle, cx.listener(|_, _, _, cx| cx.stop_propagation()))
            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.stop_propagation()))
            .child(column().id("duplicate-review-card").w(relative(0.94)).h(relative(0.92)).min_w_0().min_h_0()
                .p_4().gap_3().bg(rgb(PANEL)).border_1().border_color(rgb(BORDER)).rounded_md()
                .child(panel_header("Same files / Relocation candidates", "Ordinary files only. Groups use historical cached hashes; no CUE operations or file deletion."))
                .child(panel_toolbar()
                    .child(button("review-refresh", "Refresh", cx, |this, _, cx| {
                        if this.duplicate_review.busy.is_none() {
                            this.duplicate_review.invalidate();
                            this.duplicate_review.fingerprint = None;
                            this.duplicate_review.feedback = None;
                            this.request_duplicate_groups(0, cx);
                            cx.notify();
                        }
                    }).when(busy, |view| view.opacity(0.5)))
                    .child(button("review-close", "Close / defer", cx, |this, window, cx| this.close_duplicate_review(window, cx))))
                .child(row().flex_1().min_h_0().items_stretch()
                    .child(column().id("duplicate-groups-scroll").w(relative(0.32)).min_h_0().overflow_y_scroll().child(groups))
                    .child(column().id("duplicate-members-scroll").flex_1().min_w_0().min_h_0().overflow_y_scroll().child(members)))
                .child(decisions)
                .child(caption("Close defers undecided pairs and requests cancellation of ongoing verification. Keep independent is a saved decision, not a merge.")))
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
        let mut pager = panel_toolbar().child(caption(format!(
            "{}–{} of {total}",
            if total == 0 { 0 } else { offset + 1 },
            (offset + size).min(total)
        )));
        for (next, label) in [
            (offset.saturating_sub(size), "Previous"),
            (offset + size, "Next"),
        ] {
            let enabled = !disabled
                && (if label == "Previous" {
                    offset > 0
                } else {
                    next < total
                });
            pager = pager.child(
                button(
                    (
                        if groups {
                            "review-group-page"
                        } else {
                            "review-member-page"
                        },
                        next,
                    ),
                    label,
                    cx,
                    move |this, _, cx| {
                        if !enabled || this.duplicate_review.busy.is_some() {
                            return;
                        }
                        if groups {
                            this.request_duplicate_groups(next, cx);
                        } else if let Some(hash) = this.duplicate_review.fingerprint.clone() {
                            this.request_duplicate_members(hash, next, cx);
                        }
                        cx.notify();
                    },
                )
                .when(!enabled, |view| view.opacity(0.4)),
            );
        }
        pager
    }
}

impl Drop for GuiApp {
    fn drop(&mut self) {
        self.cancel_duplicate_task();
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
    use super::DuplicateReview;

    #[test]
    fn refresh_rejects_outstanding_group_and_member_results() {
        let mut review = DuplicateReview {
            open: true,
            epoch: 4,
            groups_request: 2,
            members_request: 3,
            ..Default::default()
        };
        assert!(review.accepts_groups(4, 2));
        assert!(review.accepts_members(4, 3));
        review.invalidate();
        assert!(!review.accepts_groups(4, 2));
        assert!(!review.accepts_members(4, 3));
    }

    #[test]
    fn close_reopen_never_accepts_previous_dialog_results() {
        let mut review = DuplicateReview {
            open: true,
            epoch: 4,
            groups_request: 1,
            members_request: 1,
            ..Default::default()
        };
        review.open = false;
        assert!(!review.accepts_groups(4, 1));
        assert!(!review.accepts_members(4, 1));
        review = DuplicateReview {
            open: true,
            epoch: 5,
            groups_request: 1,
            members_request: 1,
            ..Default::default()
        };
        assert!(!review.accepts_groups(4, 1));
        assert!(!review.accepts_members(4, 1));
        assert!(review.accepts_groups(5, 1));
        assert!(review.accepts_members(5, 1));
    }
}
