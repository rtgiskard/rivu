//! One metadata edit session follows either an explicit selection or playback.
//!
//! Input entities hold tag drafts; the window holds the decoder hint enum draft. A loaded
//! track supplies display details; its refresh must not move a dirty edit's
//! baseline. Identity epochs reject responses and save acknowledgements from a
//! previous selection, including a switch away and back to the same track.
//! Playlist container/membership changes do not change this track identity;
//! they must not cancel the editor or its in-flight save.
//! Dirty or saving playback-follow sessions stay on their original identity.
//! Manual changes defer the whole selection operation behind one confirmation;
//! no browser selection is mutated until Discard (or a clean save) resolves it.

pub(in crate::gui) mod session;

use super::{BORDER, Field, GuiApp, PANEL, UI_INSET, column, icon_button, panel_header, row};
use crate::{model::Query, response::ViewResponse};
use gpui::{AnyElement, App, Context, div, prelude::*, px, rgb};
use session::{MetadataDraft, MetadataValues};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) struct ManualIntentId(u64);

/// Only explicit input advances this identity. Completion supplies data for that
/// input, but cannot create, replace or cancel a newer desired action.
#[derive(Default)]
pub(in crate::gui) struct ManualIntents {
    serial: u64,
    preparing: bool,
    pending: Option<(ManualIntentId, ManualAction)>,
}

impl ManualIntents {
    pub(in crate::gui) fn begin(&mut self) -> ManualIntentId {
        self.serial = self.serial.wrapping_add(1);
        self.pending = None;
        self.preparing = true;
        ManualIntentId(self.serial)
    }

    pub(in crate::gui) fn current(&self, id: ManualIntentId) -> bool {
        self.serial == id.0 && self.preparing
    }

    fn offer(
        &mut self,
        id: ManualIntentId,
        action: ManualAction,
        protected: bool,
    ) -> Option<ManualAction> {
        if !self.current(id) {
            return None;
        }
        self.preparing = false;
        if protected {
            self.pending = Some((id, action));
            None
        } else {
            Some(action)
        }
    }

    fn take_pending(&mut self) -> Option<ManualAction> {
        let (id, action) = self.pending.take()?;
        (self.serial == id.0).then_some(action)
    }

    fn reject(&mut self, id: ManualIntentId) {
        if self.current(id) {
            self.preparing = false;
        }
    }
}

pub(in crate::gui) enum ManualAction {
    Library(super::browser::selection::LibraryCommit),
    Playlist(super::playlist_selection::EntryCommit),
    Queue(super::queue::QueueCommit),
    History {
        track_id: i64,
        multi: bool,
        structure_revision: u64,
    },
    Clear,
    ReloadPlayback,
    Close(gpui::AnyWindowHandle),
    Quit,
}

impl GuiApp {
    pub(super) fn metadata_protected(&self, cx: &App) -> bool {
        self.metadata.saving() || self.metadata_dirty(cx)
    }

    pub(in crate::gui) fn begin_manual_intent(&mut self) -> ManualIntentId {
        let id = self.manual_intents.begin();
        self.browser.library.cancel_manual_selection();
        self.browser.playlists.track_selection.cancel_manual();
        self.browser.queue.cancel_manual();
        id
    }

    pub(in crate::gui) fn manual_intent_current(&self, id: ManualIntentId) -> bool {
        self.manual_intents.current(id)
    }

    pub(in crate::gui) fn fail_manual_intent(&mut self, id: ManualIntentId) {
        self.manual_intents.reject(id);
    }

    fn manual_action_valid(&self, action: &ManualAction) -> bool {
        match action {
            ManualAction::Library(commit) => self.library_commit_valid(commit),
            ManualAction::Playlist(commit) => self.entry_commit_valid(commit),
            ManualAction::Queue(commit) => self.queue_commit_valid(commit),
            ManualAction::History {
                structure_revision, ..
            } => *structure_revision == self.state.library.structure_revision,
            _ => true,
        }
    }

