//! One metadata edit session follows either an explicit selection or playback.
//!
//! The feature owns tag input entities and the decoder hint draft. A loaded
//! track supplies display details; its refresh must not move a dirty edit's
//! baseline. Identity epochs reject responses and save acknowledgements from a
//! previous selection, including a switch away and back to the same track.
//! Playlist container/membership changes do not change this track identity;
//! they must not cancel the editor or its in-flight save.
//! Dirty or saving playback-follow sessions stay on their original identity.
//! Manual changes defer the whole selection operation behind one confirmation;
//! no browser selection is mutated until Discard (or a clean save) resolves it.

mod artwork;
mod read;
pub(in crate::gui) mod session;

#[cfg(test)]
mod tests;

use super::{
    artwork::{Artwork, ArtworkImages, ArtworkLoader, ArtworkView},
    components::{artwork::artwork_preview, track::favorite_button},
    feature::FeatureContext,
    format_time,
};
use crate::{artwork::ArtworkManager, projection::GuiSnapshot};
use chrono::{DateTime, Local};
use gpui::{
    AnyElement, App, Context, Div, Entity, EventEmitter, Font, SharedString, Subscription, Window,
    div, prelude::*, px, rgb,
};
use read::{MetadataReadCoordinator, MetadataReadTask, ReadCompletion, ReadEnvironment};
use rivu_format::media::DecoderHint;
use rivu_protocol::model::{Command, PlaybackStatus, Query, Track};
use rivu_ui::{
    FormField, IconButton, IconButtonDensity, IconButtonStyle, active_control, button, caption,
    column, disabled_control,
    input::{Input, InputEvent},
    label_value_row, panel_surface, row, section_group,
};
use session::{
    MetadataAvailability, MetadataDraft, MetadataReadState, MetadataSession, MetadataValues,
};
use std::{collections::HashMap, rc::Rc, sync::Arc, time::Instant};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Title,
    Artist,
    Album,
}

pub(in crate::gui) enum MetadataEvent {
    ReloadPlaybackRequested,
    /// The coordinator resumes a pending action, or playback after protection ends.
    /// A no-op Save must not introduce a follow transition.
    ResumeManualAction {
        resume_follow: bool,
    },
    TrackChanged,
    Error(String),
}

pub(in crate::gui) struct SavedHintChanged(pub Track);

/// One editor per window. Hiding never drops a draft or transfers it to another window.
pub(in crate::gui) struct Metadata {
    context: FeatureContext,
    state: Rc<GuiSnapshot>,
    metadata: MetadataSession,
    metadata_read: MetadataReadCoordinator,
    inputs: HashMap<Field, Entity<Input>>,
    metadata_decoder_hint: DecoderHint,
    visible: bool,
    error: Option<String>,
    ui_font: Font,
    artwork_manager: Arc<ArtworkManager>,
    artwork_loader: ArtworkLoader,
    artwork_view: ArtworkView,
    default_album: Entity<Artwork>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<MetadataEvent> for Metadata {}
impl EventEmitter<SavedHintChanged> for Metadata {}

impl Metadata {
    pub(in crate::gui) fn new(
        context: FeatureContext,
        ui_font: Font,
        artwork_manager: Arc<ArtworkManager>,
        artwork_images: ArtworkImages,
        default_album: Entity<Artwork>,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = context.snapshot();
        let mut inputs = HashMap::new();
        let mut subscriptions = Vec::new();
        for (field, placeholder) in [
            (Field::Title, "shell-title"),
            (Field::Artist, "shell-artist"),
            (Field::Album, "shell-album"),
        ] {
            let input =
                cx.new(|cx| Input::new(context.tr(placeholder), "", context.tr(placeholder), cx));
            subscriptions.push(cx.subscribe(&input, |this, _, event, cx| {
                if matches!(event, InputEvent::Changed { user: true, .. }) {
                    this.draft_changed(cx);
                }
            }));
            inputs.insert(field, input);
        }
        let mut this = Self {
            context,
            state,
            metadata: MetadataSession::default(),
            metadata_read: MetadataReadCoordinator::default(),
            inputs,
            metadata_decoder_hint: DecoderHint::default(),
            visible: false,
            error: None,
            ui_font,
            artwork_manager,
            artwork_loader: ArtworkLoader::default(),
            artwork_view: ArtworkView::new(artwork_images),
            default_album,
            _subscriptions: subscriptions,
        };
        this.sync_metadata_default(cx);
        this
    }

