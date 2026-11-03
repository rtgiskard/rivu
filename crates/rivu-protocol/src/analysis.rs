//! Immutable audio-analysis display contracts. Producers own execution and storage pools.

use rivu_format::config::{AnalysisWindow, Config, VisualizationSignal};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VisualizerDemand {
    pub stereo_density: bool,
    pub spatial_spectrum: bool,
    pub band_energy: bool,
    /// Unwindowed stereo samples for the oscilloscope.
    pub pcm: bool,
    /// Full-resolution non-DC FFT bins for the spectrogram.
    pub spectrogram: bool,
}

impl VisualizerDemand {
    fn from_bits(bits: u64) -> Self {
        Self {
            stereo_density: bits & 1 != 0,
            spatial_spectrum: bits & 2 != 0,
            band_energy: bits & 4 != 0,
            pcm: bits & 16 != 0,
            spectrogram: bits & 32 != 0,
        }
    }

    fn bits(self) -> u64 {
        u64::from(self.stereo_density)
            | (u64::from(self.spatial_spectrum) << 1)
            | (u64::from(self.band_energy) << 2)
            | (u64::from(self.pcm) << 4)
            | (u64::from(self.spectrogram) << 5)
    }
}

/// Complete coalesced demand from the display consumer, independent of playback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplayDemand {
    pub enabled: bool,
    pub visualizer: VisualizerDemand,
}

impl DisplayDemand {
    /// Decode packed display flags, ignoring any higher producer revision bits.
    /// Revision and capture identity remain producer-owned.
    pub fn from_bits(bits: u64) -> Self {
        Self {
            enabled: bits & 8 != 0,
            visualizer: VisualizerDemand::from_bits(bits),
        }
    }

    /// Pack display flags without any producer revision or capture identity.
    pub fn bits(self) -> u64 {
        self.visualizer.bits() | (u64::from(self.enabled) << 3)
    }
}

pub const DENSITY_SIDE: usize = 256;

/// Immutable density cells shared by publications, staging, and visible panels.
/// An empty snapshot owns no allocation; producers publish bounded shared slots.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DensitySnapshot(Option<Arc<Vec<f32>>>);

impl DensitySnapshot {
    /// Publish an existing immutable storage slot without allocating or copying.
    /// The producer retains responsibility for its bounded storage pool.
    pub fn from_shared(storage: Arc<Vec<f32>>) -> Self {
        Self(Some(storage))
    }

    /// Inspect the immutable shared slot, if this publication is available.
    pub fn shared_storage(&self) -> Option<&Arc<Vec<f32>>> {
        self.0.as_ref()
    }
}

impl std::ops::Deref for DensitySnapshot {
    type Target = [f32];