    fn manual_action_target(&self, action: &ManualAction) -> Option<i64> {
        match action {
            ManualAction::Library(commit) => self.library_commit_target(commit),
            ManualAction::Playlist(commit) => Some(commit.target.track_id),
            ManualAction::Queue(commit) => Some(commit.target.track_id),
            ManualAction::History {
                track_id, multi, ..
            } => {
                if *multi && self.browser.library.selected().contains(track_id) {
                    self.browser
                        .library
                        .selected()
                        .iter()
                        .copied()
                        .filter(|id| id != track_id)
                        .min()
                } else {
                    Some(*track_id)
                }
            }
            _ => None,
        }
    }

    pub(in crate::gui) fn ready_manual_action(
        &mut self,
        id: ManualIntentId,
        action: ManualAction,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if !self.manual_intent_current(id) {
            return;
        }
        if !self.manual_action_valid(&action) {
            self.manual_intents.reject(id);
            self.reject_stale_manual_action(cx);
            return;
        }
        let draft = MetadataDraft {
            title: self.inputs[&Field::Title].read(cx).text(),
            artist: self.inputs[&Field::Artist].read(cx).text(),
            album: self.inputs[&Field::Album].read(cx).text(),
            decoder_hint: self.metadata_decoder_hint,
        };
        let protected = self
            .metadata
            .needs_confirmation(self.manual_action_target(&action), draft);
        if let Some(action) = self.manual_intents.offer(id, action, protected) {
            self.commit_manual_action(action, cx);
        } else {
            cx.notify();
        }
    }

    fn reject_stale_manual_action(&mut self, cx: &mut Context<Self>) {
        self.error = Some(self.i18n.text("gl-metadata-stale-action").into());
        cx.notify();
    }

    pub(super) fn manual_confirmation_open(&self) -> bool {
        self.manual_intents.pending.is_some()
    }

    pub(super) fn cancel_manual_action(&mut self, cx: &mut Context<Self>) {
        self.begin_manual_intent();
        cx.notify();
    }

    fn resume_manual_action(&mut self, discard: bool, cx: &mut Context<Self>) {
        let Some(action) = self.manual_intents.take_pending() else {
            return;
        };
        self.refresh(cx);
        // Validate the frozen target before touching the original editor or draft.
        if !self.manual_action_valid(&action) {
            self.reject_stale_manual_action(cx);
            return;
        }
        if discard {
            self.metadata.discard_draft();
            self.set_metadata_values(MetadataValues::default(), cx);
        }
        self.commit_manual_action(action, cx);
        cx.notify();
    }

    fn discard_metadata_and_continue(&mut self, cx: &mut Context<Self>) {
        self.resume_manual_action(true, cx);
    }

    fn commit_manual_action(&mut self, action: ManualAction, cx: &mut Context<Self>) {
        match action {
            ManualAction::Library(commit) => self.commit_library_selection(commit, cx),
            ManualAction::Playlist(commit) => self.commit_entry_selection(commit, cx),
            ManualAction::Queue(commit) => self.commit_queue_selection(commit, cx),
            ManualAction::History {
                track_id, multi, ..
            } => self.commit_history_selection(track_id, multi, cx),
            ManualAction::Clear => {
                self.browser.clear_selection();
                self.clear_metadata_selection(cx);
            }
            ManualAction::ReloadPlayback => {
                self.metadata.discard_draft();
                self.clear_metadata_selection(cx);
            }
            ManualAction::Close(window) => {
                cx.defer(move |cx| {
                    let _ = window.update(cx, |_, window, _| window.remove_window());
                });
            }
            ManualAction::Quit => self.finish_quit(),
        }
        cx.notify();
    }

    pub(super) fn request_clear_selection(&mut self, cx: &mut Context<Self>) {
        let id = self.begin_manual_intent();
        self.ready_manual_action(id, ManualAction::Clear, cx);
    }

