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
    IconButton, IconButtonDensity, IconButtonStyle, Popover, activate_control, button, caption,
    checkbox, column, disabled_control, dropdown_trigger, icon,
    input::{Input, InputEvent},
    menu_item, row, scrollable_area,
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
pub(super) enum Page {
    Graph,
    Equalizer,
    Restoration,
    Speech,
    Separation,
    Results,
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
    page: Page,
    fields: HashMap<String, Field>,
    error: Option<String>,
    pending: usize,
    commands: VecDeque<Submission>,
    command_active: bool,
    geometry_epoch: u64,
    graph_ui: GraphUi,
    equalizer_ui: EqualizerUi,
    speech_advanced: bool,
    separation_advanced: bool,
    preset_open: bool,
    preset_manage: bool,
    preset_bounds: HashMap<u64, Rc<Cell<Option<Bounds<Pixels>>>>>,
    task_details: bool,
}

impl AudioProcessing {
    pub(super) fn new(context: FeatureContext, cx: &mut Context<Self>) -> Self {
        let state = context.snapshot();
        let mut this = Self {
            context,
            state,
            page: Page::Graph,
            fields: HashMap::new(),
            error: None,
            pending: 0,
            commands: VecDeque::with_capacity(64),
            command_active: false,
            geometry_epoch: 0,
            graph_ui: GraphUi::default(),
            equalizer_ui: EqualizerUi::default(),
            speech_advanced: false,
            separation_advanced: false,
            preset_open: false,
            preset_manage: false,
            preset_bounds: HashMap::new(),
            task_details: false,
        };
        this.sync_fields(cx);
        this
    }

    fn tr(&self, id: &str) -> SharedString {
        self.context.tr(id)
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
        if self.state.system.processing.as_ref().map(|task| task.id)
            != state.system.processing.as_ref().map(|task| task.id)
        {
            self.task_details = false;
        }
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
        let workbench_pointer = self.workbench_pointer_active();
        self.cancel_workbench_resize();
        if graph_pointer || eq_pointer || workbench_pointer {
            cx.notify();
        }
    }

    pub(super) fn invalidate_geometry(&mut self, cx: &mut Context<Self>) {
        self.geometry_epoch = self.geometry_epoch.wrapping_add(1);
        self.invalidate_equalizer_geometry(cx);
        for bounds in self.graph_ui.bounds.values() {
            bounds.set(None);
        }
        for bounds in self.preset_bounds.values() {
            bounds.set(None);
        }
        self.preset_open = false;
        self.cancel_pointer(cx);
    }