    fn deref(&self) -> &Self::Target {
        self.0.as_deref().map_or(&[], Vec::as_slice)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl From<Vec<f32>> for DensitySnapshot {
    fn from(cells: Vec<f32>) -> Self {
        Self(Some(Arc::new(cells)))
    }
}

/// Immutable coherent-gain-calibrated stereo dB amplitudes, floored at -140 dB,
/// for FFT bins `1..=fft_size / 2`. Bin k is at k * sample_rate / fft_size Hz.
/// An empty snapshot is unavailable; clones share one bounded producer-owned
/// slot. Use the publication's sample rate and settings to identify the axis.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpectrumSnapshot(Option<Arc<Vec<f32>>>);

impl SpectrumSnapshot {
    /// Publish an existing immutable storage slot without allocating or copying.
    /// The producer retains responsibility for its bounded storage pool.
    pub fn from_shared(storage: Arc<Vec<f32>>) -> Self {
        Self(Some(storage))
    }

    /// Inspect the immutable shared slot, if this publication is available.
    pub fn shared_storage(&self) -> Option<&Arc<Vec<f32>>> {
        self.0.as_ref()
    }
}

impl std::ops::Deref for SpectrumSnapshot {
    type Target = [f32];

    fn deref(&self) -> &Self::Target {
        self.0.as_deref().map_or(&[], Vec::as_slice)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl From<Vec<f32>> for SpectrumSnapshot {
    fn from(bins: Vec<f32>) -> Self {
        Self(Some(Arc::new(bins)))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SpatialBin {
    pub frequency_hz: f32,
    /// Energy balance, not a phase-derived source position.
    pub pan: f32,
    /// Coherent-gain-normalized mean channel power. Finite values are in
    /// 1e-14..=1; positive infinity remains missing data for the GUI to filter.
    pub power: f32,
}

/// Immutable recent stereo PCM, shared without copying between displays.
/// An empty snapshot owns no allocation; live publications use bounded slots.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PcmSnapshot(Option<Arc<Vec<[f32; 2]>>>);

impl PcmSnapshot {
    /// Publish an existing immutable storage slot without allocating or copying.
    /// The producer retains responsibility for its bounded storage pool.
    pub fn from_shared(storage: Arc<Vec<[f32; 2]>>) -> Self {
        Self(Some(storage))
    }

    /// Inspect the immutable shared slot, if this publication is available.
    pub fn shared_storage(&self) -> Option<&Arc<Vec<[f32; 2]>>> {
        self.0.as_ref()
    }
}

impl std::ops::Deref for PcmSnapshot {
    type Target = [[f32; 2]];

    fn deref(&self) -> &Self::Target {
        self.0.as_deref().map_or(&[], Vec::as_slice)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl From<Vec<[f32; 2]>> for PcmSnapshot {
    fn from(samples: Vec<[f32; 2]>) -> Self {
        Self(Some(Arc::new(samples)))
    }
}

/// `demand` is the request mask. An empty requested snapshot means unavailable
/// for this publication; it never prevents publishing the base spectrum/RMS.
#[derive(Debug, Default)]
pub struct VisualizerFrame {
    pub demand: VisualizerDemand,
    pub density: DensitySnapshot,
    pub spatial: Vec<SpatialBin>,
    pub pcm: PcmSnapshot,
    pub spectrogram: SpectrumSnapshot,
    pub correlation: Option<f32>,
    pub balance: Option<f32>,
    pub side_ratio: Option<f32>,
    /// Window-power-normalized stereo RMS in 20–250, 250–2500, 2500–20000 Hz.
    pub band_rms: [f32; 3],
}

impl Clone for VisualizerFrame {
    fn clone(&self) -> Self {
        let mut frame = Self::default();
        frame.clone_from(self);
        frame
    }

    fn clone_from(&mut self, source: &Self) {
        self.demand = source.demand;
        if source.demand.stereo_density {
            self.density.clone_from(&source.density);
        } else {
            self.density = DensitySnapshot::default();
        }
        if source.demand.spatial_spectrum {
            self.spatial.clone_from(&source.spatial);
        } else {
            self.spatial = Vec::new();
        }
        if source.demand.pcm {
            self.pcm.clone_from(&source.pcm);
        } else {
            self.pcm = PcmSnapshot::default();
        }
        if source.demand.spectrogram {
            self.spectrogram.clone_from(&source.spectrogram);
        } else {
            self.spectrogram = SpectrumSnapshot::default();
        }
        self.correlation = source.correlation;
        self.balance = source.balance;
        self.side_ratio = source.side_ratio;
        self.band_rms = source.band_rms;
    }
}

#[derive(Clone, Debug, Default)]
pub struct AnalysisFrame {
    pub sequence: u64,
    pub display_token: u64,
    /// PCM fence epoch and admitted source/seek intent producing this publication.
    pub audio_epoch: u64,
    pub audio_intent: u64,
    /// True signal continuity; pause and display capture restarts preserve it.
    pub generation: u64,
    /// Absent until the first publication; identifies the FFT producing this frame.
    pub settings: Option<AnalysisSettings>,
    pub sample_rate: u32,
    /// Monotonic amount of accepted audio represented by this frame.
    pub sample_time: Duration,
    pub frequencies_hz: Vec<f32>,
    pub spectrum_db: Vec<f32>,
    pub rms_left: f32,
    pub rms_right: f32,
    pub visualizer: VisualizerFrame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnalysisSettings {
    pub fps: u32,
    pub fft_size: u32,
    pub window: AnalysisWindow,
    pub signal: VisualizationSignal,
}

impl From<&Config> for AnalysisSettings {
    fn from(config: &Config) -> Self {
        Self {
            fps: config.analysis_rate_hz,
            fft_size: config.analysis_fft_size,
            window: config.analysis_window,
            signal: config.visualization_signal,
        }
    }
}

impl AnalysisSettings {
    /// Whether retained samples and publications have the same signal meaning.
    /// Analysis cadence changes scheduling only, not the FFT or its continuity.
    pub fn signal_compatible_with(self, other: Self) -> bool {
        self.fft_size == other.fft_size
            && self.window == other.window
            && self.signal == other.signal
    }

    pub fn is_valid(self) -> bool {
        (5..=60).contains(&self.fps)
            && matches!(
                self.fft_size,
                512 | 1024 | 2048 | 4096 | 8192 | 16384 | 32768
            )
    }
}