    pub(super) fn render_manual_confirmation(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.manual_intents.pending.as_ref()?;
        let detail = if self.metadata.saving() {
            self.i18n.text("gl-metadata-discard-saving")
        } else {
            self.i18n.text("gl-metadata-discard-note")
        };
        Some(
            div()
                .id("metadata-confirmation-modal")
                .occlude()
                .absolute()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgb(0x000000).alpha(0.58))
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(gpui::MouseButton::Middle, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(
                    column()
                        .id("metadata-confirmation-card")
                        .w(gpui::rems(26.25))
                        .max_w(gpui::relative(0.9))
                        .p(px(UI_INSET))
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .rounded_md()
                        .child(panel_header(
                            self.tr("gl-metadata-discard-title"),
                            detail.to_owned(),
                        ))
                        .child(
                            row()
                                .justify_end()
                                .child(icon_button(
                                    "metadata-confirmation-cancel",
                                    "",
                                    self.tr("gl-cancel"),
                                    cx,
                                    |this, _, cx| this.cancel_manual_action(cx),
                                ))
                                .child(super::components::danger_control(icon_button(
                                    "metadata-confirmation-discard",
                                    "󰆴",
                                    self.tr("gl-metadata-discard"),
                                    cx,
                                    |this, _, cx| this.discard_metadata_and_continue(cx),
                                ))),
                        ),
                )
                .into_any_element(),
        )
    }

    pub(super) fn metadata_dirty(&self, cx: &App) -> bool {
        self.metadata.dirty(MetadataDraft {
            title: self.inputs[&Field::Title].read(cx).text(),
            artist: self.inputs[&Field::Artist].read(cx).text(),
            album: self.inputs[&Field::Album].read(cx).text(),
            decoder_hint: self.metadata_decoder_hint,
        })
    }

    pub(super) fn sync_metadata_inputs(&mut self, cx: &mut Context<Self>) {
        let enabled = self.metadata.editable();
        for field in [Field::Title, Field::Artist, Field::Album] {
            self.inputs[&field].update(cx, |input, cx| input.set_enabled(enabled, cx));
        }
    }

    fn set_metadata_values(&mut self, values: MetadataValues, cx: &mut Context<Self>) {
        self.set_value(Field::Title, values.title, cx);
        self.set_value(Field::Artist, values.artist, cx);
        self.set_value(Field::Album, values.album, cx);
        self.metadata_decoder_hint = values.decoder_hint;
        self.sync_metadata_inputs(cx);
    }

    pub(super) fn request_track(&mut self, id: i64, cx: &mut Context<Self>) {
        debug_assert!(self.metadata.identity() == Some(id) || !self.metadata_protected(cx));
        if self.metadata.select(id) {
            self.set_metadata_values(MetadataValues::default(), cx);
        }
        self.sync_metadata_query();
        self.metadata_needs_refresh = true;
        self.demand_metadata(cx);
    }

    pub(super) fn reload_playback_metadata(&mut self, cx: &mut Context<Self>) {
        let id = self.begin_manual_intent();
        self.ready_manual_action(id, ManualAction::ReloadPlayback, cx);
    }

    pub(super) fn sync_metadata_query(&mut self) {
        if let Some(track_id) = self.metadata.selected() {
            let query = Query::Track { track_id };
            if self
                .metadata_projection
                .sync(&query, self.state.query_revisions(None))
            {
                self.metadata_needs_refresh = true;
            }
        } else {
            self.metadata_projection.cancel();
            self.metadata_needs_refresh = false;
            self.metadata_queued = None;
        }
    }

    pub(super) fn demand_metadata(&mut self, cx: &mut Context<Self>) {
        if self.panel_demand("metadata")
            && self.metadata_needs_refresh
            && let Some(id) = self.metadata.selected()
        {
            self.metadata_needs_refresh = false;
            self.load_metadata_track(id, cx);
        }
    }

