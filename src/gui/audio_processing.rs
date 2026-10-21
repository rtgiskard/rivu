//! Dockable processing controls. Core owns committed parameters and task results;
//! this owner retains only field drafts, selection and cancellable pointer intent.
use super::{feature::FeatureContext, layout::interaction::DockGeometry};
use crate::{
    model::{
        AuditionMode, Command, DenoisePatch, EqBandEdit, EqualizerEdit, NeuralPatch,
        NoiseSelection, ProcessingOperation, ProcessingPhase, Track,
    },
    projection::GuiSnapshot,
};
use gpui::{prelude::*, *};
use rivu_dsp::equalizer::{ChannelTarget, EqBand, FilterKind, MAX_BANDS};
use rivu_ui::{
    button, caption, checkbox, column, disabled_control,
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
}

struct NodeDrag {
    panel: u64,
    band: usize,
    members: [usize; MAX_BANDS],
    member_count: usize,
    press_position: Point<Pixels>,
    moved: bool,
    original_frequency: f64,
    original_gain: f64,
    bounds: Bounds<Pixels>,
    geometry: DockGeometry,
    geometry_epoch: u64,
    frequency: f64,
    gain: f64,
    _subscriptions: [Subscription; 2],
}

type MeasuredGraph = Rc<Cell<Option<(Bounds<Pixels>, DockGeometry)>>>;

