//! Dockable processing controls. Core owns committed parameters and task results;
//! this owner retains only field drafts, selection and cancellable pointer intent.
use super::{feature::FeatureContext, layout::interaction::DockGeometry};
use crate::projection::GuiSnapshot;
use gpui::{prelude::*, *};
use rivu_dsp::denoise::{DenoiseConfig, DenoiseMode};
use rivu_format::config::{NeuralConfig, NeuralDevice};
use rivu_protocol::model::{
    AuditionMode, Command, DenoisePatch, EqualizerEdit, NeuralPatch, NoiseSelection,
    ProcessingOperation, ProcessingPhase, Track,
};
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

mod processing_graph_ui;
use processing_graph_ui::GraphUi;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum DenoiseField {
    Mode,
    Strength,
    SpectralGainFloorDb,
    AttackMs,
    ReleaseMs,
    SmoothingBins,
    TransientProtection,
    Wet,
    FftSize,
    StationaryNoiseDbfs,
    HumEnabled,
    HumFundamentalHz,
    HumHarmonics,
    HumQ,
    ImpulseEnabled,
    ImpulseMaxWidth,
    ImpulseThreshold,
    ImpulseFloorDbfs,
    ImpulseTransientProtection,
}

impl DenoiseField {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Mode => "mode",
            Self::Strength => "strength",
            Self::SpectralGainFloorDb => "spectral_gain_floor_db",
            Self::AttackMs => "attack_ms",
            Self::ReleaseMs => "release_ms",
            Self::SmoothingBins => "smoothing_bins",
            Self::TransientProtection => "transient_protection",
            Self::Wet => "wet",
            Self::FftSize => "fft_size",
            Self::StationaryNoiseDbfs => "stationary_noise_dbfs",
            Self::HumEnabled => "hum.enabled",
            Self::HumFundamentalHz => "hum.fundamental_hz",
            Self::HumHarmonics => "hum.harmonics",
            Self::HumQ => "hum.q",
            Self::ImpulseEnabled => "impulse.enabled",
            Self::ImpulseMaxWidth => "impulse.max_width",
            Self::ImpulseThreshold => "impulse.threshold",
            Self::ImpulseFloorDbfs => "impulse.floor_dbfs",
            Self::ImpulseTransientProtection => "impulse.transient_protection",
        }
    }
    pub(super) fn text(self, config: &DenoiseConfig) -> String {
        match self {
            Self::Mode => match config.mode {
                DenoiseMode::Off => "off",
                DenoiseMode::Stationary => "stationary",
                DenoiseMode::Profile => "profile",
                DenoiseMode::Adaptive => "adaptive",
            }
            .to_owned(),
            Self::Strength => config.strength.to_string(),
            Self::SpectralGainFloorDb => config.spectral_gain_floor_db.to_string(),
            Self::AttackMs => config.attack_ms.to_string(),
            Self::ReleaseMs => config.release_ms.to_string(),
            Self::SmoothingBins => config.smoothing_bins.to_string(),
            Self::TransientProtection => config.transient_protection.to_string(),
            Self::Wet => config.wet.to_string(),
            Self::FftSize => config.fft_size.to_string(),
            Self::StationaryNoiseDbfs => config.stationary_noise_dbfs.to_string(),
            Self::HumEnabled => config.hum.enabled.to_string(),
            Self::HumFundamentalHz => config.hum.fundamental_hz.to_string(),
            Self::HumHarmonics => config.hum.harmonics.to_string(),
            Self::HumQ => config.hum.q.to_string(),
            Self::ImpulseEnabled => config.impulse.enabled.to_string(),
            Self::ImpulseMaxWidth => config.impulse.max_width.to_string(),
            Self::ImpulseThreshold => config.impulse.threshold.to_string(),
            Self::ImpulseFloorDbfs => config.impulse.floor_dbfs.to_string(),
            Self::ImpulseTransientProtection => config.impulse.transient_protection.to_string(),
        }
    }
    pub(super) fn apply(self, config: &mut DenoiseConfig, text: &str) -> Result<(), String> {
        match self {
            Self::Mode => {
                config.mode = match text {
                    "off" => DenoiseMode::Off,
                    "stationary" => DenoiseMode::Stationary,
                    "profile" => DenoiseMode::Profile,
                    "adaptive" => DenoiseMode::Adaptive,
                    _ => return Err("Invalid denoise mode".to_owned()),
                }
            }
            Self::Strength => config.strength = parse_processing_number(text)?,
            Self::SpectralGainFloorDb => {
                config.spectral_gain_floor_db = parse_processing_number(text)?
            }
            Self::AttackMs => config.attack_ms = parse_processing_number(text)?,
            Self::ReleaseMs => config.release_ms = parse_processing_number(text)?,
            Self::SmoothingBins => {
                config.smoothing_bins = text.parse::<usize>().map_err(|error| error.to_string())?
            }
            Self::TransientProtection => {
                config.transient_protection = parse_processing_number(text)?
            }
            Self::Wet => config.wet = parse_processing_number(text)?,
            Self::FftSize => {
                config.fft_size = text.parse::<usize>().map_err(|error| error.to_string())?
            }
            Self::StationaryNoiseDbfs => {
                config.stationary_noise_dbfs = parse_processing_number(text)?
            }
            Self::HumEnabled => {
                config.hum.enabled = text.parse::<bool>().map_err(|error| error.to_string())?
            }
            Self::HumFundamentalHz => config.hum.fundamental_hz = parse_processing_number(text)?,
            Self::HumHarmonics => {
                config.hum.harmonics = text.parse::<usize>().map_err(|error| error.to_string())?
            }
            Self::HumQ => config.hum.q = parse_processing_number(text)?,
            Self::ImpulseEnabled => {
                config.impulse.enabled = text.parse::<bool>().map_err(|error| error.to_string())?
            }
            Self::ImpulseMaxWidth => {
                config.impulse.max_width =
                    text.parse::<usize>().map_err(|error| error.to_string())?
            }
            Self::ImpulseThreshold => config.impulse.threshold = parse_processing_number(text)?,
            Self::ImpulseFloorDbfs => config.impulse.floor_dbfs = parse_processing_number(text)?,
            Self::ImpulseTransientProtection => {
                config.impulse.transient_protection = parse_processing_number(text)?
            }
        }
        Ok(())
    }
    pub(super) fn patch(self, text: &str) -> Result<DenoisePatch, String> {
        let mut patch = DenoisePatch::default();
        match self {
            Self::Mode => {
                patch.mode = Some(match text {
                    "off" => DenoiseMode::Off,
                    "stationary" => DenoiseMode::Stationary,
                    "profile" => DenoiseMode::Profile,
                    "adaptive" => DenoiseMode::Adaptive,
                    _ => return Err("Invalid denoise mode".to_owned()),
                })
            }
            Self::Strength => patch.strength = Some(parse_processing_number(text)?),
            Self::SpectralGainFloorDb => {
                patch.spectral_gain_floor_db = Some(parse_processing_number(text)?)
            }
            Self::AttackMs => patch.attack_ms = Some(parse_processing_number(text)?),
            Self::ReleaseMs => patch.release_ms = Some(parse_processing_number(text)?),
            Self::SmoothingBins => {
                patch.smoothing_bins =
                    Some(text.parse::<usize>().map_err(|error| error.to_string())?)
            }
            Self::TransientProtection => {
                patch.transient_protection = Some(parse_processing_number(text)?)
            }
            Self::Wet => patch.wet = Some(parse_processing_number(text)?),
            Self::FftSize => {
                patch.fft_size = Some(text.parse::<usize>().map_err(|error| error.to_string())?)
            }
            Self::StationaryNoiseDbfs => {
                patch.stationary_noise_dbfs = Some(parse_processing_number(text)?)
            }
            Self::HumEnabled => {
                patch.hum_enabled = Some(text.parse::<bool>().map_err(|error| error.to_string())?)
            }
            Self::HumFundamentalHz => {
                patch.hum_fundamental_hz = Some(parse_processing_number(text)?)
            }
            Self::HumHarmonics => {
                patch.hum_harmonics =
                    Some(text.parse::<usize>().map_err(|error| error.to_string())?)
            }
            Self::HumQ => patch.hum_q = Some(parse_processing_number(text)?),
            Self::ImpulseEnabled => {
                patch.impulse_enabled =
                    Some(text.parse::<bool>().map_err(|error| error.to_string())?)
            }
            Self::ImpulseMaxWidth => {
                patch.impulse_max_width =
                    Some(text.parse::<usize>().map_err(|error| error.to_string())?)
            }
            Self::ImpulseThreshold => {
                patch.impulse_threshold = Some(parse_processing_number(text)?)
            }
            Self::ImpulseFloorDbfs => {
                patch.impulse_floor_dbfs = Some(parse_processing_number(text)?)
            }
            Self::ImpulseTransientProtection => {
                patch.impulse_transient_protection = Some(parse_processing_number(text)?)
            }
        }
        Ok(patch)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum NeuralField {
    SpeechInterpreter,
    SeparationInterpreter,
    Worker,
    SpeechManifest,
    SeparationManifest,
    Device,
    Threads,
    SpeechWetMix,
    SpeechAttenuationLimitDb,
    SpeechPostFilter,
    SeparationOverlap,
}

impl NeuralField {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::SpeechInterpreter => "speech_interpreter",
            Self::SeparationInterpreter => "separation_interpreter",
            Self::Worker => "worker",
            Self::SpeechManifest => "speech_manifest",
            Self::SeparationManifest => "separation_manifest",
            Self::Device => "device",
            Self::Threads => "threads",
            Self::SpeechWetMix => "speech_wet_mix",
            Self::SpeechAttenuationLimitDb => "speech_attenuation_limit_db",
            Self::SpeechPostFilter => "speech_post_filter",
            Self::SeparationOverlap => "separation_overlap",
        }
    }
    pub(super) fn text(self, config: &NeuralConfig) -> String {
        match self {
            Self::SpeechInterpreter => config.speech_interpreter.display().to_string(),
            Self::SeparationInterpreter => config.separation_interpreter.display().to_string(),
            Self::Worker => config.worker.display().to_string(),
            Self::SpeechManifest => config
                .speech_manifest
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            Self::SeparationManifest => config
                .separation_manifest
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            Self::Device => match config.device {
                NeuralDevice::Cpu => "cpu",
                NeuralDevice::Cuda => "cuda",
            }
            .to_owned(),
            Self::Threads => config.threads.to_string(),
            Self::SpeechWetMix => config.speech_wet_mix.to_string(),
            Self::SpeechAttenuationLimitDb => config.speech_attenuation_limit_db.to_string(),
            Self::SpeechPostFilter => config.speech_post_filter.to_string(),
            Self::SeparationOverlap => config.separation_overlap.to_string(),
        }
    }
    pub(super) fn apply(self, config: &mut NeuralConfig, text: &str) -> Result<(), String> {
        match self {
            Self::SpeechInterpreter => config.speech_interpreter = PathBuf::from(text),
            Self::SeparationInterpreter => config.separation_interpreter = PathBuf::from(text),
            Self::Worker => config.worker = PathBuf::from(text),
            Self::SpeechManifest => {
                config.speech_manifest = (!text.is_empty()).then(|| PathBuf::from(text))
            }
            Self::SeparationManifest => {
                config.separation_manifest = (!text.is_empty()).then(|| PathBuf::from(text))
            }
            Self::Device => {
                config.device = match text {
                    "cpu" => NeuralDevice::Cpu,
                    "cuda" => NeuralDevice::Cuda,
                    _ => return Err("Invalid neural device".to_owned()),
                }
            }
            Self::Threads => {
                config.threads = text.parse::<u8>().map_err(|error| error.to_string())?
            }
            Self::SpeechWetMix => config.speech_wet_mix = parse_processing_number(text)?,
            Self::SpeechAttenuationLimitDb => {
                config.speech_attenuation_limit_db = parse_processing_number(text)?
            }
            Self::SpeechPostFilter => {
                config.speech_post_filter =
                    text.parse::<bool>().map_err(|error| error.to_string())?
            }
            Self::SeparationOverlap => config.separation_overlap = parse_processing_number(text)?,
        }
        Ok(())
    }
    pub(super) fn patch(self, text: &str) -> Result<NeuralPatch, String> {
        let mut patch = NeuralPatch::default();
        match self {
            Self::SpeechInterpreter => patch.speech_interpreter = Some(PathBuf::from(text)),
            Self::SeparationInterpreter => patch.separation_interpreter = Some(PathBuf::from(text)),
            Self::Worker => patch.worker = Some(PathBuf::from(text)),
            Self::SpeechManifest => {
                patch.speech_manifest = Some((!text.is_empty()).then(|| PathBuf::from(text)))
            }
            Self::SeparationManifest => {
                patch.separation_manifest = Some((!text.is_empty()).then(|| PathBuf::from(text)))
            }
            Self::Device => {
                patch.device = Some(match text {
                    "cpu" => NeuralDevice::Cpu,
                    "cuda" => NeuralDevice::Cuda,
                    _ => return Err("Invalid neural device".to_owned()),
                })
            }
            Self::Threads => {
                patch.threads = Some(text.parse::<u8>().map_err(|error| error.to_string())?)
            }
            Self::SpeechWetMix => patch.speech_wet_mix = Some(parse_processing_number(text)?),
            Self::SpeechAttenuationLimitDb => {
                patch.speech_attenuation_limit_db = Some(parse_processing_number(text)?)
            }
            Self::SpeechPostFilter => {
                patch.speech_post_filter =
                    Some(text.parse::<bool>().map_err(|error| error.to_string())?)
            }
            Self::SeparationOverlap => {
                patch.separation_overlap = Some(parse_processing_number(text)?)
            }
        }
        Ok(patch)
    }
}