    fn tr(&self, id: &str) -> SharedString {
        self.context.tr(id)
    }

    pub(in crate::gui) fn reconcile(&mut self, state: Rc<GuiSnapshot>, cx: &mut Context<Self>) {
        if Rc::ptr_eq(&self.state, &state) {
            return;
        }
        let current_changed = match (&self.state.current_track, &state.current_track) {
            (Some(previous), Some(next)) => !Arc::ptr_eq(previous, next),
            (None, None) => false,
            _ => true,
        };
        let query_changed = self.metadata.identity().is_some_and(|track_id| {
            let query = Query::Track { track_id };
            query.stamp(self.state.query_revisions_for(&query))
                != query.stamp(state.query_revisions_for(&query))
        });
        let display_changed = current_changed
            || query_changed
            || !Arc::ptr_eq(&self.state.system.config, &state.system.config)
            || self.state.playback.status != state.playback.status
            || self.state.playback.last_heard_at != state.playback.last_heard_at;
        self.state = state;
        if current_changed {
            self.sync_metadata_default(cx);
        }
        if query_changed {
            self.demand_metadata(cx);
        }
        if display_changed {
            cx.notify();
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.reconcile(self.context.snapshot(), cx);
    }

    pub(in crate::gui) fn set_ui_font(&mut self, font: Font, cx: &mut Context<Self>) {
        if self.ui_font != font {
            self.ui_font = font;
            cx.notify();
        }
    }

    /// Panel demand is separate from editor identity and window-hide cancellation.
    pub(in crate::gui) fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            self.refresh(cx);
            self.demand_metadata(cx);
        } else {
            self.artwork_view.replace(None, cx);
            let _ = self.artwork_loader.prepare(None);
        }
        cx.notify();
    }