pub(super) struct AudioProcessing {
    context: FeatureContext,
    state: Rc<GuiSnapshot>,
    selected: Option<Arc<Track>>,
    use_selected: bool,
    page: Page,
    band: usize,
    add_band_count: usize,
    response_channel: usize,
    fields: HashMap<String, Field>,
    response: Arc<Vec<f64>>,
    response_error: Option<String>,
    maximum_frequency: f64,
    drag: Option<NodeDrag>,
    bounds: HashMap<u64, MeasuredGraph>,
    panel_scrolls: HashMap<u64, Rc<ScrollHandle>>,
    error: Option<String>,
    pending: usize,
    commands: VecDeque<Submission>,
    command_active: bool,
    removing_band: Option<(Rc<GuiSnapshot>, usize)>,
    geometry_epoch: u64,
    graph_ui: GraphUi,
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
            band: 0,
            add_band_count: 1,
            response_channel: 0,
            fields: HashMap::new(),
            response: Arc::new(Vec::new()),
            response_error: None,
            maximum_frequency: 20_000.,
            drag: None,
            bounds: HashMap::new(),
            panel_scrolls: HashMap::new(),
            error: None,
            pending: 0,
            commands: VecDeque::with_capacity(64),
            command_active: false,
            removing_band: None,
            geometry_epoch: 0,
            graph_ui: GraphUi::default(),
        };
        this.sync_fields(cx);
        this.update_response();
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
        let config_changed = self.state.system.config.equalizer != state.system.config.equalizer;
        let format_changed = self
            .state
            .current_track()
            .map(|t| (t.id, t.sample_rate, t.channels))
            != state
                .current_track()
                .map(|t| (t.id, t.sample_rate, t.channels));
        if self.state.system.config.equalizer.bands.len()
            != state.system.config.equalizer.bands.len()
        {
            self.remap_band_fields(&state.system.config.equalizer.bands, cx);
        }
        let changed = !Arc::ptr_eq(&self.state.system.config, &state.system.config)
            || self.state.system.processing != state.system.processing
            || format_changed;
        self.state = state;
        let remaining = MAX_BANDS.saturating_sub(self.state.system.config.equalizer.bands.len());
        self.add_band_count = self.add_band_count.clamp(1, remaining.max(1));
        self.reconcile_graph(cx);
        if config_changed || format_changed {
            self.drag = None;
            self.band = self.band.min(
                self.state
                    .system
                    .config
                    .equalizer
                    .bands
                    .len()
                    .saturating_sub(1),
            );
            self.update_response();
        }
        if changed {
            self.sync_fields(cx);
            cx.notify();
        }
    }

    pub(super) fn cancel_pointer(&mut self, cx: &mut Context<Self>) {
        let graph_pointer = self.graph_ui.pointer.take().is_some();
        let eq_pointer = self.drag.take().is_some();
        if eq_pointer {
            self.update_response();
        }
        if graph_pointer || eq_pointer {
            cx.notify();
        }
    }

    pub(super) fn invalidate_geometry(&mut self, cx: &mut Context<Self>) {
        self.geometry_epoch = self.geometry_epoch.wrapping_add(1);
        for bounds in self.bounds.values() {
            bounds.set(None);
        }
        for bounds in self.graph_ui.bounds.values() {
            bounds.set(None);
        }
        self.cancel_pointer(cx);
    }

    fn node_geometry_valid(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        self.reconcile(self.context.snapshot(), cx);
        let current = DockGeometry::current(window, self.state.system.config.ui_scale);
        if self.drag.as_ref().is_some_and(|drag| {
            drag.geometry == current && drag.geometry_epoch == self.geometry_epoch
        }) {
            true
        } else {
            self.cancel_pointer(cx);
            false
        }
    }

    pub(super) fn pointer_active(&self) -> bool {
        self.drag.is_some() || self.graph_ui.pointer.is_some()
    }

    pub(super) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.graph_ui.input_focused(window, cx)
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
        self.bounds.retain(|id, _| layout.contains_panel(*id));
        self.panel_scrolls
            .retain(|id, _| layout.contains_panel(*id));
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
        if self
            .drag
            .as_ref()
            .is_some_and(|d| !visible || !layout.is_panel_active(d.panel))
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
        self.commands.push_back(Submission {
            command,
            field: submitted,
        });
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
        }) = self.commands.pop_front()
        else {
            return;
        };
        self.command_active = true;
        if let Command::EqualizerEdit {
            edit: EqualizerEdit::RemoveBand { index },
        } = &command
        {
            self.removing_band = Some((Rc::clone(&self.state), *index));
        }
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
                this.removing_band = None;
                this.dispatch_next(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn eq(&mut self, edit: EqualizerEdit, cx: &mut Context<Self>) {
        self.send(Command::EqualizerEdit { edit }, cx);
    }

    fn band_edit(&mut self, index: usize, field: EqBandEdit, cx: &mut Context<Self>) {
        self.eq(EqualizerEdit::BandField { index, field }, cx);
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

    fn remap_band_fields(&mut self, next: &[EqBand], cx: &mut Context<Self>) {
        let previous = &self.state.system.config.equalizer.bands;
        let mut mapping = [None; MAX_BANDS];
        let removed = self
            .removing_band
            .as_ref()
            .filter(|(state, index)| {
                state.system.config.equalizer.bands == *previous
                    && *index < previous.len()
                    && previous.len() == next.len() + 1
                    && previous
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| i != index)
                        .map(|(_, band)| band)
                        .eq(next.iter())
            })
            .map(|(_, index)| *index);
        if let Some(removed) = removed {
            for (index, target) in mapping.iter_mut().enumerate().take(previous.len()) {
                if index != removed {
                    *target = Some(if index < removed { index } else { index - 1 });
                }
            }
        } else {
            // Persistent bands have no IDs. Match unchanged bands in order;
            // appending retains every old band, while removed/replaced rows retire.
            let mut lengths = [[0u8; MAX_BANDS + 1]; MAX_BANDS + 1];
            for old in (0..previous.len()).rev() {
                for new in (0..next.len()).rev() {
                    lengths[old][new] = if previous[old] == next[new] {
                        1 + lengths[old + 1][new + 1]
                    } else {
                        lengths[old + 1][new].max(lengths[old][new + 1])
                    };
                }
            }
            let (mut old, mut new) = (0, 0);
            while old < previous.len() && new < next.len() {
                if previous[old] == next[new] {
                    mapping[old] = Some(new);
                    old += 1;
                    new += 1;
                } else if lengths[old + 1][new] >= lengths[old][new + 1] {
                    old += 1;
                } else {
                    new += 1;
                }
            }
        }
        self.band = mapping
            .get(self.band)
            .copied()
            .flatten()
            .unwrap_or_else(|| self.band.min(next.len().saturating_sub(1)));
        for (key, mut field) in std::mem::take(&mut self.fields) {
            let Some((index, name)) = key
                .strip_prefix("band.")
                .and_then(|path| path.split_once('.'))
            else {
                self.fields.insert(key, field);
                continue;
            };
            let Some(new) = index
                .parse::<usize>()
                .ok()
                .and_then(|index| mapping.get(index).copied().flatten())
            else {
                continue;
            };
            let new_key = format!("band.{new}.{name}");
            if new_key != key {
                field._subscription = self.subscribe_field(&field.input, new_key.clone(), cx);
            }
            self.fields.insert(new_key, field);
        }
    }

    fn sync_fields(&mut self, cx: &mut Context<Self>) {
        let eq = self.state.system.config.equalizer.clone();
        self.field(
            "preamp_db",
            &self.tr("processing-preamp"),
            eq.preamp_db.to_string(),
            cx,
        );
        self.field(
            "output_gain_db",
            &self.tr("processing-output-gain"),
            eq.output_gain_db.to_string(),
            cx,
        );
        for (i, b) in eq.bands.iter().enumerate() {
            for (name, label, value) in [
                (
                    "frequency_hz",
                    "processing-frequency",
                    b.frequency_hz.to_string(),
                ),
                ("gain_db", "processing-gain", b.gain_db.to_string()),
                ("q", "processing-q", b.q.to_string()),
                (
                    "shelf_slope",
                    "processing-shelf-slope",
                    b.shelf_slope.to_string(),
                ),
                ("stages", "processing-stages", b.stages.to_string()),
                (
                    "channels",
                    "processing-channels",
                    match &b.channels {
                        ChannelTarget::All => "all".to_owned(),
                        ChannelTarget::Selected(channels) => channels
                            .iter()
                            .map(|c| (c + 1).to_string())
                            .collect::<Vec<_>>()
                            .join(","),
                    },
                ),
            ] {
                self.field(&format!("band.{i}.{name}"), &self.tr(label), value, cx);
            }
        }
        self.fields.retain(|key, _| {
            key.strip_prefix("band.")
                .and_then(|s| s.split('.').next())
                .and_then(|s| s.parse::<usize>().ok())
                .is_none_or(|i| i < eq.bands.len())
        });
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
        let number = || {
            value
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite())
                .ok_or_else(|| {
                    self.context
                        .i18n
                        .text("processing-invalid-number")
                        .to_owned()
                })
        };
        let edit = if key == "preamp_db" {
            Some(EqualizerEdit::PreampDb(number()?))
        } else if key == "output_gain_db" {
            Some(EqualizerEdit::OutputGainDb(number()?))
        } else if let Some(path) = key.strip_prefix("band.") {
            let (index, name) = path.split_once('.').ok_or("Invalid band field")?;
            let index = index.parse::<usize>().map_err(|e| e.to_string())?;
            let field = match name {
                "frequency_hz" => EqBandEdit::FrequencyHz(number()?),
                "gain_db" => EqBandEdit::GainDb(number()?),
                "q" => EqBandEdit::Q(number()?),
                "shelf_slope" => EqBandEdit::ShelfSlope(number()?),
                "stages" => EqBandEdit::Stages(value.parse::<u8>().map_err(|e| e.to_string())?),
                "channels" => EqBandEdit::Channels(if value.eq_ignore_ascii_case("all") {
                    ChannelTarget::All
                } else {
                    let channels = value
                        .split(',')
                        .map(|c| {
                            c.trim()
                                .parse::<usize>()
                                .ok()
                                .and_then(|n| n.checked_sub(1))
                                .ok_or_else(|| {
                                    self.context.i18n.text("processing-channel-help").to_owned()
                                })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    ChannelTarget::Selected(channels)
                }),
                _ => return Ok(None),
            };
            Some(EqualizerEdit::BandField { index, field })
        } else {
            None
        };
        if let Some(edit) = edit {
            return Ok(Some(Command::EqualizerEdit { edit }));
        }
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
        let persisted = key == "preamp_db"
            || key == "output_gain_db"
            || key.starts_with("band.")
            || key.starts_with("denoise.")
            || key.starts_with("neural.");
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
        column()
            .min_w_0()
            .gap_1()
            .child(caption(theme, label))
            .child(control)
            .opacity(if enabled { 1. } else { 0.5 })
    }

    fn update_response(&mut self) {
        let Some(track) = self.state.current_track() else {
            self.response = Arc::new(Vec::new());
            self.response_error = Some(
                self.context
                    .i18n
                    .text("processing-response-unknown")
                    .to_owned(),
            );
            return;
        };
        if track.sample_rate == 0 || track.channels == 0 {
            self.response = Arc::new(Vec::new());
            self.response_error = Some(
                self.context
                    .i18n
                    .text("processing-response-unknown")
                    .to_owned(),
            );
            return;
        }
        self.maximum_frequency = (track.sample_rate as f64 * 0.499).min(96_000.);
        self.response_channel = self.response_channel.min(track.channels as usize - 1);
        let frequencies = (0..257)
            .map(|i| 20. * (self.maximum_frequency / 20.).powf(i as f64 / 256.))
            .collect::<Vec<_>>();
        let mut response = vec![0.; frequencies.len()];
        let mut config = self.state.system.config.equalizer.clone();
        if let Some(drag) = &self.drag
            && let Some(band) = config.bands.get_mut(drag.band)
        {
            band.frequency_hz = drag.frequency;
            band.gain_db = drag.gain;
        }
        match config.sample_response_db(
            track.sample_rate,
            track.channels as usize,
            self.response_channel,
            &frequencies,
            &mut response,
        ) {
            Ok(()) => {
                self.response = Arc::new(response);
                self.response_error = None;
            }
            Err(error) => {
                self.response = Arc::new(Vec::new());
                self.response_error = Some(error.to_string());
            }
        }
    }

    fn frequency_x(&self, frequency: f64) -> f32 {
        ((frequency / 20.).ln() / (self.maximum_frequency / 20.).ln()).clamp(0., 1.) as f32
    }

    fn register_pointer_handlers(
        weak: WeakEntity<Self>,
        panel: u64,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let entity = weak.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture {
                let _ = entity.update(cx, |this, cx| {
                    if this.drag.as_ref().is_some_and(|d| d.panel == panel) {
                        if !event.dragging() {
                            this.cancel_pointer(cx);
                        } else if this.node_geometry_valid(window, cx) {
                            this.move_node(event.position, cx);
                        }
                        window.prevent_default();
                    }
                });
            }
        });
        let entity = weak.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture && event.button == MouseButton::Left {
                let _ = entity.update(cx, |this, cx| {
                    if this.drag.as_ref().is_some_and(|d| d.panel == panel)
                        && this.node_geometry_valid(window, cx)
                    {
                        this.move_node(event.position, cx);
                        if let Some(drag) = this.drag.take() {
                            if drag.moved {
                                this.update_response();
                                let band = &this.state.system.config.equalizer.bands[drag.band];
                                if band.frequency_hz != drag.frequency || band.gain_db != drag.gain
                                {
                                    this.band_edit(
                                        drag.band,
                                        EqBandEdit::Position {
                                            frequency_hz: drag.frequency,
                                            gain_db: drag.gain,
                                        },
                                        cx,
                                    );
                                }
                            } else if drag.member_count > 1 {
                                let current = drag.members[..drag.member_count]
                                    .iter()
                                    .position(|index| *index == drag.band)
                                    .unwrap_or(0);
                                this.band = drag.members[(current + 1) % drag.member_count];
                            }
                            cx.notify();
                        }
                    }
                });
            }
        });
        window.on_mouse_event(move |_: &MouseExitEvent, phase, _, cx| {
            if phase == DispatchPhase::Capture {
                let _ = weak.update(cx, |this, cx| this.cancel_pointer(cx));
            }
        });
    }

    fn graph(&mut self, panel: u64, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let measured = self
            .bounds
            .entry(panel)
            .or_insert_with(|| Rc::new(Cell::new(None)))
            .clone();
        let curve = Arc::clone(&self.response);
        let nodes = self
            .state
            .system
            .config
            .equalizer
            .bands
            .iter()
            .take(MAX_BANDS)
            .enumerate()
            .filter(|(_, b)| b.enabled)
            .map(|(i, b)| {
                let (frequency, gain) = self
                    .drag
                    .as_ref()
                    .filter(|d| d.band == i)
                    .map(|d| (d.frequency, d.gain))
                    .unwrap_or((b.frequency_hz, b.gain_db));
                (i, self.frequency_x(frequency), (0.5 - gain / 48.) as f32)
            })
            .collect::<Vec<_>>();
        let selected = self.band;
        let bounds_copy = measured.clone();
        let weak = cx.entity().downgrade();
        let geometry_owner = weak.clone();
        let geometry_epoch = self.geometry_epoch;
        let ui_scale = self.state.system.config.ui_scale;
        let mut controls = Vec::with_capacity(nodes.len());
        for &(index, x, y) in &nodes {
            let band = &self.state.system.config.equalizer.bands[index];
            let mut stack_count = 0usize;
            let mut representative = index;
            for &(other, other_x, other_y) in &nodes {
                let overlaps = if let Some((bounds, _)) = measured.get() {
                    let dx = f32::from(bounds.size.width) * (x - other_x);
                    let dy = f32::from(bounds.size.height) * (y - other_y);
                    dx * dx + dy * dy < 225.
                } else {
                    x == other_x && y == other_y
                };
                if overlaps {
                    stack_count += 1;
                    if other == selected || (representative != selected && other < representative) {
                        representative = other;
                    }
                }
            }
            controls.push(
                div()
                    .id(SharedString::from(format!("eq-node-{panel}-{index}")))
                    .absolute()
                    .left(relative(x))
                    .top(relative(y))
                    .ml(-px(8.))
                    .mt(-px(8.))
                    .size(px(16.))
                    .tab_index(0)
                    .tab_stop(!self.response.is_empty())
                    .role(Role::Slider)
                    .aria_label(SharedString::from(format!(
                        "{} {}: {}",
                        self.context.i18n.text("processing-node"),
                        index + 1,
                        self.context.i18n.text("processing-node-help")
                    )))
                    .aria_value(SharedString::from(format!(
                        "{:.1} Hz, {:.2} dB",
                        band.frequency_hz, band.gain_db
                    )))
                    .border_1()
                    .border_color(transparent_black())
                    .focus(|style| style.border_color(rgb(theme.accent)))
                    .when(stack_count > 1 && representative == index, |node| {
                        node.child(
                            div()
                                .absolute()
                                .left(px(14.))
                                .top(-px(14.))
                                .px_1()
                                .rounded_sm()
                                .bg(rgb(theme.panel))
                                .text_color(rgb(if index == selected {
                                    theme.accent
                                } else {
                                    theme.text
                                }))
                                .text_xs()
                                .whitespace_nowrap()
                                .child(self.context.i18n.format(
                                    "processing-overlap-count",
                                    &[("count", stack_count.into()), ("band", (index + 1).into())],
                                ))
                                .id(SharedString::from(format!("eq-stack-{panel}-{index}")))
                                .tooltip(rivu_ui::text_tooltip(self.tr("processing-overlap-help"))),
                        )
                    })
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        this.node_key(index, event, cx)
                    }))
                    .into_any_element(),
            );
        }
        div()
            .id(("eq-graph", panel))
            .relative()
            .w_full()
            .h(rems(12.))
            .flex_shrink_0()
            .bg(rgb(theme.bg))
            .border_1()
            .border_color(rgb(theme.border))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if this.response.is_empty() {
                        return;
                    }
                    let Some((bounds, geometry)) = measured.get() else {
                        return;
                    };
                    if geometry_epoch != this.geometry_epoch
                        || geometry
                            != DockGeometry::current(
                                window,
                                this.context.snapshot().system.config.ui_scale,
                            )
                    {
                        return;
                    }
                    let mut members = [0; MAX_BANDS];
                    let mut member_count = 0;
                    let mut nearest = None;
                    let mut nearest_distance = f32::INFINITY;
                    for (index, band) in this
                        .state
                        .system
                        .config
                        .equalizer
                        .bands
                        .iter()
                        .take(MAX_BANDS)
                        .enumerate()
                        .filter(|(_, band)| band.enabled)
                    {
                        let x = bounds.origin.x
                            + bounds.size.width * this.frequency_x(band.frequency_hz);
                        let y = bounds.origin.y
                            + bounds.size.height * ((0.5 - band.gain_db / 48.) as f32);
                        let dx = f32::from(x - event.position.x);
                        let dy = f32::from(y - event.position.y);
                        let distance = dx * dx + dy * dy;
                        if distance < 225. {
                            members[member_count] = index;
                            member_count += 1;
                            if distance < nearest_distance {
                                nearest = Some(index);
                                nearest_distance = distance;
                            }
                        }
                    }
                    let target = if members[..member_count].contains(&this.band) {
                        Some(this.band)
                    } else {
                        nearest
                    };
                    if let Some(index) = target {
                        this.band = index;
                        let b = &this.state.system.config.equalizer.bands[index];
                        let frequency = b.frequency_hz;
                        let gain = b.gain_db;
                        let subscriptions = [
                            cx.observe_window_activation(window, |this, window, cx| {
                                if !window.is_window_active() {
                                    this.cancel_pointer(cx)
                                }
                            }),
                            cx.observe_window_visibility(window, |this, visibility, _, cx| {
                                if !matches!(visibility, WindowVisibility::Visible) {
                                    this.cancel_pointer(cx)
                                }
                            }),
                        ];
                        this.drag = Some(NodeDrag {
                            panel,
                            band: index,
                            members,
                            member_count,
                            press_position: event.position,
                            moved: false,
                            original_frequency: frequency,
                            original_gain: gain,
                            bounds,
                            geometry,
                            geometry_epoch,
                            frequency,
                            gain,
                            _subscriptions: subscriptions,
                        });
                        cx.stop_propagation();
                        cx.notify();
                    }
                }),
            )
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let geometry = DockGeometry::current(window, ui_scale);
                        let _ = geometry_owner.update(cx, |this, cx| {
                            if geometry_epoch != this.geometry_epoch {
                                return;
                            }
                            if bounds_copy.get() != Some((bounds, geometry)) {
                                cx.notify();
                            }
                            bounds_copy.set(Some((bounds, geometry)));
                            if this.drag.as_ref().is_some_and(|d| {
                                d.panel == panel && (d.bounds != bounds || d.geometry != geometry)
                            }) {
                                this.cancel_pointer(cx);
                            }
                        });
                        bounds
                    },
                    move |bounds, _, window, cx| {
                        Self::register_pointer_handlers(weak.clone(), panel, window, cx);
                        let mut grid = PathBuilder::stroke(px(1.));
                        for fraction in [0., 0.25, 0.5, 0.75, 1.] {
                            let y = bounds.origin.y + bounds.size.height * fraction;
                            grid.move_to(point(bounds.origin.x, y));
                            grid.line_to(point(bounds.right(), y));
                        }
                        for fraction in [0., 0.25, 0.5, 0.75, 1.] {
                            let x = bounds.origin.x + bounds.size.width * fraction;
                            grid.move_to(point(x, bounds.origin.y));
                            grid.line_to(point(x, bounds.bottom()));
                        }
                        if let Ok(path) = grid.build() {
                            window.paint_path(path, rgb(theme.border));
                        }
                        let mut path = PathBuilder::stroke(px(2.));
                        for (i, db) in curve.iter().enumerate() {
                            let p = point(
                                bounds.origin.x + bounds.size.width * (i as f32 / 256.),
                                bounds.origin.y
                                    + bounds.size.height
                                        * (0.5 - (*db).clamp(-24., 24.) as f32 / 48.),
                            );
                            if i == 0 {
                                path.move_to(p);
                            } else {
                                path.line_to(p);
                            }
                        }
                        if !curve.is_empty()
                            && let Ok(path) = path.build()
                        {
                            window.paint_path(path, rgb(theme.accent));
                        }
                        for (index, x, y) in nodes
                            .iter()
                            .filter(|(index, _, _)| *index != selected)
                            .chain(nodes.iter().filter(|(index, _, _)| *index == selected))
                        {
                            let center = point(
                                bounds.origin.x + bounds.size.width * *x,
                                bounds.origin.y + bounds.size.height * *y,
                            );
                            window.paint_quad(fill(
                                Bounds::new(center - point(px(4.), px(4.)), size(px(8.), px(8.))),
                                rgb(if *index == selected {
                                    theme.accent
                                } else {
                                    theme.text
                                }),
                            ));
                        }
                    },
                )
                .size_full(),
            )
            .children(controls)
            .into_any_element()
    }

    fn node_key(&mut self, index: usize, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if event.keystroke.modifiers != Modifiers::default() {
            return;
        }
        let Some(band) = self.state.system.config.equalizer.bands.get(index) else {
            return;
        };
        let field = match event.keystroke.key.as_str() {
            "left" => EqBandEdit::FrequencySteps {
                steps: -1,
                maximum_hz: self.maximum_frequency,
            },
            "right" => EqBandEdit::FrequencySteps {
                steps: 1,
                maximum_hz: self.maximum_frequency,
            },
            "up" | "down"
                if matches!(
                    band.kind,
                    FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf
                ) =>
            {
                EqBandEdit::GainSteps(if event.keystroke.key == "up" { 1 } else { -1 })
            }
            "enter" | "space" => {
                self.band = index;
                cx.notify();
                cx.stop_propagation();
                return;
            }
            _ => return,
        };
        self.cancel_pointer(cx);
        self.band = index;
        self.band_edit(index, field, cx);
        cx.stop_propagation();
    }

    fn move_node(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if let Some(drag) = self.drag.as_mut() {
            if drag.bounds.size.width <= px(0.) || drag.bounds.size.height <= px(0.) {
                return;
            }
            let delta = position - drag.press_position;
            let dx = f32::from(delta.x);
            let dy = f32::from(delta.y);
            if !drag.moved && dx * dx + dy * dy < 9. {
                return;
            }
            drag.moved = true;
            let original_x =
                (drag.original_frequency / 20.).ln() / (self.maximum_frequency / 20.).ln();
            let x = (original_x + (delta.x / drag.bounds.size.width) as f64).clamp(0., 1.);
            drag.frequency = 20. * (self.maximum_frequency / 20.).powf(x);
            let kind = self.state.system.config.equalizer.bands[drag.band].kind;
            if matches!(
                kind,
                FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf
            ) {
                drag.gain = (drag.original_gain - (delta.y / drag.bounds.size.height) as f64 * 48.)
                    .clamp(-24., 24.);
            }
            cx.notify();
            self.update_response();
        }
    }

    fn equalizer_view(&mut self, panel: u64, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let eq = self.state.system.config.equalizer.clone();
        let mut content = column()
            .gap_3()
            .child(
                row()
                    .flex_wrap()
                    .gap_2()
                    .child(checkbox(
                        ("eq-enabled", panel),
                        self.tr("processing-eq-enabled"),
                        eq.enabled,
                        true,
                        cx,
                        |this, value, cx| this.eq(EqualizerEdit::Enabled(value), cx),
                    ))
                    .child(button(
                        ("eq-flat", panel),
                        self.tr("processing-flat"),
                        cx,
                        |this, _, cx| {
                            let mut config = rivu_dsp::equalizer::EqualizerConfig::default();
                            config.enabled = this.state.system.config.equalizer.enabled;
                            this.eq(EqualizerEdit::Replace { config }, cx);
                        },
                    ))
                    .child(button(
                        ("eq-ab", panel),
                        self.tr("processing-ab"),
                        cx,
                        |this, _, cx| this.eq(EqualizerEdit::SwapAb, cx),
                    )),
            )
            .child(self.graph(panel, window, cx))
            .child(caption(theme, self.tr("processing-graph-help")));
        if self.response_error.is_none() {
            let mut labels = row().justify_between().gap_1();
            for fraction in [0., 0.25, 0.5, 0.75, 1.] {
                let frequency = 20. * (self.maximum_frequency / 20.).powf(fraction);
                labels = labels.child(caption(
                    theme,
                    if frequency >= 1000. {
                        format!("{:.1}k", frequency / 1000.)
                    } else {
                        format!("{frequency:.0}")
                    },
                ));
            }
            content = content.child(labels);
        }
        if let Some(error) = &self.response_error {
            content = content.child(caption(theme, error.clone()).text_color(rgb(theme.error)));
        }
        if let Some(track) = self.state.current_track() {
            content = content.child(caption(
                theme,
                format!(
                    "{} Hz · {} {} · 20–{:.0} Hz · ±24 dB",
                    track.sample_rate,
                    track.channels,
                    self.context.i18n.text("processing-channel-count"),
                    self.maximum_frequency
                ),
            ));
            let mut channels = row().flex_wrap().gap_1();
            for channel in 0..track.channels as usize {
                channels = channels.child(
                    button(
                        SharedString::from(format!("response-channel-{channel}")),
                        format!(
                            "{} {}",
                            self.context.i18n.text("processing-response-channel"),
                            channel + 1
                        ),
                        cx,
                        move |this, _, cx| {
                            this.response_channel = channel;
                            this.update_response();
                            cx.notify();
                        },
                    )
                    .when(channel == self.response_channel, |v| {
                        v.bg(rgb(theme.selected))
                    }),
                );
            }
            content = content.child(channels);
        }
        content = content
            .child(self.field_view("preamp_db", self.tr("processing-preamp"), true, cx))
            .child(self.field_view(
                "output_gain_db",
                self.tr("processing-output-gain"),
                true,
                cx,
            ));
        let mut bands = row().flex_wrap().gap_1();
        for (index, band) in eq.bands.iter().enumerate() {
            bands = bands.child(
                button(
                    SharedString::from(format!("select-band-{index}")),
                    format!(
                        "{} {}{}",
                        index + 1,
                        if band.enabled { "" } else { "○ " },
                        if band.solo { "S" } else { "" }
                    ),
                    cx,
                    move |this, _, cx| {
                        this.cancel_pointer(cx);
                        this.band = index;
                        cx.notify();
                    },
                )
                .when(index == self.band, |v| v.bg(rgb(theme.selected))),
            );
        }
        let remaining = MAX_BANDS.saturating_sub(eq.bands.len());
        let count = self.add_band_count.min(remaining);
        let add_disabled = self.pending > 0 || remaining == 0;
        let add_controls = row()
            .flex_wrap()
            .gap_2()
            .child(disabled_control(
                theme,
                button(("eq-add-less", panel), "−", cx, |this, _, cx| {
                    if this.pending == 0 && this.add_band_count > 1 {
                        this.add_band_count -= 1;
                        cx.notify();
                    }
                })
                .aria_label(self.tr("processing-add-fewer")),
                add_disabled || count <= 1,
            ))
            .child(caption(
                theme,
                self.context.i18n.format(
                    "processing-add-count",
                    &[("count", count.into()), ("remaining", remaining.into())],
                ),
            ))
            .child(disabled_control(
                theme,
                button(("eq-add-more", panel), "+", cx, |this, _, cx| {
                    this.reconcile(this.context.snapshot(), cx);
                    let remaining =
                        MAX_BANDS.saturating_sub(this.state.system.config.equalizer.bands.len());
                    if this.pending == 0 && this.add_band_count < remaining {
                        this.add_band_count += 1;
                        cx.notify();
                    }
                })
                .aria_label(self.tr("processing-add-more")),
                add_disabled || count >= remaining,
            ))
            .child(disabled_control(
                theme,
                button(
                    ("add-band", panel),
                    self.tr("processing-add-bands"),
                    cx,
                    |this, _, cx| {
                        this.reconcile(this.context.snapshot(), cx);
                        let remaining = MAX_BANDS
                            .saturating_sub(this.state.system.config.equalizer.bands.len());
                        let count = this.add_band_count.min(remaining);
                        if this.pending == 0 && count > 0 {
                            this.cancel_pointer(cx);
                            this.eq(EqualizerEdit::AddBands { count }, cx);
                        }
                    },
                )
                .tooltip(rivu_ui::text_tooltip(self.tr("processing-add-bands-help"))),
                add_disabled,
            ));
        content = content.child(bands).child(add_controls);
        if let Some(band) = eq.bands.get(self.band) {
            let index = self.band;
            content = content.child(
                row()
                    .flex_wrap()
                    .gap_2()
                    .child(checkbox(
                        ("band-enabled", panel),
                        self.tr("processing-enabled"),
                        band.enabled,
                        true,
                        cx,
                        move |this, value, cx| {
                            this.band_edit(index, EqBandEdit::Enabled(value), cx)
                        },
                    ))
                    .child(checkbox(
                        ("band-solo", panel),
                        self.tr("processing-solo"),
                        band.solo,
                        true,
                        cx,
                        move |this, value, cx| this.band_edit(index, EqBandEdit::Solo(value), cx),
                    ))
                    .child(button(
                        ("remove-band", panel),
                        self.tr("processing-remove-band"),
                        cx,
                        move |this, _, cx| this.eq(EqualizerEdit::RemoveBand { index }, cx),
                    )),
            );
            let mut types = row().flex_wrap().gap_1();
            for (kind, key) in [
                (FilterKind::Peak, "processing-peak"),
                (FilterKind::LowShelf, "processing-low-shelf"),
                (FilterKind::HighShelf, "processing-high-shelf"),
                (FilterKind::LowPass, "processing-low-pass"),
                (FilterKind::HighPass, "processing-high-pass"),
                (FilterKind::BandPass, "processing-band-pass"),
                (FilterKind::Notch, "processing-notch"),
            ] {
                types = types.child(
                    button(
                        SharedString::from(format!("band-kind-{kind:?}")),
                        self.tr(key),
                        cx,
                        move |this, _, cx| this.band_edit(index, EqBandEdit::Kind(kind), cx),
                    )
                    .when(kind == band.kind, |v| v.bg(rgb(theme.selected))),
                );
            }
            content = content.child(types);
            for (name, key, applicable) in [
                ("frequency_hz", "processing-frequency", true),
                (
                    "gain_db",
                    "processing-gain",
                    matches!(
                        band.kind,
                        FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf
                    ),
                ),
                (
                    "q",
                    "processing-q",
                    !matches!(band.kind, FilterKind::LowShelf | FilterKind::HighShelf),
                ),
                (
                    "shelf_slope",
                    "processing-shelf-slope",
                    matches!(band.kind, FilterKind::LowShelf | FilterKind::HighShelf),
                ),
                (
                    "stages",
                    "processing-stages",
                    !matches!(
                        band.kind,
                        FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf
                    ),
                ),
                ("channels", "processing-channels", true),
            ] {
                if applicable {
                    content = content.child(self.field_view(
                        &format!("band.{index}.{name}"),
                        self.tr(key),
                        true,
                        cx,
                    ));
                }
            }
            content = content
                .child(caption(theme, self.tr("processing-channel-help")))
                .child(caption(theme, self.tr("processing-stage-help")));
        }
        content.child(self.preset_view(panel, cx))
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
        // Register visibility on every render, but retain the Rc identity used
        // by the scrollbar's gesture owner across panel rerenders.
        let handle = self
            .context
            .scroll
            .borrow_mut()
            .content(("audio_processing", panel));
        let scroll = Rc::clone(
            self.panel_scrolls
                .entry(panel)
                .or_insert_with(|| Rc::new(handle)),
        );
        let mut tabs = row().flex_wrap().gap_1();
        for (page, label) in [
            (Page::Graph, "processing-routing-graph"),
            (Page::Equalizer, "processing-equalizer"),
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
        let body = match self.page {
            Page::Graph => self.routing_graph_view(panel, window, cx),
            Page::Equalizer => self.equalizer_view(panel, window, cx),
            Page::Restoration => self.restoration_view(panel, cx),
            Page::Neural => self.neural_view(panel, cx),
        };
        let input = self
            .input_track()
            .map(|t| format!("{} · {} Hz · {} ch", t.title, t.sample_rate, t.channels))
            .unwrap_or_else(|| self.context.i18n.text("processing-no-input").to_owned());
        let mut content = column()
            .gap_3()
            .p_3()
            .flex_shrink_0()
            .child(tabs)
            .child(caption(theme, self.tr("processing-safety")))
            .child(
                row()
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
                    ),
            )
            .child(caption(theme, input))
            .child(body.flex_shrink_0())
            .child(
                if self
                    .state
                    .system
                    .processing
                    .as_ref()
                    .is_some_and(|task| task.operation == ProcessingOperation::Graph)
                {
                    self.graph_task_status(panel, cx)
                } else {
                    self.task_status(panel, cx)
                },
            );
        if self.fields.iter().any(|(key, field)| {
            field.dirty && (key.starts_with("denoise.") || key.starts_with("neural."))
        }) {
            content = content.child(caption(theme, self.tr("processing-pending-edits")));
        }
        if self.pending > 0 {
            content = content.child(caption(theme, self.tr("processing-applying")));
        }
        if let Some(error) = &self.error {
            content = content.child(caption(theme, error.clone()).text_color(rgb(theme.error)));
        }
        scrollable_area(
            ("processing-area", panel),
            div()
                .id(("processing-scroll", panel))
                .size_full()
                .min_w_0()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&scroll)
                .child(content),
            &scroll,
        )
        .into_any_element()
    }
}
