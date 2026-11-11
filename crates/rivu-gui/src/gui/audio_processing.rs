//! Dockable processing controls. Core owns committed parameters and task results;
//! this owner retains only field drafts, selection and cancellable pointer intent.
use super::{feature::FeatureContext, interaction::WindowGeometry};
use crate::projection::GuiSnapshot;
use gpui::{prelude::*, *};
use rivu_dsp::denoise::{DenoiseConfig, DenoiseMode};
use rivu_format::config::{NeuralConfig, NeuralDevice};
use rivu_protocol::model::{
    AuditionMode, Command, DenoisePatch, EqualizerEdit, NeuralPatch, NoiseSelection,
    ProcessingOperation, ProcessingPhase, ProcessingToggle, Track,
};
use rivu_ui::{
    IconButton, IconButtonDensity, IconButtonStyle, Popover, activate_control, button, caption,
    checkbox, column, disabled_control, dropdown_trigger, icon,
    input::{Input, InputEvent},
    menu_item, row, scrollable_area,
};
use std::{
    cell::Cell,
    collections::{HashMap, VecDeque},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
};

mod graph_editor;
use graph_editor::GraphEditorState;
mod equalizer_editor;
use equalizer_editor::EqualizerEditors;
mod results;
use results::ResultsState;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    Graph,
    Equalizer,
    Restoration,
    Speech,
    Separation,
    Results,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldInputKind {
    Number,
    Choice,
    Boolean,
    Path,
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
    fn label_id(self) -> &'static str {
        match self {
            Self::Mode => "processing-denoise-mode",
            Self::Strength => "processing-strength",
            Self::SpectralGainFloorDb => "processing-spectral-gain-floor",
            Self::AttackMs => "processing-attack",
            Self::ReleaseMs => "processing-release",
            Self::SmoothingBins => "processing-smoothing",
            Self::TransientProtection | Self::ImpulseTransientProtection => "processing-transients",
            Self::Wet => "processing-wet",
            Self::FftSize => "processing-fft",
            Self::StationaryNoiseDbfs => "processing-stationary-floor",
            Self::HumEnabled | Self::ImpulseEnabled => "processing-enabled",
            Self::HumFundamentalHz => "processing-hum-frequency",
            Self::HumHarmonics => "processing-harmonics",
            Self::HumQ => "processing-q",
            Self::ImpulseMaxWidth => "processing-impulse-width",
            Self::ImpulseThreshold => "processing-threshold",
            Self::ImpulseFloorDbfs => "processing-impulse-floor",
        }
    }

    fn group_label_id(self) -> Option<&'static str> {
        match self {
            Self::HumEnabled | Self::HumFundamentalHz | Self::HumHarmonics | Self::HumQ => {
                Some("processing-hum")
            }
            Self::ImpulseEnabled
            | Self::ImpulseMaxWidth
            | Self::ImpulseThreshold
            | Self::ImpulseFloorDbfs
            | Self::ImpulseTransientProtection => Some("processing-impulse"),
            _ => None,
        }
    }

    fn choices(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Mode => &[
                ("off", "processing-off"),
                ("stationary", "processing-stationary"),
                ("profile", "processing-profile"),
                ("adaptive", "processing-adaptive"),
            ],
            Self::FftSize => &[
                ("256", "256"),
                ("512", "512"),
                ("1024", "1024"),
                ("2048", "2048"),
                ("4096", "4096"),
                ("8192", "8192"),
            ],
            _ => &[],
        }
    }

    fn input_kind(self) -> FieldInputKind {
        match self {
            Self::Mode | Self::FftSize => FieldInputKind::Choice,
            Self::HumEnabled | Self::ImpulseEnabled => FieldInputKind::Boolean,
            _ => FieldInputKind::Number,
        }
    }

    fn numeric_step(self) -> Option<f64> {
        if self.input_kind() != FieldInputKind::Number {
            return None;
        }
        Some(match self {
            Self::SmoothingBins
            | Self::HumHarmonics
            | Self::ImpulseMaxWidth
            | Self::HumFundamentalHz => 1.,
            _ => 0.1,
        })
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
        // Parsing is shared with standalone patches; graph validation remains
        // the caller's responsibility after applying all selected fields.
        let patch = self.patch(text)?;
        match self {
            Self::Mode => config.mode = patch.mode.expect("parsed mode"),
            Self::Strength => config.strength = patch.strength.expect("parsed strength"),
            Self::SpectralGainFloorDb => {
                config.spectral_gain_floor_db =
                    patch.spectral_gain_floor_db.expect("parsed gain floor");
            }
            Self::AttackMs => config.attack_ms = patch.attack_ms.expect("parsed attack"),
            Self::ReleaseMs => config.release_ms = patch.release_ms.expect("parsed release"),
            Self::SmoothingBins => {
                config.smoothing_bins = patch.smoothing_bins.expect("parsed smoothing")
            }
            Self::TransientProtection => {
                config.transient_protection = patch
                    .transient_protection
                    .expect("parsed transient protection");
            }
            Self::Wet => config.wet = patch.wet.expect("parsed wet mix"),
            Self::FftSize => config.fft_size = patch.fft_size.expect("parsed FFT size"),
            Self::StationaryNoiseDbfs => {
                config.stationary_noise_dbfs =
                    patch.stationary_noise_dbfs.expect("parsed noise floor");
            }
            Self::HumEnabled => config.hum.enabled = patch.hum_enabled.expect("parsed hum enabled"),
            Self::HumFundamentalHz => {
                config.hum.fundamental_hz = patch.hum_fundamental_hz.expect("parsed hum frequency");
            }
            Self::HumHarmonics => {
                config.hum.harmonics = patch.hum_harmonics.expect("parsed harmonics")
            }
            Self::HumQ => config.hum.q = patch.hum_q.expect("parsed hum Q"),
            Self::ImpulseEnabled => {
                config.impulse.enabled = patch.impulse_enabled.expect("parsed impulse enabled")
            }
            Self::ImpulseMaxWidth => {
                config.impulse.max_width = patch.impulse_max_width.expect("parsed impulse width")
            }
            Self::ImpulseThreshold => {
                config.impulse.threshold =
                    patch.impulse_threshold.expect("parsed impulse threshold")
            }
            Self::ImpulseFloorDbfs => {
                config.impulse.floor_dbfs = patch.impulse_floor_dbfs.expect("parsed impulse floor")
            }
            Self::ImpulseTransientProtection => {
                config.impulse.transient_protection = patch
                    .impulse_transient_protection
                    .expect("parsed impulse transient protection");
            }
        }
        Ok(())
    }
    pub(super) fn patch(self, text: &str) -> Result<DenoisePatch, String> {
        let mut patch = DenoisePatch::default();
        self.patch_into(&mut patch, text)?;
        Ok(patch)
    }
    fn patch_into(self, patch: &mut DenoisePatch, text: &str) -> Result<(), String> {
        let text = text.trim();
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
        Ok(())
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
    fn label_id(self) -> &'static str {
        match self {
            Self::SpeechInterpreter => "processing-speech-interpreter",
            Self::SeparationInterpreter => "processing-separation-interpreter",
            Self::Worker => "processing-worker",
            Self::SpeechManifest => "processing-speech-manifest",
            Self::SeparationManifest => "processing-separation-manifest",
            Self::Device => "processing-device",
            Self::Threads => "processing-threads",
            Self::SpeechWetMix => "processing-speech-wet",
            Self::SpeechAttenuationLimitDb => "processing-speech-attenuation",
            Self::SpeechPostFilter => "processing-speech-post-filter",
            Self::SeparationOverlap => "processing-separation-overlap",
        }
    }

    fn choices(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Device => &[
                ("cpu", "processing-fields-cpu"),
                ("cuda", "processing-fields-cuda"),
            ],
            _ => &[],
        }
    }

    fn input_kind(self) -> FieldInputKind {
        match self {
            Self::Device => FieldInputKind::Choice,
            Self::SpeechPostFilter => FieldInputKind::Boolean,
            Self::SpeechInterpreter
            | Self::SeparationInterpreter
            | Self::Worker
            | Self::SpeechManifest
            | Self::SeparationManifest => FieldInputKind::Path,
            _ => FieldInputKind::Number,
        }
    }

    fn numeric_step(self) -> Option<f64> {
        match self {
            Self::Threads => Some(1.),
            Self::SpeechWetMix | Self::SeparationOverlap => Some(0.01),
            Self::SpeechAttenuationLimitDb => Some(0.1),
            _ => None,
        }
    }

    fn path_placeholder(self) -> Option<&'static str> {
        Some(match self {
            Self::SpeechManifest | Self::SeparationManifest => "processing-manifest-placeholder",
            Self::SpeechInterpreter | Self::SeparationInterpreter => {
                "processing-interpreter-placeholder"
            }
            Self::Worker => "processing-worker-placeholder",
            _ => return None,
        })
    }

    fn path_help(self) -> Option<&'static str> {
        Some(match self {
            Self::SpeechManifest | Self::SeparationManifest => "processing-manifest-path-help",
            Self::SpeechInterpreter => "processing-speech-interpreter-help",
            Self::SeparationInterpreter => "processing-separation-interpreter-help",
            Self::Worker => "processing-worker-path-help",
            _ => return None,
        })
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
        let patch = self.patch(text)?;
        match self {
            Self::SpeechInterpreter => {
                config.speech_interpreter =
                    patch.speech_interpreter.expect("parsed speech interpreter")
            }
            Self::SeparationInterpreter => {
                config.separation_interpreter = patch
                    .separation_interpreter
                    .expect("parsed separation interpreter")
            }
            Self::Worker => config.worker = patch.worker.expect("parsed worker"),
            Self::SpeechManifest => {
                config.speech_manifest = patch.speech_manifest.expect("parsed speech manifest")
            }
            Self::SeparationManifest => {
                config.separation_manifest = patch
                    .separation_manifest
                    .expect("parsed separation manifest")
            }
            Self::Device => config.device = patch.device.expect("parsed device"),
            Self::Threads => config.threads = patch.threads.expect("parsed threads"),
            Self::SpeechWetMix => {
                config.speech_wet_mix = patch.speech_wet_mix.expect("parsed speech wet mix")
            }
            Self::SpeechAttenuationLimitDb => {
                config.speech_attenuation_limit_db = patch
                    .speech_attenuation_limit_db
                    .expect("parsed speech attenuation");
            }
            Self::SpeechPostFilter => {
                config.speech_post_filter =
                    patch.speech_post_filter.expect("parsed speech postfilter")
            }
            Self::SeparationOverlap => {
                config.separation_overlap =
                    patch.separation_overlap.expect("parsed separation overlap")
            }
        }
        Ok(())
    }
    pub(super) fn patch(self, text: &str) -> Result<NeuralPatch, String> {
        let mut patch = NeuralPatch::default();
        self.patch_into(&mut patch, text)?;
        Ok(patch)
    }
    fn patch_into(self, patch: &mut NeuralPatch, text: &str) -> Result<(), String> {
        let text = if self.input_kind() == FieldInputKind::Path {
            text
        } else {
            text.trim()
        };
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
        Ok(())
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
    Graph(graph_editor::NodeField),
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

    fn value_text(self, text: &str) -> &str {
        match self {
            Self::Neural(field) if field.input_kind() == FieldInputKind::Path => text,
            Self::Graph(binding) if binding.field.path_placeholder().is_some() => text,
            Self::GraphExport | Self::PresetPath => text,
            _ => text.trim(),
        }
    }
}