    pub(in crate::gui) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.inputs
            .values()
            .any(|input| input.read(cx).is_focused(window))
    }

    pub(in crate::gui) fn dismiss_input_composition(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.inputs
            .values()
            .find(|input| input.read(cx).is_focused(window))
            .is_some_and(|input| input.update(cx, |input, cx| input.dismiss_composition(cx)))
    }

    pub(in crate::gui) fn protected(&self, cx: &App) -> bool {
        self.metadata.saving() || self.dirty(cx)
    }

    pub(in crate::gui) fn dirty(&self, cx: &App) -> bool {
        self.metadata.dirty(self.draft(cx))
    }

    pub(in crate::gui) fn needs_confirmation(&self, target: Option<i64>, cx: &App) -> bool {
        self.metadata.needs_confirmation(target, self.draft(cx))
    }

    pub(in crate::gui) fn saving(&self) -> bool {
        self.metadata.saving()
    }

    pub(in crate::gui) fn track(&self) -> Option<&Track> {
        self.metadata.track()
    }

    pub(in crate::gui) fn track_snapshot(&self) -> Option<Arc<Track>> {
        self.metadata.track_snapshot()
    }

    pub(in crate::gui) fn discard(&mut self, cx: &mut Context<Self>) {
        self.metadata.discard_draft();
        self.set_metadata_values(MetadataValues::default(), cx);
        cx.notify();
    }

    fn draft<'a>(&self, cx: &'a App) -> MetadataDraft<'a> {
        MetadataDraft {
            title: self.inputs[&Field::Title].read(cx).text(),
            artist: self.inputs[&Field::Artist].read(cx).text(),
            album: self.inputs[&Field::Album].read(cx).text(),
            decoder_hint: self.metadata_decoder_hint,
        }
    }

    fn draft_changed(&mut self, cx: &mut Context<Self>) {
        self.resume_when_clean(cx);
        cx.notify();
    }

    fn resume_when_clean(&mut self, cx: &mut Context<Self>) {
        if self.protected(cx) {
            return;
        }
        let draft = self.draft(cx);
        if let Some(values) = self.metadata.reconcile_clean(draft) {
            self.set_metadata_values(values, cx);
        }
        // The root arbitrates the newest manual intent before following playback.
        cx.emit(MetadataEvent::ResumeManualAction {
            resume_follow: true,
        });
    }

    fn set_decoder_hint(&mut self, hint: DecoderHint, cx: &mut Context<Self>) {
        if self.metadata.editable() && self.metadata_decoder_hint != hint {
            self.metadata_decoder_hint = hint;
            self.draft_changed(cx);
        }
    }

    fn sync_metadata_inputs(&mut self, cx: &mut Context<Self>) {
        let enabled = self.metadata.editable();
        for input in self.inputs.values() {
            input.update(cx, |input, cx| input.set_enabled(enabled, cx));
        }
    }

    fn set_metadata_values(&mut self, values: MetadataValues, cx: &mut Context<Self>) {
        for (field, value) in [
            (Field::Title, values.title),
            (Field::Artist, values.artist),
            (Field::Album, values.album),
        ] {
            self.inputs[&field].update(cx, |input, cx| input.set_text(value, cx));
        }
        self.metadata_decoder_hint = values.decoder_hint;
        self.sync_metadata_inputs(cx);
    }

    pub(in crate::gui) fn request_track(&mut self, id: i64, cx: &mut Context<Self>) {
        debug_assert!(self.metadata.identity() == Some(id) || !self.protected(cx));
        if self.metadata.select(id) {
            self.set_metadata_values(MetadataValues::default(), cx);
        }
        let query = Query::Track { track_id: id };
        self.metadata_read
            .request(id, self.state.query_revisions_for(&query), Instant::now());
        self.demand_metadata(cx);
        cx.notify();
    }

    fn reload_playback_metadata(&mut self, cx: &mut Context<Self>) {
        cx.emit(MetadataEvent::ReloadPlaybackRequested);
    }

    fn sync_metadata_query(&mut self) {
        let selected = self.metadata.identity();
        let revisions = selected.map_or_else(Default::default, |track_id| {
            self.state.query_revisions_for(&Query::Track { track_id })
        });
        self.metadata_read.sync(selected, revisions, Instant::now());
    }

    /// Called on window hide, not on ordinary panel demand changes.
    pub(in crate::gui) fn cancel_read(&mut self) {
        self.metadata_read.cancel();
    }

    fn demand_metadata(&mut self, cx: &mut Context<Self>) {
        self.sync_metadata_query();
        let Some(task) =
            self.metadata_read
                .prepare(&mut self.metadata, self.visible, Instant::now())
        else {
            return;
        };
        cx.emit(MetadataEvent::TrackChanged);
        self.sync_metadata_inputs(cx);
        self.run_metadata_read(task, cx);
    }

    fn run_metadata_read(&mut self, task: MetadataReadTask, cx: &mut Context<Self>) {
        let MetadataReadTask {
            read,
            delay,
            ticket,
        } = task;
        let handle = self.context.handle().clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            if !this
                .update(cx, |this, cx| {
                    this.refresh(cx);
                    let environment = ReadEnvironment {
                        visible: this.visible,
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
            let response = super::query::run(
                handle,
                cx,
                move |handle| async move { handle.query_if_current(read.query(), ticket).await },
                |response, cx| {
                    let _ = this.update(cx, |this, cx| {
                        this.refresh(cx);
                        let environment = ReadEnvironment {
                            visible: this.visible,
                            revisions: this.state.query_revisions_for(&read.query()),
                        };
                        if this.metadata_read.timeout(
                            read,
                            &mut this.metadata,
                            environment,
                            response,
                        ) {
                            this.sync_metadata_inputs(cx);
                            cx.notify();
                        }
                    });
                },
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                let environment = ReadEnvironment {
                    visible: this.visible,
                    revisions: this.state.query_revisions_for(&read.query()),
                };
                let draft = this.draft(cx);
                match this.metadata_read.finish(
                    read,
                    &mut this.metadata,
                    environment,
                    response,
                    draft,
                    || this.context.i18n.text("gl-metadata-load-failed").into(),
                ) {
                    ReadCompletion::Retired => this.demand_metadata(cx),
                    ReadCompletion::Loaded(values) => {
                        if let Some(values) = values {
                            this.set_metadata_values(values, cx);
                        }
                        this.resume_follow(cx);
                        cx.emit(MetadataEvent::TrackChanged);
                    }
                    ReadCompletion::Failed => {}
                }
                this.sync_metadata_inputs(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::gui) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.metadata.clear_selection();
        self.set_metadata_values(MetadataValues::default(), cx);
        self.sync_metadata_default(cx);
        self.sync_metadata_query();
        cx.emit(MetadataEvent::TrackChanged);
        cx.notify();
    }

    fn sync_metadata_default(&mut self, cx: &mut Context<Self>) {
        let draft = self.draft(cx);
        if let Some(values) = self
            .metadata
            .sync_playback(self.state.current_track(), draft)
        {
            self.set_metadata_values(values, cx);
        }
        self.sync_metadata_inputs(cx);
    }

    pub(in crate::gui) fn resume_follow(&mut self, cx: &mut Context<Self>) {
        if self.metadata.selected().is_none()
            && self.metadata.identity() != self.state.current_track().map(|track| track.id)
            && !matches!(self.metadata.read_state(), MetadataReadState::Pending(_))
        {
            let previous = self.metadata.editor_identity();
            self.sync_metadata_default(cx);
            if self.metadata.editor_identity() != previous {
                cx.emit(MetadataEvent::TrackChanged);
                cx.notify();
            }
        }
    }

    pub(in crate::gui) fn save(&mut self, cx: &mut Context<Self>) {
        let draft = self.draft(cx);
        let values = MetadataValues {
            title: draft.title.to_owned(),
            artist: draft.artist.to_owned(),
            album: draft.album.to_owned(),
            decoder_hint: draft.decoder_hint,
        };
        let Some((token, command)) = self.metadata.begin_save(values.as_draft()) else {
            if !self.metadata.saving() && !self.dirty(cx) {
                cx.emit(MetadataEvent::ResumeManualAction {
                    resume_follow: false,
                });
            }
            return;
        };
        let hint_change = self.metadata.submitted_hint_change(&command);
        let handle = self.context.handle().clone();
        cx.spawn(async move |this, cx| {
            let ack = handle.request_ack(command).await;
            let _ = this.update(cx, |this, cx| {
                this.finish_save(token, values, hint_change, ack, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn finish_save(
        &mut self,
        token: session::SaveToken,
        values: MetadataValues,
        hint_change: Option<Track>,
        ack: rivu_protocol::response::Ack,
        cx: &mut Context<Self>,
    ) {
        // A committed source change survives editor selection/discard.
        if ack.is_success()
            && let Some(track) = hint_change
        {
            cx.emit(SavedHintChanged(track));
        }
        if !self.metadata.complete_save(token, values, ack.is_success()) {
            return;
        }
        if ack.is_success() {
            if let Some(id) = self.metadata.selected() {
                self.request_track(id, cx);
            } else {
                self.sync_metadata_default(cx);
            }
            cx.emit(MetadataEvent::TrackChanged);
        } else {
            self.report_error(
                ack.error
                    .unwrap_or_else(|| self.context.i18n.text("gl-metadata-save-failed").into()),
                cx,
            );
        }
        // Successful writes acknowledge only the submitted baseline; failures retain it.
        // Either completion releases saving protection, but never a newer dirty draft.
        self.resume_when_clean(cx);
        cx.notify();
    }

    fn report_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.error = Some(error.clone());
        cx.emit(MetadataEvent::Error(error));
    }

    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(error) = self.context.handle().send(command) {
            self.report_error(format!("{error:#}"), cx);
        }
        cx.notify();
    }

    fn panel_field(
        &self,
        theme: &'static rivu_ui::theme::Palette,
        field: Field,
        label: SharedString,
    ) -> Div {
        FormField::new(theme, label, self.inputs[&field].clone()).into_div()
    }

    fn last_played_text(&self, track_id: i64, stored_at: Option<i64>) -> String {
        let current = self
            .state
            .current_track()
            .is_some_and(|track| track.id == track_id);
        if current
            && self.state.playback.status == PlaybackStatus::Playing
            && self.state.playback.last_heard_at.is_some()
        {
            return self.context.i18n.text("gl-played-now").into();
        }
        let played_at = if current && self.state.playback.status == PlaybackStatus::Paused {
            self.state.playback.last_heard_at
        } else {
            stored_at
        };
        match played_at.and_then(|time| DateTime::from_timestamp(time, 0)) {
            Some(time) => self.context.i18n.format(
                "gl-played-at",
                &[
                    (
                        "time",
                        time.with_timezone(&Local)
                            .format("%Y-%m-%d %H:%M:%S")
                            .to_string()
                            .into(),
                    ),
                    (
                        "paused",
                        usize::from(
                            current && self.state.playback.status == PlaybackStatus::Paused,
                        )
                        .into(),
                    ),
                ],
            ),
            None => self.context.i18n.text("gl-played-none").into(),
        }
    }

    pub(in crate::gui) fn release_render_images(&self, window: &mut Window, cx: &mut App) {
        self.artwork_view.release_all_images(window, cx);
    }
}

fn format_cue_frame(frame: u64) -> String {
    let seconds = frame / 75;
    format!("{:02}:{:02}:{:02}", seconds / 60, seconds % 60, frame % 75)
}

fn metadata_copy(
    label: SharedString,
    value: SharedString,
    hint: Option<SharedString>,
    cx: &mut Context<Metadata>,
) -> Option<gpui::Stateful<Div>> {
    let hint = hint.filter(|_| !value.is_empty() && value.as_ref() != "—")?;
    Some(rivu_ui::hover_reveal(
        label.clone(),
        "label-value-row",
        IconButton::new(label, "", hint)
            .build(
                IconButtonStyle::Quiet,
                IconButtonDensity::Regular,
                cx,
                move |_, _, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(value.to_string()));
                },
            )
            .text_size(gpui::rems(0.75)),
    ))
}

fn metadata_rows(
    theme: &'static rivu_ui::theme::Palette,
    rows: impl IntoIterator<Item = [(SharedString, SharedString); 2]>,
    label_widths: [gpui::Pixels; 2],
    copy_hint: Option<SharedString>,
    cx: &mut Context<Metadata>,
) -> Div {
    column()
        .gap_1()
        .children(rows.into_iter().map(|[first, second]| {
            metadata_row(
                label_value_row(
                    theme,
                    first.0.clone(),
                    first.1.clone(),
                    label_widths[0],
                    metadata_copy(first.0, first.1, copy_hint.clone(), cx),
                ),
                label_value_row(
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
    row()
        .flex_wrap()
        .gap_4()
        .items_start()
        .child(first.flex_1().flex_basis(gpui::rems(16.)).min_w_0())
        .child(second.flex_1().flex_basis(gpui::rems(16.)).min_w_0())
}

fn metadata_group(
    theme: &'static rivu_ui::theme::Palette,
    title: impl Into<SharedString>,
    content: impl IntoElement,
) -> Div {
    section_group(theme, title, content)
}

impl Metadata {
    pub(in crate::gui) fn render_panel(
        &mut self,
        panel_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let owner = cx.weak_entity();
        window.defer(cx, move |_, cx| {
            let _ = owner.update(cx, |this, cx| {
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
        let protected = self.protected(cx);
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
            MetadataAvailability::Unknown if identity.is_none() => {
                self.context.i18n.text("gl-no-track")
            }
            MetadataAvailability::Unknown => self.context.i18n.text("gl-details-not-loaded"),
            MetadataAvailability::Present(_) => self.context.i18n.text("gl-ready"),
            MetadataAvailability::Unavailable(_) => self.context.i18n.text("gl-track-unavailable"),
        };
        let status: SharedString = match self.metadata.read_state() {
            MetadataReadState::Idle => presence_status.to_owned().into(),
            MetadataReadState::Pending(_) => self
                .context
                .i18n
                .format(
                    "gl-details-loading-status",
                    &[("status", (presence_status).into())],
                )
                .into(),
            MetadataReadState::Failed(error) => self
                .context
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
                .context
                .i18n
                .format(
                    "gl-details-failed-status",
                    &[
                        ("status", presence_status.into()),
                        (
                            "error",
                            self.context.i18n.text("gl-metadata-wrong-track").into(),
                        ),
                    ],
                )
                .into(),
        };
        let status: SharedString = if protected {
            self.context
                .i18n
                .format(
                    "gl-details-protected-status",
                    &[
                        ("status", (status.as_ref()).into()),
                        (
                            "draft",
                            (self.context.i18n.text(if self.metadata.saving() {
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
            || self.context.i18n.text("gl-track-details").to_owned(),
            |id| {
                self.context
                    .i18n
                    .format("gl-track-id", &[("id", (id.to_string()).into())])
            },
        );
        let file: [[(SharedString, SharedString); 2]; 1] = [[
            (
                self.tr("gl-file-path"),
                track.map_or_else(
                    || placeholder.clone(),
                    |track| track.path.to_string_lossy().into_owned().into(),
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
        let track_kind: (SharedString, SharedString) = (
            self.tr("gl-track-kind"),
            track.map_or_else(
                || placeholder.clone(),
                |track| {
                    self.tr(if track.cue.is_some() {
                        "gl-cue-segment"
                    } else {
                        "gl-whole-file"
                    })
                },
            ),
        );
        let cue = track.and_then(|track| track.cue.as_ref());
        let cue_fields: [[(SharedString, SharedString); 2]; 2] = [
            [
                (
                    self.tr("gl-cue-sheet"),
                    cue.map_or_else(
                        || placeholder.clone(),
                        |cue| cue.sheet.to_string_lossy().into_owned().into(),
                    ),
                ),
                (
                    self.tr("gl-cue-number"),
                    cue.map_or_else(|| placeholder.clone(), |cue| cue.number.to_string().into()),
                ),
            ],
            [
                (
                    self.tr("gl-cue-start"),
                    cue.map_or_else(
                        || placeholder.clone(),
                        |cue| format_cue_frame(cue.start_frame).into(),
                    ),
                ),
                (
                    self.tr("gl-cue-end"),
                    cue.map_or_else(
                        || placeholder.clone(),
                        |cue| {
                            cue.end_frame.map_or_else(
                                || self.tr("gl-file-end"),
                                |frame| format_cue_frame(frame).into(),
                            )
                        },
                    ),
                ),
            ],
        ];
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
                .chain(&cue_fields)
                .chain(&tags)
                .map(|fields| &fields[index].0)
                .chain((index == 1).then_some(&disc.0))
                .chain((index == 0).then_some(&track_kind.0))
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
                        .shape_line(
                            label.clone(),
                            window.rem_size() * 0.75 * rivu_ui::theme::font_size_scale(cx),
                            &[run],
                            None,
                        )
                        .width()
                        .ceil()
                })
                .fold(px(0.), gpui::Pixels::max);
            // Layout rounds fixed widths to device pixels; never round a measured label down.
            let scale = window.scale_factor();
            px((f32::from(width) * scale).ceil() / scale)
        });
        let scroll = self
            .context
            .scroll
            .borrow_mut()
            .content(("metadata", panel_id));
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
                    .gap_3()
                    .child(rivu_ui::panel_heading_row(
                        rivu_ui::list_panel_header(theme, heading, status.clone())
                            .id(("metadata-status", panel_id))
                            .tooltip(rivu_ui::text_tooltip(status)),
                        rivu_ui::panel_header_actions()
                            .child(
                                favorite_button(("metadata-favorite", panel_id), favorite, |key| {
                                    self.tr(key)
                                })
                                .enabled(editable)
                                .build(
                                    IconButtonStyle::Surface,
                                    IconButtonDensity::Regular,
                                    cx,
                                    move |this, _, cx| {
                                        if this.metadata.editor_identity() != editor_identity {
                                            return;
                                        }
                                        let Some((track_id, favorite)) = this
                                            .metadata
                                            .track()
                                            .map(|track| (track.id, track.favorite))
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
                                ),
                            )
                            .child(
                                IconButton::new(
                                    ("metadata-save", panel_id),
                                    "󰆓",
                                    self.tr("gl-save-metadata"),
                                )
                                .enabled(can_save)
                                .build(
                                    IconButtonStyle::Surface,
                                    IconButtonDensity::Regular,
                                    cx,
                                    move |this, _, cx| {
                                        if this.metadata.editor_identity() == editor_identity {
                                            this.save(cx);
                                        }
                                    },
                                ),
                            )
                            .child(
                                IconButton::new(
                                    ("metadata-retry", panel_id),
                                    "󰑐",
                                    self.tr("gl-reload-playback"),
                                )
                                .build(
                                    IconButtonStyle::Surface,
                                    IconButtonDensity::Regular,
                                    cx,
                                    move |this, _, cx| {
                                        if this.metadata.editor_identity() == editor_identity {
                                            this.reload_playback_metadata(cx);
                                        }
                                    },
                                ),
                            ),
                    ))
                    .child(metadata_group(
                        theme,
                        self.tr("gl-file"),
                        column()
                            .gap_1()
                            .child(metadata_row(
                                div().flex_1().min_w_0().child(
                                    label_value_row(
                                        theme,
                                        file[0][0].0.clone(),
                                        file[0][0].1.clone(),
                                        label_widths[0],
                                        metadata_copy(
                                            file[0][0].0.clone(),
                                            file[0][0].1.clone(),
                                            copy_hint.clone(),
                                            cx,
                                        ),
                                    )
                                    .id(("metadata-path", panel_id))
                                    .tooltip(rivu_ui::text_tooltip(file[0][0].1.clone())),
                                ),
                                label_value_row(
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
                            ))
                            .child(label_value_row(
                                theme,
                                track_kind.0.clone(),
                                track_kind.1.clone(),
                                label_widths[0],
                                metadata_copy(track_kind.0, track_kind.1, copy_hint.clone(), cx),
                            ))
                            .when(cue.is_some(), |file| {
                                file.child(metadata_rows(
                                    theme,
                                    cue_fields,
                                    label_widths,
                                    copy_hint.clone(),
                                    cx,
                                ))
                                .child(caption(theme, self.tr("gl-cue-range-note")))
                            }),
                    ))
                    .child(metadata_group(
                        theme,
                        self.tr("gl-metadata"),
                        metadata_rows(theme, metadata, label_widths, copy_hint.clone(), cx),
                    ))
                    .child(
                        FormField::new(
                            theme,
                            self.tr("gl-decoder-hint"),
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
                                                        {
                                                            this.set_decoder_hint(decoder_hint, cx);
                                                        }
                                                    },
                                                )
                                                .role(gpui::Role::RadioButton)
                                                .aria_toggled(
                                                    if editable
                                                        && self.metadata_decoder_hint
                                                            == decoder_hint
                                                    {
                                                        gpui::Toggled::True
                                                    } else {
                                                        gpui::Toggled::False
                                                    },
                                                ),
                                                editable
                                                    && self.metadata_decoder_hint == decoder_hint,
                                            ),
                                            !editable,
                                        )
                                    },
                                ),
                            ),
                        )
                        .hint(self.tr("gl-decoder-save-note"))
                        .hint(self.tr("gl-decoder-warning"))
                        .into_div()
                        .gap_2()
                        .p_2()
                        .border_1()
                        .border_color(gpui::transparent_black()),
                    )
                    .child(metadata_group(
                        theme,
                        self.tr("gl-tags"),
                        column()
                            .gap_2()
                            .child(caption(theme, self.tr("gl-metadata-edit-note")))
                            .child(metadata_row(
                                self.panel_field(theme, Field::Title, self.tr("gl-title")),
                                self.panel_field(theme, Field::Artist, self.tr("gl-artist")),
                            ))
                            .child(metadata_row(
                                self.panel_field(theme, Field::Album, self.tr("gl-album")),
                                label_value_row(
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
mod spatial_tests {
    use super::*;
    use core::prelude::v1::test;
    use std::cell::Cell;

    struct MetadataPair {
        width: gpui::Pixels,
        bounds: Rc<Cell<[gpui::Bounds<gpui::Pixels>; 2]>>,
    }

    impl gpui::Render for MetadataPair {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = rivu_ui::theme::palette(cx);
            let [first, second] =
                [(0, "Duration", "12:34"), (1, "Codec", "FLAC")].map(|(index, label, value)| {
                    let bounds = self.bounds.clone();
                    label_value_row(theme, label, value, px(64.), None)
                        .relative()
                        .child(
                            gpui::canvas(
                                move |value, _, _| {
                                    let mut measured = bounds.get();
                                    measured[index] = value;
                                    bounds.set(measured);
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        )
                });
            div()
                .size_full()
                .child(metadata_row(first, second).w(self.width))
        }
    }

    #[gpui::test]
    fn metadata_pairs_stack_at_narrow_width_and_restore_equal_columns(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            rivu_ui::theme::set_palette(crate::theme::palette(crate::theme::Theme::default()), cx);
        });
        let bounds = Rc::new(Cell::new([gpui::Bounds::default(); 2]));
        let window = cx.add_window({
            let bounds = bounds.clone();
            move |_, _| MetadataPair {
                width: px(640.),
                bounds,
            }
        });
        for rem in [12., 24.] {
            for font_size in [8, 18] {
                cx.update(|cx| {
                    rivu_ui::theme::set_typography(
                        gpui::font("sans-serif"),
                        super::super::appearance::fonts::SYMBOLS_FAMILY,
                        font_size,
                        cx,
                    );
                });
                let mut wide_bounds = None;
                for width_rems in [40., 15., 40.] {
                    let width = px(rem * width_rems);
                    window
                        .update(cx, |this, window, cx| {
                            this.width = width;
                            window.set_rem_size(px(rem));
                            cx.notify();
                        })
                        .unwrap();
                    cx.update_window(window.into(), |_, window, cx| {
                        window.draw(cx).clear(cx);
                    })
                    .unwrap();
                    let [first, second] = bounds.get();
                    assert_eq!(first.size.width, second.size.width);
                    assert!(first.right() <= width && second.right() <= width);
                    if width_rems < 32. {
                        assert_eq!(first.left(), second.left());
                        assert!(second.top() >= first.bottom());
                        assert_eq!(first.size.width, width);
                    } else {
                        assert_eq!(first.top(), second.top());
                        assert!(second.left() > first.right());
                        if let Some(original) = wide_bounds {
                            assert_eq!([first, second], original);
                        }
                        wide_bounds = Some([first, second]);
                    }
                }
            }
        }
    }
}
