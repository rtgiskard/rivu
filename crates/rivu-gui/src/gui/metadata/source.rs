//! Explicit source-tag operations and identity-bound staged display values.

use super::{
    Metadata, MetadataEvent,
    session::{EditorIdentity, Field},
};
use gpui::{prelude::*, *};
use rivu_format::tags::{TagEncoding, TagValues};
use rivu_protocol::model::{Query, SourceTagRecovery, Track};
use rivu_ui::{
    FocusScope, IconButton, IconButtonDensity, IconButtonStyle, Popover, caption, column,
    disabled_control, menu_item,
};
use std::{cell::Cell, collections::HashMap, rc::Rc, sync::Arc, time::Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SourceAction {
    Load(TagEncoding),
    Sync,
    RetrySync,
    Save,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SourceToken {
    pub identity: EditorIdentity,
    serial: u64,
    pub action: SourceAction,
    pub database_serial: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SourceStatus {
    Staged,
    Synced,
    Saved,
    Failed(String),
}

struct Overlay {
    values: TagValues,
    fields: u8,
}

/// Source results never own database values or unsubmitted inline input.
#[derive(Default)]
pub(super) struct SourceTags {
    serial: u64,
    generation: u64,
    pending: Option<SourceToken>,
    overlay: Option<Overlay>,
    pub status: Option<SourceStatus>,
    // Observed Core state, not a receipt owned by this window.
    recovery: Option<Arc<SourceTagRecovery>>,
}

fn field_bit(field: Field) -> u8 {
    match field {
        Field::Title => 1,
        Field::Artist => 2,
        Field::Album => 4,
        Field::ReleaseDate => 8,
        Field::ReplayGainTrackGain => 64,
        Field::PlayCount | Field::DecoderHint => 0,
    }
}

pub(super) fn database_tags(track: &Track) -> TagValues {
    TagValues {
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        release_date: track.release_date.clone(),
        track_number: track.track_number,
        disc_number: track.disc_number,
        replaygain_track_gain: track.replaygain_track_gain,
    }
}

impl SourceTags {
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn recovery(&self, track_id: i64) -> Option<&SourceTagRecovery> {
        self.recovery
            .as_deref()
            .filter(|save| save.track_id == track_id)
    }

    pub(super) fn allows(&self, track_id: i64, action: SourceAction) -> bool {
        match action {
            SourceAction::Load(_) => self.recovery(track_id).is_none(),
            SourceAction::Save => self.recovery.is_none(),
            SourceAction::Sync => true,
            SourceAction::RetrySync => self.recovery(track_id).is_some(),
        }
    }

    pub(super) fn change_selection(&mut self) {
        // Keep accepted writes busy until their acknowledgement, even after leaving the editor.
        if self.pending.is_some_and(|token| {
            !matches!(token.action, SourceAction::Save | SourceAction::RetrySync)
        }) {
            self.pending = None;
        }
        self.edited();
        self.overlay = None;
        self.status = None;
    }

    pub(super) fn observe_recovery(
        &mut self,
        track_id: Option<i64>,
        recovery: Option<&Arc<SourceTagRecovery>>,
    ) -> bool {
        if self.recovery.as_ref().map(Arc::as_ptr) == recovery.map(Arc::as_ptr) {
            return false;
        }
        let was_recovering = track_id.is_some_and(|id| self.recovery(id).is_some());
        self.recovery = recovery.cloned();
        let is_recovering = track_id.is_some_and(|id| self.recovery(id).is_some());
        if was_recovering != is_recovering {
            // Discard the editor's old staging and fence any in-flight file read.
            self.committed();
            self.edited();
            self.status = None;
        }
        true
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn staged_field(&self, track_id: i64, field: Field) -> Option<&TagValues> {
        let bit = field_bit(field);
        if bit == 0 {
            return None;
        }
        self.recovery(track_id).map(|save| &save.tags).or_else(|| {
            self.overlay
                .as_ref()
                .filter(|overlay| overlay.fields & bit != 0)
                .map(|overlay| &overlay.values)
        })
    }

    pub fn field_text(&self, field: Field, track: &Track) -> String {
        let staged = self.staged_field(track.id, field);
        match field {
            Field::Title => staged.map_or(&track.title, |values| &values.title).clone(),
            Field::Artist => staged
                .map_or(&track.artist, |values| &values.artist)
                .clone(),
            Field::Album => staged.map_or(&track.album, |values| &values.album).clone(),
            Field::ReleaseDate => staged
                .map_or(&track.release_date, |values| &values.release_date)
                .clone()
                .unwrap_or_default(),
            Field::PlayCount => track.play_count.to_string(),
            Field::DecoderHint => String::new(),
            Field::ReplayGainTrackGain => staged
                .map_or(track.replaygain_track_gain, |v| v.replaygain_track_gain)
                .map(|v| v.to_string())
                .unwrap_or_default(),
        }
    }

    pub fn numbers(&self, track: &Track) -> (Option<u32>, Option<u32>) {
        if let Some(save) = self.recovery(track.id) {
            return (save.tags.track_number, save.tags.disc_number);
        }
        let overlay = self.overlay.as_ref();
        (
            overlay
                .filter(|value| value.fields & 16 != 0)
                .map_or(track.track_number, |value| value.values.track_number),
            overlay
                .filter(|value| value.fields & 32 != 0)
                .map_or(track.disc_number, |value| value.values.disc_number),
        )
    }

    pub fn edited(&mut self) {
        self.serial = self.serial.wrapping_add(1);
    }

    pub fn values(&self, track: &Track) -> TagValues {
        let (track_number, disc_number) = self.numbers(track);
        TagValues {
            title: self.field_text(Field::Title, track),
            artist: self.field_text(Field::Artist, track),
            album: self.field_text(Field::Album, track),
            release_date: self
                .staged_field(track.id, Field::ReleaseDate)
                .map_or(&track.release_date, |tags| &tags.release_date)
                .clone(),
            track_number,
            disc_number,
            replaygain_track_gain: self
                .staged_field(track.id, Field::ReplayGainTrackGain)
                .map_or(track.replaygain_track_gain, |v| v.replaygain_track_gain),
        }
    }

    pub fn begin(
        &mut self,
        identity: EditorIdentity,
        action: SourceAction,
        database_serial: u64,
    ) -> Option<SourceToken> {
        if self.busy() || !self.allows(identity.track_id, action) {
            return None;
        }
        self.edited();
        let token = SourceToken {
            identity,
            serial: self.serial,
            action,
            database_serial,
        };
        self.pending = Some(token);
        Some(token)
    }

    /// Retires only the matching operation; newer edits may keep its values out.
    pub fn finish(&mut self, token: SourceToken) -> Option<bool> {
        if self.pending != Some(token) {
            return None;
        }
        self.pending = None;
        Some(self.serial == token.serial)
    }

    pub fn stage(&mut self, values: TagValues) {
        self.generation = self.generation.wrapping_add(1);
        self.overlay = Some(Overlay {
            values,
            fields: 127,
        });
        self.status = Some(SourceStatus::Staged);
    }

    pub fn field_saved(&mut self, field: Field) {
        if let Some(overlay) = &mut self.overlay {
            overlay.fields &= !field_bit(field);
            if overlay.fields == 0 {
                self.overlay = None;
            }
        }
    }

    pub fn committed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.overlay = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SupportReason {
    Unavailable,
    Missing,
    Cue,
    Unsupported,
}

/// Known container and codec hints only; Core validates actual file content on demand.
pub(super) fn source_support(track: Option<&Track>) -> Result<(), SupportReason> {
    let track = track.ok_or(SupportReason::Unavailable)?;
    if track.missing {
        return Err(SupportReason::Missing);
    }
    if track.cue.is_some() {
        return Err(SupportReason::Cue);
    }
    let extension = track
        .path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let codec = track.codec.to_ascii_lowercase();
    let codec = codec
        .split_once(" (")
        .map_or(codec.as_str(), |(name, _)| name);
    let supported = match extension.to_ascii_lowercase().as_str() {
        "mp3" | "mp2" | "mp1" => matches!(codec, "mp3" | "mp2" | "mp1" | "mpeg"),
        "aac" => matches!(codec, "aac"),
        "wav" | "wave" | "aif" | "aiff" | "aifc" => {
            codec.starts_with("pcm")
                || codec.starts_with("adpcm_")
                || matches!(codec, "wav" | "aiff" | "alaw" | "mulaw" | "adpcm")
        }
        "flac" => codec == "flac",
        "ogg" | "oga" => matches!(codec, "vorbis" | "opus" | "speex" | "flac"),
        "opus" => codec == "opus",
        "spx" => codec == "speex",
        "m4a" | "m4b" | "mp4" => matches!(codec, "aac" | "alac"),
        "ape" => matches!(codec, "ape" | "monkey's audio"),
        "wv" => matches!(codec, "wavpack" | "wv"),
        "mpc" | "mpp" | "mp+" => matches!(codec, "mpc" | "musepack" | "mpc7" | "mpc8"),
        _ => false,
    };
    if supported {
        Ok(())
    } else {
        Err(SupportReason::Unsupported)
    }
}

pub(super) struct SourceMenu {
    panel: Option<u64>,
    encodings: bool,
    scope: FocusScope,
    return_focus: Option<FocusHandle>,
    pub bounds: HashMap<u64, Rc<Cell<Option<Bounds<Pixels>>>>>,
}

impl SourceMenu {
    pub fn new(cx: &mut App) -> Self {
        Self {
            panel: None,
            encodings: false,
            scope: FocusScope::new(cx),
            return_focus: None,
            bounds: HashMap::new(),
        }
    }
    pub fn close(&mut self) {
        self.panel = None;
        self.encodings = false;
        self.return_focus = None;
    }
    pub fn retain(&mut self, visible: &[u64]) {
        self.bounds.retain(|panel, _| visible.contains(panel));
        if self.panel.is_some_and(|panel| !visible.contains(&panel)) {
            self.close();
        }
    }
}

impl Metadata {
    fn source_menu_enabled(&self) -> bool {
        self.metadata.editable() && !self.metadata.any_saving() && !self.metadata.source.busy()
    }

    pub(super) fn close_source_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.source_menu.return_focus.take();
        self.source_menu.close();
        if let Some(focus) = focus {
            focus.focus(window, cx);
        }
        cx.notify();
    }

    /// Window capture routing gives the local menu the same first refusal as shell menus.
    pub(in crate::gui) fn source_menu_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.source_menu.panel.is_none() {
            return false;
        }
        if rivu_ui::tab_key(event) {
            self.source_menu
                .scope
                .advance(event.keystroke.modifiers.shift, window, cx);
        } else if !event.keystroke.modifiers.modified() {
            match event.keystroke.key.as_str() {
                "escape" | "left" if self.source_menu.encodings => {
                    self.source_menu.encodings = false;
                    self.source_menu.scope.focus(window, cx);
                }
                "escape" => self.close_source_menu(window, cx),
                "up" | "down" => {
                    self.source_menu
                        .scope
                        .advance(event.keystroke.key == "up", window, cx)
                }
                // Native focused menu controls own activation.
                "enter" | "space" => return false,
                _ => {}
            }
        }
        cx.notify();
        true
    }

    fn source_reason(&self) -> Option<SharedString> {
        source_support(self.metadata.track()).err().map(|reason| {
            self.tr(match reason {
                SupportReason::Unavailable => "gl-tags-unavailable",
                SupportReason::Missing => "gl-tags-missing",
                SupportReason::Cue => "gl-tags-cue",
                SupportReason::Unsupported => "gl-tags-unsupported",
            })
        })
    }

    pub(super) fn source_status(&self) -> Option<SharedString> {
        let source = &self.metadata.source;
        let track_id = self.metadata.identity()?;
        if source
            .pending
            .is_some_and(|token| token.identity.track_id == track_id)
        {
            return Some(self.tr("gl-tags-working"));
        }
        if let Some(recovery) = source.recovery(track_id) {
            return Some(
                self.context
                    .i18n
                    .format(
                        "gl-tags-file-saved-db-failed",
                        &[("error", recovery.error.as_str().into())],
                    )
                    .into(),
            );
        }
        source.status.as_ref().map(|status| match status {
            SourceStatus::Staged => self.tr("gl-tags-staged"),
            SourceStatus::Synced => self.tr("gl-tags-synced"),
            SourceStatus::Saved => self.tr("gl-tags-saved"),
            SourceStatus::Failed(error) => self
                .context
                .i18n
                .format("gl-tags-failed", &[("error", error.as_str().into())])
                .into(),
        })
    }

    pub(super) fn source_menu_trigger(&mut self, panel: u64, cx: &mut Context<Self>) -> Div {
        let enabled = self.source_menu_enabled();
        let identity = self.metadata.editor_identity();
        let bounds = self
            .source_menu
            .bounds
            .entry(panel)
            .or_insert_with(|| Rc::new(Cell::new(None)))
            .clone();
        div()
            .relative()
            .child(
                canvas(move |value, _, _| bounds.set(Some(value)), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(
                IconButton::new(
                    ("metadata-tags-menu", panel),
                    "󰇘",
                    self.tr("gl-tags-actions"),
                )
                .surface(crate::gui::appearance::material::surface(cx))
                .enabled(enabled)
                .build(
                    IconButtonStyle::Surface,
                    IconButtonDensity::Regular,
                    cx,
                    move |this, window, cx| {
                        if !this.source_menu_enabled()
                            || this.metadata.editor_identity() != identity
                        {
                            return;
                        }
                        if this.source_menu.panel == Some(panel) {
                            this.close_source_menu(window, cx);
                            return;
                        }
                        this.cancel_edit(cx);
                        this.close_hint(cx);
                        this.source_menu.return_focus = window.focused(cx);
                        this.source_menu.panel = Some(panel);
                        this.source_menu.encodings = false;
                        this.source_menu.scope.focus(window, cx);
                        cx.notify();
                    },
                ),
            )
    }

    pub(super) fn render_source_menu(
        &self,
        panel: u64,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.source_menu.panel != Some(panel) {
            return None;
        }
        let bounds = self.source_menu.bounds.get(&panel)?.get()?;
        let theme = rivu_ui::theme::palette(cx);
        let identity = self.metadata.editor_identity();
        let reason = self.source_reason();
        let supported = reason.is_none();
        let enabled = self.source_menu_enabled();
        let allows = |action| {
            identity.is_some_and(|identity| self.metadata.source.allows(identity.track_id, action))
        };
        let load_action = SourceAction::Load(TagEncoding::Declared);
        let load_enabled = enabled && supported && allows(load_action);
        // Only action rows contribute to the menu's intrinsic width.
        let notice = |text| {
            div().w_0().min_w_full().child(
                caption(theme, text)
                    .w_full()
                    .px_2()
                    .py_1()
                    .whitespace_normal(),
            )
        };
        let mut menu = column();
        if self.source_menu.encodings {
            menu = menu.child(rivu_ui::menu_back(
                "metadata-tags-back",
                self.tr("gl-tags-load"),
                cx,
                |this, window, cx| {
                    this.source_menu.encodings = false;
                    this.source_menu.scope.focus(window, cx);
                    cx.notify();
                },
            ));
            for (index, (encoding, key)) in [
                (TagEncoding::Declared, "gl-tags-encoding-declared"),
                (TagEncoding::Utf8, "gl-tags-encoding-utf8"),
                (TagEncoding::Gb18030, "gl-tags-encoding-gb18030"),
                (TagEncoding::Big5, "gl-tags-encoding-big5"),
                (TagEncoding::ShiftJis, "gl-tags-encoding-shift-jis"),
                (TagEncoding::EucKr, "gl-tags-encoding-euc-kr"),
                (TagEncoding::Windows1252, "gl-tags-encoding-windows-1252"),
                (TagEncoding::Windows1251, "gl-tags-encoding-windows-1251"),
            ]
            .into_iter()
            .enumerate()
            {
                let control = menu_item(
                    ("metadata-tags-encoding", index),
                    self.tr(key),
                    cx,
                    move |this, window, cx| {
                        if this.metadata.editor_identity() == identity
                            && this.source_menu_enabled()
                            && this.source_reason().is_none()
                            && identity.is_some_and(|id| {
                                this.metadata
                                    .source
                                    .allows(id.track_id, SourceAction::Load(encoding))
                            })
                        {
                            this.close_source_menu(window, cx);
                            this.run_source_action(SourceAction::Load(encoding), cx);
                        }
                    },
                );
                menu = menu.child(disabled_control(theme, control, !load_enabled));
            }
        } else {
            let load = menu_item(
                "metadata-tags-load",
                self.tr("gl-tags-load"),
                cx,
                move |this, window, cx| {
                    if this.metadata.editor_identity() == identity
                        && this.source_menu_enabled()
                        && this.source_reason().is_none()
                        && identity
                            .is_some_and(|id| this.metadata.source.allows(id.track_id, load_action))
                    {
                        this.source_menu.encodings = true;
                        this.source_menu.scope.focus(window, cx);
                        cx.notify();
                    }
                },
            )
            .child(caption(theme, ""));
            menu = menu.child(disabled_control(theme, load, !load_enabled));
            for (action, key, id) in [
                (SourceAction::Sync, "gl-tags-sync", "metadata-tags-sync"),
                (SourceAction::Save, "gl-tags-save", "metadata-tags-save"),
            ] {
                let allowed =
                    enabled && allows(action) && (action == SourceAction::Sync || supported);
                let control = menu_item(id, self.tr(key), cx, move |this, window, cx| {
                    if this.metadata.editor_identity() == identity {
                        this.close_source_menu(window, cx);
                        this.run_source_action(action, cx);
                    }
                });
                menu = menu.child(disabled_control(theme, control, !allowed));
            }
            if let Some(reason) = reason {
                menu = menu.child(notice(reason));
            }
            if identity.is_some_and(|id| self.metadata.source.recovery(id.track_id).is_some()) {
                menu = menu.child(notice(self.tr("gl-tags-sync-only")));
            }
        }
        let surface = self
            .source_menu
            .scope
            .menu_surface(Popover::menu(theme, ("metadata-tags-popup", panel), window))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                // Pointer dismissal leaves the next control's focus handoff alone.
                this.source_menu.close();
                cx.notify();
                cx.stop_propagation();
            }))
            .child(menu);
        Some(
            deferred(
                anchored()
                    .position(bounds.bottom_left())
                    .snap_to_window()
                    .child(surface),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    pub(super) fn run_source_action(&mut self, action: SourceAction, cx: &mut Context<Self>) {
        if !self.source_menu_enabled() {
            return;
        }
        if matches!(action, SourceAction::Load(_) | SourceAction::Save)
            && self.source_reason().is_some()
        {
            return;
        }
        let Some((token, command)) = self.metadata.begin_source(action) else {
            return;
        };
        self.cancel_edit(cx);
        self.source_menu.close();
        let handle = self.context.handle().clone();
        cx.spawn(async move |this, cx| {
            let delivery = handle.request_command(command).await;
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                this.finish_source_action(token, delivery, cx)
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn finish_source_action(
        &mut self,
        token: SourceToken,
        delivery: crate::frontend::CommandDelivery,
        cx: &mut Context<Self>,
    ) {
        let same_editor = self.metadata.editor_identity() == Some(token.identity);
        let track_id = token.identity.track_id;
        let Some(current) = self.metadata.source.finish(token) else {
            return;
        };
        let source = delivery
            .reply()
            .and_then(|reply| reply.receipt())
            .and_then(|receipt| receipt.outcome.source_tags());
        if !delivery.is_success() {
            let error = delivery
                .observed_result()
                .expect_err("failed source command");
            let observed_save_failure = source.is_some_and(|source| source.file_saved)
                && delivery.observation == crate::frontend::CommandObservation::Refreshed;
            if same_editor
                && !observed_save_failure
                && self.metadata.source.recovery(track_id).is_none()
            {
                self.metadata.source.status = Some(SourceStatus::Failed(error));
            }
            cx.notify();
            return;
        }
        let observation = delivery.observed_result();
        let observed = observation.is_ok();
        if let Err(error) = observation {
            self.report_error(error, cx);
        } else if let Some(error) = source.and_then(|source| source.refresh_error.as_ref()) {
            self.report_error(error.clone(), cx);
        }
        if self.metadata.identity() != Some(track_id) {
            // Never publish the other track's values or status in this editor.
            cx.notify();
            return;
        }
        if let Some(tags) = source.and_then(|source| source.tags.as_ref()) {
            match token.action {
                SourceAction::Load(_) if same_editor && current => {
                    self.metadata.source.stage(tags.clone());
                }
                SourceAction::Load(_) => {}
                SourceAction::Sync | SourceAction::RetrySync | SourceAction::Save => {
                    if same_editor {
                        // The receipt records server success, but cannot replace the
                        // database view or retire staging without a fresh observation.
                        if observed {
                            self.metadata.commit_tags(token, tags);
                            if current {
                                self.metadata.source.committed();
                            }
                        }
                        self.metadata.source.status =
                            Some(if source.is_some_and(|source| source.file_saved) {
                                SourceStatus::Saved
                            } else {
                                SourceStatus::Synced
                            });
                    }
                    if !observed {
                        cx.notify();
                        return;
                    }
                    let query = Query::Track { track_id };
                    self.metadata_read.request(
                        track_id,
                        self.state.query_revisions_for(&query),
                        Instant::now(),
                    );
                    self.demand_metadata(cx);
                    cx.emit(MetadataEvent::TrackChanged);
                }
            }
        } else if same_editor {
            self.metadata.source.status = Some(SourceStatus::Failed(
                self.tr("gl-tags-response-invalid").to_string(),
            ));
        }
        cx.notify();
    }
}
