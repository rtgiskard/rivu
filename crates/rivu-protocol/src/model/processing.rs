use rivu_dsp::equalizer::config::{ChannelTarget, EqBand, EqualizerConfig, FilterKind};
pub use rivu_format::config::processing_graph::{
    EqImplementation, GraphEdge, GraphEdit, GraphNode, GraphNodeKind, GraphPort, NoiseSelection,
    ProcessingGraph,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphExecutionMode {
    #[default]
    Auto,
    Offline,
    Realtime,
    Hybrid,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EqualizerEdit {
    Enabled(bool),
    PreampDb(f64),
    OutputGainDb(f64),
    Band {
        index: usize,
        band: EqBand,
    },
    BandField {
        index: usize,
        field: EqBandEdit,
    },
    AddBand {
        band: EqBand,
    },
    AddBands {
        count: usize,
    },
    RemoveBand {
        index: usize,
    },
    Replace {
        config: EqualizerConfig,
    },
    SavePreset {
        name: String,
        #[serde(default)]
        overwrite: bool,
    },
    LoadPreset {
        name: String,
    },
    DeletePreset {
        name: String,
    },
    ImportPreset {
        path: PathBuf,
        name: String,
        #[serde(default)]
        overwrite: bool,
    },
    ExportPreset {
        path: PathBuf,
        name: String,
    },
    SwapAb,
    ResizeBands {
        count: usize,
    },
}

impl EqualizerEdit {
    /// Apply monitor/node-independent parameter edits to a caller-owned candidate.
    /// Returns owner-only preset/A-B operations untouched. The caller validates
    /// and commits the whole candidate atomically; errors must discard it.
    pub fn apply_config(
        self,
        config: &mut EqualizerConfig,
        sample_rate: Option<u32>,
    ) -> anyhow::Result<Option<Self>> {
        use EqualizerEdit::*;
        use anyhow::{Context, ensure};
        match self {
            Enabled(value) => config.enabled = value,
            PreampDb(value) => config.preamp_db = value,
            OutputGainDb(value) => config.output_gain_db = value,
            Band { index, band } => {
                *config
                    .bands
                    .get_mut(index)
                    .context("Equalizer band no longer exists")? = band
            }
            BandField { index, field } => {
                let band = config
                    .bands
                    .get_mut(index)
                    .context("Equalizer band no longer exists")?;
                match field {
                    EqBandEdit::Enabled(v) => band.enabled = v,
                    EqBandEdit::Solo(v) => band.solo = v,
                    EqBandEdit::Kind(v) => {
                        band.kind = v;
                        band.stages = 1;
                    }
                    EqBandEdit::FrequencyHz(v) => band.frequency_hz = v,
                    EqBandEdit::GainDb(v) => band.gain_db = v,
                    EqBandEdit::Q(v) => band.q = v,
                    EqBandEdit::ShelfSlope(v) => band.shelf_slope = v,
                    EqBandEdit::Stages(v) => band.stages = v,
                    EqBandEdit::Channels(v) => band.channels = v,
                    EqBandEdit::Position {
                        frequency_hz,
                        gain_db,
                    } => {
                        band.frequency_hz = frequency_hz;
                        band.gain_db = gain_db;
                    }
                    EqBandEdit::FrequencySteps { steps, maximum_hz } => {
                        ensure!(
                            maximum_hz.is_finite() && maximum_hz >= 20.0,
                            "Invalid equalizer frequency bound"
                        );
                        let maximum_hz =
                            sample_rate
                                .filter(|rate| *rate > 0)
                                .map_or(maximum_hz, |rate| {
                                    maximum_hz.min(
                                        f64::from(rate)
                                            * rivu_dsp::equalizer::config::MAX_FREQUENCY_RATE_RATIO,
                                    )
                                });
                        ensure!(
                            maximum_hz >= 20.0,
                            "Current stream has no usable equalizer frequency range"
                        );
                        band.frequency_hz = (band.frequency_hz
                            * 2.0_f64.powf(f64::from(steps) / 12.0))
                        .clamp(20.0, maximum_hz);
                    }
                    EqBandEdit::GainSteps(steps) => {
                        band.gain_db = (band.gain_db + f64::from(steps) * 0.5).clamp(-24.0, 24.0)
                    }
                }
            }
            AddBand { band } => config.bands.push(band),
            AddBands { count } => {
                use rivu_dsp::equalizer::config::{EqBand, MAX_BANDS, MAX_FREQUENCY_RATE_RATIO};
                let bands = &mut config.bands;
                ensure!(count > 0, "Equalizer band count must be positive");
                ensure!(
                    count <= MAX_BANDS.saturating_sub(bands.len()),
                    "Equalizer supports at most {MAX_BANDS} bands"
                );
                let maximum_hz = sample_rate
                    .filter(|rate| *rate > 0)
                    .map_or(20_000.0, |rate| {
                        20_000.0_f64.min(f64::from(rate) * MAX_FREQUENCY_RATE_RATIO)
                    });
                ensure!(
                    maximum_hz >= 20.0,
                    "Current stream has no usable equalizer frequency range"
                );
                let minimum_log = 20.0_f64.ln();
                let maximum_log = maximum_hz.ln();
                bands.reserve(count);
                if bands.is_empty() {
                    for index in 0..count {
                        let fraction = (index as f64 + 0.5) / count as f64;
                        bands.push(EqBand {
                            frequency_hz: (minimum_log + fraction * (maximum_log - minimum_log))
                                .exp()
                                .clamp(20.0, maximum_hz),
                            ..Default::default()
                        });
                    }
                } else {
                    // Only placement anchors are clamped and sorted; old bands keep
                    // their order and every setting, including out-of-range Hz.
                    let mut anchors = [0.0; MAX_BANDS + 2];
                    let mut length = bands.len() + 2;
                    anchors[0] = minimum_log;
                    for (anchor, band) in anchors[1..length - 1].iter_mut().zip(bands.iter()) {
                        *anchor = band.frequency_hz.clamp(20.0, maximum_hz).ln();
                    }
                    anchors[length - 1] = maximum_log;
                    anchors[..length].sort_unstable_by(f64::total_cmp);
                    for _ in 0..count {
                        let mut largest_gap = 0;
                        for index in 1..length - 1 {
                            if anchors[index + 1] - anchors[index]
                                > anchors[largest_gap + 1] - anchors[largest_gap]
                            {
                                largest_gap = index;
                            }
                        }
                        let midpoint = (anchors[largest_gap] + anchors[largest_gap + 1]) * 0.5;
                        bands.push(EqBand {
                            frequency_hz: midpoint.exp().clamp(20.0, maximum_hz),
                            ..Default::default()
                        });
                        anchors.copy_within(largest_gap + 1..length, largest_gap + 2);
                        anchors[largest_gap + 1] = midpoint;
                        length += 1;
                    }
                }
            }
            ResizeBands { count } => {
                use rivu_dsp::equalizer::config::MAX_BANDS;
                ensure!(
                    count <= MAX_BANDS,
                    "Equalizer supports at most {MAX_BANDS} bands"
                );
                if count > config.bands.len() {
                    return AddBands {
                        count: count - config.bands.len(),
                    }
                    .apply_config(config, sample_rate);
                }
                config.bands.truncate(count);
            }
            RemoveBand { index } => {
                ensure!(
                    index < config.bands.len(),
                    "Equalizer band no longer exists"
                );
                config.bands.remove(index);
            }
            Replace { config: value } => *config = value,
            owner_edit => return Ok(Some(owner_edit)),
        }
        Ok(None)
    }
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
    pub spectral_gain_floor_db: Option<f32>,
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
    pub denoised: bool,
    pub vocals: bool,
    pub accompaniment: bool,
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
    /// Retained graph run used for snapshot actions, distinct from an export receipt.
    pub graph_run_id: Option<u64>,
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
    /// Full causal PCM retained for suffix execution, before timeline trimming.
    #[serde(default)]
    pub context: Option<GraphOutputContext>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphOutputContext {
    /// Source-relative frame represented by the first context frame.
    pub start_frame: i64,
}

#[cfg(test)]
mod equalizer_edit_tests {
    use super::*;

    #[test]
    fn node_candidates_share_parameter_semantics_without_mutating_monitor_config() {
        let monitor = EqualizerConfig {
            enabled: true,
            preamp_db: -1.125,
            bands: vec![EqBand {
                frequency_hz: 731.25,
                gain_db: 2.625,
                ..Default::default()
            }],
            ..Default::default()
        };
        let baseline = monitor.clone();
        let mut node = monitor.clone();
        assert!(
            EqualizerEdit::BandField {
                index: 0,
                field: EqBandEdit::Position {
                    frequency_hz: 1523.875,
                    gain_db: -3.0625,
                }
            }
            .apply_config(&mut node, Some(48_000))
            .unwrap()
            .is_none()
        );
        assert!(
            EqualizerEdit::AddBands { count: 31 }
                .apply_config(&mut node, Some(48_000))
                .unwrap()
                .is_none()
        );
        assert_eq!(monitor, baseline);
        assert_eq!(node.bands.len(), 32);
        assert_eq!(node.bands[0].frequency_hz, 1523.875);
        assert_eq!(node.bands[0].gain_db, -3.0625);
        assert_eq!(node.preamp_db, monitor.preamp_db);
        let full = node.clone();
        assert!(
            EqualizerEdit::AddBands { count: 1 }
                .apply_config(&mut node, Some(48_000))
                .is_err()
        );
        assert_eq!(node, full);
    }

    #[test]
    fn preset_and_ab_operations_remain_with_the_monitor_owner() {
        let baseline = EqualizerConfig {
            enabled: true,
            output_gain_db: -2.75,
            ..Default::default()
        };
        for edit in [
            EqualizerEdit::SwapAb,
            EqualizerEdit::LoadPreset {
                name: "Reference".to_owned(),
            },
            EqualizerEdit::ImportPreset {
                path: "not-read.json".into(),
                name: "Reference".to_owned(),
                overwrite: false,
            },
            EqualizerEdit::ExportPreset {
                path: "not-written.json".into(),
                name: "Reference".to_owned(),
            },
        ] {
            let mut candidate = baseline.clone();
            let expected = edit.clone();
            assert_eq!(
                edit.apply_config(&mut candidate, Some(48_000)).unwrap(),
                Some(expected)
            );
            assert_eq!(candidate, baseline);
        }
    }
}
