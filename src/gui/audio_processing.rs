//! Dockable processing controls. Core owns committed parameters and task results;
//! this owner retains only field drafts, selection and cancellable pointer intent.
use super::{feature::FeatureContext, layout::interaction::DockGeometry};
use crate::{
    model::{
        AuditionMode, Command, DenoisePatch, EqualizerEdit, NeuralPatch, NoiseSelection,
        ProcessingOperation, ProcessingPhase, Track,
    },
    projection::GuiSnapshot,
};
use gpui::{prelude::*, *};
use rivu_ui::{
    FormField, button, caption, checkbox, column, disabled_control,
    input::{Input, InputEvent},
    row, scrollable_area,
};
use serde_json::{Value, json};
use std::{
    cell::Cell,
    collections::{HashMap, VecDeque},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
};

#[path = "audio_processing_graph.rs"]
mod processing_graph_ui;
use processing_graph_ui::GraphUi;
#[path = "audio_processing_equalizer.rs"]
mod equalizer_editor;
use equalizer_editor::EqualizerUi;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Graph,
    Equalizer,
    Restoration,
    Neural,
}

struct Field {
    input: Entity<Input>,
    committed: String,
    dirty: bool,
    _subscription: Subscription,
}

struct Submission {
    command: Command,
    field: Option<(Entity<Input>, String)>,
    equalizer_field: Option<equalizer_editor::SubmittedField>,
}

type MeasuredGraph = Rc<Cell<Option<(Bounds<Pixels>, DockGeometry)>>>;

pub(super) struct AudioProcessing {
    context: FeatureContext,
    state: Rc<GuiSnapshot>,
    selected: Option<Arc<Track>>,
    use_selected: bool,
    page: Page,
    fields: HashMap<String, Field>,
    error: Option<String>,
    pending: usize,
    commands: VecDeque<Submission>,
    command_active: bool,
    geometry_epoch: u64,
    graph_ui: GraphUi,
    equalizer_ui: EqualizerUi,
}

impl AudioProcessing {
    pub(super) fn new(context: FeatureContext, cx: &mut Context<Self>) -> Self {
        let state = context.snapshot();
        let mut this = Self {
            context,
            state,
            selected: None,
            use_selected: false,
            page: Page::Graph,
            fields: HashMap::new(),
            error: None,
            pending: 0,
            commands: VecDeque::with_capacity(64),
            command_active: false,
            geometry_epoch: 0,
            graph_ui: GraphUi::default(),
            equalizer_ui: EqualizerUi::default(),
        };
        this.sync_fields(cx);
        this
    }

    fn tr(&self, id: &str) -> SharedString {
        self.context.tr(id)
    }

    pub(super) fn set_selected(&mut self, track: Option<Arc<Track>>, cx: &mut Context<Self>) {
        if self.selected.as_deref() != track.as_deref() {
            self.selected = track;
            cx.notify();
        }
    }

    pub(super) fn reconcile(&mut self, state: Rc<GuiSnapshot>, cx: &mut Context<Self>) {
        let format_changed = self
            .state
            .current_track()
            .map(|t| (t.id, t.sample_rate, t.channels))
            != state
                .current_track()
                .map(|t| (t.id, t.sample_rate, t.channels));
        let changed = !Arc::ptr_eq(&self.state.system.config, &state.system.config)
            || self.state.system.processing != state.system.processing
            || self.state.system.processing_audition != state.system.processing_audition
            || self.state.playback.status != state.playback.status
            || format_changed;
        self.state = state;
        self.reconcile_graph(cx);
        self.reconcile_equalizers(cx);
        if changed {
            self.sync_fields(cx);
            cx.notify();
        }
    }

    pub(super) fn cancel_pointer(&mut self, cx: &mut Context<Self>) {
        let graph_pointer = self.graph_ui.pointer.take().is_some();
        let eq_pointer = self.cancel_equalizer_pointer(cx);
        if graph_pointer || eq_pointer {
            cx.notify();
        }
    }

    pub(super) fn invalidate_geometry(&mut self, cx: &mut Context<Self>) {
        self.geometry_epoch = self.geometry_epoch.wrapping_add(1);
        self.invalidate_equalizer_geometry(cx);
        for bounds in self.graph_ui.bounds.values() {
            bounds.set(None);
        }
        self.cancel_pointer(cx);
    }

    pub(super) fn pointer_active(&self) -> bool {
        self.equalizer_pointer_active() || self.graph_ui.pointer.is_some()
    }

