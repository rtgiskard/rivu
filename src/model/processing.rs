use rivu_dsp::equalizer::{ChannelTarget, EqBand, EqualizerConfig, FilterKind};
pub use rivu_format::config::processing_graph::{
    EqImplementation, GraphEdge, GraphEdit, GraphExecutionMode, GraphNode, GraphNodeKind,
    GraphPort, NoiseSelection, ProcessingGraph,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum EqBandEdit {
    Enabled(bool),
    Solo(bool),
    Kind(FilterKind),
    FrequencyHz(f64),
    GainDb(f64),
    Q(f64),
    ShelfSlope(f64),
    Stages(u8),
    Channels(ChannelTarget),
    Position { frequency_hz: f64, gain_db: f64 },
    FrequencySteps { steps: i32, maximum_hz: f64 },
    GainSteps(i32),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum EqualizerEdit {
    Enabled(bool),
    PreampDb(f64),
    OutputGainDb(f64),
    Band { index: usize, band: EqBand },
    BandField { index: usize, field: EqBandEdit },
    AddBand { band: EqBand },
    AddBands { count: usize },
    RemoveBand { index: usize },
    Replace { config: EqualizerConfig },
    SavePreset { name: String },
    LoadPreset { name: String },
    DeletePreset { name: String },
    ImportPreset { path: PathBuf, name: String },
    ExportPreset { path: PathBuf, name: String },
    SwapAb,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NeuralPatch {
    pub speech_interpreter: Option<PathBuf>,
    pub separation_interpreter: Option<PathBuf>,
    pub worker: Option<PathBuf>,
    #[serde(default, deserialize_with = "manifest_patch")]
    pub speech_manifest: Option<Option<PathBuf>>,
    #[serde(default, deserialize_with = "manifest_patch")]
    pub separation_manifest: Option<Option<PathBuf>>,
    pub device: Option<rivu_format::config::NeuralDevice>,
    pub threads: Option<u8>,
    pub speech_wet_mix: Option<f32>,
    pub speech_attenuation_limit_db: Option<f32>,
    pub speech_post_filter: Option<bool>,
    pub separation_overlap: Option<f32>,
}

fn manifest_patch<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<PathBuf>>, D::Error> {
    if d.is_human_readable() {
        Option::<PathBuf>::deserialize(d).map(Some)
    } else {
        Option::<Option<PathBuf>>::deserialize(d)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DenoisePatch {
    pub mode: Option<rivu_dsp::denoise::DenoiseMode>,
    pub strength: Option<f32>,
    pub noise_floor_db: Option<f32>,
    pub attack_ms: Option<f32>,
    pub release_ms: Option<f32>,
    pub smoothing_bins: Option<usize>,
    pub transient_protection: Option<f32>,
    pub wet: Option<f32>,
    pub fft_size: Option<usize>,
    pub stationary_noise_dbfs: Option<f32>,
    pub hum_enabled: Option<bool>,
    pub hum_fundamental_hz: Option<f32>,
    pub hum_harmonics: Option<usize>,
    pub hum_q: Option<f32>,
    pub impulse_enabled: Option<bool>,
    pub impulse_max_width: Option<usize>,
    pub impulse_threshold: Option<f32>,
    pub impulse_floor_dbfs: Option<f32>,
    pub impulse_transient_protection: Option<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingOperation {
    Denoise,
    Speech,
    Separate,
    Graph,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingPhase {
    Queued,
    Rendering,
    Processing,
    Complete,
    Cancelled,
    Failed,
}
impl ProcessingPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Cancelled | Self::Failed)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditionMode {
    Original,
    Denoised,
    Vocals,
    Accompaniment,
    Mix,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessingOutputs {
    pub denoised: Option<PathBuf>,
    pub vocals: Option<PathBuf>,
    pub accompaniment: Option<PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessingSettings {
    pub denoise: rivu_dsp::denoise::DenoiseConfig,
    pub neural: rivu_format::config::NeuralConfig,
    pub profile: Option<NoiseSelection>,
    #[serde(default)]
    pub graph: Option<ProcessingGraph>,
    #[serde(default)]
    pub graph_target: Option<GraphPort>,
    #[serde(default)]
    pub graph_mode: Option<GraphExecutionMode>,
    #[serde(default)]
    pub graph_boundary: Option<GraphPort>,
    #[serde(default)]
    pub graph_export_path: Option<PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessingTaskStatus {
    pub id: u64,
    pub track_id: i64,
    pub operation: ProcessingOperation,
    pub phase: ProcessingPhase,
    pub completed: u64,
    pub total: Option<u64>,
    pub error: Option<String>,
    pub outputs: Option<ProcessingOutputs>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub frames: Option<u64>,
    pub model_id: Option<String>,
    pub model_sha256: Option<String>,
    pub source_path: PathBuf,
    pub source_version: String,
    pub source_cue: Option<super::CueSegment>,
    pub settings: ProcessingSettings,
    #[serde(default)]
    pub graph_revision: Option<u64>,
    #[serde(default)]
    pub graph_outputs: Vec<GraphNodeOutput>,
    #[serde(default)]
    pub graph_exported_path: Option<PathBuf>,
    #[serde(default)]
    pub graph_nodes: Vec<GraphNodeStatus>,
    #[serde(default)]
    pub graph_plan: Option<GraphPlanInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphNodeStatus {
    pub node: u64,
    pub phase: ProcessingPhase,
    pub error: Option<String>,
    pub model_id: Option<String>,
    pub model_sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphPlanInfo {
    pub estimated_state_bytes: u64,
    pub estimated_work_per_frame: u64,
    pub latency_frames: u64,
    pub tail_frames: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphNodeOutput {
    pub port: GraphPort,
    pub path: PathBuf,
    /// Full causal PCM retained for suffix execution, before timeline trimming.
    #[serde(default)]
    pub context: Option<GraphOutputContext>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphOutputContext {
    pub path: PathBuf,
    /// Source-relative frame represented by the first context frame.
    pub start_frame: i64,
}
