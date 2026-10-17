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

mod read;
pub(in crate::gui) use read::MetadataReadCoordinator;

pub(in crate::gui) mod session;

use super::{
    Field, GuiApp, UI_INSET,
    components::{
        active_control, artwork_preview, button, caption, column, disabled_control, icon_button,
        metadata_item, panel_header, panel_surface, row, section_group,
    },
    format_time,
};
use crate::model::{Command, DecoderHint, Query};
use gpui::{AnyElement, App, Context, Div, SharedString, Window, div, prelude::*, px, rgb};
use read::{MetadataReadTask, ReadCompletion, ReadEnvironment};
use session::{MetadataAvailability, MetadataDraft, MetadataReadState, MetadataValues};
use std::time::Instant;

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
    Playlist(super::playlists::selection::EntryCommit),
    Queue(super::queue::QueueCommit),
    History {
        commit: super::browser::HistoryCommit,
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
        self.browser.playlist_selection_mut().cancel_manual();
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
                commit,
                structure_revision,
            } => {
                *structure_revision == self.state.library.structure_revision
                    && self.browser.history.commit_valid(commit)
            }
            _ => true,
        }
    }

    fn manual_action_target(&self, action: &ManualAction) -> Option<i64> {
        match action {
            ManualAction::Library(commit) => self.library_commit_target(commit),
            ManualAction::Playlist(commit) => Some(commit.target.track_id),
            ManualAction::Queue(commit) => Some(commit.target.track_id),
            ManualAction::History { commit, .. } => Some(commit.track_id),
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
            ManualAction::History { commit, .. } => self.commit_history_selection(commit, cx),
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
        let theme = crate::gui::theme::palette(cx);
        self.manual_intents.pending.as_ref()?;
        let detail = if self.metadata.saving() {
            self.i18n.text("gl-metadata-discard-saving")
        } else {
            self.i18n.text("gl-metadata-discard-note")
        };
        Some(
            crate::gui::components::modal_backdrop("metadata-confirmation-modal")
                .map(|surface| self.keyboard_scope.surface(surface))
                .child(
                    column()
                        .id("metadata-confirmation-card")
                        .w(gpui::rems(26.25))
                        .max_w(gpui::relative(0.9))
                        .p(px(UI_INSET))
                        .bg(rgb(theme.panel))
                        .border_1()
                        .border_color(rgb(theme.border))
                        .rounded_md()
                        .child(panel_header(
                            theme,
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
                                .child(super::components::danger_control(
                                    theme,
                                    icon_button(
                                        "metadata-confirmation-discard",
                                        "󰆴",
                                        self.tr("gl-metadata-discard"),
                                        cx,
                                        |this, _, cx| this.discard_metadata_and_continue(cx),
                                    ),
                                )),
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
        let query = Query::Track { track_id: id };
        self.metadata_read
            .request(id, self.state.query_revisions_for(&query), Instant::now());
        self.demand_metadata(cx);
    }

    pub(super) fn reload_playback_metadata(&mut self, cx: &mut Context<Self>) {
        let id = self.begin_manual_intent();
        self.ready_manual_action(id, ManualAction::ReloadPlayback, cx);
    }

    pub(super) fn sync_metadata_query(&mut self) {
        let selected = self.metadata.selected();
        let revisions = selected.map_or_else(Default::default, |track_id| {
            self.state.query_revisions_for(&Query::Track { track_id })
        });
        self.metadata_read.sync(selected, revisions, Instant::now());
    }

    pub(super) fn cancel_metadata_read(&mut self) {
        self.metadata_read.cancel();
    }

    pub(super) fn demand_metadata(&mut self, cx: &mut Context<Self>) {
        let visible = self.panel_demand("metadata");
        let Some(task) = self
            .metadata_read
            .prepare(&mut self.metadata, visible, Instant::now())
        else {
            return;
        };
        self.sync_full_waveform_request();
        self.sync_metadata_inputs(cx);
        self.run_metadata_read(task, cx);
    }

    fn run_metadata_read(&mut self, task: MetadataReadTask, cx: &mut Context<Self>) {
        let MetadataReadTask {
            read,
            delay,
            ticket,
        } = task;
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            if !this
                .update(cx, |this, cx| {
                    this.refresh(cx);
                    this.sync_metadata_query();
                    let environment = ReadEnvironment {
                        visible: this.panel_demand("metadata"),
                        revisions: this.state.query_revisions_for(&read.query()),
                    };
                    let submit = this.metadata_read.submit(read, &this.metadata, environment);
                    if !submit {
                        this.demand_metadata(cx);
                        cx.notify();
                    }
                    submit
                })
                .unwrap_or(false)
            {
                return;
            }
            let response = cx
                .background_executor()
                .spawn(async move { handle.query_if_current(read.query(), ticket) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                this.sync_metadata_query();
                let environment = ReadEnvironment {
                    visible: this.panel_demand("metadata"),
                    revisions: this.state.query_revisions_for(&read.query()),
                };
                let draft = MetadataDraft {
                    title: this.inputs[&Field::Title].read(cx).text(),
                    artist: this.inputs[&Field::Artist].read(cx).text(),
                    album: this.inputs[&Field::Album].read(cx).text(),
                    decoder_hint: this.metadata_decoder_hint,
                };
                match this.metadata_read.finish(
                    read,
                    &mut this.metadata,
                    environment,
                    response,
                    draft,
                    || this.i18n.text("gl-metadata-load-failed").into(),
                ) {
                    ReadCompletion::Retired => this.demand_metadata(cx),
                    ReadCompletion::Loaded(values) => {
                        if let Some(values) = values {
                            this.set_metadata_values(values, cx);
                        }
                        this.resume_metadata_follow(cx);
                        this.sync_waveform(cx);
                    }
                    ReadCompletion::Failed => {}
                }
                this.sync_metadata_inputs(cx);
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

fn metadata_copy(
    label: SharedString,
    value: SharedString,
    hint: Option<SharedString>,
    cx: &mut Context<GuiApp>,
) -> Option<gpui::Stateful<Div>> {
    let hint = hint.filter(|_| !value.is_empty() && value.as_ref() != "—")?;
    let theme = super::theme::palette(cx);
    Some(
        super::components::compact_icon_button(
            theme,
            icon_button(label, "", hint, cx, move |_, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(value.to_string()));
            }),
        )
        .h(gpui::rems(1.25))
        .min_h_0()
        .text_size(gpui::rems(0.75))
        .opacity(0.)
        .group_hover("metadata-field", |style| style.opacity(1.))
        .focus(|style| style.opacity(1.)),
    )
}

fn metadata_rows(
    theme: &'static crate::theme::Palette,
    rows: impl IntoIterator<Item = [(SharedString, SharedString); 2]>,
    label_widths: [gpui::Pixels; 2],
    copy_hint: Option<SharedString>,
    cx: &mut Context<GuiApp>,
) -> Div {
    column()
        .gap_1()
        .children(rows.into_iter().map(|[first, second]| {
            metadata_row(
                metadata_item(
                    theme,
                    first.0.clone(),
                    first.1.clone(),
                    label_widths[0],
                    metadata_copy(first.0, first.1, copy_hint.clone(), cx),
                ),
                metadata_item(
                    theme,
                    second.0.clone(),
                    second.1.clone(),
                    label_widths[1],
                    metadata_copy(second.0, second.1, copy_hint.clone(), cx),
                ),
            )
        }))
}

fn metadata_row(first: Div, second: Div) -> Div {
    div()
        .grid()
        .grid_cols(2)
        .gap_4()
        .items_start()
        .child(first.min_w_0())
        .child(second.min_w_0())
}

fn metadata_group(
    theme: &'static crate::theme::Palette,
    title: impl Into<SharedString>,
    content: impl IntoElement,
) -> Div {
    section_group(theme, title, content)
}

impl GuiApp {
    pub(super) fn metadata_panel(
        &mut self,
        panel_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = crate::gui::theme::palette(cx);
        let owner = cx.weak_entity();
        window.defer(cx, move |_, cx| {
            let _ = owner.update(cx, |this, cx| {
                this.sync_metadata_query();
                this.demand_metadata(cx);
            });
        });
        self.sync_metadata_inputs(cx);
        self.prepare_artwork(cx);
        let track = self.metadata.track();
        let id = track.map(|track| track.id);
        let favorite = track.is_some_and(|track| track.favorite);
        let editable = self.metadata.editable();
        let editor_identity = self.metadata.editor_identity();
        let protected = self.metadata_protected(cx);
        let can_save = self.metadata.can_save(MetadataDraft {
            title: self.inputs[&Field::Title].read(cx).text(),
            artist: self.inputs[&Field::Artist].read(cx).text(),
            album: self.inputs[&Field::Album].read(cx).text(),
            decoder_hint: self.metadata_decoder_hint,
        });
        let identity = self.metadata.identity();
        let follow_paused = self.metadata.selected().is_none()
            && protected
            && identity != self.state.current_track().map(|track| track.id);
        let placeholder: SharedString = self.tr("gl-not-loaded");
        let copy_hint = track.map(|_| self.tr("gl-copy-value"));
        let presence_status = match self.metadata.availability() {
            MetadataAvailability::Unknown if identity.is_none() => self.i18n.text("gl-no-track"),
            MetadataAvailability::Unknown => self.i18n.text("gl-details-not-loaded"),
            MetadataAvailability::Present(_) => self.i18n.text("gl-ready"),
            MetadataAvailability::Unavailable(_) => self.i18n.text("gl-track-unavailable"),
        };
        let status: SharedString = match self.metadata.read_state() {
            MetadataReadState::Idle => presence_status.to_owned().into(),
            MetadataReadState::Pending(_) => self
                .i18n
                .format(
                    "gl-details-loading-status",
                    &[("status", (presence_status).into())],
                )
                .into(),
            MetadataReadState::Failed(error) => self
                .i18n
                .format(
                    "gl-details-failed-status",
                    &[
                        ("status", (presence_status).into()),
                        ("error", (error.as_str()).into()),
                    ],
                )
                .into(),
            MetadataReadState::WrongTrack => self
                .i18n
                .format(
                    "gl-details-failed-status",
                    &[
                        ("status", presence_status.into()),
                        ("error", self.i18n.text("gl-metadata-wrong-track").into()),
                    ],
                )
                .into(),
        };
        let status: SharedString = if protected {
            self.i18n
                .format(
                    "gl-details-protected-status",
                    &[
                        ("status", (status.as_ref()).into()),
                        (
                            "draft",
                            (self.i18n.text(if self.metadata.saving() {
                                "gl-metadata-saving"
                            } else {
                                "gl-unsaved"
                            }))
                            .into(),
                        ),
                        ("follow", (usize::from(follow_paused)).into()),
                    ],
                )
                .into()
        } else {
            status
        };
        let heading = self.metadata.selected().or(id).map_or_else(
            || self.i18n.text("gl-track-details").to_owned(),
            |id| {
                self.i18n
                    .format("gl-track-id", &[("id", (id.to_string()).into())])
            },
        );
        let file: [[(SharedString, SharedString); 2]; 1] = [[
            (
                self.tr("gl-file-path"),
                track.map_or_else(
                    || placeholder.clone(),
                    |track| {
                        if track.missing {
                            self.tr("gl-missing")
                        } else {
                            track.path.to_string_lossy().into_owned().into()
                        }
                    },
                ),
            ),
            (
                self.tr("gl-availability"),
                track.map_or_else(
                    || placeholder.clone(),
                    |track| {
                        if track.missing {
                            self.tr("gl-missing")
                        } else {
                            self.tr("gl-available")
                        }
                    },
                ),
            ),
        ]];
        let metadata: [[(SharedString, SharedString); 2]; 3] = [
            [
                (
                    self.tr("gl-codec"),
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            if track.codec.is_empty() {
                                "—".into()
                            } else {
                                track.codec.to_ascii_lowercase().into()
                            }
                        },
                    ),
                ),
                (
                    self.tr("gl-sample-rate"),
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            if track.sample_rate == 0 {
                                "—".into()
                            } else {
                                format!("{} Hz", track.sample_rate).into()
                            }
                        },
                    ),
                ),
            ],
            [
                (
                    self.tr("gl-channels"),
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            if track.channels == 0 {
                                "—".into()
                            } else {
                                track.channels.to_string().into()
                            }
                        },
                    ),
                ),
                (
                    self.tr("gl-duration"),
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            track
                                .duration
                                .map_or("—".into(), |value| format_time(value).into())
                        },
                    ),
                ),
            ],
            [
                (
                    self.tr("gl-bitrate"),
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            track.bitrate_bps.map_or("—".into(), |value| {
                                format!(
                                    "{}{:.0} kbps",
                                    if track.bitrate_exact { "" } else { "≈" },
                                    value as f64 / 1000.0
                                )
                                .into()
                            })
                        },
                    ),
                ),
                (
                    self.tr("gl-bits-sample"),
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            track
                                .bits_per_sample
                                .map_or("—".into(), |value| value.to_string().into())
                        },
                    ),
                ),
            ],
        ];
        let disc: (SharedString, SharedString) = (
            self.tr("gl-disc"),
            track.map_or_else(
                || placeholder.clone(),
                |track| {
                    track
                        .disc_number
                        .map_or("—".into(), |value| value.to_string().into())
                },
            ),
        );
        let tags: [[(SharedString, SharedString); 2]; 2] = [
            [
                (
                    self.tr("gl-track"),
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            track
                                .track_number
                                .map_or("—".into(), |value| value.to_string().into())
                        },
                    ),
                ),
                (
                    self.tr("gl-release"),
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| {
                            track
                                .release_date
                                .as_deref()
                                .unwrap_or("—")
                                .to_owned()
                                .into()
                        },
                    ),
                ),
            ],
            [
                (
                    self.tr("gl-play-count"),
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| track.play_count.to_string().into(),
                    ),
                ),
                (
                    self.tr("gl-last-played"),
                    track.map_or_else(
                        || placeholder.clone(),
                        |track| self.last_played_text(track.id, track.last_played).into(),
                    ),
                ),
            ],
        ];
        // Static labels are measured even before values load, using the caption font.
        // Every card shares the same two column widths.
        let label_widths = std::array::from_fn(|index| {
            let width = file
                .iter()
                .chain(&metadata)
                .chain(&tags)
                .map(|fields| &fields[index].0)
                .chain((index == 1).then_some(&disc.0))
                .map(|label| {
                    let run = gpui::TextRun {
                        len: label.len(),
                        font: self.ui_font.clone(),
                        color: rgb(theme.muted).into(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    window
                        .text_system()
                        .shape_line(label.clone(), window.rem_size() * 0.75, &[run], None)
                        .width()
                        .ceil()
                })
                .fold(px(0.), gpui::Pixels::max);
            // Layout rounds fixed widths to device pixels; never round a measured label down.
            let scale = window.scale_factor();
            px((f32::from(width) * scale).ceil() / scale)
        });
        let path_tooltip: SharedString = track
            .filter(|track| track.missing)
            .map(|track| track.path.to_string_lossy().into_owned().into())
            .unwrap_or_else(|| file[0][0].1.clone());
        let scroll = self.scroll_views.content(("metadata", panel_id));
        let details = div()
            .id(("metadata-details", panel_id))
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .child(
                column()
                    .flex_shrink_0()
                    .p(px(UI_INSET))
                    .gap_3()
                    .child(
                        column()
                            .gap_1()
                            .child(
                                row()
                                    .justify_between()
                                    .items_center()
                                    .child(
                                        caption(theme, heading)
                                            .text_lg()
                                            .text_color(rgb(theme.text)),
                                    )
                                    .child(
                                        row()
                                            .gap_2()
                                            .child(disabled_control(
                                                theme,
                                                icon_button(
                                                    ("metadata-favorite", panel_id),
                                                    if favorite { "󰓎" } else { "󰓐" },
                                                    if favorite {
                                                        self.tr("gl-unfavorite")
                                                    } else {
                                                        self.tr("gl-favorite")
                                                    },
                                                    cx,
                                                    move |this, _, cx| {
                                                        if this.metadata.editor_identity()
                                                            != editor_identity
                                                        {
                                                            return;
                                                        }
                                                        let Some((track_id, favorite)) =
                                                            this.metadata.track().map(|track| {
                                                                (track.id, track.favorite)
                                                            })
                                                        else {
                                                            return;
                                                        };
                                                        this.send(
                                                            Command::SetFavorite {
                                                                track_ids: vec![track_id],
                                                                favorite: !favorite,
                                                            },
                                                            cx,
                                                        );
                                                    },
                                                )
                                                .text_color(rgb(if favorite {
                                                    theme.accent
                                                } else {
                                                    theme.text
                                                })),
                                                !editable,
                                            ))
                                            .child(disabled_control(
                                                theme,
                                                icon_button(
                                                    ("metadata-save", panel_id),
                                                    "󰆓",
                                                    self.tr("gl-save-metadata"),
                                                    cx,
                                                    move |this, _, cx| {
                                                        if this.metadata.editor_identity()
                                                            == editor_identity
                                                        {
                                                            this.save_metadata(cx);
                                                        }
                                                    },
                                                ),
                                                !can_save,
                                            ))
                                            .child(icon_button(
                                                ("metadata-retry", panel_id),
                                                "󰑐",
                                                self.tr("gl-reload-playback"),
                                                cx,
                                                move |this, _, cx| {
                                                    if this.metadata.editor_identity()
                                                        == editor_identity
                                                    {
                                                        this.reload_playback_metadata(cx);
                                                    }
                                                },
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .id(("metadata-status", panel_id))
                                    .min_w_0()
                                    .h(gpui::rems(1.25))
                                    .child(
                                        caption(theme, status.clone())
                                            .text_xs()
                                            .text_color(rgb(theme.muted))
                                            .truncate(),
                                    )
                                    .tooltip(move |_, cx| {
                                        cx.new(|_| crate::gui::components::ButtonTooltip {
                                            text: status.clone(),
                                        })
                                        .into()
                                    }),
                            ),
                    )
                    .child(metadata_group(
                        theme,
                        self.tr("gl-file"),
                        metadata_row(
                            div().flex_1().min_w_0().child(
                                metadata_item(
                                    theme,
                                    file[0][0].0.clone(),
                                    file[0][0].1.clone(),
                                    label_widths[0],
                                    metadata_copy(
                                        file[0][0].0.clone(),
                                        path_tooltip.clone(),
                                        copy_hint.clone(),
                                        cx,
                                    ),
                                )
                                .id(("metadata-path", panel_id))
                                .tooltip(move |_, cx| {
                                    cx.new(|_| crate::gui::components::ButtonTooltip {
                                        text: path_tooltip.clone(),
                                    })
                                    .into()
                                }),
                            ),
                            metadata_item(
                                theme,
                                file[0][1].0.clone(),
                                file[0][1].1.clone(),
                                label_widths[1],
                                metadata_copy(
                                    file[0][1].0.clone(),
                                    file[0][1].1.clone(),
                                    copy_hint.clone(),
                                    cx,
                                ),
                            ),
                        ),
                    ))
                    .child(metadata_group(
                        theme,
                        self.tr("gl-metadata"),
                        metadata_rows(theme, metadata, label_widths, copy_hint.clone(), cx),
                    ))
                    .child(metadata_group(
                        theme,
                        self.tr("gl-decoder-hint"),
                        column()
                            .gap_2()
                            .child(
                                row().flex_wrap().children(
                                    [
                                        (
                                            "metadata-decoder-auto",
                                            self.tr("gl-auto"),
                                            DecoderHint::Auto,
                                        ),
                                        (
                                            "metadata-decoder-native",
                                            self.tr("gl-native"),
                                            DecoderHint::Native,
                                        ),
                                        (
                                            "metadata-decoder-ffmpeg",
                                            SharedString::from("FFmpeg"),
                                            DecoderHint::Ffmpeg,
                                        ),
                                    ]
                                    .into_iter()
                                    .map(
                                        |(key, label, decoder_hint)| {
                                            disabled_control(
                                                theme,
                                                active_control(
                                                    theme,
                                                    button(
                                                        (key, panel_id),
                                                        label,
                                                        cx,
                                                        move |this, _, cx| {
                                                            if this.metadata.editor_identity()
                                                                == editor_identity
                                                                && this.metadata.editable()
                                                            {
                                                                this.metadata_decoder_hint =
                                                                    decoder_hint;
                                                                cx.notify();
                                                            }
                                                        },
                                                    ),
                                                    editable
                                                        && self.metadata_decoder_hint
                                                            == decoder_hint,
                                                ),
                                                !editable,
                                            )
                                        },
                                    ),
                                ),
                            )
                            .child(
                                caption(theme, self.tr("gl-decoder-save-note"))
                                    .text_xs()
                                    .text_color(rgb(theme.muted)),
                            )
                            .child(
                                caption(theme, self.tr("gl-decoder-warning"))
                                    .text_xs()
                                    .text_color(rgb(theme.muted)),
                            ),
                    ))
                    .child(metadata_group(
                        theme,
                        self.tr("gl-tags"),
                        column()
                            .gap_2()
                            .child(metadata_row(
                                self.panel_field(theme, Field::Title, self.tr("gl-title")),
                                self.panel_field(theme, Field::Artist, self.tr("gl-artist")),
                            ))
                            .child(metadata_row(
                                self.panel_field(theme, Field::Album, self.tr("gl-album")),
                                metadata_item(
                                    theme,
                                    disc.0.clone(),
                                    disc.1.clone(),
                                    label_widths[1],
                                    metadata_copy(disc.0, disc.1, copy_hint.clone(), cx),
                                ),
                            ))
                            .child(metadata_rows(theme, tags, label_widths, copy_hint, cx).gap_2())
                            .when(track.is_some(), |tags| {
                                tags.child(match self.artwork_view.image(cx) {
                                    Some(image) => artwork_preview(
                                        theme,
                                        self.tr("shell-panel-album-artwork"),
                                        image.size_full(),
                                        self.artwork_view.aspect_ratio(cx),
                                    ),
                                    None => artwork_preview(
                                        theme,
                                        self.tr("shell-panel-album-artwork"),
                                        self.default_album
                                            .clone()
                                            .cached(gpui::StyleRefinement::default().size_full()),
                                        Some(1.0),
                                    ),
                                })
                            }),
                    )),
            );
        panel_surface(("metadata-panel", panel_id))
            .child(details)
            .into_any_element()
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
        assert!(owner.offer(range, ManualAction::Clear, true).is_none());
        assert!(matches!(owner.take_pending(), Some(ManualAction::Quit)));
        let navigation = owner.begin();
        let clean_selection = owner.begin();
        assert!(matches!(
            owner.offer(clean_selection, ManualAction::ReloadPlayback, false),
            Some(ManualAction::ReloadPlayback)
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