    fn load_metadata_track(&mut self, id: i64, cx: &mut Context<Self>) {
        self.metadata_needs_refresh = false;
        self.sync_full_waveform_request();
        let token = self.metadata.begin_load(id);
        let query = Query::Track { track_id: id };
        self.metadata_projection
            .sync(&query, self.state.query_revisions(None));
        let generation = self.metadata_projection.generation();
        if self.metadata_worker {
            self.metadata_queued = Some(id);
            return;
        }
        self.metadata_worker = true;
        self.sync_metadata_inputs(cx);
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            // Retain the existing quiet interval without querying superseded selections.
            cx.background_executor()
                .timer(std::time::Duration::from_millis(80))
                .await;
            // Logical cancellation never releases the physical worker gate.
            let response = cx
                .background_executor()
                .spawn(async move { handle.query(Query::Track { track_id: id }) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.metadata_worker = false;
                this.refresh(cx);
                if let Some(id) = this.metadata_queued.take() {
                    if !this.panel_demand("metadata") {
                        this.metadata_needs_refresh = true;
                        return;
                    }
                    this.load_metadata_track(id, cx);
                    return;
                }
                if !this.metadata.accepts_load(token) {
                    return;
                }
                let result = match response.result {
                    Ok(result) => result,
                    Err(error) => {
                        this.metadata.fail_load(token, error.to_string());
                        this.sync_metadata_inputs(cx);
                        cx.notify();
                        return;
                    }
                };
                this.sync_metadata_query();
                if !this
                    .metadata_projection
                    .accepts(&query, generation, response.revisions)
                {
                    this.demand_metadata(cx);
                    return;
                }
                if let ViewResponse::Track(track) = result {
                    let draft = MetadataDraft {
                        title: this.inputs[&Field::Title].read(cx).text(),
                        artist: this.inputs[&Field::Artist].read(cx).text(),
                        album: this.inputs[&Field::Album].read(cx).text(),
                        decoder_hint: this.metadata_decoder_hint,
                    };
                    if let Some(values) = this.metadata.accept_load(token, track, draft) {
                        this.set_metadata_values(values, cx);
                    }
                    this.resume_metadata_follow(cx);
                    this.sync_waveform(cx);
                    this.sync_metadata_inputs(cx);
                } else {
                    this.metadata
                        .fail_load(token, this.i18n.text("gl-metadata-load-failed").into());
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn clear_metadata_selection(&mut self, cx: &mut Context<Self>) {
        self.metadata.clear_selection();
        self.set_metadata_values(MetadataValues::default(), cx);
        self.sync_metadata_default(cx);
        self.sync_metadata_query();
        self.sync_waveform(cx);
    }

    pub(super) fn sync_metadata_default(&mut self, cx: &mut Context<Self>) {
        let draft = MetadataDraft {
            title: self.inputs[&Field::Title].read(cx).text(),
            artist: self.inputs[&Field::Artist].read(cx).text(),
            album: self.inputs[&Field::Album].read(cx).text(),
            decoder_hint: self.metadata_decoder_hint,
        };
        if let Some(values) = self
            .metadata
            .sync_playback(self.state.current_track(), draft)
        {
            self.set_metadata_values(values, cx);
        }
        self.sync_metadata_inputs(cx);
    }

    fn resume_metadata_follow(&mut self, cx: &mut Context<Self>) {
        if self.metadata.selected().is_none()
            && self.metadata.identity() != self.state.current_track().map(|track| track.id)
            && !matches!(
                self.metadata.read_state(),
                session::MetadataReadState::Pending(_)
            )
        {
            self.sync_metadata_default(cx);
        }
    }

    pub(super) fn save_metadata(&mut self, cx: &mut Context<Self>) {
        let values = MetadataValues {
            title: self.value(Field::Title, cx),
            artist: self.value(Field::Artist, cx),
            album: self.value(Field::Album, cx),
            decoder_hint: self.metadata_decoder_hint,
        };
        let Some((token, command)) = self.metadata.begin_save(values.as_draft()) else {
            if !self.metadata.saving() && !self.metadata_dirty(cx) {
                self.resume_manual_action(false, cx);
            }
            return;
        };
        let hint_change = self.metadata.submitted_hint_change(&command);
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let ack = cx
                .background_executor()
                .spawn(async move { handle.request_ack(command) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // A successful save changes the scan source even after editor selection moved.
                if ack.ok
                    && let Some(track) = hint_change.as_ref()
                {
                    this.full_waveform_request.saved_hint_changed(track);
                    this.waveform.update(cx, |waveform, cx| {
                        waveform.saved_hint_changed(track, cx);
                    });
                }
                if !this.metadata.complete_save(token, values, ack.ok) {
                    return;
                }
                if ack.ok {
                    // Acknowledging only the submitted baseline leaves later edits
                    // dirty. Resume the latest desired operation only when clean.
                    if !this.metadata_dirty(cx) {
                        let draft = MetadataDraft {
                            title: this.inputs[&Field::Title].read(cx).text(),
                            artist: this.inputs[&Field::Artist].read(cx).text(),
                            album: this.inputs[&Field::Album].read(cx).text(),
                            decoder_hint: this.metadata_decoder_hint,
                        };
                        if let Some(values) = this.metadata.reconcile_clean(draft) {
                            this.set_metadata_values(values, cx);
                        }
                        if this.manual_intents.pending.is_some() {
                            this.resume_manual_action(false, cx);
                        } else {
                            this.resume_metadata_follow(cx);
                        }
                    }
                    this.sync_waveform(cx);
                } else {
                    this.error = Some(
                        ack.error
                            .unwrap_or_else(|| this.i18n.text("gl-metadata-save-failed").into()),
                    );
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{ManualAction, ManualIntents};

    #[test]
    fn late_range_cannot_replace_newer_exit_or_clean_selection() {
        let mut owner = ManualIntents::default();
        let range = owner.begin();
        let quit = owner.begin();
        assert!(owner.offer(quit, ManualAction::Quit, true).is_none());
        assert!(
            owner
                .offer(
                    range,
                    ManualAction::History {
                        track_id: 2,
                        multi: false,
                        structure_revision: 0,
                    },
                    true
                )
                .is_none()
        );
        assert!(matches!(owner.take_pending(), Some(ManualAction::Quit)));
        let navigation = owner.begin();
        let clean_selection = owner.begin();
        assert!(matches!(
            owner.offer(
                clean_selection,
                ManualAction::History {
                    track_id: 7,
                    multi: false,
                    structure_revision: 0,
                },
                false
            ),
            Some(ManualAction::History { track_id: 7, .. })
        ));
        assert!(owner.offer(navigation, ManualAction::Clear, true).is_none());
        assert!(owner.take_pending().is_none());
    }

    #[test]
    fn cancel_and_new_input_prevent_old_save_resume() {
        let mut owner = ManualIntents::default();
        let first = owner.begin();
        assert!(owner.offer(first, ManualAction::Clear, true).is_none());
        owner.begin(); // Cancel invalidates intent, not the editor or draft.
        assert!(owner.take_pending().is_none());
        assert!(owner.offer(first, ManualAction::Clear, false).is_none());
        let next = owner.begin();
        assert!(owner.offer(next, ManualAction::Quit, true).is_none());
        assert!(matches!(owner.take_pending(), Some(ManualAction::Quit)));
        assert!(owner.take_pending().is_none());
    }

    #[test]
    fn completion_and_confirmation_can_each_publish_only_once() {
        let mut owner = ManualIntents::default();
        let clean = owner.begin();
        assert!(matches!(
            owner.offer(clean, ManualAction::Clear, false),
            Some(ManualAction::Clear)
        ));
        assert!(!owner.current(clean));
        assert!(owner.offer(clean, ManualAction::Quit, true).is_none());
        assert!(owner.take_pending().is_none());
        let protected = owner.begin();
        assert!(owner.offer(protected, ManualAction::Quit, true).is_none());
        assert!(owner.offer(protected, ManualAction::Clear, true).is_none());
        assert!(matches!(owner.take_pending(), Some(ManualAction::Quit)));
        assert!(owner.offer(protected, ManualAction::Clear, false).is_none());
        assert!(owner.take_pending().is_none());
        let failed = owner.begin();
        owner.reject(failed);
        assert!(owner.offer(failed, ManualAction::Clear, false).is_none());
        assert!(owner.take_pending().is_none());
    }
}