fn page_parameter_command<'a>(
    page: Page,
    fields: impl Iterator<Item = (FieldKey, &'a str)>,
) -> Result<Option<Command>, String> {
    let mut denoise = DenoisePatch::default();
    let mut neural = NeuralPatch::default();
    let mut any = false;
    for (key, value) in fields {
        match (page, key) {
            (Page::Restoration, FieldKey::Denoise(field)) => {
                field.patch_into(&mut denoise, value)?
            }
            (Page::Speech | Page::Separation, FieldKey::Neural(field)) => {
                field.patch_into(&mut neural, value)?
            }
            _ => continue,
        }
        any = true;
    }
    Ok(if !any {
        None
    } else if page == Page::Restoration {
        Some(Command::DenoiseEdit {
            patch: Box::new(denoise),
        })
    } else {
        Some(Command::NeuralEdit {
            patch: Box::new(neural),
        })
    })
}

pub(super) fn path_snapshot_current(
    requested: (u64, u64, &str),
    current: (u64, u64, Option<&str>),
) -> bool {
    requested.0 == current.0 && requested.1 == current.1 && current.2 == Some(requested.2)
}
fn submitted_text_current(
    generation: u64,
    submitted_generation: u64,
    text: &str,
    submitted_text: &str,
) -> bool {
    generation == submitted_generation && text == submitted_text
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PathRequest {
    id: u64,
    panel: u64,
    key: FieldKey,
}

fn retire_path_request(active: &mut Option<PathRequest>, request: PathRequest) -> bool {
    if *active != Some(request) {
        return false;
    }
    *active = None;
    true
}

struct Field {
    input: Entity<Input>,
    committed: String,
    dirty: bool,
    generation: u64,
    // The draft generation owned by a queued or active command; Enter must not repeat it.
    submitted: Option<u64>,
    native_epoch: Option<u64>,
    _subscription: Subscription,
}

impl Field {
    fn retire(&mut self, cx: &mut Context<AudioProcessing>) {
        if self.native_epoch.take().is_some() {
            self.input
                .update(cx, |input, cx| input.set_enabled(false, cx));
        }
    }
}

struct SubmittedParameter {
    key: FieldKey,
    generation: u64,
    text: String,
}

struct SubmittedFields {
    first: Option<SubmittedParameter>,
    rest: Vec<SubmittedParameter>,
}

struct Submission {
    command: Command,
    field: Option<SubmittedFields>,
    equalizer_field: Option<equalizer_editor::SubmittedField>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PresetAction {
    Save { overwrite: bool },
    Import { path: PathBuf, overwrite: bool },
    Delete,
}

impl PresetAction {
    fn edit(self, name: String) -> EqualizerEdit {
        match self {
            Self::Save { overwrite } => EqualizerEdit::SavePreset { name, overwrite },
            Self::Import { path, overwrite } => EqualizerEdit::ImportPreset {
                path,
                name,
                overwrite,
            },
            Self::Delete => EqualizerEdit::DeletePreset { name },
        }
    }

    fn matches_fields(&self, name: &str, path: &str, expected_name: &str) -> bool {
        name == expected_name
            && match self {
                Self::Import { path: expected, .. } => {
                    expected.as_path() == std::path::Path::new(path)
                }
                _ => true,
            }
    }
}

struct PresetConfirmation {
    id: u64,
    name: String,
    action: PresetAction,
    target: rivu_dsp::equalizer::config::EqualizerConfig,
}

impl PresetConfirmation {
    fn is_current(
        &self,
        id: u64,
        name: &str,
        path: &str,
        target: Option<&rivu_dsp::equalizer::config::EqualizerConfig>,
    ) -> bool {
        self.id == id
            && self.action.matches_fields(name, path, &self.name)
            && target == Some(&self.target)
    }
}

type MeasuredGraph = Rc<Cell<Option<(Bounds<Pixels>, WindowGeometry)>>>;

// A transient belongs to exactly one visible Monitor panel. Restoration is
// conditional on focus still belonging to this scope, never a newer editor.
struct PresetPopup {
    panel: u64,
    manage: bool,
    scope: rivu_ui::FocusScope,
    restore: Option<FocusHandle>,
    confirmation: Option<PresetConfirmation>,
}

pub(super) struct AudioProcessing {
    context: FeatureContext,
    state: Rc<GuiSnapshot>,
    page: Page,
    fields: HashMap<FieldKey, Field>,
    panel_demand: Option<(u64, bool)>,
    panel: Option<u64>,
    input_epoch: u64,
    choosing_path: Option<PathRequest>,
    chooser_generation: u64,
    error: Option<String>,
    pending: usize,
    commands: VecDeque<Submission>,
    command_active: bool,
    geometry_epoch: u64,
    graph_editor: GraphEditorState,
    equalizer_editors: EqualizerEditors,
    speech_advanced: bool,
    separation_advanced: bool,
    preset_popup: Option<PresetPopup>,
    preset_choices: rivu_ui::ChoiceState<String>,
    preset_intent_generation: u64,
    preset_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    results: ResultsState,
}

impl AudioProcessing {
    pub(super) fn new(context: FeatureContext, cx: &mut Context<Self>) -> Self {
        let state = context.snapshot();
        let mut this = Self {
            context,
            state,
            page: Page::Graph,
            fields: HashMap::new(),
            panel_demand: None,
            panel: None,
            input_epoch: 0,
            choosing_path: None,
            chooser_generation: 0,
            error: None,
            pending: 0,
            commands: VecDeque::with_capacity(64),
            command_active: false,
            geometry_epoch: 0,
            graph_editor: GraphEditorState::default(),
            equalizer_editors: EqualizerEditors::default(),
            speech_advanced: false,
            separation_advanced: false,
            preset_popup: None,
            preset_choices: rivu_ui::ChoiceState::default(),
            preset_intent_generation: 0,
            preset_bounds: Rc::new(Cell::new(None)),
            results: ResultsState::default(),
        };
        this.sync_fields(cx);
        this
    }

    fn tr(&self, id: &str) -> SharedString {
        self.context.tr(id)
    }

    pub(super) fn reconcile(&mut self, state: Rc<GuiSnapshot>, cx: &mut Context<Self>) {
        let config_changed = !Arc::ptr_eq(&self.state.system.config, &state.system.config);
        let denoise_changed =
            config_changed && self.state.system.config.denoise != state.system.config.denoise;
        let neural_changed =
            config_changed && self.state.system.config.neural != state.system.config.neural;
        let format_changed = self
            .state
            .current_track()
            .map(|t| (t.id, t.sample_rate, t.channels))
            != state
                .current_track()
                .map(|t| (t.id, t.sample_rate, t.channels));
        let title_changed = self.state.current_track().map(|track| &track.title)
            != state.current_track().map(|track| &track.title);
        let preset_names_changed = self.preset_popup.is_some()
            && config_changed
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
        let changed = config_changed
            || match (&self.state.system.processing, &state.system.processing) {
                (Some(previous), Some(current)) => !Arc::ptr_eq(previous, current),
                (None, None) => false,
                _ => true,
            }
            || self.state.system.processing_audition != state.system.processing_audition
            || self.state.system.processing_selection != state.system.processing_selection
            || self.state.playback.status != state.playback.status
            || format_changed
            || title_changed;
        if self.state.system.processing.as_ref().map(|task| task.id)
            != state.system.processing.as_ref().map(|task| task.id)
        {
            self.results.task_details = false;
        }
        self.state = state;
        if config_changed
            && let Some(popup) = &mut self.preset_popup
            && popup.confirmation.as_ref().is_some_and(|confirmation| {
                !self
                    .state
                    .system
                    .config
                    .equalizer_presets
                    .iter()
                    .any(|preset| {
                        preset.name == confirmation.name && preset.config == confirmation.target
                    })
            })
        {
            let panel = popup.panel;
            popup.confirmation = None;
            self.notify_preset_panel(panel, cx);
        }
        if self.preset_popup.is_some() && !self.state.system.config.equalizer.enabled {
            let _ = self.set_preset_popup(None, cx);
        }
        if preset_names_changed {
            self.sync_preset_choices(cx);
            if let Some(popup) = &self.preset_popup {
                self.notify_preset_panel(popup.panel, cx);
            }
        }
        self.reconcile_graph(cx);
        self.reconcile_equalizers(cx);
        if denoise_changed {
            self.sync_denoise_fields(cx);
        }
        if neural_changed {
            self.sync_neural_fields(cx);
        }
        if changed {
            cx.notify();
        }
    }

    pub(super) fn cancel_pointer(&mut self, cx: &mut Context<Self>) {
        let graph_pointer = self.graph_editor.pointer.take().is_some();
        let eq_pointer = self.cancel_equalizer_pointer(cx);
        let workbench_pointer = self.workbench_pointer_active();
        self.cancel_workbench_resize();
        if graph_pointer || eq_pointer || workbench_pointer {
            cx.notify();
        }
    }

    // A retired host loses native identities, not draft or submission generations.
    fn retire_field_inputs(&mut self, cx: &mut Context<Self>) {
        self.input_epoch = self.input_epoch.wrapping_add(1);
        for field in self.fields.values_mut() {
            field.retire(cx);
        }
    }

    fn bind_panel(&mut self, panel: u64, cx: &mut Context<Self>) {
        if self.panel == Some(panel) {
            return;
        }
        if self.panel.is_some() {
            self.retire_field_inputs(cx);
            self.invalidate_geometry(cx);
            self.choosing_path = None;
        }
        self.panel = Some(panel);
    }

    pub(super) fn invalidate_geometry(&mut self, cx: &mut Context<Self>) {
        self.geometry_epoch = self.geometry_epoch.wrapping_add(1);
        self.invalidate_equalizer_geometry(cx);
        self.graph_editor.bounds.set(None);
        self.preset_bounds.set(None);
        let _ = self.set_preset_popup(None, cx);
        self.cancel_pointer(cx);
    }

    pub(super) fn pointer_active(&self) -> bool {
        self.equalizer_pointer_active()
            || self.graph_editor.pointer.is_some()
            || self.workbench_pointer_active()
    }

    pub(super) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.equalizer_input_focused(window, cx)
            || self.panel.is_some()
                && self
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
        let demand = (layout.revision, visible);
        if self.panel_demand == Some(demand) {
            return;
        }
        let layout_changed = self
            .panel_demand
            .is_some_and(|(revision, _)| revision != layout.revision);
        self.panel_demand = Some(demand);
        if layout_changed
            || self
                .panel
                .is_some_and(|panel| !visible || !layout.is_panel_active(panel))
        {
            self.retire_field_inputs(cx);
            self.panel = None;
            self.invalidate_geometry(cx);
            self.choosing_path = None;
        }
        self.retain_equalizer_panels(layout, visible && !layout_changed, cx);
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
        if self
            .choosing_path
            .is_some_and(|request| !visible || !layout.is_panel_active(request.panel))
        {
            self.choosing_path = None;
        }
        self.retain_workbench_controls(layout, visible && !layout_changed);
        if self
            .graph_editor
            .pointer
            .as_ref()
            .is_some_and(|p| !visible || !layout.is_panel_active(p.panel))
        {
            self.cancel_pointer(cx);
        }
    }

    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if self.state.system.config.processing_bypassed
            && matches!(
                command,
                Command::AuditionProcessing { .. } | Command::ProcessingGraphAudition { .. }
            )
        {
            self.error = Some(self.tr("processing-audition-bypassed").to_string());
            cx.notify();
            return;
        }
        self.send_field(command, None, cx);
    }

    fn send_field(
        &mut self,
        command: Command,
        submitted: Option<SubmittedParameter>,
        cx: &mut Context<Self>,
    ) {
        self.enqueue_submission(
            Submission {
                command,
                field: submitted.map(|field| SubmittedFields {
                    first: Some(field),
                    rest: Vec::new(),
                }),
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
        if let Some(submitted) = &submission.field {
            for submitted in submitted.first.iter().chain(&submitted.rest) {
                if let Some(field) = self.fields.get_mut(&submitted.key) {
                    field.submitted = Some(submitted.generation);
                }
            }
        }
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
            let success = ack.ack.is_success();
            let refreshed = matches!(
                ack.observation,
                crate::frontend::CommandObservation::Refreshed
            );
            let result = ack.observed_result();
            let _ = this.update(cx, |this, cx| {
                this.pending = this.pending.saturating_sub(1);
                this.command_active = false;
                if let Err(error) = result {
                    this.error = Some(error);
                }
                if let Some(submitted) = &submitted {
                    this.release_parameter_fields(submitted);
                }
                if refreshed {
                    this.finish_equalizer_command(success);
                    this.reconcile(this.context.snapshot(), cx);
                }
                if graph_command {
                    this.graph_ack(ack.ack.status, refreshed, cx);
                }
                if refreshed {
                    if success && let Some(submitted) = submitted {
                        this.finish_parameter_fields(submitted, cx);
                    }
                    if let Some(field) = equalizer_field {
                        this.finish_equalizer_field(field, success, cx);
                    }
                }
                this.dispatch_next(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn release_parameter_fields(&mut self, submitted: &SubmittedFields) {
        for submitted in submitted.first.iter().chain(&submitted.rest) {
            if let Some(field) = self.fields.get_mut(&submitted.key)
                && field.submitted == Some(submitted.generation)
            {
                field.submitted = None;
            }
        }
    }

    fn finish_parameter_fields(&mut self, submitted: SubmittedFields, cx: &mut Context<Self>) {
        for submitted in submitted.first.into_iter().chain(submitted.rest) {
            if let Some(field) = self.fields.get_mut(&submitted.key)
                && submitted_text_current(
                    field.generation,
                    submitted.generation,
                    submitted.key.value_text(field.input.read(cx).text()),
                    &submitted.text,
                )
            {
                field.dirty = false;
                let committed = field.committed.clone();
                field
                    .input
                    .update(cx, |input, cx| input.set_text(committed, cx));
            }
        }
    }

    fn begin_path_request(&mut self, panel: u64, key: FieldKey) -> PathRequest {
        self.chooser_generation += 1;
        let request = PathRequest {
            id: self.chooser_generation,
            panel,
            key,
        };
        self.choosing_path = Some(request);
        request
    }

    fn path_placeholder(&self, key: FieldKey) -> Option<SharedString> {
        let id = match key {
            FieldKey::Neural(field) => field.path_placeholder()?,
            FieldKey::PresetPath => "processing-preset-path-placeholder",
            FieldKey::GraphExport => "processing-export-path-placeholder",
            FieldKey::Graph(binding) => binding.field.path_placeholder()?,
            _ => return None,
        };
        Some(self.tr(id))
    }

    fn path_help(&self, key: FieldKey) -> Option<SharedString> {
        let id = match key {
            FieldKey::Neural(field) => field.path_help()?,
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
                generation: 0,
                submitted: None,
                native_epoch: Some(self.input_epoch),
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
        cx.subscribe(input, move |this, input, event, cx| {
            if !this.fields.get(&name).is_some_and(|field| {
                field.input.entity_id() == input.entity_id()
                    && field.native_epoch == Some(this.input_epoch)
            }) {
                return;
            }
            if matches!(event, InputEvent::Changed { .. })
                && matches!(name, FieldKey::PresetName | FieldKey::PresetPath)
                && let Some(popup) = &mut this.preset_popup
            {
                let panel = popup.panel;
                popup.confirmation = None;
                this.notify_preset_panel(panel, cx);
            }
            match event {
                InputEvent::Changed { user: true, .. } => {
                    this.mark_field_edited(name);
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
            }
        })
    }

    fn sync_denoise_fields(&mut self, cx: &mut Context<Self>) {
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
            let label = match field.group_label_id() {
                Some(group) => format!("{} · {}", self.tr(group), self.tr(field.label_id())),
                None => self.tr(field.label_id()).to_string(),
            };
            self.field(FieldKey::Denoise(field), &label, value, cx);
        }
    }

    fn sync_neural_fields(&mut self, cx: &mut Context<Self>) {
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
                &self.tr(field.label_id()),
                value,
                cx,
            );
        }
    }

    fn sync_fields(&mut self, cx: &mut Context<Self>) {
        self.sync_denoise_fields(cx);
        self.sync_neural_fields(cx);
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
            .map(|f| key.value_text(f.input.read(cx).text()).to_owned())
            .unwrap_or_default()
    }

    fn apply_field(&mut self, key: FieldKey, cx: &mut Context<Self>) {
        if self
            .fields
            .get(&key)
            .is_some_and(|field| field.input.read(cx).is_composing())
        {
            return;
        }
        if matches!(key, FieldKey::Graph(_)) {
            self.apply_graph_field(key, cx);
            return;
        }
        let value = self.text(key, cx);
        let Some(generation) = self
            .fields
            .get(&key)
            .filter(|field| field.dirty && field.submitted != Some(field.generation))
            .map(|field| field.generation)
        else {
            return;
        };
        let result = self.field_command(key, &value);
        match result {
            Ok(Some(command)) => self.send_field(
                command,
                Some(SubmittedParameter {
                    key,
                    generation,
                    text: value,
                }),
                cx,
            ),
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

    fn field_input(
        &mut self,
        panel: u64,
        key: FieldKey,
        label: &str,
        cx: &mut Context<Self>,
    ) -> Entity<Input> {
        self.bind_panel(panel, cx);
        if self.fields[&key].native_epoch != Some(self.input_epoch) {
            let text = self.fields[&key].input.read(cx).text().to_owned();
            let placeholder = self
                .path_placeholder(key)
                .unwrap_or_else(|| label.to_owned().into());
            let input = cx.new(|cx| Input::new(label.to_owned(), text, placeholder, cx));
            let subscription = self.subscribe_field(&input, key, cx);
            let field = self
                .fields
                .get_mut(&key)
                .expect("existing processing field");
            field.input = input;
            field.native_epoch = Some(self.input_epoch);
            field._subscription = subscription;
        }
        let input = self.fields[&key].input.clone();
        let enabled = !matches!(key, FieldKey::PresetName | FieldKey::PresetPath)
            || self.state.system.config.equalizer.enabled;
        input.update(cx, |input, cx| input.set_enabled(enabled, cx));
        input
    }

    fn field_view(
        &mut self,
        panel: u64,
        key: FieldKey,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let label: SharedString = label.into();
        let input = self.field_input(panel, key, &label, cx);
        Self::parameter_row(
            caption(theme, label.clone())
                .id(SharedString::from(format!("field-label-{key:?}")))
                .tooltip(rivu_ui::text_tooltip(self.path_help(key).unwrap_or(label))),
            input,
        )
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
        self.preset_choices.close();
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
                confirmation: None,
            }),
            cx,
        );
        if !manage {
            self.sync_preset_choices(cx);
        }
        cx.notify();
    }

    fn sync_preset_choices(&mut self, cx: &App) {
        if !self
            .preset_popup
            .as_ref()
            .is_some_and(|popup| !popup.manage)
        {
            return;
        }
        let selected_name = self
            .preset_choices
            .selected_index()
            .and_then(|index| self.preset_choices.item(index))
            .map(|item| item.value.clone())
            .unwrap_or_else(|| self.text(FieldKey::PresetName, cx));
        let presets = &self.state.system.config.equalizer_presets;
        let selected = presets
            .iter()
            .position(|preset| preset.name == selected_name)
            .unwrap_or(0);
        self.preset_choices.open(
            presets
                .iter()
                .map(|preset| rivu_ui::ChoiceItem::new(preset.name.clone(), preset.name.clone())),
            selected,
        );
    }

    fn choose_preset(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.system.config.equalizer.enabled
            || !self
                .preset_popup
                .as_ref()
                .is_some_and(|popup| !popup.manage)
        {
            return;
        }
        let Some(name) = self
            .preset_choices
            .item(index)
            .map(|item| item.value.clone())
        else {
            return;
        };
        if !self
            .state
            .system
            .config
            .equalizer_presets
            .iter()
            .any(|preset| preset.name == name)
        {
            return;
        }
        if let Some(field) = self.fields.get_mut(&FieldKey::PresetName) {
            field
                .input
                .update(cx, |input, cx| input.set_text(name.clone(), cx));
            field.dirty = false;
        }
        self.close_preset(window, cx);
        self.eq(EqualizerEdit::LoadPreset { name }, cx);
    }

    fn dismiss_preset_choices(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(_) = self.preset_popup.as_ref().filter(|popup| !popup.manage) else {
            return false;
        };
        if self
            .preset_bounds
            .get()
            .is_some_and(|bounds| bounds.contains(&event.position))
        {
            return false;
        }
        self.close_preset(window, cx);
        true
    }

    fn request_preset_action(&mut self, panel: u64, action: PresetAction, cx: &mut Context<Self>) {
        if !self.state.system.config.equalizer.enabled
            || !self
                .preset_popup
                .as_ref()
                .is_some_and(|popup| popup.panel == panel && popup.manage)
        {
            return;
        }
        let name = self.text(FieldKey::PresetName, cx);
        if name.is_empty() {
            self.error = Some(self.tr("processing-preset-name-required").to_string());
            cx.notify();
            return;
        }
        let target = self
            .state
            .system
            .config
            .equalizer_presets
            .iter()
            .find(|preset| preset.name == name);
        let destructive = matches!(
            action,
            PresetAction::Save { overwrite: true }
                | PresetAction::Import {
                    overwrite: true,
                    ..
                }
                | PresetAction::Delete
        );
        if destructive {
            let Some(target) = target else {
                return;
            };
            self.preset_intent_generation = self.preset_intent_generation.wrapping_add(1);
            self.preset_popup.as_mut().unwrap().confirmation = Some(PresetConfirmation {
                id: self.preset_intent_generation,
                name,
                action,
                target: target.config.clone(),
            });
            self.notify_preset_panel(panel, cx);
            cx.notify();
        } else {
            self.eq(action.edit(name), cx);
        }
    }

    fn confirm_preset_action(&mut self, panel: u64, id: u64, cx: &mut Context<Self>) {
        let name = self.text(FieldKey::PresetName, cx);
        let path = self.text(FieldKey::PresetPath, cx);
        let Some(popup) = self
            .preset_popup
            .as_mut()
            .filter(|popup| popup.panel == panel && popup.manage)
        else {
            return;
        };
        if !popup
            .confirmation
            .as_ref()
            .is_some_and(|confirmation| confirmation.id == id)
        {
            return;
        }
        let confirmation = popup.confirmation.take().unwrap();
        let current = self
            .state
            .system
            .config
            .equalizer_presets
            .iter()
            .find(|preset| preset.name == confirmation.name);
        if self.state.system.config.equalizer.enabled
            && confirmation.is_current(id, &name, &path, current.map(|preset| &preset.config))
        {
            self.eq(confirmation.action.edit(confirmation.name), cx);
        }
        self.notify_preset_panel(panel, cx);
        cx.notify();
    }

    fn preset_confirmation_view(
        &self,
        panel: u64,
        confirmation: &PresetConfirmation,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let id = confirmation.id;
        let deleting = confirmation.action == PresetAction::Delete;
        let mut content = column()
            .gap_2()
            .child(caption(
                theme,
                self.tr(match confirmation.action {
                    PresetAction::Delete => "processing-preset-confirm-delete",
                    PresetAction::Import { .. } => "processing-preset-confirm-import-overwrite",
                    PresetAction::Save { .. } => "processing-preset-confirm-overwrite",
                }),
            ))
            .child(caption(theme, confirmation.name.clone()));
        if let PresetAction::Import { path, .. } = &confirmation.action {
            content = content
                .child(caption(theme, self.tr("processing-preset-import-source")))
                .child(caption(theme, path.display().to_string()));
        }
        content
            .child(menu_item(
                ("confirm-preset", panel),
                self.tr(if deleting {
                    "processing-delete"
                } else {
                    "processing-preset-overwrite"
                }),
                cx,
                move |this, _, cx| this.confirm_preset_action(panel, id, cx),
            ))
            .child(menu_item(
                ("cancel-preset", panel),
                self.tr("action-cancel"),
                cx,
                move |this, _, cx| {
                    if let Some(popup) = this
                        .preset_popup
                        .as_mut()
                        .filter(|popup| popup.panel == panel)
                        && popup
                            .confirmation
                            .as_ref()
                            .is_some_and(|confirmation| confirmation.id == id)
                    {
                        popup.confirmation = None;
                        this.notify_preset_panel(panel, cx);
                        cx.notify();
                    }
                },
            ))
    }

    fn preset_view(&mut self, panel: u64, window: &Window, cx: &mut Context<Self>) -> Div {
        self.bind_panel(panel, cx);
        let theme = rivu_ui::theme::palette(cx);
        let enabled = self.state.system.config.equalizer.enabled;
        for key in [FieldKey::PresetName, FieldKey::PresetPath] {
            if let Some(field) = self.fields.get(&key) {
                field.input.update(cx, |input, cx| {
                    input.set_enabled(enabled && field.native_epoch == Some(self.input_epoch), cx)
                });
            }
        }
        let bounds = self.preset_bounds.clone();
        let measure = bounds.clone();
        let owner = cx.entity().downgrade();
        let epoch = self.geometry_epoch;
        let name = self.text(FieldKey::PresetName, cx);
        let preset_exists = self
            .state
            .system
            .config
            .equalizer_presets
            .iter()
            .any(|preset| preset.name == name);
        let name_empty = name.is_empty();
        let label = if name.is_empty() {
            self.tr("processing-presets")
        } else {
            name.into()
        };
        let expanded = self
            .preset_popup
            .as_ref()
            .is_some_and(|popup| popup.panel == panel && !popup.manage);
        let trigger = rivu_ui::ChoiceSelect::new(("eq-presets", panel), label)
            .build(
                self,
                move |this| {
                    this.preset_popup
                        .as_ref()
                        .is_some_and(|popup| popup.panel == panel && !popup.manage)
                        .then_some(&mut this.preset_choices)
                },
                cx,
                move |this, window, cx| this.toggle_preset(panel, false, window, cx),
                Self::choose_preset,
                Self::close_preset,
            )
            .aria_expanded(expanded);
        let trigger = disabled_control(theme, trigger, !enabled);
        let mut content = column().min_w_0().gap_1().child(
            row()
                .relative()
                .min_w_0()
                .w_auto()
                .max_w_full()
                .gap_1()
                .child(
                    canvas(
                        move |bounds, _, cx| {
                            if let Some(owner) = owner.upgrade()
                                && owner.read(cx).panel == Some(panel)
                                && owner.read(cx).geometry_epoch == epoch
                            {
                                measure.set(Some(bounds));
                            }
                        },
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
            let scope = popup.scope.clone();
            let choices = if self.preset_choices.is_open() {
                rivu_ui::ChoicePopup::new(
                    ("eq-preset-choices", panel),
                    ("eq-preset-choice-scroll", panel),
                    bounds,
                )
                .render(
                    self,
                    |this| &this.preset_choices,
                    Self::choose_preset,
                    Self::dismiss_preset_choices,
                    window,
                    cx,
                )
            } else {
                deferred(
                    anchored()
                        .position(bounds.bottom_left())
                        .snap_to_window()
                        .child(
                            Popover::surface(theme, ("eq-preset-empty", panel))
                                .p_1()
                                .on_mouse_down_out(cx.listener(|this, event, window, cx| {
                                    if this.dismiss_preset_choices(event, window, cx) {
                                        cx.stop_propagation();
                                    }
                                }))
                                .child(caption(theme, self.tr("processing-preset-empty"))),
                        ),
                )
                .with_priority(2)
                .into_any_element()
            };
            content = content.child(
                scope
                    .surface(div().id(("eq-preset-choice-scope", panel)).absolute())
                    .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        match this
                            .preset_choices
                            .key(event, rivu_ui::ChoiceMode::Select, false, window)
                            .apply(window, cx)
                        {
                            rivu_ui::ChoiceAction::Commit(index) => {
                                this.choose_preset(index, window, cx)
                            }
                            rivu_ui::ChoiceAction::Dismiss => this.close_preset(window, cx),
                            rivu_ui::ChoiceAction::Navigated => cx.notify(),
                            _ if event.keystroke.key == "escape"
                                && !event.keystroke.modifiers.modified() =>
                            {
                                this.close_preset(window, cx);
                                cx.stop_propagation();
                            }
                            _ => {}
                        }
                    }))
                    .child(choices),
            );
        }
        if let Some(bounds) = bounds.get().filter(|_| {
            self.preset_popup
                .as_ref()
                .is_some_and(|popup| popup.panel == panel && popup.manage)
        }) {
            let name_field = self.field_view(
                panel,
                FieldKey::PresetName,
                self.tr("processing-preset-name"),
                cx,
            );
            let path_field = self.field_view(
                panel,
                FieldKey::PresetPath,
                self.tr("processing-preset-path"),
                cx,
            );
            let popup = self.preset_popup.as_ref().expect("open preset management");
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
                                .child(if let Some(confirmation) = &popup.confirmation {
                                    self.preset_confirmation_view(panel, confirmation, cx)
                                } else {
                                    column()
                                        .gap_1()
                                        .child(name_field)
                                        .child(disabled_control(
                                            theme,
                                            menu_item(
                                                ("save-preset", panel),
                                                self.tr("processing-preset-save-new"),
                                                cx,
                                                move |this, _, cx| {
                                                    this.request_preset_action(
                                                        panel,
                                                        PresetAction::Save { overwrite: false },
                                                        cx,
                                                    )
                                                },
                                            ),
                                            preset_exists || name_empty,
                                        ))
                                        .child(disabled_control(
                                            theme,
                                            menu_item(
                                                ("overwrite-preset", panel),
                                                self.tr("processing-preset-overwrite"),
                                                cx,
                                                move |this, _, cx| {
                                                    this.request_preset_action(
                                                        panel,
                                                        PresetAction::Save { overwrite: true },
                                                        cx,
                                                    )
                                                },
                                            ),
                                            !preset_exists,
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
                                        .child(disabled_control(
                                            theme,
                                            menu_item(
                                                ("delete-preset", panel),
                                                self.tr("processing-delete"),
                                                cx,
                                                move |this, _, cx| {
                                                    this.request_preset_action(
                                                        panel,
                                                        PresetAction::Delete,
                                                        cx,
                                                    )
                                                },
                                            ),
                                            !preset_exists,
                                        ))
                                        .child(menu_item(
                                            ("copy-eq", panel),
                                            self.tr("processing-copy"),
                                            cx,
                                            |this, _, cx| match serde_json::to_string_pretty(
                                                &this.state.system.config.equalizer,
                                            ) {
                                                Ok(text) => cx.write_to_clipboard(
                                                    ClipboardItem::new_string(text),
                                                ),
                                                Err(error) => {
                                                    this.error = Some(error.to_string());
                                                    cx.notify();
                                                }
                                            },
                                        ))
                                        .child(path_field)
                                        .child(menu_item(
                                            ("import-preset", panel),
                                            self.tr(if preset_exists {
                                                "processing-preset-import-overwrite"
                                            } else {
                                                "processing-import"
                                            }),
                                            cx,
                                            move |this, _, cx| {
                                                let name = this.text(FieldKey::PresetName, cx);
                                                let overwrite = this
                                                    .state
                                                    .system
                                                    .config
                                                    .equalizer_presets
                                                    .iter()
                                                    .any(|preset| preset.name == name);
                                                let path = PathBuf::from(
                                                    this.text(FieldKey::PresetPath, cx),
                                                );
                                                this.request_preset_action(
                                                    panel,
                                                    PresetAction::Import { path, overwrite },
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
                                        ))
                                }),
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
        &mut self,
        panel: u64,
        fields: &[DenoiseField],
        cx: &mut Context<Self>,
    ) -> Div {
        let mut content = column().min_w_0().gap_1();
        for &field in fields {
            content = content.child(self.field_view(
                panel,
                FieldKey::Denoise(field),
                self.tr(field.label_id()),
                cx,
            ));
        }
        content
    }

    fn restoration_view(&mut self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let state = Rc::clone(&self.state);
        let config = &state.system.config.denoise;
        let selected_mode = DenoiseField::Mode.text(config);
        let mut modes = row().gap_1();
        for &(slug, label) in DenoiseField::Mode.choices() {
            modes = modes.child(
                button(
                    SharedString::from(format!("denoise-mode-{slug}")),
                    self.tr(label),
                    cx,
                    move |this, _, cx| {
                        this.patch_denoise(
                            DenoiseField::Mode.patch(slug).expect("valid mode choice"),
                            cx,
                        )
                    },
                )
                .debug_selector(move || format!("denoise-mode-{slug}"))
                .flex_shrink_0()
                .whitespace_nowrap()
                .when(selected_mode == slug, |view| view.bg(rgb(theme.selected))),
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
                panel,
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
            content = content.child(self.parameter_controls(
                panel,
                &[DenoiseField::StationaryNoiseDbfs],
                cx,
            ));
        }
        if config.mode == DenoiseMode::Profile {
            content = content
                .child(self.field_view(
                    panel,
                    FieldKey::ProfileStart,
                    self.tr("processing-profile-start"),
                    cx,
                ))
                .child(self.field_view(
                    panel,
                    FieldKey::ProfileEnd,
                    self.tr("processing-profile-end"),
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
                "processing-hum",
                config.hum.enabled,
                ProcessingToggle::Hum,
                &[
                    DenoiseField::HumFundamentalHz,
                    DenoiseField::HumHarmonics,
                    DenoiseField::HumQ,
                ][..],
            ),
            (
                "processing-impulse",
                config.impulse.enabled,
                ProcessingToggle::Impulse,
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
                .child(
                    checkbox(
                        SharedString::from(format!("restoration-enable-{group}-{panel}")),
                        self.tr(group),
                        enabled,
                        true,
                        cx,
                        move |this, _, cx| {
                            this.send(Command::ToggleProcessing { target: toggle }, cx);
                        },
                    )
                    .debug_selector(move || format!("restoration-enable-{group}-{panel}")),
                );
            if enabled {
                section = section.child(self.parameter_controls(panel, fields, cx));
            }
            content = content.child(section);
        }
        content.child(
            div()
                .pt_2()
                .border_t_1()
                .border_color(rgb(theme.border))
                .child(self.parameter_controls(panel, &[DenoiseField::Wet], cx)),
        )
    }

    fn neural_fields(&mut self, panel: u64, fields: &[NeuralField], cx: &mut Context<Self>) -> Div {
        let mut content = column().min_w_0().gap_1();
        for &field in fields {
            content = content.child(self.field_view(
                panel,
                FieldKey::Neural(field),
                self.tr(field.label_id()),
                cx,
            ));
        }
        content
    }

    fn neural_view(&mut self, panel: u64, speech: bool, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let state = Rc::clone(&self.state);
        let neural = &state.system.config.neural;
        let open = if speech {
            self.speech_advanced
        } else {
            self.separation_advanced
        };
        let mut parameters = self.neural_fields(
            panel,
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
                    |this, _, cx| {
                        this.send(
                            Command::ToggleProcessing {
                                target: ProcessingToggle::SpeechPostFilter,
                            },
                            cx,
                        )
                    },
                )
                .debug_selector(|| "speech-post-filter".to_owned())
                .aria_label(self.tr("processing-speech-post-filter")),
            ));
        }
        let model = self.neural_fields(
            panel,
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
            let mut runtime =
                self.neural_fields(panel, &[NeuralField::Worker, NeuralField::Threads], cx);
            let mut devices = row().gap_1().flex_wrap();
            let selected_device = NeuralField::Device.text(neural);
            for &(value, label) in NeuralField::Device.choices() {
                devices = devices.child(
                    button(
                        SharedString::from(format!("neural-device-{value}")),
                        self.tr(label),
                        cx,
                        move |this, _, cx| {
                            this.patch_neural(
                                NeuralField::Device
                                    .patch(value)
                                    .expect("valid device choice"),
                                cx,
                            )
                        },
                    )
                    .debug_selector(move || format!("neural-device-{value}"))
                    .when(selected_device == value, |v| v.bg(rgb(theme.selected))),
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
        if self.fields.iter().any(|(key, field)| {
            field.dirty && self.page_parameter_key(*key) && field.input.read(cx).is_composing()
        }) {
            return;
        }
        // Fields already owned by a queued command keep that command's generation.
        let fields: Vec<_> = self
            .fields
            .iter()
            .filter(|(key, field)| {
                field.dirty
                    && field.submitted != Some(field.generation)
                    && self.page_parameter_key(**key)
            })
            .map(|(key, field)| SubmittedParameter {
                key: *key,
                generation: field.generation,
                text: key.value_text(field.input.read(cx).text()).to_owned(),
            })
            .collect();
        if fields.is_empty() {
            return;
        }
        match page_parameter_command(
            self.page,
            fields
                .iter()
                .map(|submitted| (submitted.key, submitted.text.as_str())),
        ) {
            Ok(Some(command)) => self.enqueue_submission(
                Submission {
                    command,
                    field: Some(SubmittedFields {
                        first: None,
                        rest: fields,
                    }),
                    equalizer_field: None,
                },
                cx,
            ),
            Ok(None) => {}
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
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
        let mut actions = row().flex_wrap().gap_1().min_w_0();
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

    fn audition(&mut self, id: u64, mode: AuditionMode, cx: &mut Context<Self>) {
        if self.state.system.config.processing_bypassed {
            self.error = Some(self.tr("processing-audition-bypassed").to_string());
            cx.notify();
            return;
        }
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
        self.bind_panel(panel, cx);
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
        let transport = self.tr(match self.state.playback.status {
            rivu_protocol::model::PlaybackStatus::Playing => "gl-playing",
            rivu_protocol::model::PlaybackStatus::Paused => "gl-paused",
            rivu_protocol::model::PlaybackStatus::Stopped => "gl-stopped",
        });
        let audio_label = |audition: Option<rivu_protocol::model::ProcessingAudition>| {
            audition.map_or_else(
                || self.tr("processing-workspace-original"),
                |audition| {
                    self.tr(match audition.mode {
                        Some(rivu_protocol::model::GraphExecutionMode::Auto) => {
                            "processing-graph-auto"
                        }
                        Some(rivu_protocol::model::GraphExecutionMode::Realtime) => {
                            "processing-graph-realtime"
                        }
                        Some(rivu_protocol::model::GraphExecutionMode::Hybrid) => {
                            "processing-graph-hybrid"
                        }
                        Some(rivu_protocol::model::GraphExecutionMode::Offline) => {
                            "processing-graph-cached"
                        }
                        None => "processing-workspace-processed",
                    })
                },
            )
        };
        let actual = self.state.system.processing_audition;
        let selection = self.state.system.processing_selection;
        let mut selected_audio_text = format!(
            "{}: {} · {transport}",
            self.tr("processing-workspace-actual-audio"),
            audio_label(actual),
        );
        if self.state.system.config.processing_bypassed {
            selected_audio_text
                .push_str(&format!(" · {}", self.tr("processing-workspace-bypassed")));
        } else if self.state.system.config.equalizer.enabled {
            selected_audio_text.push_str(&format!(
                " · {}",
                self.tr("processing-workspace-monitor-on")
            ));
        }
        if selection != actual {
            selected_audio_text.push_str(&format!(
                " · {}: {}",
                self.tr("processing-workspace-selected-audio"),
                audio_label(selection),
            ));
        }
        let mut audio_details = selected_audio_text.clone();
        for (label, audition) in [
            ("processing-workspace-selected-audio", selection),
            ("processing-workspace-actual-audio", actual),
        ] {
            if let Some(audition) = audition {
                audio_details.push_str(&format!(
                    "\n{}: {} #{}",
                    self.tr(label),
                    audio_label(Some(audition)),
                    audition.id
                ));
                if let Some(port) = audition.output {
                    audio_details.push_str(&format!(" · #{}:{}", port.node, port.port));
                }
            }
        }
        if let Some(track) = self.state.current_track() {
            audio_details.push_str(&format!(
                "\n{} · {} Hz · {} ch\n{}",
                track.title,
                track.sample_rate,
                track.channels,
                track.path.display()
            ));
        }
        let selected_audio = caption(theme, selected_audio_text)
            .min_w_0()
            .truncate()
            .id(("processing-audition-status", panel))
            .tooltip(rivu_ui::text_tooltip(audio_details));
        let mut selected_audio = row()
            .min_w_0()
            .gap_2()
            .items_center()
            .child(selected_audio.flex_1());
        if let Some(audition) = self.state.system.processing_selection {
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
        let bypassed = self.state.system.config.processing_bypassed;
        selected_audio = selected_audio.child(
            IconButton::new(
                ("processing-bypass", panel),
                "󰓅",
                self.tr(if bypassed {
                    "processing-workspace-resume"
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
                    this.send(
                        Command::ToggleProcessing {
                            target: ProcessingToggle::Bypass,
                        },
                        cx,
                    );
                },
            )
            .debug_selector(move || format!("processing-bypass-{panel}"))
            .text_color(rgb(if bypassed {
                theme.warning
            } else {
                theme.accent
            })),
        );
        if self.page != Page::Graph {
            self.deactivate_graph_inputs(window, cx);
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
        let path = " runtime.with.dots/worker with spaces.py ";
        NeuralField::Worker.apply(&mut config, path).unwrap();
        assert_eq!(NeuralField::Worker.text(&config), path);
        assert_eq!(
            NeuralField::Worker.patch(path).unwrap().worker,
            Some(PathBuf::from(path))
        );
        assert!(NeuralField::SpeechWetMix.patch("NaN").is_err());
        assert!(NeuralField::Threads.patch("256").is_err());
    }

    #[test]
    fn page_commands_keep_literal_paths_clear_intent_and_numeric_normalization() {
        let path = " /runtime/worker with spaces.py ";
        let worker = FieldKey::Neural(NeuralField::Worker);
        let manifest = FieldKey::Neural(NeuralField::SpeechManifest);
        let threads = FieldKey::Neural(NeuralField::Threads);
        let command = page_parameter_command(
            Page::Speech,
            [
                (worker, worker.value_text(path)),
                (manifest, manifest.value_text("")),
                (threads, threads.value_text(" 4 ")),
            ]
            .into_iter(),
        )
        .unwrap()
        .unwrap();
        let Command::NeuralEdit { patch } = command else {
            panic!("neural parameter command");
        };
        assert_eq!(patch.worker, Some(PathBuf::from(path)));
        assert_eq!(patch.speech_manifest, Some(None));
        assert_eq!(patch.separation_manifest, None);
        assert_eq!(patch.threads, Some(4));
        assert_eq!(patch.speech_wet_mix, None);
        let mut config = NeuralConfig::default();
        let previous = config.clone();
        for invalid in ["NaN", "inf", "-inf", "1e", ""] {
            assert!(
                NeuralField::SpeechWetMix
                    .apply(&mut config, invalid)
                    .is_err()
            );
            assert_eq!(config, previous);
        }
    }

    #[test]
    fn preset_confirmation_is_bound_to_intent_name_import_path_and_target() {
        let target = rivu_dsp::equalizer::config::EqualizerConfig::default();
        let confirmation = PresetConfirmation {
            id: 7,
            name: "Reference".to_owned(),
            action: PresetAction::Import {
                path: "first.json".into(),
                overwrite: true,
            },
            target: target.clone(),
        };
        assert!(confirmation.is_current(7, "Reference", "first.json", Some(&target)));
        assert!(!confirmation.is_current(8, "Reference", "first.json", Some(&target)));
        assert!(!confirmation.is_current(7, "Other", "first.json", Some(&target)));
        assert!(!confirmation.is_current(7, "Reference", "second.json", Some(&target)));
        assert!(!confirmation.is_current(7, "Reference", "first.json", None));
        let mut replaced = target.clone();
        replaced.preamp_db = -3.0;
        assert!(!confirmation.is_current(7, "Reference", "first.json", Some(&replaced)));
    }
}
