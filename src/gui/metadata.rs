//! Compact track details with one cancellable inline editor per window.
//! Each Enter submits only that field; decoder hint writes are independent.
//! Selection epochs and edit serials prevent late work from touching a new editor.

mod artwork;
mod read;
pub(in crate::gui) mod session;

#[cfg(test)]
mod tests;

use super::{
    artwork::{ArtworkImageRegistry, ArtworkLoader, ArtworkView, DefaultArtwork},
    components::{artwork::artwork_preview, track::favorite_button},
    feature::FeatureContext,
    format_time,
};
use crate::projection::GuiSnapshot;
use chrono::{DateTime, Local};
use gpui::{
    AnyElement, App, Context, Div, Entity, EventEmitter, Focusable, Font, SharedString,
    Subscription, Window, div, prelude::*, px, rgb,
};
use read::{MetadataReadCoordinator, MetadataReadTask, ReadCompletion, ReadEnvironment};
use rivu_format::decoder_hint::DecoderHint;
use rivu_metadata::artwork::ArtworkManager;
use rivu_protocol::model::{Command, PlaybackStatus, Query, Track};
use rivu_ui::{
    ChoiceItem, ChoicePopup, ChoiceSelect, ChoiceState, IconButton, IconButtonDensity,
    IconButtonStyle, caption, column, disabled_control,
    input::{Input, InputEvent},
    label_value_row, panel_surface, row, section_group,
};
use session::{Field, MetadataAvailability, MetadataReadState, MetadataSession, SavedValue};
use std::{cell::Cell, collections::HashMap, rc::Rc, sync::Arc, time::Instant};

struct InlineEdit {
    field: Field,
    baseline: String,
    serial: u64,
    error: Option<String>,
    input: Entity<Input>,
    _submit: Subscription,
}

pub(in crate::gui) enum MetadataEvent {
    ReloadPlaybackRequested,
    TrackChanged,
    Error(String),
}

pub(in crate::gui) struct SavedHintChanged(pub Track);

/// One inline editor per window; accepted commands outlive its focus or selection.
pub(in crate::gui) struct Metadata {
    context: FeatureContext,
    state: Rc<GuiSnapshot>,
    metadata: MetadataSession,
    metadata_read: MetadataReadCoordinator,
    edit: Option<InlineEdit>,
    edit_serial: u64,
    edit_blur: Option<Subscription>,
    hint_choices: ChoiceState<DecoderHint>,
    hint_panel: Option<u64>,
    hint_bounds: HashMap<u64, Rc<Cell<Option<gpui::Bounds<gpui::Pixels>>>>>,
    visible: bool,
    error: Option<String>,
    ui_font: Font,
    artwork_manager: Arc<ArtworkManager>,
    artwork_loader: ArtworkLoader,
    artwork_view: ArtworkView,
    default_artwork: Entity<DefaultArtwork>,
}

impl EventEmitter<MetadataEvent> for Metadata {}
impl EventEmitter<SavedHintChanged> for Metadata {}

impl Metadata {
    pub(in crate::gui) fn new(
        context: FeatureContext,
        ui_font: Font,
        artwork_manager: Arc<ArtworkManager>,
        artwork_images: ArtworkImageRegistry,
        default_artwork: Entity<DefaultArtwork>,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = context.snapshot();
        let mut this = Self {
            context,
            state,
            metadata: MetadataSession::default(),
            metadata_read: MetadataReadCoordinator::default(),
            edit: None,
            edit_serial: 0,
            edit_blur: None,
            hint_choices: ChoiceState::default(),
            hint_panel: None,
            hint_bounds: HashMap::new(),
            visible: false,
            error: None,
            ui_font,
            artwork_manager,
            artwork_loader: ArtworkLoader::default(),
            artwork_view: ArtworkView::new(artwork_images),
            default_artwork,
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
            self.cancel_edit(cx);
            self.close_hint(cx);
            self.artwork_view.replace(None, cx);
        }
        cx.notify();
    }

