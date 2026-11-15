use rivu_format::config::ReplayGainPeakPolicy;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Runtime facts from the active output, never persisted in the library.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioOutputState {
    pub source_generation: u64,
    pub segment: u64,
    pub active: bool,
    pub source: Arc<AudioSourceFormat>,
    pub graph_processing: bool,
    pub monitor_eq: bool,
    pub master_gain_db: Option<f64>,
    pub replaygain: ReplayGainStatus,
    pub stream: Arc<AudioStreamFormat>,
    pub sink: Option<Arc<str>>,
    pub remix_allowed: bool,
    /// PipeWire graph clock observations, not the physical device format.
    pub graph_rate: Option<u32>,
    pub quantum_frames: Option<u32>,
    pub device_rate: Option<u32>,
    pub device_format: Option<String>,
    pub metrics: OutputMetrics,
    /// Confirmed totals across output restarts in this playback session.
    pub session_underruns: u64,
    pub session_missing_seconds: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioSourceFormat {
    pub codec: String,
    pub sample_rate: u32,
    pub bits_per_sample: Option<u32>,
    pub channels: Vec<String>,
    pub processed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioStreamFormat {
    pub sample_rate: u32,
    pub sample_format: String,
    pub channels: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayGainStatus {
    pub enabled: bool,
    pub peak_policy: ReplayGainPeakPolicy,
    pub requested_gain_db: Option<f64>,
    /// Finite decision in dB; extremely negative values may underflow linear f32 to mute.
    pub applied_gain_db: f64,
    pub source_peak: Option<f64>,
    pub processed_source: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OutputMetrics {
    pub underrun_events: u64,
    pub underrun_callbacks: u64,
    pub missing_frames: u64,
    pub last_underrun_ago_seconds: Option<f64>,
    /// Unconfirmed supply wait; EOF may prove that this was only a normal tail.
    pub pending_missing_frames: u64,
    pub startup_wait_frames: u64,
    pub buffered_frames: u64,
    pub low_water_frames: Option<u64>,
    /// Linear sample peaks, measured after application DSP and gain, before PipeWire.
    /// None means no new audio was observed in this sampling window.
    pub sample_peak: Option<f32>,
    pub sample_peak_hold: Option<f32>,
    /// Frames with at least one sample exceeding full scale, not clipped samples.
    pub over_range_frames: u64,
}