    pub(super) fn pointer_active(&self) -> bool {
        self.equalizer_pointer_active()
            || self.graph_ui.pointer.is_some()
            || self.workbench_pointer_active()
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
        self.preset_bounds
            .retain(|id, _| layout.contains_panel(*id));
        if !visible {
            self.preset_open = false;
            self.preset_manage = false;
            self.cancel_pointer(cx);
        }
        self.graph_ui
            .bounds
            .retain(|id, _| layout.contains_panel(*id));
        self.graph_ui.retain_panel_inputs(layout);
        self.retain_workbench_controls(layout, visible);
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

    fn path_placeholder(&self, key: &str) -> Option<SharedString> {
        let id = match key.rsplit(['.', '/']).next()? {
            "speech_manifest" | "separation_manifest" => "processing-manifest-placeholder",
            "speech_interpreter" | "separation_interpreter" => "processing-interpreter-placeholder",
            "worker" => "processing-worker-placeholder",
            "preset_path" => "processing-preset-path-placeholder",
            "graph_export" => "processing-export-path-placeholder",
            _ => return None,
        };
        Some(self.tr(id))
    }

    fn path_help(&self, key: &str) -> Option<SharedString> {
        let id = match key.rsplit(['.', '/']).next()? {
            "speech_manifest" | "separation_manifest" => "processing-manifest-path-help",
            "speech_interpreter" => "processing-speech-interpreter-help",
            "separation_interpreter" => "processing-separation-interpreter-help",
            "worker" => "processing-worker-path-help",
            _ => return None,
        };
        Some(self.tr(id))
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
        let placeholder = self
            .path_placeholder(key)
            .unwrap_or_else(|| label.to_owned().into());
        let input = cx.new(|cx| Input::new(label.to_owned(), value.clone(), placeholder, cx));
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
        let control = row()
            .min_w_0()
            .items_center()
            .gap_2()
            .child(
                caption(theme, label.clone())
                    .w(relative(0.42))
                    .min_w_0()
                    .truncate()
                    .id(SharedString::from(format!("field-label-{key}")))
                    .tooltip(rivu_ui::text_tooltip(self.path_help(key).unwrap_or(label))),
            )
            .child(div().flex_1().min_w_0().child(field.input.clone()));
        control.opacity(if enabled { 1. } else { 0.5 })
    }

    fn preset_view(&mut self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let enabled = self.state.system.config.equalizer.enabled;
        if !enabled {
            self.preset_open = false;
            self.preset_manage = false;
        }
        for key in ["preset_name", "preset_path"] {
            if let Some(field) = self.fields.get(key) {
                field
                    .input
                    .update(cx, |input, cx| input.set_enabled(enabled, cx));
            }
        }
        let bounds = self
            .preset_bounds
            .entry(panel)
            .or_insert_with(|| Rc::new(Cell::new(None)))
            .clone();
        let measure = bounds.clone();
        let name = self.text("preset_name", cx);
        let label = if name.is_empty() {
            self.tr("processing-presets")
        } else {
            name.into()
        };
        let trigger = activate_control(
            dropdown_trigger(theme, ("eq-presets", panel), label, self.preset_open, cx),
            cx,
            |this, _, cx| {
                this.preset_open = !this.preset_open;
                this.preset_manage = false;
                cx.notify();
            },
        );
        let mut content = column().min_w_0().gap_1().child(
            row()
                .relative()
                .min_w_0()
                .w_auto()
                .max_w_full()
                .gap_1()
                .child(
                    canvas(
                        move |bounds, _, _| measure.set(Some(bounds)),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .child(div().w(rems(6.)).min_w_0().child(trigger))
                .child(
                    IconButton::new(
                        ("manage-presets", panel),
                        "󰒓",
                        self.tr("processing-preset-manage"),
                    )
                    .toggled(self.preset_manage)
                    .build(
                        IconButtonStyle::Quiet,
                        IconButtonDensity::Compact,
                        cx,
                        |this, _, cx| {
                            this.preset_manage = !this.preset_manage;
                            this.preset_open = false;
                            cx.notify();
                        },
                    ),
                ),
        );
        if self.preset_open
            && let Some(bounds) = bounds.get()
        {
            let mut choices = Popover::surface(theme, ("eq-preset-choices", panel))
                .role(Role::Menu)
                .flex()
                .flex_col()
                .max_h(rems(12.))
                .overflow_y_scroll()
                .p_1()
                .w(bounds.size.width)
                .on_mouse_down_out(cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    if !bounds.contains(&event.position) {
                        this.preset_open = false;
                        cx.notify();
                    }
                }))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" {
                        this.preset_open = false;
                        cx.notify();
                    }
                }));
            for (index, preset) in self
                .state
                .system
                .config
                .equalizer_presets
                .iter()
                .enumerate()
            {
                let name = preset.name.clone();
                choices = choices.child(menu_item(
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
                        this.preset_open = false;
                        this.eq(EqualizerEdit::LoadPreset { name: name.clone() }, cx);
                    },
                ));
            }
            if self.state.system.config.equalizer_presets.is_empty() {
                choices = choices.child(caption(theme, self.tr("processing-preset-empty")));
            }
            content = content.child(
                deferred(
                    anchored()
                        .position(bounds.bottom_left())
                        .snap_to_window()
                        .child(choices),
                )
                .with_priority(2),
            );
        }
        if self.preset_manage
            && let Some(bounds) = bounds.get()
        {
            content = content.child(
                deferred(
                    anchored()
                        .position(bounds.bottom_left())
                        .snap_to_window()
                        .child(
                            Popover::surface(theme, ("eq-preset-management", panel))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .p_2()
                                .w(rems(24.))
                                .max_h(rems(24.))
                                .overflow_y_scroll()
                                .on_mouse_down_out(cx.listener(
                                    move |this, event: &MouseDownEvent, _, cx| {
                                        if !bounds.contains(&event.position) {
                                            this.preset_manage = false;
                                            cx.notify();
                                        }
                                    },
                                ))
                                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                    if event.keystroke.key == "escape" {
                                        this.preset_manage = false;
                                        cx.notify();
                                    }
                                }))
                                .child(self.field_view(
                                    "preset_name",
                                    self.tr("processing-preset-name"),
                                    true,
                                    cx,
                                ))
                                .child(menu_item(
                                    ("save-preset", panel),
                                    self.tr("processing-save-copy"),
                                    cx,
                                    |this, _, cx| {
                                        this.eq(
                                            EqualizerEdit::SavePreset {
                                                name: this.text("preset_name", cx),
                                            },
                                            cx,
                                        );
                                    },
                                ))
                                .child(menu_item(
                                    ("load-preset", panel),
                                    self.tr("processing-load"),
                                    cx,
                                    |this, _, cx| {
                                        this.eq(
                                            EqualizerEdit::LoadPreset {
                                                name: this.text("preset_name", cx),
                                            },
                                            cx,
                                        );
                                    },
                                ))
                                .child(menu_item(
                                    ("delete-preset", panel),
                                    self.tr("processing-delete"),
                                    cx,
                                    |this, _, cx| {
                                        this.eq(
                                            EqualizerEdit::DeletePreset {
                                                name: this.text("preset_name", cx),
                                            },
                                            cx,
                                        );
                                    },
                                ))
                                .child(menu_item(
                                    ("copy-eq", panel),
                                    self.tr("processing-copy"),
                                    cx,
                                    |this, _, cx| match serde_json::to_string_pretty(
                                        &this.state.system.config.equalizer,
                                    ) {
                                        Ok(text) => {
                                            cx.write_to_clipboard(ClipboardItem::new_string(text))
                                        }
                                        Err(error) => {
                                            this.error = Some(error.to_string());
                                            cx.notify();
                                        }
                                    },
                                ))
                                .child(self.field_view(
                                    "preset_path",
                                    self.tr("processing-preset-path"),
                                    true,
                                    cx,
                                ))
                                .child(menu_item(
                                    ("import-preset", panel),
                                    self.tr("processing-import"),
                                    cx,
                                    |this, _, cx| {
                                        this.eq(
                                            EqualizerEdit::ImportPreset {
                                                name: this.text("preset_name", cx),
                                                path: PathBuf::from(this.text("preset_path", cx)),
                                            },
                                            cx,
                                        );
                                    },
                                ))
                                .child(menu_item(
                                    ("export-preset", panel),
                                    self.tr("processing-export"),
                                    cx,
                                    |this, _, cx| {
                                        this.eq(
                                            EqualizerEdit::ExportPreset {
                                                name: this.text("preset_name", cx),
                                                path: PathBuf::from(this.text("preset_path", cx)),
                                            },
                                            cx,
                                        );
                                    },
                                )),
                        ),
                )
                .with_priority(2),
            );
        }
        div().child(disabled_control(
            theme,
            content
                .id(("eq-presets-disabled", panel))
                .opacity(if enabled { 1. } else { 0.5 }),
            !enabled,
        ))
    }

    fn parameter_controls(
        &self,
        prefix: &str,
        keys: &[&str],
        value: &Value,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut content = column().min_w_0().gap_1();
        for &key in keys {
            let path = format!("{prefix}.{key}");
            if let Some(checked) = value[key].as_bool() {
                let neural = prefix == "neural";
                let local = path
                    .strip_prefix(if neural { "neural." } else { "denoise." })
                    .unwrap_or(&path)
                    .to_owned();
                content = content.child(checkbox(
                    SharedString::from(path),
                    self.parameter_label(key),
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
                content =
                    content.child(self.field_view(&path, self.parameter_label(key), true, cx));
            }
        }
        content
    }

    fn restoration_view(&self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let config =
            serde_json::to_value(&self.state.system.config.denoise).expect("serializable denoise");
        let mut modes = row().gap_1();
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
                .flex_shrink_0()
                .whitespace_nowrap()
                .when(config["mode"] == mode, |v| v.bg(rgb(theme.selected))),
            );
        }
        let mut content = column()
            .min_w_0()
            .gap_2()
            .child(caption(theme, self.tr("processing-denoise-mode")))
            .child(
                row()
                    .min_w_0()
                    .id(("denoise-modes", panel))
                    .overflow_x_scroll()
                    .child(modes),
            );
        if config["mode"] != "off" {
            content = content.child(self.parameter_controls(
                "denoise",
                &[
                    "strength",
                    "noise_floor_db",
                    "attack_ms",
                    "release_ms",
                    "smoothing_bins",
                    "transient_protection",
                    "fft_size",
                ],
                &config,
                cx,
            ));
        }
        if config["mode"] == "stationary" || config["mode"] == "adaptive" {
            content = content.child(self.parameter_controls(
                "denoise",
                &["stationary_noise_dbfs"],
                &config,
                cx,
            ));
        }
        if config["mode"] == "profile" {
            content = content
                .child(self.field_view(
                    "profile_start",
                    self.tr("processing-profile-start"),
                    true,
                    cx,
                ))
                .child(self.field_view("profile_end", self.tr("processing-profile-end"), true, cx));
        }
        let mut content = column().min_w_0().gap_3().child(rivu_ui::section_group(
            theme,
            self.tr("processing-noise-reduction"),
            content,
        ));
        for (group, keys) in [
            ("hum", &["fundamental_hz", "harmonics", "q"][..]),
            (
                "impulse",
                &[
                    "max_width",
                    "threshold",
                    "floor_dbfs",
                    "transient_protection",
                ][..],
            ),
        ] {
            let enabled = config[group]["enabled"] == true;
            let prefix = format!("denoise.{group}");
            let enabled_key = format!("{group}.enabled");
            let mut section = column()
                .min_w_0()
                .gap_1()
                .pt_2()
                .border_t_1()
                .border_color(rgb(theme.border))
                .child(checkbox(
                    SharedString::from(format!("restoration-enable-{group}-{panel}")),
                    self.parameter_label(group),
                    enabled,
                    true,
                    cx,
                    move |this, enabled, cx| {
                        this.patch_denoise(&enabled_key, json!(enabled), cx);
                    },
                ));
            if enabled {
                section = section.child(self.parameter_controls(&prefix, keys, &config[group], cx));
            }
            content = content.child(section);
        }
        content.child(
            div()
                .pt_2()
                .border_t_1()
                .border_color(rgb(theme.border))
                .child(self.parameter_controls("denoise", &["wet"], &config, cx)),
        )
    }

    fn neural_view(&self, panel: u64, speech: bool, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let neural =
            serde_json::to_value(&self.state.system.config.neural).expect("serializable neural");
        let open = if speech {
            self.speech_advanced
        } else {
            self.separation_advanced
        };
        let parameters = self.parameter_controls(
            "neural",
            if speech {
                &[
                    "speech_wet_mix",
                    "speech_attenuation_limit_db",
                    "speech_post_filter",
                ][..]
            } else {
                &["separation_overlap"][..]
            },
            &neural,
            cx,
        );
        let model = self.parameter_controls(
            "neural",
            if speech {
                &["speech_manifest", "speech_interpreter"][..]
            } else {
                &["separation_manifest", "separation_interpreter"][..]
            },
            &neural,
            cx,
        );
        let mut content = column()
            .min_w_0()
            .gap_3()
            .child(rivu_ui::section_group(
                theme,
                self.tr("processing-parameters"),
                parameters,
            ))
            .child(rivu_ui::section_group(
                theme,
                self.tr("processing-model-runtime"),
                model,
            ))
            .child(activate_control(
                dropdown_trigger(
                    theme,
                    ("neural-advanced", panel),
                    self.tr("processing-runtime-options"),
                    open,
                    cx,
                ),
                cx,
                move |this, _, cx| {
                    if speech {
                        this.speech_advanced = !this.speech_advanced;
                    } else {
                        this.separation_advanced = !this.separation_advanced;
                    }
                    cx.notify();
                },
            ));
        if open {
            content = content.child(self.parameter_controls(
                "neural",
                &["worker", "threads"],
                &neural,
                cx,
            ));
            let mut devices = row()
                .gap_1()
                .child(caption(theme, self.tr("processing-device")));
            for (device, label) in [("cpu", "CPU"), ("cuda", "CUDA")] {
                devices = devices.child(
                    button(
                        SharedString::from(format!("neural-device-{device}")),
                        label,
                        cx,
                        move |this, _, cx| this.patch_neural("device", json!(device), cx),
                    )
                    .when(neural["device"] == device, |v| v.bg(rgb(theme.selected))),
                );
            }
            content = content.child(devices);
        }
        content
    }

    fn input_track(&self) -> Option<&Track> {
        self.state.current_track()
    }

    fn page_parameter_key(&self, key: &str) -> bool {
        match self.page {
            Page::Restoration => key.starts_with("denoise."),
            Page::Speech => {
                key.starts_with("neural.speech_")
                    || matches!(key, "neural.worker" | "neural.threads")
            }
            Page::Separation => {
                key.starts_with("neural.separation_")
                    || matches!(key, "neural.worker" | "neural.threads")
            }
            _ => false,
        }
    }

    fn page_has_parameter_drafts(&self) -> bool {
        self.fields
            .iter()
            .any(|(key, field)| field.dirty && self.page_parameter_key(key))
    }

    fn apply_page_parameters(&mut self, cx: &mut Context<Self>) {
        let mut submissions = Vec::new();
        for (key, field) in &self.fields {
            if !field.dirty || !self.page_parameter_key(key) {
                continue;
            }
            let value = field.input.read(cx).text().trim().to_owned();
            match self.field_command(key, &value) {
                Ok(Some(command)) => {
                    submissions.push((key.clone(), command, field.input.clone(), value))
                }
                Ok(None) => {}
                Err(error) => {
                    self.error = Some(error);
                    cx.notify();
                    return;
                }
            }
        }
        if self.pending + submissions.len() > 64 {
            self.error = Some(self.tr("processing-command-queue-full").to_string());
            cx.notify();
            return;
        }
        submissions.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (_, command, input, value) in submissions {
            self.send_field(command, Some((input, value)), cx);
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
        let drafts = self.page_has_parameter_drafts();
        let mut actions = row().gap_1().flex_shrink_0();
        if drafts {
            actions = actions.child(disabled_control(
                theme,
                button(
                    ("apply-processing-parameters", panel),
                    self.tr("processing-apply-changes"),
                    cx,
                    |this, _, cx| this.apply_page_parameters(cx),
                ),
                self.pending > 0,
            ));
        }
        actions = actions.child(disabled_control(
            theme,
            button(
                SharedString::from(format!("start-processing-{panel}-{operation:?}")),
                self.tr("processing-start"),
                cx,
                move |this, _, cx| this.start(operation, cx),
            )
            .gap_1()
            .child(icon("", cx))
            .bg(rgb(theme.selected))
            .text_color(rgb(theme.accent)),
            busy || !supported || !model_ready || self.pending > 0 || drafts,
        ));
        let mut content = column().gap_1().child(actions);
        if !supported {
            content = content.child(
                caption(
                    theme,
                    self.tr(if self.input_track().is_none() {
                        "processing-source-required"
                    } else {
                        match operation {
                            ProcessingOperation::Speech => "processing-speech-layout",
                            ProcessingOperation::Separate => "processing-separation-layout",
                            ProcessingOperation::Denoise => "processing-restoration-layout",
                            ProcessingOperation::Graph => "processing-input-unsupported",
                        }
                    }),
                )
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
        if self.page_has_parameter_drafts() {
            self.error = Some(self.tr("processing-pending-edits").to_string());
            cx.notify();
            return;
        }
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
                    "{} #{} · {operation} · {} · {phase}",
                    self.context.i18n.text("processing-task"),
                    id,
                    task.source_path
                        .file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_else(|| "—".into()),
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
            if !terminal {
                content = content.child(
                    button(
                        ("cancel-processing", panel),
                        self.tr("processing-cancel"),
                        cx,
                        move |this, _, cx| this.send(Command::CancelProcessing { id }, cx),
                    )
                    .self_start(),
                );
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
                        .child(
                            button(
                                ("apply-mix-gains", panel),
                                self.tr("processing-apply-mix"),
                                cx,
                                move |this, _, cx| this.audition(id, AuditionMode::Mix, cx),
                            )
                            .self_start(),
                        );
                }
            }
            content = content.child(activate_control(
                dropdown_trigger(
                    theme,
                    ("processing-task-details", panel),
                    self.tr(if self.task_details {
                        "processing-result-hide-details"
                    } else {
                        "processing-result-show-details"
                    }),
                    self.task_details,
                    cx,
                ),
                cx,
                |this, _, cx| {
                    this.task_details = !this.task_details;
                    cx.notify();
                },
            ));
            if self.task_details {
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
        let mut tabs = row()
            .gap_1()
            .id(("processing-tab-list", panel))
            .role(Role::TabList);
        for (page, label) in [
            (Page::Graph, "processing-tab-graph"),
            (Page::Equalizer, "processing-tab-equalizer"),
            (Page::Restoration, "processing-tab-restoration"),
            (Page::Speech, "processing-tab-speech"),
            (Page::Separation, "processing-tab-separation"),
            (Page::Results, "processing-tab-results"),
        ] {
            tabs = tabs.child(
                button(
                    SharedString::from(format!("processing-page-{panel}-{label}")),
                    self.tr(label),
                    cx,
                    move |this, _, cx| {
                        this.cancel_pointer(cx);
                        this.close_workbench_controls();
                        this.preset_open = false;
                        this.preset_manage = false;
                        this.page = page;
                        cx.notify();
                    },
                )
                .role(Role::Tab)
                .aria_selected(self.page == page)
                .flex_shrink_0()
                .whitespace_nowrap()
                .rounded_none()
                .border_0()
                .border_b_2()
                .border_color(if self.page == page {
                    rgb(theme.accent).into()
                } else {
                    transparent_black()
                })
                .bg(transparent_black())
                .text_color(rgb(if self.page == page {
                    theme.text
                } else {
                    theme.muted
                }))
                .group_hover("control", |style| {
                    style.bg(transparent_black()).text_color(rgb(theme.accent))
                }),
            );
        }
        let monitor_status = caption(
            theme,
            self.tr(if self.state.system.config.equalizer.enabled {
                "processing-workspace-monitor-on"
            } else {
                "processing-workspace-monitor-off"
            }),
        )
        .flex_shrink_0()
        .whitespace_nowrap();
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
        let selected_audio_text = match self.state.current_track() {
            Some(track) => format!(
                "{}: {selected_audio} · {transport} · {} · {} Hz · {} ch",
                self.tr("processing-workspace-selected-audio"),
                track.title,
                track.sample_rate,
                track.channels,
            ),
            None => format!(
                "{}: {selected_audio} · {transport} · —",
                self.tr("processing-workspace-selected-audio"),
            ),
        };
        let selected_audio = caption(theme, selected_audio_text.clone())
            .min_w_0()
            .truncate()
            .id(("processing-audition-status", panel))
            .tooltip(rivu_ui::text_tooltip(selected_audio_text));
        let mut selected_audio = row()
            .min_w_0()
            .gap_2()
            .items_center()
            .child(selected_audio.flex_1());
        if let Some(audition) = self.state.system.processing_audition {
            let id = audition.id;
            selected_audio = selected_audio.child(
                IconButton::new(
                    ("processing-original-source", panel),
                    "󰑖",
                    self.tr("processing-workspace-restore-original"),
                )
                .build(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Compact,
                    cx,
                    move |this, _, cx| this.send(Command::CancelProcessing { id }, cx),
                ),
            );
        }
        if self.pending > 0 {
            selected_audio = selected_audio.child(
                caption(theme, self.tr("processing-applying"))
                    .flex_shrink_0()
                    .whitespace_nowrap(),
            );
        }
        selected_audio = selected_audio.child(monitor_status);
        if self.page != Page::Graph {
            self.deactivate_graph_inputs(panel, window, cx);
        }
        if !matches!(self.page, Page::Graph | Page::Equalizer) {
            self.deactivate_equalizer_panel(panel, window, cx);
        }
        let body = match self.page {
            Page::Graph => self
                .routing_graph_view(panel, window, cx)
                .into_any_element(),
            Page::Results => self.processing_results_view(panel, cx).into_any_element(),
            page => {
                let body = match page {
                    Page::Equalizer => self.equalizer_view(panel, window, cx),
                    Page::Restoration => self.restoration_view(panel, cx),
                    Page::Speech => self.neural_view(panel, true, cx),
                    Page::Separation => self.neural_view(panel, false, cx),
                    Page::Graph | Page::Results => unreachable!(),
                };
                self.processing_scroll(panel, "processing-page-body", body.p_2())
            }
        };
        let mut footer = column().min_w_0().gap_1();
        footer = match self.page {
            Page::Graph => footer.child(self.graph_execution_bar(panel, cx)),
            Page::Restoration => {
                footer.child(self.task_controls(panel, ProcessingOperation::Denoise, cx))
            }
            Page::Speech => {
                footer.child(self.task_controls(panel, ProcessingOperation::Speech, cx))
            }
            Page::Separation => {
                footer.child(self.task_controls(panel, ProcessingOperation::Separate, cx))
            }
            Page::Equalizer | Page::Results => footer,
        };
        if self.page != Page::Results {
            footer = footer.child(self.graph_task_summary(panel, cx));
        }
        footer = footer.child(selected_audio);
        if self.page_has_parameter_drafts() {
            footer = footer.child(caption(theme, self.tr("processing-pending-edits")));
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
            .gap_1()
            .child(self.processing_workbench_measure(panel, cx))
            .child(
                row()
                    .min_w_0()
                    .flex_shrink_0()
                    .id(("processing-tabs", panel))
                    .overflow_x_scroll()
                    .child(tabs),
            )
            .child(div().flex_1().min_h_0().min_w_0().child(body))
            .child(footer.flex_shrink_0())
            .into_any_element()
    }
}