    pub(in crate::gui) fn retain_artwork_consumers(&mut self, visible: &[u64]) {
        self.artwork_view.retain_consumers(visible);
        self.hint_bounds.retain(|panel, _| visible.contains(panel));
        if self
            .hint_panel
            .is_some_and(|panel| !visible.contains(&panel))
        {
            self.hint_panel = None;
            self.hint_choices.close();
        }
    }

    pub(in crate::gui) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.edit
            .as_ref()
            .is_some_and(|edit| edit.input.read(cx).is_focused(window))
    }

    pub(in crate::gui) fn dismiss_input_composition(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.edit
            .as_ref()
            .map(|edit| &edit.input)
            .filter(|input| input.read(cx).is_focused(window))
            .is_some_and(|input| input.update(cx, |input, cx| input.dismiss_composition(cx)))
    }

    pub(in crate::gui) fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if self.edit.take().is_some() {
            self.edit_blur = None;
            cx.notify();
        }
    }

    pub(in crate::gui) fn track(&self) -> Option<&Track> {
        self.metadata.track()
    }

    pub(in crate::gui) fn track_snapshot(&self) -> Option<Arc<Track>> {
        self.metadata.track_snapshot()
    }

    fn begin_edit(&mut self, field: Field, window: &mut Window, cx: &mut Context<Self>) {
        let Some(track) = self.metadata.track() else {
            return;
        };
        if self.metadata.saving(field) {
            return;
        }
        let baseline = match field {
            Field::Title => track.title.clone(),
            Field::Artist => track.artist.clone(),
            Field::Album => track.album.clone(),
            Field::PlayCount => track.play_count.to_string(),
            Field::DecoderHint => return,
        };
        self.cancel_edit(cx);
        self.error = None;
        self.edit_serial = self.edit_serial.wrapping_add(1);
        let serial = self.edit_serial;
        let name = self.tr(match field {
            Field::Title => "gl-title",
            Field::Artist => "gl-artist",
            Field::Album => "gl-album",
            Field::PlayCount => "gl-play-count",
            Field::DecoderHint => unreachable!(),
        });
        let input = cx.new(|cx| Input::new(name, baseline.clone(), "", cx).embedded());
        let submit = cx.subscribe(&input, move |this, _, event, cx| {
            if !this.edit.as_ref().is_some_and(|edit| edit.serial == serial) {
                return;
            }
            match event {
                InputEvent::Submit => this.submit_field(field, cx),
                InputEvent::Changed { user: true, .. } => cx.notify(),
                _ => {}
            }
        });
        let focus = input.read(cx).focus_handle(cx);
        self.edit = Some(InlineEdit {
            field,
            baseline,
            serial,
            error: None,
            input: input.clone(),
            _submit: submit,
        });
        input.update(cx, |input, cx| input.focus_and_select_all(window, cx));
        self.edit_blur = Some(cx.on_blur(&focus, window, move |this, _, cx| {
            if this.edit.as_ref().is_some_and(|edit| edit.serial == serial) {
                this.cancel_edit(cx);
            }
        }));
        // GPUI reports blur from rendered focus paths. A focus handoff before the
        // input's first frame has no previous path, so also settle that handoff
        // after the current input cycle instead of retaining an unfocused editor.
        let owner = cx.weak_entity();
        window.defer(cx, move |window, cx| {
            let _ = owner.update(cx, |this, cx| {
                if this.edit.as_ref().is_some_and(|edit| {
                    edit.serial == serial && !edit.input.read(cx).is_focused(window)
                }) {
                    this.cancel_edit(cx);
                }
            });
        });
        cx.notify();
    }

    fn submit_field(&mut self, field: Field, cx: &mut Context<Self>) {
        let Some(edit) = self.edit.as_ref().filter(|edit| edit.field == field) else {
            return;
        };
        if self.metadata.saving(field) {
            return;
        }
        let text = edit.input.read(cx).text();
        if text == edit.baseline {
            self.cancel_edit(cx);
            return;
        }
        let serial = edit.serial;
        let value = match field {
            Field::Title => SavedValue::Title(text.to_owned()),
            Field::Artist => SavedValue::Artist(text.to_owned()),
            Field::Album => SavedValue::Album(text.to_owned()),
            Field::PlayCount => match text.parse::<u64>() {
                Ok(value) if value <= i64::MAX as u64 => SavedValue::PlayCount(value),
                _ => {
                    let error = self.tr("gl-play-count-invalid").to_string();
                    self.edit.as_mut().unwrap().error = Some(error);
                    cx.notify();
                    return;
                }
            },
            Field::DecoderHint => return,
        };
        if !self.save_value(value, Some(serial), cx) {
            self.cancel_edit(cx);
        }
    }

    fn set_decoder_hint(&mut self, hint: DecoderHint, cx: &mut Context<Self>) {
        self.save_value(SavedValue::DecoderHint(hint), None, cx);
    }

    pub(in crate::gui) fn request_track(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.metadata.select(id) {
            self.cancel_edit(cx);
            self.close_hint(cx);
            self.error = None;
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
                match this.metadata_read.finish(
                    read,
                    &mut this.metadata,
                    environment,
                    response,
                    || this.context.i18n.text("gl-metadata-load-failed").into(),
                ) {
                    ReadCompletion::Retired => this.demand_metadata(cx),
                    ReadCompletion::Loaded(accepted) => {
                        if accepted && !this.metadata.editable() {
                            this.cancel_edit(cx);
                        }
                        this.resume_follow(cx);
                        cx.emit(MetadataEvent::TrackChanged);
                    }
                    ReadCompletion::Failed => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::gui) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.metadata.clear_selection();
        self.cancel_edit(cx);
        self.close_hint(cx);
        self.error = None;
        self.sync_metadata_default(cx);
        self.sync_metadata_query();
        cx.emit(MetadataEvent::TrackChanged);
        cx.notify();
    }

    fn sync_metadata_default(&mut self, cx: &mut Context<Self>) {
        let previous = self.metadata.editor_identity();
        self.metadata.sync_playback(self.state.current_track());
        if self.metadata.editor_identity() != previous {
            self.cancel_edit(cx);
            self.close_hint(cx);
            self.error = None;
        }
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

    fn save_value(
        &mut self,
        value: SavedValue,
        serial: Option<u64>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((token, command)) = self.metadata.begin_save(&value) else {
            return false;
        };
        let submitted = serial.and_then(|serial| {
            self.edit
                .as_ref()
                .map(|edit| (serial, edit.input.read(cx).text().to_owned()))
        });
        let hint_change = match value {
            SavedValue::DecoderHint(hint) => self.metadata.track().map(|track| {
                let mut track = track.clone();
                track.decoder_hint = hint;
                track
            }),
            _ => None,
        };
        self.error = None;
        if let Some(edit) = self
            .edit
            .as_mut()
            .filter(|edit| Some(edit.serial) == serial)
        {
            edit.error = None;
        }
        let handle = self.context.handle().clone();
        cx.spawn(async move |this, cx| {
            let ack = handle.request_ack(command).await;
            let _ = this.update(cx, |this, cx| {
                this.finish_save(token, submitted, hint_change, ack, cx);
            });
        })
        .detach();
        cx.notify();
        true
    }

    fn finish_save(
        &mut self,
        token: session::SaveToken,
        submitted: Option<(u64, String)>,
        hint_change: Option<Track>,
        ack: rivu_protocol::response::Ack,
        cx: &mut Context<Self>,
    ) {
        // A committed decoder change still invalidates waveform work after selection changes.
        if ack.is_success()
            && let Some(track) = hint_change
        {
            cx.emit(SavedHintChanged(track));
        }
        if !self.metadata.complete_save(token) {
            return;
        }
        if ack.is_success() {
            if let Some((serial, text)) = submitted
                && let Some(edit) = self.edit.as_mut().filter(|edit| edit.serial == serial)
            {
                if edit.input.read(cx).text() == text {
                    self.cancel_edit(cx);
                } else {
                    edit.baseline = text;
                }
            }
            let query = Query::Track {
                track_id: token.track_id,
            };
            self.metadata_read.request(
                token.track_id,
                self.state.query_revisions_for(&query),
                Instant::now(),
            );
            self.demand_metadata(cx);
            cx.emit(MetadataEvent::TrackChanged);
        } else {
            let error = ack
                .error
                .unwrap_or_else(|| self.context.i18n.text("gl-metadata-save-failed").into());
            if let Some((serial, _)) = submitted
                && let Some(edit) = self.edit.as_mut().filter(|edit| edit.serial == serial)
            {
                edit.error = Some(error.clone());
            }
            self.report_error(error, cx);
        }
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

    fn close_hint(&mut self, cx: &mut Context<Self>) {
        self.hint_choices.close();
        self.hint_panel = None;
        cx.notify();
    }

    fn choose_hint(&mut self, index: usize, _: &mut Window, cx: &mut Context<Self>) {
        let hint = self.hint_choices.item(index).map(|item| item.value);
        self.close_hint(cx);
        if let Some(hint) = hint {
            self.set_decoder_hint(hint, cx);
        }
    }

    fn decoder_view(&mut self, panel: u64, window: &Window, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let hint = self
            .metadata
            .track()
            .map_or(DecoderHint::Auto, |track| track.decoder_hint);
        let label = match hint {
            DecoderHint::Auto => self.tr("gl-auto"),
            DecoderHint::Native => self.tr("gl-native"),
            DecoderHint::Ffmpeg => "FFmpeg".into(),
        };
        let enabled = self.metadata.editable() && !self.metadata.saving(Field::DecoderHint);
        let identity = self.metadata.editor_identity();
        let bounds = self
            .hint_bounds
            .entry(panel)
            .or_insert_with(|| Rc::new(Cell::new(None)))
            .clone();
        let measure = bounds.clone();
        let trigger = ChoiceSelect::new(("metadata-decoder", panel), label).build(
            self,
            move |this| (this.hint_panel == Some(panel)).then_some(&mut this.hint_choices),
            cx,
            move |this, _, cx| {
                if !enabled || this.metadata.editor_identity() != identity {
                    return;
                }
                if this.hint_panel == Some(panel) {
                    this.close_hint(cx);
                    return;
                }
                this.hint_panel = Some(panel);
                this.hint_choices.open(
                    [
                        ChoiceItem::new(DecoderHint::Auto, this.tr("gl-auto")),
                        ChoiceItem::new(DecoderHint::Native, this.tr("gl-native")),
                        ChoiceItem::new(DecoderHint::Ffmpeg, "FFmpeg"),
                    ],
                    match hint {
                        DecoderHint::Auto => 0,
                        DecoderHint::Native => 1,
                        DecoderHint::Ffmpeg => 2,
                    },
                );
                cx.notify();
            },
            Self::choose_hint,
            |this, _, cx| this.close_hint(cx),
        );
        let mut content = column().gap_1().child(
            row()
                .gap_4()
                .child(caption(theme, self.tr("gl-decoder-hint")))
                .child(
                    div()
                        .relative()
                        .w(gpui::rems(10.))
                        .min_w_0()
                        .child(
                            gpui::canvas(
                                move |bounds, _, _| measure.set(Some(bounds)),
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        )
                        .child(disabled_control(theme, trigger, !enabled)),
                )
                .child(
                    div()
                        .id(("metadata-decoder-help", panel))
                        .flex_shrink_0()
                        .child(caption(theme, "").opacity(0.6).text_color(rgb(theme.text)))
                        .tooltip(rivu_ui::text_tooltip(self.tr("gl-decoder-note"))),
                ),
        );
        if self.hint_panel == Some(panel)
            && self.hint_choices.is_open()
            && let Some(bounds) = bounds.get()
        {
            content = content.child(
                ChoicePopup::new(
                    ("metadata-decoder-options", panel),
                    ("metadata-decoder-scroll", panel),
                    bounds,
                )
                .render(
                    self,
                    |this| &this.hint_choices,
                    Self::choose_hint,
                    |this, _, _, cx| {
                        this.close_hint(cx);
                        true
                    },
                    window,
                    cx,
                ),
            );
        }
        content
    }

    fn panel_field(
        &self,
        theme: &'static rivu_ui::theme::Palette,
        field: Field,
        label: SharedString,
        width: gpui::Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let value: SharedString = self.metadata.track().map_or_else(
            || self.tr("gl-not-loaded"),
            |track| match field {
                Field::Title => track.title.clone().into(),
                Field::Artist => track.artist.clone().into(),
                Field::Album => track.album.clone().into(),
                Field::PlayCount => track.play_count.to_string().into(),
                Field::DecoderHint => unreachable!(),
            },
        );
        let height = rivu_ui::single_line_row_height(window, cx);
        let identity = self.metadata.editor_identity();
        if let Some(edit) = self.edit.as_ref().filter(|edit| edit.field == field) {
            return row()
                .group("label-value-row")
                .min_w_0()
                .h(height)
                .gap_4()
                .child(caption(theme, label).w(width).flex_shrink_0().truncate())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .border_1()
                        .border_color(rgb(if edit.error.is_some() {
                            theme.error
                        } else {
                            theme.accent
                        }))
                        .rounded_sm()
                        .child(edit.input.clone()),
                )
                .when_some(edit.error.clone(), |row, error| {
                    row.child(
                        caption(theme, "")
                            .id(("metadata-field-error", field as usize))
                            .tooltip(rivu_ui::text_tooltip(error)),
                    )
                });
        }
        let actions = row()
            .id(("metadata-field-actions", field as usize))
            .gap_0()
            .child(rivu_ui::hover_reveal(
                ("metadata-edit-action", field as usize),
                "label-value-row",
                IconButton::new(
                    ("metadata-edit", field as usize),
                    if self.metadata.saving(field) {
                        "󰔟"
                    } else {
                        ""
                    },
                    self.tr(if self.metadata.saving(field) {
                        "gl-metadata-saving"
                    } else {
                        "gl-edit-value"
                    }),
                )
                .enabled(self.metadata.editable() && !self.metadata.saving(field))
                .build(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Compact,
                    cx,
                    move |this, window, cx| {
                        if this.metadata.editor_identity() == identity {
                            this.begin_edit(field, window, cx);
                        }
                    },
                )
                .text_size(gpui::rems(0.75)),
            ))
            .children(metadata_copy(
                label.clone(),
                value.clone(),
                self.metadata.track().map(|_| self.tr("gl-copy-value")),
                cx,
            ));
        label_value_row(theme, label, value, width, Some(actions))
            .h(height)
            .items_center()
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
                IconButtonDensity::Compact,
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
    title: impl IntoElement,
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
        self.prepare_artwork(cx);
        let decoder = self.decoder_view(panel_id, window, cx);
        let track = self.metadata.track();
        let id = track.map(|track| track.id);
        let favorite = track.is_some_and(|track| track.favorite);
        let editable = self.metadata.editable();
        let editor_identity = self.metadata.editor_identity();
        let identity = self.metadata.identity();
        let placeholder: SharedString = self.tr("gl-not-loaded");
        let copy_hint = track.map(|_| self.tr("gl-copy-value"));
        let presence_status = match self.metadata.availability() {
            MetadataAvailability::Unknown if identity.is_none() => {
                self.context.i18n.text("gl-no-track")
            }
            MetadataAvailability::Unknown => self.context.i18n.text("gl-details-not-loaded"),
            MetadataAvailability::Present(_) => self.context.i18n.text("gl-ready"),
            MetadataAvailability::Unavailable => self.context.i18n.text("gl-track-unavailable"),
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
        let heading = self.metadata.selected().or(id).map_or_else(
            || self.context.i18n.text("shell-panel-metadata").to_owned(),
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
        let release_date = (
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
        );
        let last_played = (
            self.tr("gl-last-played"),
            track.map_or_else(
                || placeholder.clone(),
                |track| self.last_played_text(track.id, track.last_played).into(),
            ),
        );
        let tags: [[(SharedString, SharedString); 2]; 1] = [[
            (
                self.tr("gl-disc"),
                track.map_or_else(
                    || placeholder.clone(),
                    |track| {
                        track
                            .disc_number
                            .map_or("—".into(), |value| value.to_string().into())
                    },
                ),
            ),
            (
                self.tr("gl-track-number"),
                track.map_or_else(
                    || placeholder.clone(),
                    |track| {
                        track
                            .track_number
                            .map_or("—".into(), |value| value.to_string().into())
                    },
                ),
            ),
        ]];
        let edit_labels = [
            [self.tr("gl-title"), self.tr("gl-artist")],
            [self.tr("gl-album"), release_date.0.clone()],
            [self.tr("gl-play-count"), last_played.0.clone()],
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
                .chain(edit_labels.iter().map(|fields| &fields[index]))
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
                    .child(decoder)
                    .child(metadata_group(
                        theme,
                        row()
                            .gap_2()
                            .child(caption(theme, self.tr("gl-tags")))
                            .child(
                                div()
                                    .id(("metadata-tags-help", panel_id))
                                    .flex_shrink_0()
                                    .child(
                                        caption(theme, "")
                                            .opacity(0.6)
                                            .text_color(rgb(theme.text)),
                                    )
                                    .tooltip(rivu_ui::text_tooltip(
                                        self.tr("gl-metadata-edit-note"),
                                    )),
                            ),
                        column()
                            .gap_1()
                            .child(metadata_row(
                                self.panel_field(
                                    theme,
                                    Field::Title,
                                    self.tr("gl-title"),
                                    label_widths[0],
                                    window,
                                    cx,
                                ),
                                self.panel_field(
                                    theme,
                                    Field::Artist,
                                    self.tr("gl-artist"),
                                    label_widths[1],
                                    window,
                                    cx,
                                ),
                            ))
                            .child(metadata_row(
                                self.panel_field(
                                    theme,
                                    Field::Album,
                                    self.tr("gl-album"),
                                    label_widths[0],
                                    window,
                                    cx,
                                ),
                                label_value_row(
                                    theme,
                                    release_date.0.clone(),
                                    release_date.1.clone(),
                                    label_widths[1],
                                    metadata_copy(
                                        release_date.0,
                                        release_date.1,
                                        copy_hint.clone(),
                                        cx,
                                    ),
                                ),
                            ))
                            .child(metadata_rows(
                                theme,
                                tags,
                                label_widths,
                                copy_hint.clone(),
                                cx,
                            ))
                            .child(metadata_row(
                                self.panel_field(
                                    theme,
                                    Field::PlayCount,
                                    self.tr("gl-play-count"),
                                    label_widths[0],
                                    window,
                                    cx,
                                ),
                                label_value_row(
                                    theme,
                                    last_played.0.clone(),
                                    last_played.1.clone(),
                                    label_widths[1],
                                    metadata_copy(last_played.0, last_played.1, copy_hint, cx),
                                ),
                            ))
                            .when(track.is_some(), |tags| {
                                tags.child(match self.artwork_view.measured_image(panel_id, cx) {
                                    Some(image) => artwork_preview(
                                        theme,
                                        self.tr("shell-panel-artwork"),
                                        image,
                                        self.artwork_view.aspect_ratio(cx),
                                    ),
                                    None => artwork_preview(
                                        theme,
                                        self.tr("shell-panel-artwork"),
                                        self.default_artwork
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
