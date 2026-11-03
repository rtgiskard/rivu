//! Immutable waveform display contracts; decoding and cancellation remain audio-owned.
use rivu_format::config::{Config, VisualizationSignal};
use rivu_format::decoder_hint::DecoderHint;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Content version of an opened file or an immutable retained PCM span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceIdentity {
    File {
        device: u64,
        inode: u64,
        len: u64,
        modified: (i64, i64),
        changed: (i64, i64),
    },
    Pcm {
        storage: u64,
        start: u64,
        frames: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackRange {
    pub start_seconds: f64,
    pub end_seconds: Option<f64>,
}

/// Captured when a decoder is loaded, never refreshed from a path or library
/// snapshot. Keep the initial duration separate from the EOF-corrected timeline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WaveformIdentity {
    Progressive {
        content: SourceIdentity,
        ffmpeg: bool,
        duration: Option<f64>,
        sample_rate: u32,
        channels: u16,
    },
    FullScan {
        duration: Option<f64>,
        sample_rate: u32,
    },
}

#[derive(Clone, Debug)]
pub struct WaveformFrame {
    pub revision: u64,
    pub complete: bool,
    pub path: Option<PathBuf>,
    pub range: Option<PlaybackRange>,
    pub decoder_hint: DecoderHint,
    pub duration: Option<f64>,
    pub span_seconds: f64,
    // Sample width is bin_frames / bin_count, independent of EOF array truncation.
    // Widen the numerator so coarsening cannot overflow the u64 sample domain.
    pub bin_frames: u128,
    pub bin_count: u64,
    pub max_peak: f32,
    /// `None` is not yet decoded; `Some(0.0)` is decoded silence.
    pub peaks: Vec<Option<f32>>,
    /// RMS of the current contiguous decoded interval in each rolling bin.
    pub rms: Vec<Option<f32>>,
    pub rms_counts: Vec<u64>,
    pub left_peaks: Vec<Option<f32>>,
    pub right_peaks: Vec<Option<f32>>,
    pub left_rms: Vec<Option<f32>>,
    pub right_rms: Vec<Option<f32>>,
    pub left_rms_counts: Vec<u64>,
    pub right_rms_counts: Vec<u64>,
    pub rms_spans: Vec<Option<std::ops::Range<u64>>>,
    /// Last PCM revision per bin, used to fence asynchronous full scans.
    pub bin_revisions: Vec<u64>,
    pub bin_epochs: Vec<u64>,
    traversal: u64,
    suspended: bool,
    suspension_generation: u64,
    pub identity: Option<WaveformIdentity>,
    pub scan_source_identity: Option<SourceIdentity>,
}

impl Default for WaveformFrame {
    fn default() -> Self {
        Self {
            revision: 0,
            path: None,
            complete: false,
            range: None,
            decoder_hint: DecoderHint::Auto,
            duration: None,
            span_seconds: 0.0,
            bin_frames: 0,
            bin_count: 0,
            max_peak: 1.0,
            peaks: Vec::new(),
            rms: Vec::new(),
            rms_counts: Vec::new(),
            left_peaks: Vec::new(),
            right_peaks: Vec::new(),
            left_rms: Vec::new(),
            right_rms: Vec::new(),
            left_rms_counts: Vec::new(),
            right_rms_counts: Vec::new(),
            rms_spans: Vec::new(),
            bin_revisions: Vec::new(),
            bin_epochs: Vec::new(),
            traversal: 1,
            suspended: false,
            suspension_generation: 0,
            identity: None,
            scan_source_identity: None,
        }
    }
}

impl WaveformFrame {
    pub fn matches(
        &self,
        path: &Path,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
    ) -> bool {
        self.path.as_deref() == Some(path)
            && self.range == range
            && self.decoder_hint == decoder_hint
    }

    pub fn suspension_generation(&self) -> u64 {
        self.suspension_generation
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    pub fn traversal_epoch(&self) -> u64 {
        self.traversal
    }

    /// Heap storage retained by the bounded display snapshot.
    pub fn retained_capacity_bytes(&self) -> usize {
        fn bytes<T>(values: &Vec<T>) -> usize {
            values.capacity() * std::mem::size_of::<T>()
        }
        bytes(&self.peaks)
            + bytes(&self.rms)
            + bytes(&self.rms_counts)
            + bytes(&self.left_peaks)
            + bytes(&self.right_peaks)
            + bytes(&self.left_rms)
            + bytes(&self.right_rms)
            + bytes(&self.left_rms_counts)
            + bytes(&self.right_rms_counts)
            + bytes(&self.rms_spans)
            + bytes(&self.bin_revisions)
            + bytes(&self.bin_epochs)
    }

    /// Record producer-owned state as snapshot metadata, without managing its lifecycle.
    pub fn set_producer_metadata(
        &mut self,
        traversal: u64,
        suspended: bool,
        suspension_generation: u64,
    ) {
        self.traversal = traversal;
        self.suspended = suspended;
        self.suspension_generation = suspension_generation;
    }
}
/// Explicit scan setup; unlike rendering, preparation may clone equalizer state.
pub struct WaveformScanParameters {
    pub signal: VisualizationSignal,
    pub equalizer: Arc<rivu_format::config::EqualizerConfig>,
    pub buffer_mib: u32,
    pub ffmpeg_enabled: bool,
}

impl From<&Config> for WaveformScanParameters {
    fn from(config: &Config) -> Self {
        Self {
            signal: config.visualization_signal,
            equalizer: Arc::new(config.equalizer.clone()),
            buffer_mib: config.media_read_buffer_mib,
            ffmpeg_enabled: config.ffmpeg_enabled,
        }
    }
}