    pub(super) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.graph_ui.input_focused(window, cx)
            || self.equalizer_input_focused(window, cx)
            || self
                .fields
                .values()
                .any(|field| field.input.read(cx).is_focused(window))
    }

    pub(super) fn sync_panels(
        &mut self,
        layout: &super::layout::Layout,
        visible: bool,
        cx: &mut Context<Self>,
    ) {
        self.retain_equalizer_panels(layout, visible, cx);
        self.graph_ui
            .bounds
            .retain(|id, _| layout.contains_panel(*id));
        self.graph_ui.retain_panel_inputs(layout);
        if self
            .graph_ui
            .pointer
            .as_ref()
            .is_some_and(|p| !visible || !layout.is_panel_active(p.panel))
        {
            self.cancel_pointer(cx);
        }
    }

    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        self.send_field(command, None, cx);
    }

    fn send_field(
        &mut self,
        command: Command,
        submitted: Option<(Entity<Input>, String)>,
        cx: &mut Context<Self>,
    ) {
        self.enqueue_submission(
            Submission {
                command,
                field: submitted,
                equalizer_field: None,
            },
            cx,
        );
    }

    fn enqueue_submission(&mut self, submission: Submission, cx: &mut Context<Self>) {
        if self.pending >= 64 {
            self.error = Some(
                self.context
                    .i18n
                    .text("processing-command-queue-full")
                    .to_owned(),
            );
            cx.notify();
            return;
        }
        self.pending += 1;
        self.error = None;
        self.commands.push_back(submission);
        self.dispatch_next(cx);
        cx.notify();
    }

    fn dispatch_next(&mut self, cx: &mut Context<Self>) {
        if self.command_active {
            return;
        }
        let Some(Submission {
            command,
            field: submitted,
            equalizer_field,
        }) = self.commands.pop_front()
        else {
            return;
        };
        self.command_active = true;
        self.begin_equalizer_command(&command);
        let handle = self.context.handle().clone();
        let graph_command = matches!(&command, Command::ProcessingGraphEdit { .. });
        cx.spawn(async move |this, cx| {
            let ack = cx
                .background_executor()
                .spawn(async move { handle.request_ack(command) })
                .await;
            let success = ack.is_success();
            let _ = this.update(cx, |this, cx| {
                this.pending = this.pending.saturating_sub(1);
                this.command_active = false;
                if !success {
                    this.error = Some(ack.error.unwrap_or_else(|| {
                        this.context
                            .i18n
                            .text("processing-command-failed")
                            .to_owned()
                    }));
                }
                this.finish_equalizer_command(success);
                this.reconcile(this.context.snapshot(), cx);
                if graph_command {
                    this.graph_ack(success, cx);
                }
                if success
                    && let Some((input, value)) = submitted
                    && let Some(field) = this
                        .fields
                        .values_mut()
                        .find(|field| field.input.entity_id() == input.entity_id())
                    && field.input.read(cx).text().trim() == value
                {
                    field.dirty = false;
                    let committed = field.committed.clone();
                    field
                        .input
                        .update(cx, |input, cx| input.set_text(committed, cx));
                }
                if let Some(field) = equalizer_field {
                    this.finish_equalizer_field(field, success, cx);
                }
                this.dispatch_next(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn field(&mut self, key: &str, label: &str, value: String, cx: &mut Context<Self>) {
        if let Some(field) = self.fields.get_mut(key) {
            if field.committed != value {
                field.committed.clone_from(&value);
                if !field.dirty {
                    field
                        .input
                        .update(cx, |input, cx| input.set_text(value, cx));
                }
            }
            return;
        }
        let input = cx.new(|cx| Input::new(label.to_owned(), value.clone(), label.to_owned(), cx));
        let subscription = self.subscribe_field(&input, key.to_owned(), cx);
        self.fields.insert(
            key.to_owned(),
            Field {
                input,
                committed: value,
                dirty: false,
                _subscription: subscription,
            },
        );
    }

    fn subscribe_field(
        &self,
        input: &Entity<Input>,
        name: String,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe(input, move |this, _, event, cx| match event {
            InputEvent::Changed { user: true } => {
                if let Some(field) = this.fields.get_mut(&name) {
                    field.dirty = true;
                }
                cx.notify();
            }
            InputEvent::Submit => this.apply_field(&name, cx),
            _ => {}
        })
    }

    fn sync_fields(&mut self, cx: &mut Context<Self>) {
        for (prefix, value) in [
            (
                "denoise",
                serde_json::to_value(&self.state.system.config.denoise)
                    .expect("serializable denoise"),
            ),
            (
                "neural",
                serde_json::to_value(&self.state.system.config.neural)
                    .expect("serializable neural"),
            ),
        ] {
            self.sync_object_fields(prefix, &value, cx);
        }
        for (key, label, value) in [
            ("preset_name", "processing-preset-name", ""),
            ("preset_path", "processing-preset-path", ""),
            ("profile_start", "processing-profile-start", "0"),
            ("profile_end", "processing-profile-end", "1"),
            ("vocal_gain", "processing-vocal-gain", "0"),
            ("accompaniment_gain", "processing-accompaniment-gain", "0"),
        ] {
            if !self.fields.contains_key(key) {
                self.field(key, &self.tr(label), value.to_owned(), cx);
            }
        }
    }

    fn sync_object_fields(&mut self, prefix: &str, value: &Value, cx: &mut Context<Self>) {
        if let Some(object) = value.as_object() {
            for (key, value) in object {
                let path = format!("{prefix}.{key}");
                if value.is_object() {
                    self.sync_object_fields(&path, value, cx);
                } else if !value.is_boolean() && key != "mode" && key != "device" {
                    let text = match value {
                        Value::Null => String::new(),
                        Value::String(s) => s.clone(),
                        _ => value.to_string(),
                    };
                    self.field(&path, &self.parameter_path_label(prefix, key), text, cx);
                }
            }
        }
    }

    fn text(&self, key: &str, cx: &App) -> String {
        self.fields
            .get(key)
            .map(|f| f.input.read(cx).text().trim().to_owned())
            .unwrap_or_default()
    }

    fn apply_field(&mut self, key: &str, cx: &mut Context<Self>) {
        if key.starts_with("graph.") {
            self.apply_graph_field(key, cx);
            return;
        }
        let value = self.text(key, cx);
        let result = self.field_command(key, &value);
        match result {
            Ok(Some(command)) => {
                let submitted = self
                    .fields
                    .get(key)
                    .map(|field| (field.input.clone(), value));
                self.send_field(command, submitted, cx);
            }
            Ok(None) => {}
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    fn field_command(&self, key: &str, value: &str) -> Result<Option<Command>, String> {
        if let Some(path) = key.strip_prefix("denoise.") {
            let value: Value = serde_json::from_str(value).map_err(|e| e.to_string())?;
            return self.denoise_command(path, value).map(Some);
        }
        if let Some(path) = key.strip_prefix("neural.") {
            let patch = match path {
                "speech_manifest" => NeuralPatch {
                    speech_manifest: Some((!value.is_empty()).then(|| PathBuf::from(value))),
                    ..Default::default()
                },
                "separation_manifest" => NeuralPatch {
                    separation_manifest: Some((!value.is_empty()).then(|| PathBuf::from(value))),
                    ..Default::default()
                },
                _ => {
                    let value = if path == "threads" {
                        json!(value.parse::<u8>().map_err(|e| e.to_string())?)
                    } else if matches!(
                        path,
                        "speech_wet_mix" | "speech_attenuation_limit_db" | "separation_overlap"
                    ) {
                        let number = value.parse::<f32>().map_err(|e| e.to_string())?;
                        if !number.is_finite() {
                            return Err(self
                                .context
                                .i18n
                                .text("processing-invalid-number")
                                .to_owned());
                        }
                        json!(number)
                    } else {
                        json!(value)
                    };
                    serde_json::from_value(json!({(path):value})).map_err(|e| e.to_string())?
                }
            };
            return Ok(Some(Command::NeuralEdit {
                patch: Box::new(patch),
            }));
        }
        Ok(None)
    }

    fn denoise_command(&self, path: &str, value: Value) -> Result<Command, String> {
        let key = path.replace('.', "_");
        let patch: DenoisePatch =
            serde_json::from_value(json!({(key):value})).map_err(|e| e.to_string())?;
        Ok(Command::DenoiseEdit {
            patch: Box::new(patch),
        })
    }

    fn patch_denoise(&mut self, path: &str, value: Value, cx: &mut Context<Self>) {
        match self.denoise_command(path, value) {
            Ok(command) => self.send(command, cx),
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    fn patch_neural(&mut self, path: &str, value: Value, cx: &mut Context<Self>) {
        match serde_json::from_value::<NeuralPatch>(json!({(path):value})) {
            Ok(patch) => self.send(
                Command::NeuralEdit {
                    patch: Box::new(patch),
                },
                cx,
            ),
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
            }
        }
    }

    fn parameter_path_label(&self, prefix: &str, key: &str) -> String {
        let label = self.parameter_label(key);
        if let Some(group) = prefix.strip_prefix("denoise.") {
            format!("{} · {label}", self.parameter_label(group))
        } else {
            label
        }
    }

    fn parameter_label(&self, key: &str) -> String {
        let id = match key {
            "mode" => "processing-denoise-mode",
            "device" => "processing-device",
            "strength" => "processing-strength",
            "noise_floor_db" => "processing-noise-floor",
            "attack_ms" => "processing-attack",
            "release_ms" => "processing-release",
            "smoothing_bins" => "processing-smoothing",
            "transient_protection" => "processing-transients",
            "wet" => "processing-wet",
            "fft_size" => "processing-fft",
            "stationary_noise_dbfs" => "processing-stationary-floor",
            "enabled" => "processing-enabled",
            "fundamental_hz" => "processing-hum-frequency",
            "hum" => "processing-hum",
            "impulse" => "processing-impulse",
            "max_width" => "processing-impulse-width",
            "floor_dbfs" => "processing-impulse-floor",
            "speech_interpreter" => "processing-speech-interpreter",
            "separation_interpreter" => "processing-separation-interpreter",
            "speech_wet_mix" => "processing-speech-wet",
            "speech_attenuation_limit_db" => "processing-speech-attenuation",
            "speech_post_filter" => "processing-speech-post-filter",
            "separation_overlap" => "processing-separation-overlap",
            "frequency_hz" => "processing-frequency",
            "harmonics" => "processing-harmonics",
            "q" => "processing-q",
            "threshold" => "processing-threshold",
            "worker" => "processing-worker",
            "speech_manifest" => "processing-speech-manifest",
            "separation_manifest" => "processing-separation-manifest",
            "threads" => "processing-threads",
            _ => return key.replace('_', " "),
        };
        self.context.i18n.text(id).to_owned()
    }

    fn field_view(
        &self,
        key: &str,
        label: impl Into<SharedString>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let field = &self.fields[key];
        let label: SharedString = label.into();
        let persisted = key.starts_with("denoise.") || key.starts_with("neural.");
        let label: SharedString = if field.dirty && persisted {
            format!("{label} · {}", self.context.i18n.text("processing-draft")).into()
        } else {
            label
        };
        let key_owned = key.to_owned();
        let mut control = row()
            .min_w_0()
            .gap_1()
            .child(div().flex_1().min_w_0().child(field.input.clone()));
        if persisted {
            control = control.child(disabled_control(
                theme,
                button(
                    SharedString::from(format!("apply-{key}")),
                    self.tr("processing-apply"),
                    cx,
                    move |this, _, cx| this.apply_field(&key_owned, cx),
                ),
                !enabled,
            ));
        }
        FormField::new(theme, label, control)
            .into_div()
            .min_w_0()
            .opacity(if enabled { 1. } else { 0.5 })
    }

    fn preset_view(&self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let mut presets = row().flex_wrap().gap_1();
        for (index, preset) in self
            .state
            .system
            .config
            .equalizer_presets
            .iter()
            .enumerate()
        {
            let name = preset.name.clone();
            presets = presets.child(button(
                SharedString::from(format!("eq-preset-{panel}-{index}")),
                name.clone(),
                cx,
                move |this, _, cx| {
                    if let Some(field) = this.fields.get_mut("preset_name") {
                        field
                            .input
                            .update(cx, |input, cx| input.set_text(name.clone(), cx));
                        field.dirty = false;
                    }
                    this.eq(EqualizerEdit::LoadPreset { name: name.clone() }, cx);
                },
            ));
        }
        if self.state.system.config.equalizer_presets.is_empty() {
            presets = presets.child(caption(theme, self.tr("processing-preset-empty")));
        }
        column()
            .gap_2()
            .child(caption(theme, self.tr("processing-presets")).text_color(rgb(theme.text)))
            .child(presets)
            .child(self.field_view("preset_name", self.tr("processing-preset-name"), true, cx))
            .child(
                row()
                    .flex_wrap()
                    .gap_1()
                    .child(button(
                        ("save-preset", panel),
                        self.tr("processing-save-copy"),
                        cx,
                        |this, _, cx| {
                            let name = this.text("preset_name", cx);
                            this.eq(EqualizerEdit::SavePreset { name }, cx);
                        },
                    ))
                    .child(button(
                        ("load-preset", panel),
                        self.tr("processing-load"),
                        cx,
                        |this, _, cx| {
                            let name = this.text("preset_name", cx);
                            this.eq(EqualizerEdit::LoadPreset { name }, cx);
                        },
                    ))
                    .child(button(
                        ("delete-preset", panel),
                        self.tr("processing-delete"),
                        cx,
                        |this, _, cx| {
                            let name = this.text("preset_name", cx);
                            this.eq(EqualizerEdit::DeletePreset { name }, cx);
                        },
                    ))
                    .child(button(
                        ("copy-eq", panel),
                        self.tr("processing-copy"),
                        cx,
                        |this, _, cx| match serde_json::to_string_pretty(
                            &this.state.system.config.equalizer,
                        ) {
                            Ok(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                            Err(error) => {
                                this.error = Some(error.to_string());
                                cx.notify();
                            }
                        },
                    )),
            )
            .child(self.field_view("preset_path", self.tr("processing-preset-path"), true, cx))
            .child(
                row()
                    .flex_wrap()
                    .gap_1()
                    .child(button(
                        ("import-preset", panel),
                        self.tr("processing-import"),
                        cx,
                        |this, _, cx| {
                            let name = this.text("preset_name", cx);
                            let path = PathBuf::from(this.text("preset_path", cx));
                            this.eq(EqualizerEdit::ImportPreset { path, name }, cx);
                        },
                    ))
                    .child(button(
                        ("export-preset", panel),
                        self.tr("processing-export"),
                        cx,
                        |this, _, cx| {
                            let name = this.text("preset_name", cx);
                            let path = PathBuf::from(this.text("preset_path", cx));
                            this.eq(EqualizerEdit::ExportPreset { path, name }, cx);
                        },
                    )),
            )
    }

    fn object_controls(&self, prefix: &str, value: &Value, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let mut content = column().gap_2();
        if let Some(object) = value.as_object() {
            for (key, value) in object {
                let path = format!("{prefix}.{key}");
                if key == "mode" || key == "device" {
                    continue;
                }
                if value.is_object() {
                    content = content.child(
                        column()
                            .gap_2()
                            .p_2()
                            .border_1()
                            .border_color(rgb(theme.border))
                            .child(
                                caption(theme, self.parameter_label(key))
                                    .text_color(rgb(theme.text)),
                            )
                            .child(self.object_controls(&path, value, cx)),
                    );
                } else if let Some(checked) = value.as_bool() {
                    let neural = prefix == "neural";
                    let local = path
                        .strip_prefix(if neural { "neural." } else { "denoise." })
                        .unwrap_or(&path)
                        .to_owned();
                    content = content.child(checkbox(
                        SharedString::from(path),
                        self.parameter_path_label(prefix, key),
                        checked,
                        true,
                        cx,
                        move |this, value, cx| {
                            if neural {
                                this.patch_neural(&local, json!(value), cx);
                            } else {
                                this.patch_denoise(&local, json!(value), cx);
                            }
                        },
                    ));
                } else if self.fields.contains_key(&path) {
                    content = content.child(self.field_view(
                        &path,
                        self.parameter_path_label(prefix, key),
                        true,
                        cx,
                    ));
                }
            }
        }
        content
    }

    fn restoration_view(&self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let config =
            serde_json::to_value(&self.state.system.config.denoise).expect("serializable denoise");
        let mut modes = row().flex_wrap().gap_1();
        for (mode, label) in [
            ("off", "processing-off"),
            ("stationary", "processing-stationary"),
            ("profile", "processing-profile"),
            ("adaptive", "processing-adaptive"),
        ] {
            modes = modes.child(
                button(
                    SharedString::from(format!("denoise-mode-{mode}")),
                    self.tr(label),
                    cx,
                    move |this, _, cx| this.patch_denoise("mode", json!(mode), cx),
                )
                .when(config["mode"] == mode, |v| v.bg(rgb(theme.selected))),
            );
        }
        column()
            .gap_3()
            .child(caption(theme, self.tr("processing-restoration-help")))
            .child(modes)
            .child(self.object_controls("denoise", &config, cx))
            .child(self.field_view(
                "profile_start",
                self.tr("processing-profile-start"),
                true,
                cx,
            ))
            .child(self.field_view("profile_end", self.tr("processing-profile-end"), true, cx))
            .child(self.task_controls(panel, ProcessingOperation::Denoise, cx))
    }

    fn neural_view(&self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let neural =
            serde_json::to_value(&self.state.system.config.neural).expect("serializable neural");
        let mut devices = row().flex_wrap().gap_1();
        for (device, label) in [("cpu", "CPU"), ("cuda", "CUDA")] {
            devices = devices.child(
                button(
                    SharedString::from(format!("neural-device-{device}")),
                    label,
                    cx,
                    move |this, _, cx| match serde_json::from_value::<NeuralPatch>(
                        json!({"device":device}),
                    ) {
                        Ok(patch) => this.send(
                            Command::NeuralEdit {
                                patch: Box::new(patch),
                            },
                            cx,
                        ),
                        Err(error) => {
                            this.error = Some(error.to_string());
                            cx.notify();
                        }
                    },
                )
                .when(neural["device"] == device, |v| v.bg(rgb(theme.selected))),
            );
        }
        column()
            .gap_3()
            .child(caption(theme, self.tr("processing-neural-help")))
            .child(self.object_controls("neural", &neural, cx))
            .child(devices)
            .child(caption(theme, self.tr("processing-speech")).text_color(rgb(theme.text)))
            .child(self.task_controls(panel, ProcessingOperation::Speech, cx))
            .child(caption(theme, self.tr("processing-separation")).text_color(rgb(theme.text)))
            .child(self.task_controls(panel, ProcessingOperation::Separate, cx))
    }

    fn input_track(&self) -> Option<&Track> {
        if self.use_selected {
            self.selected.as_deref()
        } else {
            self.state.current_track()
        }
    }

    fn task_controls(
        &self,
        panel: u64,
        operation: ProcessingOperation,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let busy = self.state.system.processing.as_ref().is_some_and(|t| {
            matches!(
                t.phase,
                ProcessingPhase::Queued | ProcessingPhase::Rendering | ProcessingPhase::Processing
            )
        });
        let supported = self.input_track().is_some_and(|t| match operation {
            ProcessingOperation::Separate => t.channels == 2,
            ProcessingOperation::Speech => (1..=2).contains(&t.channels),
            ProcessingOperation::Denoise => (1..=32).contains(&t.channels),
            ProcessingOperation::Graph => true,
        });
        let model_ready = match operation {
            ProcessingOperation::Speech => {
                self.state.system.config.neural.speech_manifest.is_some()
            }
            ProcessingOperation::Separate => self
                .state
                .system
                .config
                .neural
                .separation_manifest
                .is_some(),
            ProcessingOperation::Denoise => true,
            ProcessingOperation::Graph => true,
        };
        let mut content = column().gap_2().child(disabled_control(
            theme,
            button(
                SharedString::from(format!("start-processing-{panel}-{operation:?}")),
                self.tr("processing-start"),
                cx,
                move |this, _, cx| this.start(operation, cx),
            ),
            busy || !supported || !model_ready || self.pending > 0,
        ));
        if !supported {
            content = content.child(
                caption(theme, self.tr("processing-input-unsupported"))
                    .text_color(rgb(theme.muted)),
            );
        }
        if !model_ready {
            content = content.child(
                caption(theme, self.tr("processing-model-required")).text_color(rgb(theme.muted)),
            );
        }
        content
    }

    fn start(&mut self, operation: ProcessingOperation, cx: &mut Context<Self>) {
        let Some(track) = self.input_track() else {
            return;
        };
        let track_id = track.id;
        let profile = if operation == ProcessingOperation::Denoise
            && serde_json::to_value(&self.state.system.config.denoise)
                .ok()
                .is_some_and(|v| v["mode"] == "profile")
        {
            let range = (
                self.text("profile_start", cx).parse::<f64>(),
                self.text("profile_end", cx).parse::<f64>(),
            );
            match range {
                (Ok(start_seconds), Ok(end_seconds))
                    if start_seconds.is_finite()
                        && end_seconds.is_finite()
                        && start_seconds >= 0.
                        && end_seconds > start_seconds =>
                {
                    Some(NoiseSelection {
                        start_seconds,
                        end_seconds,
                    })
                }
                _ => {
                    self.error = Some(
                        self.context
                            .i18n
                            .text("processing-profile-invalid")
                            .to_owned(),
                    );
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        self.send(
            Command::StartProcessing {
                track_id,
                operation,
                profile,
            },
            cx,
        );
    }

    fn captured_parameters(&self, prefix: &str, value: &Value, cx: &App) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let mut content = column().min_w_0().gap_1();
        if let Some(object) = value.as_object() {
            for (key, value) in object {
                if value.is_object() {
                    content = content.child(self.captured_parameters(
                        &format!("{prefix}.{key}"),
                        value,
                        cx,
                    ));
                    continue;
                }
                let label = self.parameter_path_label(prefix, key);
                let text = match value {
                    Value::Null => "—".to_owned(),
                    Value::Bool(value) => self
                        .context
                        .i18n
                        .text(if *value {
                            "processing-enabled"
                        } else {
                            "processing-off"
                        })
                        .to_owned(),
                    Value::String(value) if key == "mode" => self
                        .context
                        .i18n
                        .text(match value.as_str() {
                            "off" => "processing-off",
                            "stationary" => "processing-stationary",
                            "profile" => "processing-profile",
                            "adaptive" => "processing-adaptive",
                            _ => unreachable!("serialized denoise mode"),
                        })
                        .to_owned(),
                    Value::String(value) => value.clone(),
                    _ => value.to_string(),
                };
                content = content.child(caption(theme, format!("{label}: {text}")));
            }
        }
        content
    }

    fn task_settings(&self, task: &crate::model::ProcessingTaskStatus, cx: &App) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let neural = &task.settings.neural;
        let parameters = match task.operation {
            ProcessingOperation::Denoise => {
                serde_json::to_value(&task.settings.denoise).expect("serializable captured denoise")
            }
            ProcessingOperation::Speech => json!({
                "speech_interpreter":neural.speech_interpreter,"worker":neural.worker,
                "speech_manifest":neural.speech_manifest,"device":neural.device,"threads":neural.threads,
                "speech_wet_mix":neural.speech_wet_mix,"speech_attenuation_limit_db":neural.speech_attenuation_limit_db,
                "speech_post_filter":neural.speech_post_filter,
            }),
            ProcessingOperation::Separate => json!({
                "separation_interpreter":neural.separation_interpreter,"worker":neural.worker,
                "separation_manifest":neural.separation_manifest,"device":neural.device,"threads":neural.threads,
                "separation_overlap":neural.separation_overlap,
            }),
            ProcessingOperation::Graph => {
                serde_json::to_value(&task.settings.graph).expect("serializable graph snapshot")
            }
        };
        let mut content = column()
            .min_w_0()
            .gap_2()
            .p_2()
            .border_1()
            .border_color(rgb(theme.border))
            .child(
                caption(theme, self.tr("processing-captured-settings")).text_color(rgb(theme.text)),
            )
            .child(self.captured_parameters(
                if task.operation == ProcessingOperation::Denoise {
                    "denoise"
                } else {
                    "neural"
                },
                &parameters,
                cx,
            ));
        if let Some(profile) = task.settings.profile {
            content = content.child(caption(
                theme,
                format!(
                    "{}: {} · {}: {}",
                    self.context.i18n.text("processing-profile-start"),
                    profile.start_seconds,
                    self.context.i18n.text("processing-profile-end"),
                    profile.end_seconds
                ),
            ));
        }
        content
    }

    fn task_status(&self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let mut content = column().gap_2();
        if let Some(task) = &self.state.system.processing {
            let id = task.id;
            let terminal = matches!(
                task.phase,
                ProcessingPhase::Complete | ProcessingPhase::Cancelled | ProcessingPhase::Failed
            );
            let phase = self.tr(match task.phase {
                ProcessingPhase::Queued => "processing-queued",
                ProcessingPhase::Rendering => "processing-rendering",
                ProcessingPhase::Processing => "processing-processing",
                ProcessingPhase::Complete => "processing-complete",
                ProcessingPhase::Cancelled => "processing-cancelled",
                ProcessingPhase::Failed => "processing-failed",
            });
            let operation = self.tr(match task.operation {
                ProcessingOperation::Denoise => "processing-restoration",
                ProcessingOperation::Speech => "processing-speech",
                ProcessingOperation::Separate => "processing-separation",
                ProcessingOperation::Graph => "processing-routing-graph",
            });
            content = content.child(caption(
                theme,
                format!(
                    "{} #{} · {} · {} {} · {}",
                    self.context.i18n.text("processing-task"),
                    id,
                    operation,
                    self.context.i18n.text("processing-source"),
                    task.track_id,
                    phase
                ),
            ));
            content = content.child(caption(
                theme,
                format!(
                    "{}: {} / {}",
                    self.context.i18n.text("processing-progress"),
                    task.completed,
                    task.total
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "?".to_owned())
                ),
            ));
            if let Some(total) = task.total.filter(|n| *n > 0) {
                content = content.child(
                    rivu_ui::ProgressBar::new(
                        ("processing-progress", panel),
                        (task.completed as f64 / total as f64) as f32,
                        self.tr("processing-progress"),
                    )
                    .render(cx),
                );
            }
            if let Some(model) = &task.model_id {
                content = content.child(caption(
                    theme,
                    format!(
                        "{} · {}",
                        model,
                        task.model_sha256.as_deref().unwrap_or("?")
                    ),
                ));
            }
            if let (Some(rate), Some(channels), Some(frames)) =
                (task.sample_rate, task.channels, task.frames)
            {
                content = content.child(caption(
                    theme,
                    format!("{rate} Hz · {channels} ch · {frames} frames"),
                ));
            }
            content = content.child(self.task_settings(task, cx));
            if !terminal {
                content = content.child(button(
                    ("cancel-processing", panel),
                    self.tr("processing-cancel"),
                    cx,
                    move |this, _, cx| this.send(Command::CancelProcessing { id }, cx),
                ));
            }
            if let Some(error) = &task.error {
                content = content.child(caption(theme, error.clone()).text_color(rgb(theme.error)));
            }
            if task.phase == ProcessingPhase::Complete && task.outputs.is_some() {
                content = content.child(caption(theme, self.tr("processing-audition-source")));
                let mut auditions = row().flex_wrap().gap_1();
                let modes = if task.operation == ProcessingOperation::Separate {
                    vec![
                        (AuditionMode::Original, "processing-original"),
                        (AuditionMode::Vocals, "processing-vocals"),
                        (AuditionMode::Accompaniment, "processing-accompaniment"),
                        (AuditionMode::Mix, "processing-mix"),
                    ]
                } else {
                    vec![
                        (AuditionMode::Original, "processing-original"),
                        (AuditionMode::Denoised, "processing-processed"),
                    ]
                };
                for (mode, label) in modes {
                    auditions = auditions.child(button(
                        SharedString::from(format!("audition-{panel}-{mode:?}")),
                        self.tr(label),
                        cx,
                        move |this, _, cx| this.audition(id, mode, cx),
                    ));
                }
                content = content.child(auditions);
                if task.operation == ProcessingOperation::Separate {
                    content = content
                        .child(self.field_view(
                            "vocal_gain",
                            self.tr("processing-vocal-gain"),
                            true,
                            cx,
                        ))
                        .child(self.field_view(
                            "accompaniment_gain",
                            self.tr("processing-accompaniment-gain"),
                            true,
                            cx,
                        ))
                        .child(button(
                            ("apply-mix-gains", panel),
                            self.tr("processing-apply-mix"),
                            cx,
                            move |this, _, cx| this.audition(id, AuditionMode::Mix, cx),
                        ));
                }
            }
        }
        content
    }

    fn audition(&mut self, id: u64, mode: AuditionMode, cx: &mut Context<Self>) {
        let gains = match mode {
            AuditionMode::Original | AuditionMode::Denoised => (Ok(0.), Ok(0.)),
            AuditionMode::Vocals => (self.text("vocal_gain", cx).parse::<f32>(), Ok(0.)),
            AuditionMode::Accompaniment => {
                (Ok(0.), self.text("accompaniment_gain", cx).parse::<f32>())
            }
            AuditionMode::Mix => (
                self.text("vocal_gain", cx).parse::<f32>(),
                self.text("accompaniment_gain", cx).parse::<f32>(),
            ),
        };
        match gains {
            (Ok(vocal_gain_db), Ok(accompaniment_gain_db))
                if vocal_gain_db.is_finite() && accompaniment_gain_db.is_finite() =>
            {
                self.send(
                    Command::AuditionProcessing {
                        id,
                        mode,
                        vocal_gain_db,
                        accompaniment_gain_db,
                    },
                    cx,
                );
            }
            _ => {
                self.error = Some(
                    self.context
                        .i18n
                        .text("processing-invalid-number")
                        .to_owned(),
                );
                cx.notify();
            }
        }
    }

    pub(super) fn render_panel(
        &mut self,
        panel: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let mut tabs = row().flex_wrap().gap_1();
        for (page, label) in [
            (Page::Graph, "processing-routing-graph"),
            (Page::Equalizer, "processing-workspace-monitor-eq"),
            (Page::Restoration, "processing-restoration"),
            (Page::Neural, "processing-neural"),
        ] {
            tabs = tabs.child(
                button(
                    SharedString::from(format!("processing-page-{panel}-{label}")),
                    self.tr(label),
                    cx,
                    move |this, _, cx| {
                        this.cancel_pointer(cx);
                        this.page = page;
                        cx.notify();
                    },
                )
                .when(self.page == page, |v| v.bg(rgb(theme.selected))),
            );
        }
        let input = self
            .input_track()
            .map(|t| format!("{} · {} Hz · {} ch", t.title, t.sample_rate, t.channels))
            .unwrap_or_else(|| self.context.i18n.text("processing-no-input").to_owned());
        let source = row()
            .flex_wrap()
            .gap_1()
            .child(
                button(
                    ("processing-current", panel),
                    self.tr("processing-current"),
                    cx,
                    |this, _, cx| {
                        this.use_selected = false;
                        cx.notify();
                    },
                )
                .when(!self.use_selected, |v| v.bg(rgb(theme.selected))),
            )
            .child(
                button(
                    ("processing-selected", panel),
                    self.tr("processing-selected"),
                    cx,
                    |this, _, cx| {
                        this.use_selected = true;
                        cx.notify();
                    },
                )
                .when(self.use_selected, |v| v.bg(rgb(theme.selected))),
            )
            .child(caption(theme, input).flex_1().min_w_0().truncate())
            .child(caption(
                theme,
                self.tr(if self.state.system.config.equalizer.enabled {
                    "processing-workspace-monitor-on"
                } else {
                    "processing-workspace-monitor-off"
                }),
            ));
        let transport = self.tr(match self.state.playback.status {
            crate::model::PlaybackStatus::Playing => "processing-workspace-playing",
            crate::model::PlaybackStatus::Paused => "processing-workspace-paused",
            crate::model::PlaybackStatus::Stopped => "processing-workspace-stopped",
        });
        let selected_audio = match self.state.system.processing_audition {
            Some(audition) => {
                let target = audition
                    .output
                    .map(|port| format!(" · #{}:{}", port.node, port.port))
                    .unwrap_or_default();
                let mode = audition
                    .mode
                    .map(|mode| {
                        self.tr(match mode {
                            crate::model::GraphExecutionMode::Auto => "processing-graph-auto",
                            crate::model::GraphExecutionMode::Realtime => {
                                "processing-graph-realtime"
                            }
                            crate::model::GraphExecutionMode::Hybrid => "processing-graph-hybrid",
                            crate::model::GraphExecutionMode::Offline => "processing-graph-cached",
                        })
                    })
                    .unwrap_or_else(|| self.tr("processing-workspace-processed"));
                format!(
                    "{} #{}{target} · {mode}",
                    self.tr("processing-task"),
                    audition.id
                )
            }
            None => self.tr("processing-workspace-original").to_string(),
        };
        let selected_audio = caption(
            theme,
            format!(
                "{}: {} · {selected_audio} · {transport}",
                self.tr("processing-workspace-selected-audio"),
                self.state
                    .current_track()
                    .map(|track| track.title.as_str())
                    .unwrap_or("—")
            ),
        )
        .min_w_0()
        .truncate();
        let mut selected_audio = row()
            .min_w_0()
            .gap_2()
            .items_center()
            .child(selected_audio.flex_1());
        if let Some(audition) = self.state.system.processing_audition {
            let id = audition.id;
            selected_audio = selected_audio.child(button(
                ("processing-original-source", panel),
                self.tr("processing-workspace-restore-original"),
                cx,
                move |this, _, cx| this.send(Command::CancelProcessing { id }, cx),
            ));
        }
        if self.page != Page::Graph {
            self.deactivate_graph_inputs(panel, window, cx);
        }
        if matches!(self.page, Page::Restoration | Page::Neural) {
            self.deactivate_equalizer_panel(panel, window, cx);
        }
        let body = match self.page {
            Page::Graph => self
                .routing_graph_view(panel, window, cx)
                .into_any_element(),
            page => {
                let body = match page {
                    Page::Equalizer => self.equalizer_view(panel, window, cx),
                    Page::Restoration => self.restoration_view(panel, cx),
                    Page::Neural => self.neural_view(panel, cx),
                    Page::Graph => unreachable!(),
                };
                let body = if self
                    .state
                    .system
                    .processing
                    .as_ref()
                    .is_some_and(|task| task.operation != ProcessingOperation::Graph)
                {
                    body.child(self.task_status(panel, cx))
                } else {
                    body
                };
                self.processing_scroll(panel, "processing-page-body", body.p_2())
            }
        };
        let mut footer = if self.page == Page::Graph {
            self.graph_execution_bar(panel, cx)
        } else {
            self.graph_task_summary(panel, cx)
        };
        if self.fields.iter().any(|(key, field)| {
            field.dirty && (key.starts_with("denoise.") || key.starts_with("neural."))
        }) {
            footer = footer.child(caption(theme, self.tr("processing-pending-edits")));
        }
        if self.pending > 0 {
            footer = footer.child(caption(theme, self.tr("processing-applying")));
        }
        if let Some(error) = &self.error {
            footer = footer.child(caption(theme, error.clone()).text_color(rgb(theme.error)));
        }
        column()
            .relative()
            .size_full()
            .min_h_0()
            .min_w_0()
            .p_2()
            .gap_2()
            .child(self.processing_workbench_measure(panel, cx))
            .child(tabs.flex_shrink_0())
            .child(source.flex_shrink_0())
            .child(selected_audio.flex_shrink_0())
            .child(div().flex_1().min_h_0().min_w_0().child(body))
            .child(footer.flex_shrink_0())
            .into_any_element()
    }
}