fn parse_processing_number(text: &str) -> Result<f32, String> {
    text.parse::<f32>()
        .map_err(|error| error.to_string())
        .and_then(|number| {
            if number.is_finite() {
                Ok(number)
            } else {
                Err("Invalid number".to_owned())
            }
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum FieldKey {
    Denoise(DenoiseField),
    Neural(NeuralField),
    Graph(processing_graph_ui::NodeField),
    GraphExport,
    PresetName,
    PresetPath,
    ProfileStart,
    ProfileEnd,
    VocalGain,
    AccompanimentGain,
}

impl FieldKey {
    fn graph_node(self) -> Option<u64> {
        match self {
            Self::Graph(binding) => Some(binding.node),
            _ => None,
        }
    }
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

// A transient belongs to exactly one visible Monitor panel. Restoration is
// conditional on focus still belonging to this scope, never a newer editor.
struct PresetPopup {
    panel: u64,
    manage: bool,
    scope: rivu_ui::FocusScope,
    restore: Option<FocusHandle>,
}

pub(super) struct AudioProcessing {
    context: FeatureContext,
    state: Rc<GuiSnapshot>,
    page: Page,
    fields: HashMap<FieldKey, Field>,
    error: Option<String>,
    pending: usize,
    commands: VecDeque<Submission>,
    command_active: bool,
    geometry_epoch: u64,
    graph_ui: GraphUi,
    equalizer_ui: EqualizerUi,
    speech_advanced: bool,
    separation_advanced: bool,
    preset_popup: Option<PresetPopup>,
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
            preset_popup: None,
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
        let preset_names_changed = self.preset_popup.is_some()
            && !Arc::ptr_eq(&self.state.system.config, &state.system.config)
            && !self
                .state
                .system
                .config
                .equalizer_presets
                .iter()
                .map(|preset| &preset.name)
                .eq(state
                    .system
                    .config
                    .equalizer_presets
                    .iter()
                    .map(|preset| &preset.name));
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
        if !self.state.system.config.equalizer.enabled {
            let _ = self.set_preset_popup(None, cx);
        }
        if preset_names_changed && let Some(popup) = &self.preset_popup {
            self.notify_preset_panel(popup.panel, cx);
        }
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
        let _ = self.set_preset_popup(None, cx);
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
        layout: &super::workspace::WorkspacePanels,
        visible: bool,
        cx: &mut Context<Self>,
    ) {
        self.retain_equalizer_panels(layout, visible, cx);
        self.preset_bounds
            .retain(|id, _| layout.contains_panel(*id));
        if self
            .preset_popup
            .as_ref()
            .is_some_and(|popup| !visible || !layout.is_panel_active(popup.panel))
        {
            let _ = self.set_preset_popup(None, cx);
        }
        if !visible {
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
            let ack = handle.request_ack(command).await;
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

    fn path_placeholder(&self, key: FieldKey) -> Option<SharedString> {
        let id = match key {
            FieldKey::Neural(NeuralField::SpeechManifest | NeuralField::SeparationManifest) => {
                "processing-manifest-placeholder"
            }
            FieldKey::Neural(
                NeuralField::SpeechInterpreter | NeuralField::SeparationInterpreter,
            ) => "processing-interpreter-placeholder",
            FieldKey::Neural(NeuralField::Worker) => "processing-worker-placeholder",
            FieldKey::PresetPath => "processing-preset-path-placeholder",
            FieldKey::GraphExport => "processing-export-path-placeholder",
            FieldKey::Graph(binding) => binding.field.path_placeholder()?,
            _ => return None,
        };
        Some(self.tr(id))
    }

    fn path_help(&self, key: FieldKey) -> Option<SharedString> {
        let id = match key {
            FieldKey::Neural(NeuralField::SpeechManifest | NeuralField::SeparationManifest) => {
                "processing-manifest-path-help"
            }
            FieldKey::Neural(NeuralField::SpeechInterpreter) => {
                "processing-speech-interpreter-help"
            }
            FieldKey::Neural(NeuralField::SeparationInterpreter) => {
                "processing-separation-interpreter-help"
            }
            FieldKey::Neural(NeuralField::Worker) => "processing-worker-path-help",
            FieldKey::Graph(binding) => binding.field.path_help()?,
            _ => return None,
        };
        Some(self.tr(id))
    }

    fn field(&mut self, key: FieldKey, label: &str, value: String, cx: &mut Context<Self>) {
        if let Some(field) = self.fields.get_mut(&key) {
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
        let subscription = self.subscribe_field(&input, key, cx);
        self.fields.insert(
            key,
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
        name: FieldKey,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe(input, move |this, _, event, cx| match event {
            InputEvent::Changed { user: true, .. } => {
                if let Some(field) = this.fields.get_mut(&name) {
                    field.dirty = true;
                }
                if name == FieldKey::PresetName {
                    this.notify_preset_labels(cx);
                }
                cx.notify();
            }
            InputEvent::Changed { user: false, .. } if name == FieldKey::PresetName => {
                this.notify_preset_labels(cx);
            }
            InputEvent::Submit => this.apply_field(name, cx),
            _ => {}
        })
    }

    fn sync_fields(&mut self, cx: &mut Context<Self>) {
        for field in [
            DenoiseField::Strength,
            DenoiseField::SpectralGainFloorDb,
            DenoiseField::AttackMs,
            DenoiseField::ReleaseMs,
            DenoiseField::SmoothingBins,
            DenoiseField::TransientProtection,
            DenoiseField::Wet,
            DenoiseField::FftSize,
            DenoiseField::StationaryNoiseDbfs,
            DenoiseField::HumFundamentalHz,
            DenoiseField::HumHarmonics,
            DenoiseField::HumQ,
            DenoiseField::ImpulseMaxWidth,
            DenoiseField::ImpulseThreshold,
            DenoiseField::ImpulseFloorDbfs,
            DenoiseField::ImpulseTransientProtection,
        ] {
            let value = field.text(&self.state.system.config.denoise);
            let (group, name) = field.name().split_once('.').unwrap_or(("", field.name()));
            let label = if group.is_empty() {
                self.parameter_label(name)
            } else {
                format!(
                    "{} · {}",
                    self.parameter_label(group),
                    self.parameter_label(name)
                )
            };
            self.field(FieldKey::Denoise(field), &label, value, cx);
        }
        for field in [
            NeuralField::SpeechInterpreter,
            NeuralField::SeparationInterpreter,
            NeuralField::Worker,
            NeuralField::SpeechManifest,
            NeuralField::SeparationManifest,
            NeuralField::Threads,
            NeuralField::SpeechWetMix,
            NeuralField::SpeechAttenuationLimitDb,
            NeuralField::SeparationOverlap,
        ] {
            let value = field.text(&self.state.system.config.neural);
            self.field(
                FieldKey::Neural(field),
                &self.parameter_label(field.name()),
                value,
                cx,
            );
        }
        for (key, label, value) in [
            (FieldKey::PresetName, "processing-preset-name", ""),
            (FieldKey::PresetPath, "processing-preset-path", ""),
            (FieldKey::ProfileStart, "processing-profile-start", "0"),
            (FieldKey::ProfileEnd, "processing-profile-end", "1"),
            (FieldKey::VocalGain, "processing-vocal-gain", "0"),
            (
                FieldKey::AccompanimentGain,
                "processing-accompaniment-gain",
                "0",
            ),
        ] {
            if !self.fields.contains_key(&key) {
                self.field(key, &self.tr(label), value.to_owned(), cx);
            }
        }
    }

    fn text(&self, key: FieldKey, cx: &App) -> String {
        self.fields
            .get(&key)
            .map(|f| f.input.read(cx).text().trim().to_owned())
            .unwrap_or_default()
    }

    fn apply_field(&mut self, key: FieldKey, cx: &mut Context<Self>) {
        if matches!(key, FieldKey::Graph(_)) {
            self.apply_graph_field(key, cx);
            return;
        }
        let value = self.text(key, cx);
        let result = self.field_command(key, &value);
        match result {
            Ok(Some(command)) => {
                let submitted = self
                    .fields
                    .get(&key)
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

    fn field_command(&self, key: FieldKey, value: &str) -> Result<Option<Command>, String> {
        match key {
            FieldKey::Denoise(field) => Ok(Some(Command::DenoiseEdit {
                patch: Box::new(field.patch(value)?),
            })),
            FieldKey::Neural(field) => Ok(Some(Command::NeuralEdit {
                patch: Box::new(field.patch(value)?),
            })),
            _ => Ok(None),
        }
    }

    fn patch_denoise(&mut self, patch: DenoisePatch, cx: &mut Context<Self>) {
        self.send(
            Command::DenoiseEdit {
                patch: Box::new(patch),
            },
            cx,
        );
    }

    fn patch_neural(&mut self, patch: NeuralPatch, cx: &mut Context<Self>) {
        self.send(
            Command::NeuralEdit {
                patch: Box::new(patch),
            },
            cx,
        );
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
            "spectral_gain_floor_db" => "processing-spectral-gain-floor",
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
        key: FieldKey,
        label: impl Into<SharedString>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let field = &self.fields[&key];
        let label: SharedString = label.into();
        Self::parameter_row(
            caption(theme, label.clone())
                .id(SharedString::from(format!("field-label-{key:?}")))
                .tooltip(rivu_ui::text_tooltip(self.path_help(key).unwrap_or(label))),
            field.input.clone(),
        )
        .opacity(if enabled { 1. } else { 0.5 })
    }

    fn parameter_row(label: impl IntoElement + Styled, control: impl IntoElement) -> Div {
        row()
            .min_w_0()
            .items_center()
            .gap_2()
            .child(label.w(relative(0.42)).flex_shrink_0().min_w_0().truncate())
            .child(div().flex_1().min_w_0().child(control))
    }

    fn set_preset_popup(
        &mut self,
        popup: Option<PresetPopup>,
        cx: &mut Context<Self>,
    ) -> Option<PresetPopup> {
        let next_panel = popup.as_ref().map(|popup| popup.panel);
        let previous = std::mem::replace(&mut self.preset_popup, popup);
        let previous_panel = previous.as_ref().map(|popup| popup.panel);
        if let Some(panel) = previous_panel {
            self.notify_preset_panel(panel, cx);
        }
        if let Some(panel) = next_panel.filter(|panel| Some(*panel) != previous_panel) {
            self.notify_preset_panel(panel, cx);
        }
        previous
    }

    fn close_preset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(popup) = self.set_preset_popup(None, cx)
            && popup.scope.handle().contains_focused(window, cx)
            && let Some(origin) = popup.restore
        {
            origin.focus(window, cx);
        }
        cx.notify();
    }

    fn toggle_preset(
        &mut self,
        panel: u64,
        manage: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.state.system.config.equalizer.enabled {
            return;
        }
        if self
            .preset_popup
            .as_ref()
            .is_some_and(|popup| popup.panel == panel && popup.manage == manage)
        {
            self.close_preset(window, cx);
            return;
        }
        let restore = self
            .preset_popup
            .as_ref()
            .and_then(|popup| popup.restore.clone())
            .or_else(|| window.focused(cx));
        let scope = rivu_ui::FocusScope::new(cx);
        scope.focus(window, cx);
        let _ = self.set_preset_popup(
            Some(PresetPopup {
                panel,
                manage,
                scope,
                restore,
            }),
            cx,
        );
        cx.notify();
    }

    fn preset_view(&mut self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let enabled = self.state.system.config.equalizer.enabled;
        for key in [FieldKey::PresetName, FieldKey::PresetPath] {
            if let Some(field) = self.fields.get(&key) {
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
        let name = self.text(FieldKey::PresetName, cx);
        let label = if name.is_empty() {
            self.tr("processing-presets")
        } else {
            name.into()
        };
        let trigger = activate_control(
            disabled_control(
                theme,
                dropdown_trigger(
                    theme,
                    ("eq-presets", panel),
                    label,
                    self.preset_popup
                        .as_ref()
                        .is_some_and(|popup| popup.panel == panel && !popup.manage),
                    cx,
                ),
                !enabled,
            ),
            cx,
            move |this, window, cx| this.toggle_preset(panel, false, window, cx),
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
                    .enabled(enabled)
                    .toggled(
                        self.preset_popup
                            .as_ref()
                            .is_some_and(|popup| popup.panel == panel && popup.manage),
                    )
                    .build(
                        IconButtonStyle::Quiet,
                        IconButtonDensity::Compact,
                        cx,
                        move |this, window, cx| this.toggle_preset(panel, true, window, cx),
                    ),
                ),
        );
        if let Some(popup) = self
            .preset_popup
            .as_ref()
            .filter(|popup| popup.panel == panel && !popup.manage)
            && let Some(bounds) = bounds.get()
        {
            let mut choices = popup
                .scope
                .menu_surface(Popover::surface(theme, ("eq-preset-choices", panel)))
                .role(Role::Menu)
                .flex()
                .flex_col()
                .max_h(rems(12.))
                .overflow_y_scroll()
                .p_1()
                .w(bounds.size.width)
                .on_mouse_down_out(
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        if !bounds.contains(&event.position) {
                            this.close_preset(window, cx);
                            cx.stop_propagation();
                        }
                    }),
                )
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
                        this.close_preset(window, cx);
                        cx.stop_propagation();
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
                    move |this, window, cx| {
                        if !this.state.system.config.equalizer.enabled {
                            return;
                        }
                        if let Some(field) = this.fields.get_mut(&FieldKey::PresetName) {
                            field
                                .input
                                .update(cx, |input, cx| input.set_text(name.clone(), cx));
                            field.dirty = false;
                        }
                        this.close_preset(window, cx);
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
        if let Some(popup) = self
            .preset_popup
            .as_ref()
            .filter(|popup| popup.panel == panel && popup.manage)
            && let Some(bounds) = bounds.get()
        {
            content = content.child(
                deferred(
                    anchored()
                        .position(bounds.bottom_left())
                        .snap_to_window()
                        .child(
                            popup
                                .scope
                                .surface(Popover::surface(theme, ("eq-preset-management", panel)))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .p_2()
                                .w(rems(24.))
                                .max_h(rems(24.))
                                .overflow_y_scroll()
                                .on_mouse_down_out(cx.listener(
                                    move |this, event: &MouseDownEvent, window, cx| {
                                        if !bounds.contains(&event.position) {
                                            this.close_preset(window, cx);
                                            cx.stop_propagation();
                                        }
                                    },
                                ))
                                .on_key_down(cx.listener(
                                    |this, event: &KeyDownEvent, window, cx| {
                                        if event.keystroke.key == "escape"
                                            && !event.keystroke.modifiers.modified()
                                        {
                                            this.close_preset(window, cx);
                                            cx.stop_propagation();
                                        }
                                    },
                                ))
                                .child(self.field_view(
                                    FieldKey::PresetName,
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
                                                name: this.text(FieldKey::PresetName, cx),
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
                                                name: this.text(FieldKey::PresetName, cx),
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
                                                name: this.text(FieldKey::PresetName, cx),
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
                                    FieldKey::PresetPath,
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
                                                name: this.text(FieldKey::PresetName, cx),
                                                path: PathBuf::from(
                                                    this.text(FieldKey::PresetPath, cx),
                                                ),
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
                                                name: this.text(FieldKey::PresetName, cx),
                                                path: PathBuf::from(
                                                    this.text(FieldKey::PresetPath, cx),
                                                ),
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

    fn parameter_controls(&self, fields: &[DenoiseField], cx: &mut Context<Self>) -> Div {
        let mut content = column().min_w_0().gap_1();
        for &field in fields {
            let name = field.name().rsplit('.').next().unwrap_or(field.name());
            content = content.child(self.field_view(
                FieldKey::Denoise(field),
                self.parameter_label(name),
                true,
                cx,
            ));
        }
        content
    }

    fn restoration_view(&self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let config = &self.state.system.config.denoise;
        let mut modes = row().gap_1();
        for (mode, slug, label) in [
            (DenoiseMode::Off, "off", "processing-off"),
            (
                DenoiseMode::Stationary,
                "stationary",
                "processing-stationary",
            ),
            (DenoiseMode::Profile, "profile", "processing-profile"),
            (DenoiseMode::Adaptive, "adaptive", "processing-adaptive"),
        ] {
            modes = modes.child(
                button(
                    SharedString::from(format!("denoise-mode-{slug}")),
                    self.tr(label),
                    cx,
                    move |this, _, cx| {
                        this.patch_denoise(
                            DenoisePatch {
                                mode: Some(mode),
                                ..Default::default()
                            },
                            cx,
                        )
                    },
                )
                .flex_shrink_0()
                .whitespace_nowrap()
                .when(config.mode == mode, |view| view.bg(rgb(theme.selected))),
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
        if config.mode != DenoiseMode::Off {
            content = content.child(self.parameter_controls(
                &[
                    DenoiseField::Strength,
                    DenoiseField::SpectralGainFloorDb,
                    DenoiseField::AttackMs,
                    DenoiseField::ReleaseMs,
                    DenoiseField::SmoothingBins,
                    DenoiseField::TransientProtection,
                    DenoiseField::FftSize,
                ],
                cx,
            ));
        }
        if matches!(config.mode, DenoiseMode::Stationary | DenoiseMode::Adaptive) {
            content =
                content.child(self.parameter_controls(&[DenoiseField::StationaryNoiseDbfs], cx));
        }
        if config.mode == DenoiseMode::Profile {
            content = content
                .child(self.field_view(
                    FieldKey::ProfileStart,
                    self.tr("processing-profile-start"),
                    true,
                    cx,
                ))
                .child(self.field_view(
                    FieldKey::ProfileEnd,
                    self.tr("processing-profile-end"),
                    true,
                    cx,
                ));
        }
        let mut content = column().min_w_0().gap_3().child(rivu_ui::section_group(
            theme,
            self.tr("processing-noise-reduction"),
            content,
        ));
        for (group, enabled, toggle, fields) in [
            (
                "hum",
                config.hum.enabled,
                DenoiseField::HumEnabled,
                &[
                    DenoiseField::HumFundamentalHz,
                    DenoiseField::HumHarmonics,
                    DenoiseField::HumQ,
                ][..],
            ),
            (
                "impulse",
                config.impulse.enabled,
                DenoiseField::ImpulseEnabled,
                &[
                    DenoiseField::ImpulseMaxWidth,
                    DenoiseField::ImpulseThreshold,
                    DenoiseField::ImpulseFloorDbfs,
                    DenoiseField::ImpulseTransientProtection,
                ][..],
            ),
        ] {
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
                        let patch = match toggle {
                            DenoiseField::HumEnabled => DenoisePatch {
                                hum_enabled: Some(enabled),
                                ..Default::default()
                            },
                            DenoiseField::ImpulseEnabled => DenoisePatch {
                                impulse_enabled: Some(enabled),
                                ..Default::default()
                            },
                            _ => unreachable!("restoration toggle"),
                        };
                        this.patch_denoise(patch, cx);
                    },
                ));
            if enabled {
                section = section.child(self.parameter_controls(fields, cx));
            }
            content = content.child(section);
        }
        content.child(
            div()
                .pt_2()
                .border_t_1()
                .border_color(rgb(theme.border))
                .child(self.parameter_controls(&[DenoiseField::Wet], cx)),
        )
    }

    fn neural_fields(&self, fields: &[NeuralField], cx: &mut Context<Self>) -> Div {
        let mut content = column().min_w_0().gap_1();
        for &field in fields {
            content = content.child(self.field_view(
                FieldKey::Neural(field),
                self.parameter_label(field.name()),
                true,
                cx,
            ));
        }
        content
    }

    fn neural_view(&self, panel: u64, speech: bool, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let neural = &self.state.system.config.neural;
        let open = if speech {
            self.speech_advanced
        } else {
            self.separation_advanced
        };
        let mut parameters = self.neural_fields(
            if speech {
                &[
                    NeuralField::SpeechWetMix,
                    NeuralField::SpeechAttenuationLimitDb,
                ][..]
            } else {
                &[NeuralField::SeparationOverlap][..]
            },
            cx,
        );
        if speech {
            parameters = parameters.child(Self::parameter_row(
                caption(theme, self.tr("processing-speech-post-filter-label"))
                    .id("speech-post-filter-label")
                    .tooltip(rivu_ui::text_tooltip(
                        self.tr("processing-speech-post-filter-help"),
                    )),
                checkbox(
                    "neural.speech_post_filter",
                    self.tr("processing-speech-post-filter-model"),
                    neural.speech_post_filter,
                    true,
                    cx,
                    |this, value, cx| {
                        this.patch_neural(
                            NeuralPatch {
                                speech_post_filter: Some(value),
                                ..Default::default()
                            },
                            cx,
                        )
                    },
                )
                .aria_label(self.tr("processing-speech-post-filter")),
            ));
        }
        let model = self.neural_fields(
            if speech {
                &[NeuralField::SpeechManifest, NeuralField::SpeechInterpreter][..]
            } else {
                &[
                    NeuralField::SeparationManifest,
                    NeuralField::SeparationInterpreter,
                ][..]
            },
            cx,
        );
        let mut model = column().min_w_0().gap_2().child(model).child(
            div()
                .mt_2()
                .pt_2()
                .border_t_1()
                .border_color(rgb(theme.border))
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
                )),
        );
        if open {
            let mut runtime = self.neural_fields(&[NeuralField::Worker, NeuralField::Threads], cx);
            let mut devices = row().gap_1().flex_wrap();
            for (device, value, label) in [
                (NeuralDevice::Cpu, "cpu", "CPU"),
                (NeuralDevice::Cuda, "cuda", "CUDA"),
            ] {
                devices = devices.child(
                    button(
                        SharedString::from(format!("neural-device-{value}")),
                        label,
                        cx,
                        move |this, _, cx| {
                            this.patch_neural(
                                NeuralPatch {
                                    device: Some(device),
                                    ..Default::default()
                                },
                                cx,
                            )
                        },
                    )
                    .when(neural.device == device, |v| v.bg(rgb(theme.selected))),
                );
            }
            runtime = runtime.child(Self::parameter_row(
                caption(theme, self.tr("processing-device")),
                devices,
            ));
            model = model.child(runtime).child(
                caption(theme, self.tr("processing-runtime-shared-help"))
                    .text_color(rgb(theme.muted)),
            );
        }
        column()
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
    }

    fn input_track(&self) -> Option<&Track> {
        self.state.current_track()
    }

    fn page_parameter_key(&self, key: FieldKey) -> bool {
        matches!(
            (self.page, key),
            (Page::Restoration, FieldKey::Denoise(_))
                | (
                    Page::Speech,
                    FieldKey::Neural(
                        NeuralField::SpeechInterpreter
                            | NeuralField::SpeechManifest
                            | NeuralField::SpeechWetMix
                            | NeuralField::SpeechAttenuationLimitDb
                            | NeuralField::Worker
                            | NeuralField::Threads,
                    ),
                )
                | (
                    Page::Separation,
                    FieldKey::Neural(
                        NeuralField::SeparationInterpreter
                            | NeuralField::SeparationManifest
                            | NeuralField::SeparationOverlap
                            | NeuralField::Worker
                            | NeuralField::Threads,
                    ),
                )
        )
    }

    fn page_has_parameter_drafts(&self) -> bool {
        self.fields
            .iter()
            .any(|(key, field)| field.dirty && self.page_parameter_key(*key))
    }

    fn apply_page_parameters(&mut self, cx: &mut Context<Self>) {
        let mut submissions = Vec::new();
        for (key, field) in &self.fields {
            if !field.dirty || !self.page_parameter_key(*key) {
                continue;
            }
            let value = field.input.read(cx).text().trim().to_owned();
            match self.field_command(*key, &value) {
                Ok(Some(command)) => submissions.push((*key, command, field.input.clone(), value)),
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
        submissions.sort_unstable_by_key(|submission| submission.0);
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
            && self.state.system.config.denoise.mode == DenoiseMode::Profile
        {
            let range = (
                self.text(FieldKey::ProfileStart, cx).parse::<f64>(),
                self.text(FieldKey::ProfileEnd, cx).parse::<f64>(),
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

    fn task_settings(&self, task: &rivu_protocol::model::ProcessingTaskStatus, cx: &App) -> Div {
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
                            FieldKey::VocalGain,
                            self.tr("processing-vocal-gain"),
                            true,
                            cx,
                        ))
                        .child(self.field_view(
                            FieldKey::AccompanimentGain,
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
            AuditionMode::Vocals => (self.text(FieldKey::VocalGain, cx).parse::<f32>(), Ok(0.)),
            AuditionMode::Accompaniment => (
                Ok(0.),
                self.text(FieldKey::AccompanimentGain, cx).parse::<f32>(),
            ),
            AuditionMode::Mix => (
                self.text(FieldKey::VocalGain, cx).parse::<f32>(),
                self.text(FieldKey::AccompanimentGain, cx).parse::<f32>(),
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
                        let _ = this.set_preset_popup(None, cx);
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
            rivu_protocol::model::PlaybackStatus::Playing => "processing-workspace-playing",
            rivu_protocol::model::PlaybackStatus::Paused => "processing-workspace-paused",
            rivu_protocol::model::PlaybackStatus::Stopped => "processing-workspace-stopped",
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
                            rivu_protocol::model::GraphExecutionMode::Auto => {
                                "processing-graph-auto"
                            }
                            rivu_protocol::model::GraphExecutionMode::Realtime => {
                                "processing-graph-realtime"
                            }
                            rivu_protocol::model::GraphExecutionMode::Hybrid => {
                                "processing-graph-hybrid"
                            }
                            rivu_protocol::model::GraphExecutionMode::Offline => {
                                "processing-graph-cached"
                            }
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
        let bypassed = !self.state.system.config.equalizer.enabled
            && self.state.system.processing_audition.is_none();
        selected_audio = selected_audio.child(monitor_status).child(
            IconButton::new(
                ("processing-bypass", panel),
                "󰓅",
                self.tr(if bypassed {
                    "processing-workspace-enable-monitor"
                } else {
                    "processing-workspace-bypass"
                }),
            )
            .toggled(bypassed)
            .selected_fill(false)
            .build(
                IconButtonStyle::Quiet,
                IconButtonDensity::Compact,
                cx,
                |this, _, cx| {
                    if !this.state.system.config.equalizer.enabled
                        && this.state.system.processing_audition.is_none()
                    {
                        this.send(
                            Command::EqualizerEdit {
                                edit: EqualizerEdit::Enabled(true),
                            },
                            cx,
                        );
                    } else {
                        this.send(Command::ProcessingBypass, cx);
                    }
                },
            )
            .text_color(rgb(if bypassed {
                theme.warning
            } else {
                theme.accent
            })),
        );
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

#[cfg(test)]
mod field_tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn denoise_bindings_distinguish_nested_fields_and_preserve_other_values() {
        let mut config = DenoiseConfig::default();
        let original = config.clone();
        DenoiseField::ImpulseTransientProtection
            .apply(&mut config, "0.25")
            .unwrap();
        assert_eq!(config.impulse.transient_protection, 0.25);
        assert_eq!(config.transient_protection, original.transient_protection);
        assert_eq!(config.hum, original.hum);
        let patch = DenoiseField::ImpulseTransientProtection
            .patch("0.25")
            .unwrap();
        assert_eq!(patch.impulse_transient_protection, Some(0.25));
        assert_eq!(patch.transient_protection, None);
        DenoiseField::SpectralGainFloorDb
            .apply(&mut config, "-18.5")
            .unwrap();
        assert_eq!(config.spectral_gain_floor_db, -18.5);
        assert_eq!(DenoiseField::SpectralGainFloorDb.text(&config), "-18.5");
        assert_eq!(config.strength, original.strength);
        let patch = DenoiseField::SpectralGainFloorDb.patch("-31.5").unwrap();
        assert_eq!(patch.spectral_gain_floor_db, Some(-31.5));
        assert_eq!(patch.strength, None);
        assert_eq!(patch.impulse_floor_dbfs, None);
        for invalid in ["NaN", "inf", "1e", ""] {
            let previous = config.clone();
            assert!(DenoiseField::Strength.apply(&mut config, invalid).is_err());
            assert_eq!(config, previous);
            assert!(DenoiseField::Strength.patch(invalid).is_err());
        }
        assert!(DenoiseField::FftSize.patch("1.5").is_err());
    }

    #[test]
    fn neural_bindings_keep_manifest_clear_distinct_from_unchanged_and_paths_literal() {
        let mut config = NeuralConfig {
            speech_manifest: Some(PathBuf::from("old.json")),
            ..Default::default()
        };
        NeuralField::SpeechManifest.apply(&mut config, "").unwrap();
        assert_eq!(config.speech_manifest, None);
        let clear = NeuralField::SpeechManifest.patch("").unwrap();
        assert_eq!(clear.speech_manifest, Some(None));
        assert_eq!(clear.separation_manifest, None);
        let path = "runtime.with.dots/worker with spaces.py";
        NeuralField::Worker.apply(&mut config, path).unwrap();
        assert_eq!(NeuralField::Worker.text(&config), path);
        assert_eq!(
            NeuralField::Worker.patch(path).unwrap().worker,
            Some(PathBuf::from(path))
        );
        NeuralField::SpeechWetMix.apply(&mut config, "0.6").unwrap();
        assert_eq!(NeuralField::SpeechWetMix.text(&config), "0.6");
        assert!(NeuralField::SpeechWetMix.patch("NaN").is_err());
        assert!(NeuralField::Threads.patch("256").is_err());
    }
}
