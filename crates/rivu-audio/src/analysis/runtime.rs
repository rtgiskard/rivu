use parking_lot::RwLock;
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer},
};
use rivu_format::config::{AnalysisWindow, Config, VisualizationSignal};
use rustfft::{Fft, FftPlanner, num_complex::Complex32};
use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VisualizerDemand {
    pub stereo_density: bool,
    pub spatial_spectrum: bool,
    pub band_energy: bool,
    /// Unwindowed stereo samples for the oscilloscope.
    pub pcm: bool,
}

impl VisualizerDemand {
    fn from_bits(bits: u64) -> Self {
        Self {
            stereo_density: bits & 1 != 0,
            spatial_spectrum: bits & 2 != 0,
            band_energy: bits & 4 != 0,
            pcm: bits & 16 != 0,
        }
    }

    fn bits(self) -> u64 {
        u64::from(self.stereo_density)
            | (u64::from(self.spatial_spectrum) << 1)
            | (u64::from(self.band_energy) << 2)
            | (u64::from(self.pcm) << 4)
    }
}

/// Complete coalesced demand from the display consumer, independent of playback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplayDemand {
    pub enabled: bool,
    pub visualizer: VisualizerDemand,
}

impl DisplayDemand {
    const MASK: u64 = 31;
    // Lower bits carry flags and a revision for every request transition;
    // upper bits fence PCM only when display capture is disabled/re-enabled.
    const CAPTURE_SHIFT: u32 = 32;
    const CAPTURE_STEP: u64 = 1 << Self::CAPTURE_SHIFT;
    const REQUEST_MASK: u64 = Self::CAPTURE_STEP - 1;

    fn capture_token(token: u64) -> u64 {
        token >> Self::CAPTURE_SHIFT
    }

    fn from_bits(bits: u64) -> Self {
        Self {
            enabled: bits & 8 != 0,
            visualizer: VisualizerDemand::from_bits(bits),
        }
    }

    fn bits(self) -> u64 {
        self.visualizer.bits() | (u64::from(self.enabled) << 3)
    }
}

pub const DENSITY_SIDE: usize = 256;
// One accumulation field plus three immutable publication slots. Three slots
// cover a GUI-held frame, the latest publication, and the next worker result.
const DENSITY_SNAPSHOT_COUNT: usize = 3;

/// Immutable density cells shared by publications, staging, and visible panels.
/// An empty snapshot owns no allocation; only the analyzer creates live slots.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DensitySnapshot(Option<Arc<Vec<f32>>>);

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

// Each prepared analyzer owns at most 160 KiB of reusable PCM sample storage.
// Slow consumers omit optional PCM instead of growing this pool.
const PCM_SNAPSHOT_COUNT: usize = 5;
const PCM_SNAPSHOT_FRAMES: usize = 4096;

/// `demand` is the request mask. An empty requested snapshot means unavailable
/// for this publication; it never prevents publishing the base spectrum/RMS.
#[derive(Debug, Default)]
pub struct VisualizerFrame {
    pub demand: VisualizerDemand,
    pub density: DensitySnapshot,
    pub spatial: Vec<SpatialBin>,
    pub pcm: PcmSnapshot,
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
        self.correlation = source.correlation;
        self.balance = source.balance;
        self.side_ratio = source.side_ratio;
        self.band_rms = source.band_rms;
    }
}

/// Lazy exponential decay keeps sample ingestion O(1), with bounded periodic
/// renormalization. Weights are normalized by accepted sample time (tau=0.5s).
struct DensityField {
    cells: Vec<f32>,
    snapshots: [Arc<Vec<f32>>; DENSITY_SNAPSHOT_COUNT],
    scale: f32,
    decay: f32,
    weight: f32,
}

impl DensityField {
    fn new(sample_rate: u32) -> Self {
        let decay = (-2.0 / sample_rate.max(1) as f32).exp();
        Self {
            cells: vec![0.0; DENSITY_SIDE * DENSITY_SIDE],
            snapshots: std::array::from_fn(|_| Arc::new(vec![0.0; DENSITY_SIDE * DENSITY_SIDE])),
            scale: 1.0,
            decay,
            weight: 1.0 - decay,
        }
    }

    fn clear(&mut self) {
        self.cells.fill(0.0);
        self.scale = 1.0;
    }

    fn push(&mut self, [left, right]: [f32; 2]) {
        self.scale *= self.decay;
        if self.scale < 0.5 {
            for cell in &mut self.cells {
                *cell *= self.scale;
            }
            self.scale = 1.0;
        }
        // Digital silence advances decay, but never draws a false center dot.
        if left == 0.0 && right == 0.0 {
            return;
        }
        let side = ((left - right) * 0.5).clamp(-1.0, 1.0);
        let mid = ((left + right) * 0.5).clamp(-1.0, 1.0);
        let last = (DENSITY_SIDE - 1) as f32;
        let x = ((side + 1.0) * 0.5 * last).round() as usize;
        let y = ((1.0 - mid) * 0.5 * last).round() as usize;
        self.cells[y * DENSITY_SIDE + x] += self.weight / self.scale;
    }

    fn snapshot(&mut self) -> DensitySnapshot {
        for slot in &mut self.snapshots {
            if let Some(cells) = Arc::get_mut(slot) {
                for (target, cell) in cells.iter_mut().zip(&self.cells) {
                    *target = cell * self.scale;
                }
                return DensitySnapshot(Some(Arc::clone(slot)));
            }
        }
        // A slow consumer may retain every slot. Omit this optional payload
        // rather than allocate, block, or delay the base spectrum publication.
        DensitySnapshot::default()
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
            fps: config.analysis_fps,
            fft_size: config.analysis_fft_size,
            window: config.analysis_window,
            signal: config.visualization_signal,
        }
    }
}

impl AnalysisSettings {
    fn valid(self) -> bool {
        (5..=60).contains(&self.fps)
            && matches!(
                self.fft_size,
                512 | 1024 | 2048 | 4096 | 8192 | 16384 | 32768
            )
    }
}

#[derive(Clone, Copy, Default)]
pub struct TapFrame {
    pub samples: [f32; 2],
    pub epoch: u64,
    /// Request identity at capture; only its capture revision fences PCM.
    pub demand_token: u64,
    /// A genuine dropped-sample or source boundary; never bridge it in an FFT.
    pub gap: bool,
}

pub struct AnalysisControl {
    pub(crate) playback_active: AtomicBool,
    settings: RwLock<AnalysisSettings>,
    /// Playback/output sample rate used to map FFT bins to frequencies.
    pub(crate) sample_rate: AtomicU32,
    /// Even epochs are stable; odd epochs mark sample-rate reset in progress.
    pub(crate) epoch: AtomicU64,
    /// Semantic resets, independent of pause/display capture epoch fences.
    continuity: AtomicU64,
    pub(crate) desired_intent: AtomicU64,
    applied_intent: AtomicU64,
    shutdown: AtomicBool,
    // Complete display demand and its transition identity are one snapshot.
    display_demand: AtomicU64,
    // One consumer permit, never an accumulating queue of analysis jobs.
    frame_requested: AtomicBool,
    // Overflow invalidates the queued prefix before a later gap-marked tap arrives.
    pub(crate) tap_overflow: AtomicBool,
    worker_thread: OnceLock<thread::Thread>,
    // Low bit is Still Mode; upper bits preserve quick destructive transitions.
    still_mode: AtomicU64,
}

impl AnalysisControl {
    pub fn frame_is_current(&self, frame: &AnalysisFrame) -> bool {
        frame.display_token == self.display_token()
            && frame.audio_epoch == self.epoch.load(Ordering::Acquire)
            && frame.audio_intent == self.desired_intent.load(Ordering::Acquire)
            && frame.audio_intent == self.applied_intent.load(Ordering::Acquire)
    }

    pub(crate) fn apply_audio_intents(&self, count: u64) {
        if count == 0 {
            return;
        }
        let applied = self.applied_intent.load(Ordering::Acquire);
        let pending = self
            .desired_intent
            .load(Ordering::Acquire)
            .wrapping_sub(applied);
        if pending != 0 {
            // One fence suffices for an already coalesced prefix: no PCM can
            // be admitted between these serialized discarded seek intents.
            self.continuity.fetch_add(1, Ordering::AcqRel);
            self.epoch.fetch_add(2, Ordering::AcqRel);
            self.applied_intent
                .store(applied.wrapping_add(count.min(pending)), Ordering::Release);
        }
    }

    fn new() -> Self {
        Self {
            playback_active: AtomicBool::new(false),
            settings: RwLock::new(AnalysisSettings::from(&Config::default())),
            sample_rate: AtomicU32::new(48_000),
            epoch: AtomicU64::new(0),
            continuity: AtomicU64::new(0),
            desired_intent: AtomicU64::new(0),
            applied_intent: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            display_demand: AtomicU64::new(0),
            frame_requested: AtomicBool::new(false),
            tap_overflow: AtomicBool::new(false),
            worker_thread: OnceLock::new(),
            still_mode: AtomicU64::new(0),
        }
    }

    #[cfg(feature = "test-support")]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub fn request_frame(&self) {
        if self.accepts_audio() {
            self.frame_requested.store(true, Ordering::Release);
            if let Some(worker) = self.worker_thread.get() {
                // A coalesced permit may be parked waiting for fresh input.
                worker.unpark();
            }
        }
    }

    pub fn display_token(&self) -> u64 {
        self.display_demand.load(Ordering::Acquire)
    }

    pub(crate) fn accepts_audio(&self) -> bool {
        self.accepts_audio_for(self.display_token())
    }

    pub(crate) fn accepts_audio_for(&self, demand_token: u64) -> bool {
        self.playback_active.load(Ordering::Acquire)
            && DisplayDemand::from_bits(demand_token).enabled
            && self.still_mode.load(Ordering::Acquire) & 1 == 0
    }

    pub fn set_display_demand(&self, demand: DisplayDemand) {
        self.update_display_demand(|_| demand);
    }

    // The public Analysis command changes only enablement, not plot selection.
    pub fn set_display_enabled(&self, enabled: bool) {
        self.update_display_demand(|demand| DisplayDemand { enabled, ..demand });
    }

    fn update_display_demand(&self, update: impl Fn(DisplayDemand) -> DisplayDemand) {
        if let Ok(previous) =
            self.display_demand
                .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    let previous = DisplayDemand::from_bits(current);
                    let demand = update(previous);
                    (previous != demand).then(|| {
                        let capture = current & !DisplayDemand::REQUEST_MASK;
                        let capture = if previous.enabled != demand.enabled {
                            capture.wrapping_add(DisplayDemand::CAPTURE_STEP)
                        } else {
                            capture
                        };
                        let request = current.wrapping_add(DisplayDemand::MASK + 1)
                            & DisplayDemand::REQUEST_MASK
                            & !DisplayDemand::MASK;
                        capture | request | demand.bits()
                    })
                })
        {
            if DisplayDemand::capture_token(previous)
                != DisplayDemand::capture_token(self.display_token())
            {
                self.frame_requested.store(false, Ordering::Release);
            }
            if let Some(worker) = self.worker_thread.get() {
                // Release old results/storage even with no outstanding frame request.
                worker.unpark();
            }
        }
    }
}

/// Capture identity for retained audio; optional payload requests do not change it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct AnalysisAudioIdentity {
    epoch: u64,
    capture_token: u64,
}

/// The audio worker is the sole producer; device callbacks never inspect analysis.
pub(crate) struct AnalysisProducer {
    ring: HeapProd<TapFrame>,
    control: Arc<AnalysisControl>,
    // A genuine PCM gap belongs to the capture in which it occurred. A fresh
    // capture must not inherit an artificial gap from suppressed old samples.
    gap: Option<AnalysisAudioIdentity>,
}

impl AnalysisProducer {
    pub(crate) fn discontinuity(&mut self) {
        self.gap = self.identity();
    }

    pub(crate) fn identity(&self) -> Option<AnalysisAudioIdentity> {
        let demand_token = self.control.display_token();
        let epoch = self.control.epoch.load(Ordering::Acquire);
        (epoch.is_multiple_of(2) && self.control.accepts_audio_for(demand_token)).then_some(
            AnalysisAudioIdentity {
                epoch,
                capture_token: DisplayDemand::capture_token(demand_token),
            },
        )
    }

    pub(crate) fn push(&mut self, pcm: &[f32], weights: &[[f32; 2]]) {
        if weights.is_empty() {
            self.discontinuity();
            return;
        }
        self.push_samples(pcm.chunks_exact(weights.len()).map(|frame| {
            let mut samples = [0.0; 2];
            for (sample, weight) in frame.iter().zip(weights) {
                samples[0] += sample * weight[0];
                samples[1] += sample * weight[1];
            }
            samples
        }));
    }

    pub(crate) fn push_samples(&mut self, samples: impl ExactSizeIterator<Item = [f32; 2]>) {
        let Some(identity) = self.identity() else {
            return;
        };
        self.push_samples_for(identity, samples);
    }

    pub(crate) fn push_samples_for(
        &mut self,
        identity: AnalysisAudioIdentity,
        samples: impl ExactSizeIterator<Item = [f32; 2]>,
    ) {
        if samples.len() == 0 {
            return;
        }
        if self.identity() != Some(identity) {
            return;
        }
        let gap = self.gap == Some(identity);
        let demand_token = self.control.display_token();
        if DisplayDemand::capture_token(demand_token) != identity.capture_token {
            return;
        }
        let frames = samples.len();
        let accepted = self
            .ring
            .push_iter(samples.enumerate().map(|(index, samples)| TapFrame {
                samples,
                epoch: identity.epoch,
                demand_token,
                gap: gap && index == 0,
            }));
        self.gap = (accepted < frames).then_some(identity);
        if self.gap.is_some() {
            self.control.tap_overflow.store(true, Ordering::Release);
        }
        if (accepted != 0 || self.gap.is_some())
            && let Some(worker) = self.control.worker_thread.get()
        {
            worker.unpark();
        }
    }
}

#[cfg(test)]
impl AnalysisProducer {
    pub(crate) fn capture_for_test(
        worker: &AnalysisWorker,
        capacity: usize,
    ) -> (Self, HeapCons<TapFrame>) {
        use ringbuf::traits::Split;
        let (ring, consumer) = HeapRb::new(capacity).split();
        (
            Self {
                ring,
                control: Arc::clone(&worker.control),
                gap: None,
            },
            consumer,
        )
    }
}

pub struct AnalysisWorker {
    pub control: Arc<AnalysisControl>,
    pub ring: Arc<HeapRb<TapFrame>>,
    worker: Option<JoinHandle<()>>,
}

impl AnalysisWorker {
    pub fn new(shared: Arc<RwLock<AnalysisFrame>>) -> std::io::Result<Self> {
        Self::with_capacity(shared, 32_768)
    }

    fn with_capacity(shared: Arc<RwLock<AnalysisFrame>>, capacity: usize) -> std::io::Result<Self> {
        let ring = Arc::new(HeapRb::new(capacity));
        let control = Arc::new(AnalysisControl::new());
        let consumer = HeapCons::new(ring.clone());
        let thread_control = control.clone();
        let worker = thread::Builder::new()
            .name("rivu-analysis".into())
            .spawn(move || analyze(consumer, thread_control, shared))?;
        let _ = control.worker_thread.set(worker.thread().clone());
        Ok(Self {
            control,
            ring,
            worker: Some(worker),
        })
    }
    pub fn producer(&self) -> HeapProd<TapFrame> {
        HeapProd::new(self.ring.clone())
    }
    pub(crate) fn audio_producer(&self) -> AnalysisProducer {
        AnalysisProducer {
            ring: self.producer(),
            control: Arc::clone(&self.control),
            gap: None,
        }
    }

    pub(crate) fn settings(&self) -> AnalysisSettings {
        *self.control.settings.read()
    }
    pub fn set_playback_active(&self, enabled: bool) {
        if self.control.playback_active.swap(enabled, Ordering::AcqRel) != enabled {
            self.control.frame_requested.store(false, Ordering::Release);
            // A quick disable/enable must invalidate queued samples even if the
            // analysis thread never observes the disabled state.
            self.control.epoch.fetch_add(2, Ordering::AcqRel);
            if let Some(worker) = &self.worker {
                worker.thread().unpark();
            }
        }
    }
    pub(crate) fn stop(&self) {
        self.set_playback_active(false);
        self.control.continuity.fetch_add(1, Ordering::AcqRel);
        self.control.epoch.fetch_add(2, Ordering::AcqRel);
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }

    pub fn set_still_mode(&self, enabled: bool) {
        let current = self.control.still_mode.load(Ordering::Acquire);
        if (current & 1 != 0) == enabled {
            return;
        }
        // Use the same reset barrier as sample-rate changes. The analysis
        // thread owns every FFT allocation and drop.
        self.control.epoch.fetch_add(1, Ordering::AcqRel);
        self.control.continuity.fetch_add(1, Ordering::AcqRel);
        self.control.frame_requested.store(false, Ordering::Release);
        self.control.still_mode.store(
            (current & !1).wrapping_add(2) | u64::from(enabled),
            Ordering::Release,
        );
        self.control.epoch.fetch_add(1, Ordering::AcqRel);
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }
    pub fn reset(&self, sample_rate: u32) {
        self.control.epoch.fetch_add(1, Ordering::AcqRel);
        self.control.continuity.fetch_add(1, Ordering::AcqRel);
        self.control.frame_requested.store(false, Ordering::Release);
        self.control
            .sample_rate
            .store(sample_rate, Ordering::Release);
        self.control.epoch.fetch_add(1, Ordering::AcqRel);
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }
    pub fn configure(&self, settings: AnalysisSettings) {
        if settings.valid() {
            let mut current = self.control.settings.write();
            if *current == settings {
                return;
            }
            *current = settings;
            self.control.frame_requested.store(false, Ordering::Release);
            self.control.continuity.fetch_add(1, Ordering::AcqRel);
            self.control.epoch.fetch_add(2, Ordering::AcqRel);
            drop(current);
            if let Some(worker) = &self.worker {
                worker.thread().unpark();
            }
        }
    }
}
impl Drop for AnalysisWorker {
    fn drop(&mut self) {
        self.control.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

fn musical_frequencies(sample_rate: u32) -> Vec<f32> {
    let nyquist = sample_rate as f32 * 0.5;
    let mut frequencies = Vec::with_capacity(12 * 16);
    if nyquist <= 0.0 {
        return frequencies;
    }
    const FIRST_NOTE: i32 = -69;
    for note in FIRST_NOTE.. {
        let center = 440.0 * 2.0_f32.powf(note as f32 / 12.0);
        if center >= nyquist {
            break;
        }
        frequencies.push(center);
    }
    frequencies.push(nyquist);
    frequencies
}

/// FFT plans and frame-sized storage exist only after accepted audio arrives.
struct SpectrumAnalyzer {
    settings: AnalysisSettings,
    sample_rate: u32,
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
    frequency: Vec<Complex32>,
    window: Vec<f32>,
    normalization: f32,
    power_normalization: f32,
    drive_bins: [(usize, usize); 3],
    frequencies: Vec<f32>,
    bands: Vec<(usize, usize)>,
    levels: Vec<f32>,
    rolling: Vec<[f32; 2]>,
    head: usize,
    filled: usize,
    generation: u64,
    metadata_dirty: bool,
    visualizer: VisualizerFrame,
    density: Option<DensityField>,
    pcm_snapshots: Option<[Arc<Vec<[f32; 2]>>; PCM_SNAPSHOT_COUNT]>,
}

impl SpectrumAnalyzer {
    fn new(settings: AnalysisSettings, sample_rate: u32) -> Self {
        let n = settings.fft_size as usize;
        let fft = FftPlanner::<f32>::new().plan_fft_forward(n);
        let window: Vec<f32> = (0..n)
            .map(|i| {
                let phase = std::f32::consts::TAU * i as f32 / n as f32;
                match settings.window {
                    AnalysisWindow::Hann => 0.5 - 0.5 * phase.cos(),
                    AnalysisWindow::BlackmanHarris => {
                        0.35875 - 0.48829 * phase.cos() + 0.14128 * (2.0 * phase).cos()
                            - 0.01168 * (3.0 * phase).cos()
                    }
                    AnalysisWindow::None => 1.0,
                }
            })
            .collect();
        // Coherent gain keeps bin-centered sine amplitudes calibrated across windows.
        let normalization = 2.0 / window.iter().sum::<f32>();
        // Parseval / window energy, not coherent gain or max-pooled display bands.
        let power_normalization = 1.0 / (n as f32 * window.iter().map(|w| w * w).sum::<f32>());
        let edge_bin = |hz: f32| {
            (hz * n as f32 / sample_rate.max(1) as f32)
                .ceil()
                .clamp(1.0, (n / 2 + 1) as f32) as usize
        };
        let edges = [20.0, 250.0, 2500.0, 20_000.0].map(edge_bin);
        let drive_bins = std::array::from_fn(|i| (edges[i], edges[i + 1]));
        let frequencies = musical_frequencies(sample_rate);
        let half_band = 2.0_f32.powf(0.5 / 12.0);
        let bin_scale = n as f32 / sample_rate.max(1) as f32;
        let bands = frequencies
            .iter()
            .map(|center| {
                let first = (center / half_band * bin_scale).ceil().max(1.0) as usize;
                let last = (center * half_band * bin_scale).floor() as usize;
                if first <= last {
                    (first.min(n / 2), (last + 1).min(n / 2 + 1))
                } else {
                    // Sub-bin musical bands use the nearest resolved FFT bin.
                    let nearest = (center * bin_scale).round().clamp(1.0, (n / 2) as f32) as usize;
                    (nearest, nearest + 1)
                }
            })
            .collect();
        Self {
            settings,
            sample_rate,
            scratch: vec![Complex32::default(); fft.get_inplace_scratch_len()],
            frequency: vec![Complex32::default(); n],
            fft,
            window,
            normalization,
            power_normalization,
            drive_bins,
            levels: vec![-140.0; frequencies.len()],
            frequencies,
            bands,
            rolling: vec![[0.0; 2]; n],
            head: 0,
            filled: 0,
            generation: 0,
            metadata_dirty: true,
            visualizer: VisualizerFrame::default(),
            density: None,
            pcm_snapshots: None,
        }
    }

    fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.clear_window();
    }

    fn clear_window(&mut self) {
        self.head = 0;
        self.filled = 0;
        if let Some(density) = &mut self.density {
            density.clear();
        }
        self.visualizer.density = DensitySnapshot::default();
        self.visualizer.spatial.clear();
        self.visualizer.correlation = None;
        self.visualizer.balance = None;
        self.visualizer.side_ratio = None;
        self.visualizer.band_rms = [0.0; 3];
        self.visualizer.pcm = PcmSnapshot::default();
    }

    fn set_visualizer_demand(&mut self, demand: VisualizerDemand) {
        if demand.stereo_density {
            if self.density.is_none() {
                self.density = Some(DensityField::new(self.sample_rate));
            }
        } else {
            self.density = None;
            self.visualizer.density = DensitySnapshot::default();
            self.visualizer.correlation = None;
            self.visualizer.balance = None;
            self.visualizer.side_ratio = None;
        }
        if demand.spatial_spectrum {
            if !self.visualizer.demand.spatial_spectrum {
                self.visualizer.spatial.reserve(self.rolling.len() / 2);
            }
        } else {
            self.visualizer.spatial = Vec::new();
        }
        if !demand.band_energy {
            self.visualizer.band_rms = [0.0; 3];
        }
        if demand.pcm {
            self.pcm_snapshots.get_or_insert_with(|| {
                std::array::from_fn(|_| {
                    Arc::new(vec![[0.0; 2]; self.rolling.len().min(PCM_SNAPSHOT_FRAMES)])
                })
            });
        } else {
            self.visualizer.pcm = PcmSnapshot::default();
            self.pcm_snapshots = None;
        }
        self.visualizer.demand = demand;
    }

    fn push(&mut self, samples: [f32; 2]) {
        // Optional plots also guard folded channels: finite decoded PCM does
        // not guarantee finite results after channel-mixing arithmetic.
        let samples = if self.visualizer.demand == VisualizerDemand::default() {
            samples
        } else {
            samples.map(|value| if value.is_finite() { value } else { 0.0 })
        };
        if let Some(density) = &mut self.density {
            density.push(samples);
        }
        self.rolling[self.head] = samples;
        self.head = (self.head + 1) % self.rolling.len();
        self.filled = (self.filled + 1).min(self.rolling.len());
    }

    fn push_tap(&mut self, frame: &TapFrame) {
        if frame.gap {
            self.reset();
        }
        self.push(frame.samples);
    }

    fn transform(&mut self) -> [f32; 2] {
        let n = self.rolling.len();
        let mut rms = [0.0_f32; 2];
        let density_demand = self.visualizer.demand.stereo_density;
        let mut energies = [0.0_f64; 3];
        for (i, samples) in self.rolling[self.head..]
            .iter()
            .chain(&self.rolling[..self.head])
            .enumerate()
        {
            rms[0] += samples[0] * samples[0];
            rms[1] += samples[1] * samples[1];
            if density_demand {
                let left = samples[0] as f64;
                let right = samples[1] as f64;
                energies[0] += left * left;
                energies[1] += right * right;
                energies[2] += left * right;
            }
            // Pack both real channels; the merged power below preserves
            // opposite-phase stereo without reconstructing either spectrum.
            self.frequency[i] =
                Complex32::new(samples[0] * self.window[i], samples[1] * self.window[i]);
        }
        self.fft
            .process_with_scratch(&mut self.frequency, &mut self.scratch);
        if self.visualizer.demand.band_energy {
            for (target, &(first, last)) in
                self.visualizer.band_rms.iter_mut().zip(&self.drive_bins)
            {
                let mut power = 0.0;
                for bin in first..last {
                    let endpoint = if bin == n / 2 { 0.5 } else { 1.0 };
                    power += (self.frequency[bin].norm_sqr()
                        + self.frequency[(n - bin) % n].norm_sqr())
                        * 0.5
                        * endpoint;
                }
                *target = (power * self.power_normalization).sqrt();
            }
        }
        if density_demand {
            let [left, right, cross] = energies;
            let total = left + right;
            self.visualizer.correlation = (left > 0.0 && right > 0.0)
                .then(|| (cross / (left * right).sqrt()).clamp(-1.0, 1.0) as f32);
            self.visualizer.balance =
                (total > 0.0).then(|| ((right - left) / total).clamp(-1.0, 1.0) as f32);
            self.visualizer.side_ratio = (total > 0.0)
                .then(|| ((total - 2.0 * cross) / (2.0 * total)).clamp(0.0, 1.0) as f32);
            // Relinquish our previous handle before looking for a reusable slot.
            self.visualizer.density = DensitySnapshot::default();
            if let Some(density) = &mut self.density {
                self.visualizer.density = density.snapshot();
            }
        }
        if self.visualizer.demand.spatial_spectrum {
            self.visualizer.spatial.clear();
            let normalization_squared = self.normalization * self.normalization;
            for bin in 1..=n / 2 {
                let packed = self.frequency[bin];
                let partner = self.frequency[(n - bin) % n].conj();
                let left = (packed + partner) * 0.5;
                let right = (packed - partner) * Complex32::new(0.0, -0.5);
                let left_power = left.norm_sqr();
                let right_power = right.norm_sqr();
                let total = left_power + right_power;
                let endpoint_gain = if bin == n / 2 { 0.25 } else { 1.0 };
                let power = total * 0.5 * endpoint_gain;
                self.visualizer.spatial.push(SpatialBin {
                    frequency_hz: bin as f32 * self.sample_rate as f32 / n as f32,
                    pan: if total > 0.0 {
                        ((right_power - left_power) / total).clamp(-1.0, 1.0)
                    } else {
                        0.0
                    },
                    power: calibrated_spatial_power(power, normalization_squared),
                });
            }
        }
        for (level, &(first, last)) in self.levels.iter_mut().zip(&self.bands) {
            let mut power = 0.0_f32;
            for bin in first..last {
                let packed = self.frequency[bin];
                let mirrored = self.frequency[(n - bin) % n];
                // Nyquist has no negative-frequency partner to double.
                let endpoint_gain = if bin == n / 2 { 0.25 } else { 1.0 };
                power = power.max((packed.norm_sqr() + mirrored.norm_sqr()) * 0.25 * endpoint_gain);
            }
            *level = (power.sqrt() * self.normalization).max(1e-7).log10() * 20.0;
        }
        [(rms[0] / n as f32).sqrt(), (rms[1] / n as f32).sqrt()]
    }

    fn snapshot_pcm(&mut self) {
        // Release the worker's handle before seeking a uniquely owned slot.
        self.visualizer.pcm = PcmSnapshot::default();
        let Some(slots) = &mut self.pcm_snapshots else {
            return;
        };
        for slot in slots {
            if let Some(samples) = Arc::get_mut(slot) {
                let n = self.rolling.len();
                let start = (self.head + n - samples.len()) % n;
                let first_len = samples.len().min(n - start);
                let second_len = samples.len() - first_len;
                let (first, second) = samples.split_at_mut(first_len);
                first.copy_from_slice(&self.rolling[start..start + first_len]);
                second.copy_from_slice(&self.rolling[..second_len]);
                self.visualizer.pcm = PcmSnapshot(Some(Arc::clone(slot)));
                return;
            }
        }
        // All slots held by slow consumers: omit PCM for this publication,
        // never allocate, wait, or pair old PCM with new metrics.
    }

    fn publish(&mut self, frame: &mut AnalysisFrame, rms: [f32; 2], sample_time: Duration) {
        if self.visualizer.demand.pcm {
            self.snapshot_pcm();
        }
        // clone_from reuses storage for steady-state publications.
        if self.metadata_dirty {
            frame.frequencies_hz.clone_from(&self.frequencies);
            self.metadata_dirty = false;
        }
        frame.spectrum_db.clone_from(&self.levels);
        frame.visualizer.clone_from(&self.visualizer);
        frame.sample_rate = self.sample_rate;
        frame.generation = self.generation;
        frame.settings = Some(self.settings);
        frame.rms_left = rms[0];
        frame.rms_right = rms[1];
        frame.sample_time = sample_time;
        frame.sequence = frame.sequence.wrapping_add(1);
    }
}

fn calibrated_spatial_power(power: f32, normalization_squared: f32) -> f32 {
    let normalized = power * normalization_squared;
    if normalized == f32::INFINITY {
        // A finite PCM sample can still overflow FFT power. Do not turn a
        // previously missing bin into a saturated bright cell.
        normalized
    } else if normalized.is_nan() {
        // Preserve the old amplitude floor for undefined FFT power.
        1e-14
    } else {
        normalized.clamp(1e-14, 1.0)
    }
}

/// Control invalidations do not require FFT plans or frame-sized storage.
struct AnalysisRuntime {
    analyzer: Option<SpectrumAnalyzer>,
    generation: u64,
    settings: AnalysisSettings,
    sample_rate: u32,
    demand: VisualizerDemand,
}

impl AnalysisRuntime {
    fn new(settings: AnalysisSettings, sample_rate: u32) -> Self {
        Self {
            analyzer: None,
            generation: 0,
            settings,
            sample_rate,
            demand: VisualizerDemand::default(),
        }
    }

    fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(analyzer) = &mut self.analyzer {
            analyzer.reset();
        }
    }

    fn clear_window(&mut self) {
        if let Some(analyzer) = &mut self.analyzer {
            analyzer.clear_window();
        }
    }

    /// Called only after a tap with the current, active capture identity arrives.
    fn prepare(&mut self) {
        let replan = self.analyzer.as_ref().is_none_or(|analyzer| {
            analyzer.sample_rate != self.sample_rate
                || analyzer.settings.fft_size != self.settings.fft_size
                || analyzer.settings.window != self.settings.window
        });
        if replan {
            let mut analyzer = SpectrumAnalyzer::new(self.settings, self.sample_rate);
            analyzer.generation = self.generation;
            self.analyzer = Some(analyzer);
        }
        let analyzer = self.analyzer.as_mut().unwrap();
        analyzer.settings = self.settings;
        analyzer.set_visualizer_demand(self.demand);
    }

    /// Consume one finite prefix; retain a newer control identity for its cycle.
    fn ingest_available(
        &mut self,
        consumer: &mut HeapCons<TapFrame>,
        control: &AnalysisControl,
        epoch: u64,
        demand_token: u64,
        active: bool,
    ) -> (usize, bool) {
        let mut accepted = 0;
        let mut remaining = consumer.occupied_len();
        while remaining != 0 {
            let (first, second) = consumer.as_slices();
            let mut consumed = 0;
            let mut controls_changed = false;
            for frame in first.iter().chain(second).take(remaining.min(1024)) {
                if frame.epoch == epoch
                    && DisplayDemand::capture_token(frame.demand_token)
                        == DisplayDemand::capture_token(demand_token)
                {
                    if active {
                        if accepted == 0 {
                            self.prepare();
                        }
                        let analyzer = self.analyzer.as_mut().unwrap();
                        analyzer.push_tap(frame);
                        if frame.gap {
                            self.generation = analyzer.generation;
                        }
                        accepted += 1;
                    }
                } else if frame.epoch == control.epoch.load(Ordering::Acquire)
                    && DisplayDemand::capture_token(frame.demand_token)
                        == DisplayDemand::capture_token(control.display_token())
                    && control.accepts_audio_for(frame.demand_token)
                {
                    // A control transition can finish while a cycle reads its
                    // snapshot. Do not discard that next cycle's fresh window.
                    controls_changed = true;
                    break;
                }
                consumed += 1;
            }
            consumer.skip(consumed);
            remaining -= consumed;
            if controls_changed || consumed == 0 {
                return (accepted, controls_changed);
            }
        }
        (accepted, false)
    }
}

/// Discard a capture window and published handles, retaining prepared machinery.
fn discard_analysis(
    runtime: &mut AnalysisRuntime,
    shared: &RwLock<AnalysisFrame>,
    display_token: u64,
    audio_epoch: u64,
    audio_intent: u64,
    demand: VisualizerDemand,
) {
    if let Some(analyzer) = &mut runtime.analyzer {
        analyzer.metadata_dirty = true;
    }
    let retired = {
        let mut frame = shared.write();
        let empty = AnalysisFrame {
            sequence: frame.sequence.wrapping_add(1),
            display_token,
            audio_epoch,
            audio_intent,
            generation: runtime.generation,
            sample_rate: runtime.sample_rate,
            sample_time: frame.sample_time,
            visualizer: VisualizerFrame {
                demand,
                ..VisualizerFrame::default()
            },
            ..AnalysisFrame::default()
        };
        std::mem::replace(&mut *frame, empty)
    };
    drop(retired);
}

fn analyze(
    mut consumer: HeapCons<TapFrame>,
    control: Arc<AnalysisControl>,
    shared: Arc<RwLock<AnalysisFrame>>,
) {
    let mut runtime = AnalysisRuntime::new(
        *control.settings.read(),
        control.sample_rate.load(Ordering::Acquire),
    );
    let mut epoch = 0;
    let mut continuity = 0;
    let mut demand_token = 0;
    let mut sample_time = Duration::ZERO;
    let mut last_frame_at: Option<Instant> = None;
    let mut fresh = false;
    loop {
        if control.shutdown.load(Ordering::Acquire) {
            break;
        }
        let previous_generation = runtime.generation;
        let current_still_token = control.still_mode.load(Ordering::Acquire);
        let still = current_still_token & 1 != 0;
        let current_demand_token = control.display_token();
        let display = DisplayDemand::from_bits(current_demand_token);
        let demand = if display.enabled && !still {
            display.visualizer
        } else {
            VisualizerDemand::default()
        };
        let current_epoch = control.epoch.load(Ordering::Acquire);
        let current_continuity = control.continuity.load(Ordering::Acquire);
        let current_intent = control.applied_intent.load(Ordering::Acquire);
        let settings = *control.settings.read();
        let sample_rate = control.sample_rate.load(Ordering::Acquire);
        if !current_epoch.is_multiple_of(2)
            || control.epoch.load(Ordering::Acquire) != current_epoch
        {
            thread::park();
            continue;
        }
        runtime.settings = settings;
        runtime.sample_rate = sample_rate;
        runtime.demand = demand;
        let semantic_reset = current_continuity != continuity;
        let capture_changed = current_epoch != epoch
            || DisplayDemand::capture_token(current_demand_token)
                != DisplayDemand::capture_token(demand_token);
        if semantic_reset || capture_changed {
            if semantic_reset {
                runtime.reset();
            } else {
                runtime.clear_window();
            }
            discard_analysis(
                &mut runtime,
                &shared,
                current_demand_token,
                current_epoch,
                current_intent,
                demand,
            );
            fresh = false;
        }
        epoch = current_epoch;
        continuity = current_continuity;
        demand_token = current_demand_token;
        let active = control.accepts_audio_for(current_demand_token);
        if active && let Some(analyzer) = &mut runtime.analyzer {
            analyzer.set_visualizer_demand(demand);
        }
        if still {
            consumer.clear();
        }
        if !active {
            control.tap_overflow.store(false, Ordering::Release);
        } else if control.tap_overflow.swap(false, Ordering::AcqRel) {
            // A stalled consumer drops the stale prefix, never blocking playback.
            // Fresh PCM wakes recovery independently of the next display request.
            consumer.clear();
            runtime.reset();
            fresh = false;
        }
        // PCM ingestion follows arrival, not the publication permit or FPS.
        let (accepted, controls_changed) = runtime.ingest_available(
            &mut consumer,
            &control,
            current_epoch,
            current_demand_token,
            active,
        );
        sample_time += Duration::from_secs_f64(accepted as f64 / sample_rate.max(1) as f64);
        fresh |= accepted != 0;
        let interval = Duration::from_secs_f64(1.0 / settings.fps as f64);
        let remaining = last_frame_at.map_or(Duration::ZERO, |started| {
            (started + interval).saturating_duration_since(Instant::now())
        });
        let ready = fresh
            && runtime
                .analyzer
                .as_ref()
                .is_some_and(|analyzer| analyzer.filled == analyzer.rolling.len());
        let requested = active
            && ready
            && remaining.is_zero()
            && control.frame_requested.swap(false, Ordering::AcqRel);
        let rms = requested.then(|| {
            last_frame_at = Some(Instant::now());
            fresh = false;
            runtime.analyzer.as_mut().unwrap().transform()
        });
        if rms.is_some() || runtime.generation != previous_generation {
            let mut frame = shared.write();
            let published = rms.is_some_and(|rms| {
                let current = control.epoch.load(Ordering::Acquire) == current_epoch
                    && current_intent == control.desired_intent.load(Ordering::Acquire)
                    && current_intent == control.applied_intent.load(Ordering::Acquire)
                    && control.accepts_audio()
                    && *control.settings.read() == settings
                    && control.display_token() == current_demand_token
                    && control.still_mode.load(Ordering::Acquire) == current_still_token
                    && !control.tap_overflow.load(Ordering::Acquire)
                    && !control.shutdown.load(Ordering::Acquire);
                if current {
                    runtime
                        .analyzer
                        .as_mut()
                        .unwrap()
                        .publish(&mut frame, rms, sample_time);
                }
                current
            });
            if published {
                frame.display_token = current_demand_token;
                frame.audio_epoch = current_epoch;
                frame.audio_intent = current_intent;
            } else if runtime.generation != previous_generation {
                // A ready replacement and its generation commit together. Only
                // a genuine wait for fresh analysis exposes an invalid frame.
                frame.visualizer = VisualizerFrame {
                    demand,
                    ..VisualizerFrame::default()
                };
                frame.display_token = current_demand_token;
                frame.audio_epoch = current_epoch;
                frame.audio_intent = current_intent;
                frame.generation = runtime.generation;
                frame.settings = None;
            }
        }
        if controls_changed || (active && consumer.occupied_len() != 0) {
            continue;
        }
        if active && fresh && ready && control.frame_requested.load(Ordering::Acquire) {
            // Only a ready, requested publication owns a deadline. Keep an
            // incomplete window's permit parked until fresh PCM or a request
            // wakes ingestion, without consuming the permit or polling.
            let remaining = last_frame_at.map_or(Duration::ZERO, |started| {
                (started + interval).saturating_duration_since(Instant::now())
            });
            if remaining.is_zero() {
                continue;
            }
            thread::park_timeout(remaining);
        } else {
            thread::park();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_actual_epoch_is_rejected_without_a_display_transition() {
        let control = AnalysisControl::new();
        let frame = AnalysisFrame::default();
        assert!(control.frame_is_current(&frame));
        control.epoch.fetch_add(2, Ordering::AcqRel);
        assert!(!control.frame_is_current(&frame));
    }

    #[test]
    fn queued_source_and_seek_intents_cannot_resurrect_old_publications() {
        let control = AnalysisControl::new();
        let held = AnalysisFrame::default();
        control.desired_intent.fetch_add(2, Ordering::AcqRel);
        assert!(!control.frame_is_current(&held));
        control.apply_audio_intents(1);
        let intermediate = AnalysisFrame {
            audio_epoch: control.epoch.load(Ordering::Acquire),
            audio_intent: 1,
            ..AnalysisFrame::default()
        };
        assert!(!control.frame_is_current(&intermediate));
        control.apply_audio_intents(1);
        assert!(!control.frame_is_current(&held));
        assert!(!control.frame_is_current(&intermediate));
        let current = AnalysisFrame {
            audio_epoch: control.epoch.load(Ordering::Acquire),
            audio_intent: 2,
            ..AnalysisFrame::default()
        };
        assert!(control.frame_is_current(&current));
    }

    #[test]
    fn retained_audio_cannot_acquire_a_new_capture_identity() {
        let worker = AnalysisWorker::new(Arc::default()).unwrap();
        worker.set_playback_active(true);
        worker.control.set_display_enabled(true);
        let (mut producer, mut captured) = AnalysisProducer::capture_for_test(&worker, 8);
        let identity = producer.identity().unwrap();
        worker.control.set_display_enabled(false);
        worker.control.set_display_enabled(true);
        producer.push_samples_for(identity, std::iter::once([0.75; 2]));
        assert!(captured.try_pop().is_none());
        let current = producer.identity().unwrap();
        producer.push_samples_for(current, std::iter::once([0.25; 2]));
        let frame = captured.try_pop().unwrap();
        assert_eq!(frame.samples, [0.25; 2]);
        assert_eq!(frame.demand_token, worker.control.display_token());
        assert!(!frame.gap);
    }

    #[test]
    fn worker_producer_marks_only_real_pcm_gaps_with_native_downmix() {
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        // This producer uses a separate capture ring; keep the real consumer
        // from clearing its overflow flag while producer behavior is inspected.
        let held = shared.read();
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        worker.set_playback_active(true);
        worker.control.set_display_enabled(true);
        let (mut producer, mut captured) = AnalysisProducer::capture_for_test(&worker, 2);
        let weights = [[1.0, 0.0], [0.0, 1.0], [0.5, 0.5]];
        producer.push(&[1.0, 2.0, 4.0, 2.0, 4.0, 8.0, 3.0, 6.0, 12.0], &weights);
        assert_eq!(captured.try_pop().unwrap().samples, [3.0, 4.0]);
        assert_eq!(captured.try_pop().unwrap().samples, [6.0, 8.0]);
        assert!(captured.try_pop().is_none());
        assert!(worker.control.tap_overflow.load(Ordering::Acquire));
        producer.push(&[0.5, 1.0, 2.0], &weights);
        let recovered = captured.try_pop().unwrap();
        assert_eq!(recovered.samples, [1.5, 2.0]);
        assert!(recovered.gap);
        worker.set_still_mode(true);
        producer.push(&[99.0; 3], &weights);
        assert!(captured.try_pop().is_none());
        worker.set_still_mode(false);
        producer.push(&[0.5, 1.0, 2.0], &weights);
        assert!(!captured.try_pop().unwrap().gap);
        worker.control.set_display_enabled(false);
        producer.push(&[99.0; 3], &weights);
        assert!(captured.try_pop().is_none());
        worker.control.set_display_enabled(true);
        producer.push(&[0.5, 1.0, 2.0], &weights);
        assert!(!captured.try_pop().unwrap().gap);
        drop(held);
    }

    const DENSITY_DEMAND: VisualizerDemand = VisualizerDemand {
        stereo_density: true,
        spatial_spectrum: false,
        band_energy: false,
        pcm: false,
    };
    const SPATIAL_DEMAND: VisualizerDemand = VisualizerDemand {
        stereo_density: false,
        spatial_spectrum: true,
        band_energy: false,
        pcm: false,
    };
    const BAND_ENERGY_DEMAND: VisualizerDemand = VisualizerDemand {
        stereo_density: false,
        spatial_spectrum: false,
        band_energy: true,
        pcm: false,
    };
    const PCM_DEMAND: VisualizerDemand = VisualizerDemand {
        stereo_density: false,
        spatial_spectrum: false,
        band_energy: false,
        pcm: true,
    };
    const ALL_DEMAND: VisualizerDemand = VisualizerDemand {
        stereo_density: true,
        spatial_spectrum: true,
        band_energy: true,
        pcm: true,
    };

    fn visualizer_settings() -> AnalysisSettings {
        AnalysisSettings {
            fps: 20,
            fft_size: 512,
            window: AnalysisWindow::None,
            signal: VisualizationSignal::After,
        }
    }

    fn visualizer_analyzer(demand: VisualizerDemand) -> SpectrumAnalyzer {
        let mut analyzer = SpectrumAnalyzer::new(visualizer_settings(), 5120);
        analyzer.set_visualizer_demand(demand);
        analyzer
    }

    #[test]
    fn pcm_preserves_unwindowed_stereo_and_latest_bounded_sample_order() {
        for window in [
            AnalysisWindow::None,
            AnalysisWindow::Hann,
            AnalysisWindow::BlackmanHarris,
        ] {
            for fft_size in [512, 8192] {
                let mut analyzer = SpectrumAnalyzer::new(
                    AnalysisSettings {
                        fft_size,
                        window,
                        ..visualizer_settings()
                    },
                    48_000,
                );
                analyzer.set_visualizer_demand(PCM_DEMAND);
                let total = fft_size as usize + 37;
                let sample =
                    |index: usize| [index as f32 / 2048.0 - 1.25, 0.75 - index as f32 / 4096.0];
                for index in 0..total {
                    analyzer.push(sample(index));
                }
                let rms = analyzer.transform();
                let mut frame = AnalysisFrame::default();
                analyzer.publish(&mut frame, rms, Duration::from_secs(1));
                let count = (fft_size as usize).min(PCM_SNAPSHOT_FRAMES);
                assert_eq!(frame.visualizer.pcm.len(), count);
                for (offset, actual) in frame.visualizer.pcm.iter().enumerate() {
                    assert_eq!(*actual, sample(total - count + offset));
                }
            }
        }
    }

    #[test]
    fn pcm_snapshot_starvation_omits_only_pcm_and_reuses_released_slots() {
        let mut analyzer = visualizer_analyzer(PCM_DEMAND);
        let slots = analyzer.pcm_snapshots.as_ref().unwrap().each_ref();
        let pointers = slots.map(|slot| slot.as_ptr());
        let mut frame = AnalysisFrame::default();
        let mut retained = Vec::new();
        for (index, pointer) in pointers.iter().enumerate() {
            let samples = [0.125 * index as f32, -0.0625 * index as f32];
            for _ in 0..512 {
                analyzer.push(samples);
            }
            let rms = analyzer.transform();
            analyzer.publish(
                &mut frame,
                rms,
                Duration::from_millis(100 * (index + 1) as u64),
            );
            assert_eq!(frame.visualizer.pcm.as_ptr(), *pointer);
            retained.push((frame.clone(), samples));
        }
        let previous = frame.clone();
        for _ in 0..512 {
            analyzer.push([0.75, -0.5]);
        }
        let rms = analyzer.transform();
        analyzer.publish(&mut frame, rms, Duration::from_secs(1));
        assert_eq!(frame.sequence, previous.sequence + 1);
        assert_eq!(frame.sample_time, Duration::from_secs(1));
        assert_eq!(frame.generation, previous.generation);
        assert_eq!(frame.spectrum_db, analyzer.levels);
        assert!(!frame.spectrum_db.is_empty());
        assert_eq!(frame.rms_left, 0.75);
        assert_eq!(frame.rms_right, 0.5);
        assert_eq!(frame.visualizer.demand, PCM_DEMAND);
        assert!(frame.visualizer.pcm.is_empty());
        for (snapshot, samples) in &retained {
            assert!(
                snapshot
                    .visualizer
                    .pcm
                    .iter()
                    .all(|actual| actual == samples)
            );
        }

        let released = retained.remove(0).0.visualizer.pcm.as_ptr();
        analyzer.publish(&mut frame, rms, Duration::from_secs(1));
        assert_eq!(frame.visualizer.pcm.as_ptr(), released);
        assert_eq!(frame.sequence, previous.sequence + 2);
        assert_eq!(frame.sample_time, Duration::from_secs(1));
        assert!(
            frame
                .visualizer
                .pcm
                .iter()
                .all(|sample| *sample == [0.75, -0.5])
        );
        assert_eq!(
            analyzer
                .pcm_snapshots
                .as_ref()
                .unwrap()
                .each_ref()
                .map(|slot| slot.as_ptr()),
            pointers,
        );
        for (snapshot, samples) in &retained {
            assert!(
                snapshot
                    .visualizer
                    .pcm
                    .iter()
                    .all(|actual| actual == samples)
            );
        }

        let weak = analyzer
            .pcm_snapshots
            .as_ref()
            .unwrap()
            .each_ref()
            .map(Arc::downgrade);
        analyzer.set_visualizer_demand(VisualizerDemand::default());
        frame = AnalysisFrame::default();
        retained.clear();
        drop(previous);
        assert!(frame.visualizer.pcm.is_empty());
        assert!(weak.iter().all(|slot| slot.upgrade().is_none()));
    }

    #[test]
    fn pcm_reset_and_demand_release_handles_without_mutating_retained_samples() {
        let mut analyzer = visualizer_analyzer(PCM_DEMAND);
        for _ in 0..512 {
            analyzer.push([0.5, -0.25]);
        }
        let rms = analyzer.transform();
        let mut frame = AnalysisFrame::default();
        analyzer.publish(&mut frame, rms, Duration::from_secs(1));
        let retained = frame.visualizer.pcm.clone();
        let slots = analyzer.pcm_snapshots.as_ref().unwrap().each_ref();
        let pointers = slots.map(|slot| slot.as_ptr());
        let weak = slots.map(Arc::downgrade);
        analyzer.reset();
        frame.visualizer.clone_from(&analyzer.visualizer);
        assert!(analyzer.visualizer.pcm.is_empty());
        assert!(frame.visualizer.pcm.is_empty());
        assert_eq!(
            analyzer
                .pcm_snapshots
                .as_ref()
                .unwrap()
                .each_ref()
                .map(|slot| slot.as_ptr()),
            pointers,
        );
        assert!(retained.iter().all(|sample| *sample == [0.5, -0.25]));
        analyzer.set_visualizer_demand(VisualizerDemand::default());
        assert!(analyzer.pcm_snapshots.is_none());
        assert!(weak[0].upgrade().is_some());
        assert!(weak[1..].iter().all(|slot| slot.upgrade().is_none()));

        // A disabled style cannot keep a stale handle through staging clones.
        let disabled = VisualizerFrame {
            pcm: retained,
            ..VisualizerFrame::default()
        };
        frame.visualizer.clone_from(&disabled);
        assert!(frame.visualizer.pcm.is_empty());
        drop(disabled);
        assert!(weak.iter().all(|slot| slot.upgrade().is_none()));
    }

    #[test]
    fn worker_pcm_control_transitions_invalidate_and_require_a_current_complete_window() {
        for transition in ["hide", "pause", "still", "reset", "settings"] {
            let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
            let worker = AnalysisWorker::new(shared.clone()).unwrap();
            let settings = AnalysisSettings {
                fps: 60,
                ..visualizer_settings()
            };
            worker.configure(settings);
            worker.control.set_display_demand(DisplayDemand {
                enabled: true,
                visualizer: PCM_DEMAND,
            });
            set_test_playback(&worker, true);
            let mut producer = worker.producer();
            let old_epoch = worker.control.epoch.load(Ordering::Acquire);
            let old_token = worker.control.display_token();
            push_window(&mut producer, old_epoch, old_token, [0.5, -0.25]);
            worker.control.request_frame();
            wait_for_frame(&shared, |frame| frame.settings.is_some());
            let retired = shared.read().clone();
            let old_slot = Arc::downgrade(retired.visualizer.pcm.0.as_ref().unwrap());

            match transition {
                "hide" => {
                    worker.control.set_display_enabled(false);
                    wait_for_frame(&shared, |frame| frame.settings.is_none());
                    assert!(shared.read().visualizer.pcm.is_empty());
                    worker.control.set_display_enabled(true);
                }
                "pause" => {
                    worker.set_playback_active(false);
                    wait_for_frame(&shared, |frame| frame.settings.is_none());
                    assert!(shared.read().visualizer.pcm.is_empty());
                    worker.set_playback_active(true);
                }
                "still" => {
                    worker.set_still_mode(true);
                    wait_for_frame(&shared, |frame| frame.settings.is_none());
                    assert!(shared.read().visualizer.pcm.is_empty());
                    worker.set_still_mode(false);
                }
                "reset" => worker.reset(96_000),
                "settings" => worker.configure(AnalysisSettings {
                    fft_size: 1024,
                    window: AnalysisWindow::Hann,
                    signal: VisualizationSignal::Before,
                    ..settings
                }),
                _ => unreachable!(),
            }
            let epoch = worker.control.epoch.load(Ordering::Acquire);
            let token = worker.control.display_token();
            wait_for_frame(&shared, |frame| {
                frame.display_token == token && frame.settings.is_none()
            });
            assert!(shared.read().visualizer.pcm.is_empty());
            push_window(&mut producer, old_epoch, old_token, [1.0, -1.0]);
            let samples = [0.125, -0.375];
            let count = worker.settings().fft_size as usize;
            for _ in 0..count - 1 {
                assert!(
                    producer
                        .try_push(TapFrame {
                            samples,
                            epoch,
                            demand_token: token,
                            gap: false,
                        })
                        .is_ok()
                );
            }
            worker.control.request_frame();
            wait_until(|| producer.occupied_len() == 0);
            assert_eq!(shared.read().settings, None);
            assert!(shared.read().visualizer.pcm.is_empty());
            assert!(
                producer
                    .try_push(TapFrame {
                        samples,
                        epoch,
                        demand_token: token,
                        gap: false,
                    })
                    .is_ok()
            );
            worker.control.request_frame();
            wait_for_frame(&shared, |frame| frame.settings.is_some());
            let frame = shared.read();
            assert_eq!(frame.display_token, token);
            assert_eq!(frame.settings, Some(worker.settings()));
            assert_eq!(
                frame.generation == retired.generation,
                matches!(transition, "hide" | "pause"),
            );
            assert_eq!(frame.visualizer.pcm.len(), count);
            assert!(frame.visualizer.pcm.iter().all(|actual| *actual == samples));
            assert!(
                retired
                    .visualizer
                    .pcm
                    .iter()
                    .all(|sample| *sample == [0.5, -0.25])
            );
            drop(frame);
            drop(retired);
            // Destructive settings replace storage only after current audio;
            // suspension alone preserves the prepared pool for reuse.
            assert_eq!(
                old_slot.upgrade().is_none(),
                matches!(transition, "reset" | "settings")
            );
        }
    }

    #[test]
    fn real_first_pcm_gap_after_pause_still_changes_generation() {
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        worker.configure(AnalysisSettings {
            fps: 60,
            ..visualizer_settings()
        });
        set_test_playback(&worker, true);
        let mut producer = worker.audio_producer();
        producer.push_samples(std::iter::repeat_n([0.25; 2], 512));
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| frame.settings.is_some());
        let previous = shared.read().clone();
        worker.set_playback_active(false);
        worker.set_playback_active(true);
        // A genuine source gap can coincide with the first capture frame.
        producer.discontinuity();
        producer.push_samples(std::iter::repeat_n([0.5; 2], 512));
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| {
            worker.control.frame_is_current(frame) && frame.settings.is_some()
        });
        assert!(shared.read().generation > previous.generation);
    }

    #[test]
    fn stop_is_a_semantic_reset_even_when_already_paused() {
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        worker.configure(AnalysisSettings {
            fps: 60,
            ..visualizer_settings()
        });
        set_test_playback(&worker, true);
        let mut producer = worker.audio_producer();
        producer.push_samples(std::iter::repeat_n([0.25; 2], 512));
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| frame.settings.is_some());
        let generation = shared.read().generation;
        worker.set_playback_active(false);
        wait_for_frame(&shared, |frame| frame.settings.is_none());
        assert_eq!(shared.read().generation, generation);
        worker.stop();
        wait_for_frame(&shared, |frame| frame.generation > generation);
        assert!(shared.read().spectrum_db.is_empty());
    }

    #[test]
    fn worker_pcm_starvation_publishes_current_spectrum_and_band_energy() {
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        worker.configure(AnalysisSettings {
            fps: 60,
            ..visualizer_settings()
        });
        let demand = VisualizerDemand {
            band_energy: true,
            ..PCM_DEMAND
        };
        worker.control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: demand,
        });
        set_test_playback(&worker, true);
        let mut producer = worker.audio_producer();
        let push_tone = |producer: &mut AnalysisProducer, amplitude: f32| {
            producer.push_samples((0..512).map(move |i| {
                let value = (std::f32::consts::TAU * 8.0 * i as f32 / 512.0).sin() * amplitude;
                [value, -value]
            }));
        };
        let mut retained = Vec::new();
        for index in 0..PCM_SNAPSHOT_COUNT {
            let sequence = shared.read().sequence;
            push_tone(&mut producer, 0.1 * (index + 1) as f32);
            worker.control.request_frame();
            wait_for_frame(&shared, |frame| {
                frame.sequence > sequence && frame.settings.is_some()
            });
            let snapshot = shared.read().visualizer.pcm.clone();
            assert_eq!(snapshot.len(), 512);
            retained.push((snapshot.clone(), snapshot.to_vec()));
        }
        let previous = shared.read().clone();
        push_tone(&mut producer, 0.75);
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| frame.sequence > previous.sequence);
        let starved = shared.read().clone();
        assert!(starved.visualizer.pcm.is_empty());
        assert_eq!(starved.visualizer.demand, demand);
        assert_eq!(starved.generation, previous.generation);
        assert!(starved.sample_time > previous.sample_time);
        assert_ne!(starved.spectrum_db, previous.spectrum_db);
        assert!(starved.visualizer.band_rms[1] > previous.visualizer.band_rms[1]);
        let released = retained.remove(0).0.as_ptr();
        push_tone(&mut producer, 0.25);
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| frame.sequence > starved.sequence);
        assert_eq!(shared.read().visualizer.pcm.as_ptr(), released);
        for (snapshot, samples) in retained {
            assert_eq!(&*snapshot, samples.as_slice());
        }
    }

    #[test]
    fn demand_tokens_preserve_request_and_capture_transition_identities() {
        let control = AnalysisControl::new();
        let mut previous = control.display_token();
        for bits in 0..=DisplayDemand::MASK {
            let demand = DisplayDemand::from_bits(bits);
            assert_eq!(demand.bits(), bits);
            control.set_display_demand(demand);
            let token = control.display_token();
            assert_eq!(DisplayDemand::from_bits(token), demand);
            if bits != 0 {
                assert_ne!(token, previous);
                assert_eq!(
                    DisplayDemand::capture_token(token) != DisplayDemand::capture_token(previous),
                    DisplayDemand::from_bits(previous).enabled != demand.enabled,
                );
            }
            previous = token;
            control.set_display_demand(demand);
            assert_eq!(control.display_token(), token);
        }
        let before = control.display_token();
        control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: PCM_DEMAND,
        });
        control.set_display_demand(DisplayDemand::from_bits(DisplayDemand::MASK));
        assert_ne!(control.display_token(), before);
        assert_eq!(
            DisplayDemand::capture_token(control.display_token()),
            DisplayDemand::capture_token(before)
        );
        let retired = AnalysisFrame {
            display_token: before,
            ..AnalysisFrame::default()
        };
        assert!(!control.frame_is_current(&retired));
    }

    #[test]
    fn queued_pcm_with_an_old_request_token_completes_the_same_capture_window() {
        let control = AnalysisControl::new();
        control.playback_active.store(true, Ordering::Release);
        control.set_display_enabled(true);
        let old_token = control.display_token();
        let ring = Arc::new(HeapRb::new(512));
        let mut producer = HeapProd::new(ring.clone());
        let mut consumer = HeapCons::new(ring);
        let mut runtime = AnalysisRuntime::new(visualizer_settings(), 5120);
        for _ in 0..511 {
            assert!(
                producer
                    .try_push(TapFrame {
                        samples: [0.25; 2],
                        demand_token: old_token,
                        ..TapFrame::default()
                    })
                    .is_ok()
            );
        }
        assert_eq!(
            runtime.ingest_available(&mut consumer, &control, 0, old_token, true),
            (511, false)
        );
        let fft = Arc::clone(&runtime.analyzer.as_ref().unwrap().fft);
        control.request_frame();
        control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: PCM_DEMAND,
        });
        assert!(control.frame_requested.load(Ordering::Acquire));
        runtime.demand = PCM_DEMAND;
        assert!(
            producer
                .try_push(TapFrame {
                    samples: [0.75; 2],
                    demand_token: old_token,
                    ..TapFrame::default()
                })
                .is_ok()
        );
        assert_eq!(
            runtime.ingest_available(&mut consumer, &control, 0, control.display_token(), true),
            (1, false)
        );
        let analyzer = runtime.analyzer.as_mut().unwrap();
        assert_eq!(analyzer.filled, 512);
        assert_eq!(analyzer.generation, 0);
        assert!(Arc::ptr_eq(&analyzer.fft, &fft));
        let rms = analyzer.transform();
        let mut frame = AnalysisFrame::default();
        analyzer.publish(&mut frame, rms, Duration::from_millis(100));
        assert!(frame.rms_left > 0.25);
        assert_eq!(frame.visualizer.pcm.last(), Some(&[0.75; 2]));
    }

    #[test]
    fn simultaneous_visualizer_payloads_share_one_accepted_window() {
        let settings = AnalysisSettings {
            fps: 20,
            fft_size: 4096,
            window: AnalysisWindow::None,
            signal: VisualizationSignal::After,
        };
        let mut combined = SpectrumAnalyzer::new(settings, 32768);
        combined.set_visualizer_demand(ALL_DEMAND);
        let mut separate = [
            DENSITY_DEMAND,
            SPATIAL_DEMAND,
            BAND_ENERGY_DEMAND,
            PCM_DEMAND,
        ]
        .map(|demand| {
            let mut analyzer = SpectrumAnalyzer::new(settings, 32768);
            analyzer.set_visualizer_demand(demand);
            analyzer
        });
        let tones = [(128.0, 0.1), (1024.0, 0.2), (8192.0, 0.3)];
        for i in 0..settings.fft_size {
            let value = tones
                .iter()
                .map(|(frequency, amplitude)| {
                    amplitude * (std::f32::consts::TAU * frequency * i as f32 / 32768.0).sin()
                })
                .sum::<f32>();
            let samples = [value, -value];
            combined.push(samples);
            for analyzer in &mut separate {
                analyzer.push(samples);
            }
        }
        let rms = combined.transform();
        for analyzer in &mut separate {
            assert_eq!(analyzer.transform(), rms);
            assert_eq!(analyzer.levels, combined.levels);
        }
        let payload = &combined.visualizer;
        assert_eq!(payload.density, separate[0].visualizer.density);
        assert!(payload.density.iter().any(|value| *value > 0.0));
        assert_eq!(payload.correlation, Some(-1.0));
        assert_eq!(payload.balance, Some(0.0));
        assert_eq!(payload.side_ratio, Some(1.0));
        assert_eq!(payload.spatial.len(), settings.fft_size as usize / 2);
        for (actual, expected) in payload.spatial.iter().zip(&separate[1].visualizer.spatial) {
            assert_eq!(actual.frequency_hz, expected.frequency_hz);
            assert_eq!(actual.pan, expected.pan);
            assert_eq!(actual.power, expected.power);
        }
        assert_eq!(payload.band_rms, separate[2].visualizer.band_rms);
        for (band, (frequency, amplitude)) in tones.into_iter().enumerate() {
            assert!((payload.band_rms[band] - amplitude / 2.0_f32.sqrt()).abs() < 0.0001);
            let bin = payload
                .spatial
                .iter()
                .find(|bin| bin.frequency_hz == frequency)
                .unwrap();
            assert!(bin.pan.abs() < 0.0001);
            assert!((bin.power - amplitude * amplitude).abs() < 1e-5);
        }
        let mut frame = AnalysisFrame::default();
        combined.publish(&mut frame, rms, Duration::from_millis(125));
        assert_eq!(frame.visualizer.demand, ALL_DEMAND);
        assert_eq!(frame.visualizer.density, separate[0].visualizer.density);
        assert_eq!(
            frame.visualizer.spatial.len(),
            separate[1].visualizer.spatial.len()
        );
        assert_eq!(frame.visualizer.band_rms, separate[2].visualizer.band_rms);
        let mut pcm_frame = AnalysisFrame::default();
        separate[3].publish(&mut pcm_frame, rms, Duration::from_millis(125));
        assert_eq!(frame.visualizer.pcm, pcm_frame.visualizer.pcm);
        assert_eq!(frame.sample_time, Duration::from_millis(125));
        assert_eq!(frame.sequence, 1);
    }

    #[test]
    fn demand_relinquishes_unused_payloads_and_reuses_active_storage() {
        let mut analyzer = visualizer_analyzer(ALL_DEMAND);
        let fft = analyzer.fft.clone();
        let frequency = analyzer.frequency.as_ptr();
        let mut frame = AnalysisFrame::default();
        for _ in 0..8 {
            analyzer.set_visualizer_demand(ALL_DEMAND);
            for i in 0..512 {
                let tone = 0.5 * (std::f32::consts::TAU * i as f32 / 32.0).sin();
                analyzer.push([tone, -tone]);
            }
            let rms = analyzer.transform();
            analyzer.publish(&mut frame, rms, Duration::from_millis(100));
            let density = analyzer.visualizer.density.as_ptr();
            let snapshots = analyzer
                .density
                .as_ref()
                .unwrap()
                .snapshots
                .each_ref()
                .map(|slot| slot.as_ptr());
            let cells = analyzer.density.as_ref().unwrap().cells.as_ptr();
            let spatial = analyzer.visualizer.spatial.as_ptr();
            let published_density = frame.visualizer.density.as_ptr();
            assert_eq!(published_density, density);
            let published_spatial = frame.visualizer.spatial.as_ptr();
            analyzer.set_visualizer_demand(ALL_DEMAND);
            analyzer.transform();
            analyzer.publish(&mut frame, rms, Duration::from_millis(100));
            assert!(snapshots.contains(&analyzer.visualizer.density.as_ptr()));
            assert_ne!(analyzer.visualizer.density.as_ptr(), density);
            assert_eq!(analyzer.density.as_ref().unwrap().cells.as_ptr(), cells);
            assert_eq!(analyzer.visualizer.spatial.as_ptr(), spatial);
            assert_eq!(
                frame.visualizer.density.as_ptr(),
                analyzer.visualizer.density.as_ptr(),
            );
            assert_eq!(
                analyzer
                    .density
                    .as_ref()
                    .unwrap()
                    .snapshots
                    .each_ref()
                    .map(|slot| slot.as_ptr()),
                snapshots,
            );
            assert_eq!(frame.visualizer.spatial.as_ptr(), published_spatial);
            assert_eq!(
                analyzer.visualizer.density.len(),
                DENSITY_SIDE * DENSITY_SIDE
            );
            assert_eq!(analyzer.visualizer.spatial.capacity(), 256);
            let drive = analyzer.visualizer.band_rms;
            analyzer.set_visualizer_demand(BAND_ENERGY_DEMAND);
            frame.visualizer.clone_from(&analyzer.visualizer);
            assert!(analyzer.density.is_none());
            for payload in [&analyzer.visualizer, &frame.visualizer] {
                assert!(payload.density.is_empty());
                assert_eq!(payload.spatial.capacity(), 0);
                assert_eq!(payload.correlation, None);
                assert_eq!(payload.balance, None);
                assert_eq!(payload.side_ratio, None);
                assert_eq!(payload.band_rms, drive);
            }
            assert!(drive[0] > 0.0);
            analyzer.set_visualizer_demand(VisualizerDemand::default());
            frame.visualizer.clone_from(&analyzer.visualizer);
            assert_eq!(frame.visualizer.band_rms, [0.0; 3]);
            assert!(Arc::ptr_eq(&analyzer.fft, &fft));
            assert_eq!(analyzer.frequency.as_ptr(), frequency);
        }
    }

    #[test]
    fn density_snapshots_isolate_slow_consumers_and_bound_reusable_storage() {
        let mut analyzer = visualizer_analyzer(ALL_DEMAND);
        let slots = analyzer
            .density
            .as_ref()
            .unwrap()
            .snapshots
            .each_ref()
            .map(|slot| slot.as_ptr());
        let mut frame = AnalysisFrame::default();
        let mut retained = Vec::new();
        for index in 0..DENSITY_SNAPSHOT_COUNT {
            for _ in 0..512 {
                analyzer.push([0.2 * (index + 1) as f32, 0.0]);
            }
            let rms = analyzer.transform();
            analyzer.publish(
                &mut frame,
                rms,
                Duration::from_millis(100 * (index + 1) as u64),
            );
            assert!(slots.contains(&frame.visualizer.density.as_ptr()));
            retained.push((frame.clone(), frame.visualizer.density.to_vec()));
        }
        let previous = frame.clone();
        for _ in 0..512 {
            analyzer.push([0.0, 0.8]);
        }
        let rms = analyzer.transform();
        assert!(analyzer.visualizer.density.is_empty());
        analyzer.publish(&mut frame, rms, Duration::from_millis(400));
        assert_eq!(frame.sequence, previous.sequence + 1);
        assert_eq!(frame.sample_time, Duration::from_millis(400));
        assert_eq!(frame.generation, previous.generation);
        assert!(frame.visualizer.density.is_empty());
        assert_eq!(frame.visualizer.demand, ALL_DEMAND);
        assert_eq!(frame.visualizer.balance, Some(1.0));
        assert_eq!(frame.rms_left, 0.0);
        assert!((frame.rms_right - 0.8).abs() < 0.0001);
        assert!(!frame.spectrum_db.is_empty());
        assert_eq!(frame.visualizer.band_rms, analyzer.visualizer.band_rms);
        for (snapshot, cells) in &retained {
            assert_eq!(&*snapshot.visualizer.density, cells.as_slice());
        }

        // Releasing one old consumer permits the next result without growing
        // the pool, and includes all samples accumulated during coalescing.
        let released = retained.remove(0).0.visualizer.density.as_ptr();
        let rms = analyzer.transform();
        analyzer.publish(&mut frame, rms, Duration::from_millis(400));
        assert_eq!(frame.visualizer.density.as_ptr(), released);
        assert_eq!(frame.sequence, previous.sequence + 2);
        assert_eq!(frame.sample_time, Duration::from_millis(400));
        assert_eq!(frame.visualizer.balance, Some(1.0));
        let density = analyzer.density.as_ref().unwrap();
        assert_eq!(
            density.snapshots.each_ref().map(|slot| slot.as_ptr()),
            slots,
        );
        assert!(
            frame
                .visualizer
                .density
                .iter()
                .zip(&density.cells)
                .all(|(actual, cell)| *actual == cell * density.scale)
        );
        for (snapshot, cells) in &retained {
            assert_eq!(&*snapshot.visualizer.density, cells.as_slice());
        }
        let weak = density.snapshots.each_ref().map(Arc::downgrade);
        analyzer.set_visualizer_demand(VisualizerDemand::default());
        frame = AnalysisFrame::default();
        assert!(frame.visualizer.density.is_empty());
        retained.clear();
        drop(previous);
        assert!(weak.iter().all(|slot| slot.upgrade().is_none()));
    }

    #[test]
    fn band_energy_bands_integrate_power_without_phase_cancellation() {
        for window in [
            AnalysisWindow::None,
            AnalysisWindow::Hann,
            AnalysisWindow::BlackmanHarris,
        ] {
            for (frequency, band) in [(128.0, 0), (1024.0, 1), (8192.0, 2)] {
                let mut analyzer = SpectrumAnalyzer::new(
                    AnalysisSettings {
                        fps: 20,
                        fft_size: 4096,
                        window,
                        signal: VisualizationSignal::After,
                    },
                    32768,
                );
                analyzer.set_visualizer_demand(BAND_ENERGY_DEMAND);
                for i in 0..4096 {
                    let value =
                        0.5 * (std::f32::consts::TAU * frequency * i as f32 / 32768.0).sin();
                    analyzer.push([value, -value]);
                }
                analyzer.transform();
                assert!((analyzer.visualizer.band_rms[band] - 0.5 / 2.0_f32.sqrt()).abs() < 0.0001);
                for other in 0..3 {
                    if other != band {
                        assert!(analyzer.visualizer.band_rms[other] < 0.0001);
                    }
                }
                analyzer.reset();
                assert_eq!(analyzer.visualizer.band_rms, [0.0; 3]);
            }
        }
    }

    #[test]
    fn band_energy_nyquist_is_not_doubled_and_silence_has_no_drive() {
        let mut analyzer = SpectrumAnalyzer::new(
            AnalysisSettings {
                fps: 20,
                fft_size: 512,
                window: AnalysisWindow::None,
                signal: VisualizationSignal::After,
            },
            8000,
        );
        analyzer.set_visualizer_demand(BAND_ENERGY_DEMAND);
        for i in 0..512 {
            analyzer.push([if i % 2 == 0 { 0.5 } else { -0.5 }, 0.0]);
        }
        analyzer.transform();
        assert!((analyzer.visualizer.band_rms[2] - 0.5 / 2.0_f32.sqrt()).abs() < 0.0001);
        for _ in 0..512 {
            analyzer.push([0.0; 2]);
        }
        analyzer.transform();
        assert_eq!(analyzer.visualizer.band_rms, [0.0; 3]);
    }

    #[test]
    fn density_axes_and_stereo_statistics_describe_actual_signal() {
        for inverted in [false, true] {
            let mut analyzer = visualizer_analyzer(DENSITY_DEMAND);
            for i in 0..512 {
                let left = 0.8 * (std::f32::consts::TAU * i as f32 / 32.0).sin();
                analyzer.push([left, if inverted { -left } else { left }]);
            }
            analyzer.transform();
            let payload = &analyzer.visualizer;
            assert_eq!(payload.correlation, Some(if inverted { -1.0 } else { 1.0 }));
            assert_eq!(payload.balance, Some(0.0));
            assert_eq!(payload.side_ratio, Some(if inverted { 1.0 } else { 0.0 }));
            let occupied: Vec<_> = payload
                .density
                .iter()
                .enumerate()
                .filter(|(_, value)| **value > 0.0)
                .map(|(index, _)| index)
                .collect();
            assert!(occupied.len() > 8);
            assert!(occupied.iter().all(|index| {
                if inverted {
                    index / DENSITY_SIDE == 128
                } else {
                    index % DENSITY_SIDE == 128
                }
            }));
            assert!(occupied.iter().any(|index| {
                if inverted {
                    index % DENSITY_SIDE < 64
                } else {
                    index / DENSITY_SIDE < 64
                }
            }));
        }
    }

    #[test]
    fn density_silence_and_nonfinite_samples_do_not_create_center_dot() {
        let mut analyzer = visualizer_analyzer(DENSITY_DEMAND);
        for _ in 0..512 {
            analyzer.push([f32::NAN, f32::INFINITY]);
        }
        assert_eq!(analyzer.transform(), [0.0; 2]);
        assert!(
            analyzer
                .visualizer
                .density
                .iter()
                .all(|value| *value == 0.0)
        );
        assert_eq!(analyzer.visualizer.correlation, None);
        assert_eq!(analyzer.visualizer.balance, None);
        assert_eq!(analyzer.visualizer.side_ratio, None);
        analyzer.push([4.0, -4.0]);
        analyzer.transform();
        assert!(analyzer.visualizer.density[128 * DENSITY_SIDE + 255] > 0.0);
    }

    #[test]
    fn density_ingests_once_and_decays_by_accepted_time() {
        let mut analyzer = visualizer_analyzer(DENSITY_DEMAND);
        for _ in 0..512 {
            analyzer.push([0.5, 0.5]);
        }
        analyzer.transform();
        let first = analyzer.visualizer.density.clone();
        analyzer.transform();
        assert_eq!(analyzer.visualizer.density, first);
        for _ in 0..2560 {
            analyzer.push([0.0; 2]);
        }
        analyzer.transform();
        let initial: f32 = first.iter().sum();
        let remaining: f32 = analyzer.visualizer.density.iter().sum();
        assert!((remaining / initial - (-1.0_f32).exp()).abs() < 0.001);
    }

    #[test]
    fn gaps_and_demand_roundtrips_discard_density_history() {
        let mut analyzer = visualizer_analyzer(DENSITY_DEMAND);
        for _ in 0..512 {
            analyzer.push([0.7, -0.7]);
        }
        analyzer.push_tap(&TapFrame {
            gap: true,
            ..TapFrame::default()
        });
        for _ in 1..512 {
            analyzer.push([0.0; 2]);
        }
        analyzer.transform();
        assert!(
            analyzer
                .visualizer
                .density
                .iter()
                .all(|value| *value == 0.0)
        );
        analyzer.push([0.7, -0.7]);
        analyzer.set_visualizer_demand(SPATIAL_DEMAND);
        assert!(analyzer.density.is_none());
        analyzer.set_visualizer_demand(DENSITY_DEMAND);
        analyzer.transform();
        assert!(
            analyzer
                .visualizer
                .density
                .iter()
                .all(|value| *value == 0.0)
        );
        analyzer.set_visualizer_demand(VisualizerDemand::default());
        assert!(analyzer.density.is_none());
        assert!(analyzer.visualizer.density.is_empty());
        assert!(analyzer.visualizer.spatial.is_empty());
    }

    #[test]
    fn spatial_channel_tones_preserve_frequency_energy_and_phase() {
        for channels in [[1.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, -1.0], [1.0, 0.5]] {
            let mut analyzer = visualizer_analyzer(SPATIAL_DEMAND);
            for i in 0..512 {
                let tone = 0.5 * (std::f32::consts::TAU * i as f32 / 32.0).sin();
                analyzer.push(channels.map(|channel| channel * tone));
            }
            analyzer.transform();
            let strongest = analyzer
                .visualizer
                .spatial
                .iter()
                .max_by(|a, b| a.power.total_cmp(&b.power))
                .unwrap();
            assert_eq!(strongest.frequency_hz, 160.0);
            let channel_power = channels.map(|channel| channel * channel);
            let expected_pan =
                (channel_power[1] - channel_power[0]) / (channel_power[0] + channel_power[1]);
            assert!((strongest.pan - expected_pan).abs() < 1e-5);
            let expected_power = 0.25 * (channel_power[0] + channel_power[1]) * 0.5;
            assert!((strongest.power - expected_power).abs() < 1e-5);
        }
        let mut analyzer = visualizer_analyzer(SPATIAL_DEMAND);
        for i in 0..512 {
            let phase = std::f32::consts::TAU * i as f32;
            analyzer.push([0.5 * (phase / 32.0).sin(), 0.25 * (phase / 16.0).sin()]);
        }
        analyzer.transform();
        for (hz, pan) in [(160.0, -1.0), (320.0, 1.0)] {
            let bin = analyzer
                .visualizer
                .spatial
                .iter()
                .find(|bin| bin.frequency_hz == hz)
                .unwrap();
            assert!((bin.pan - pan).abs() < 1e-5);
            assert!(bin.power > 0.01);
        }
    }

    #[test]
    fn spatial_nyquist_uses_single_sided_endpoint_normalization() {
        let mut analyzer = visualizer_analyzer(SPATIAL_DEMAND);
        for i in 0..512 {
            analyzer.push([if i % 2 == 0 { 0.5 } else { -0.5 }, 0.0]);
        }
        analyzer.transform();
        let nyquist = analyzer.visualizer.spatial.last().unwrap();
        assert_eq!(nyquist.frequency_hz, 2560.0);
        assert_eq!(nyquist.pan, -1.0);
        assert!((nyquist.power - 0.125).abs() < 1e-5);
        assert!((10.0 * nyquist.power.log10() - analyzer.levels.last().unwrap()).abs() < 0.001);
    }

    #[test]
    fn density_panned_channel_readouts_are_correct() {
        for (samples, balance) in [([0.5, 0.0], -1.0), ([0.0, 0.5], 1.0)] {
            let mut analyzer = visualizer_analyzer(DENSITY_DEMAND);
            for _ in 0..512 {
                analyzer.push(samples);
            }
            analyzer.transform();
            assert_eq!(analyzer.visualizer.correlation, None);
            assert_eq!(analyzer.visualizer.balance, Some(balance));
            assert_eq!(analyzer.visualizer.side_ratio, Some(0.5));
        }
    }

    #[test]
    fn demand_roundtrip_rejects_retired_payloads_but_admits_same_capture_pcm() {
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        worker.configure(AnalysisSettings {
            fps: 60,
            ..visualizer_settings()
        });
        worker.control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: DENSITY_DEMAND,
        });
        set_test_playback(&worker, true);
        let mut producer = worker.audio_producer();
        let identity = producer.identity().unwrap();
        producer.push_samples(std::iter::repeat_n([0.7, -0.7], 512));
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| frame.settings.is_some());
        let retired = shared.read().clone();
        let old_cells = retired.visualizer.density.to_vec();
        worker.control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: VisualizerDemand::default(),
        });
        worker.control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: DENSITY_DEMAND,
        });
        assert!(!worker.control.frame_is_current(&retired));
        assert!(producer.identity() == Some(identity));
        producer.push_samples_for(identity, std::iter::repeat_n([0.5, 0.5], 512));
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| {
            worker.control.frame_is_current(frame) && frame.settings.is_some()
        });
        let frame = shared.read();
        assert_eq!(frame.generation, retired.generation);
        assert_eq!(frame.visualizer.correlation, Some(1.0));
        assert!(frame.sample_time > retired.sample_time);
        assert!(frame.visualizer.density[64 * DENSITY_SIDE + 128] > 0.0);
        assert_eq!(&*retired.visualizer.density, old_cells.as_slice());
    }

    #[test]
    fn demand_changes_keep_spectrum_window_and_release_disabled_payloads() {
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        worker.configure(AnalysisSettings {
            fps: 60,
            ..visualizer_settings()
        });
        worker.control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: DENSITY_DEMAND,
        });
        set_test_playback(&worker, true);
        let mut producer = worker.audio_producer();
        let identity = producer.identity().unwrap();
        producer.push_samples((0..512).map(|i| {
            let sample = (std::f32::consts::TAU * 8.0 * i as f32 / 512.0).sin() * 0.5;
            [sample, -sample]
        }));
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| frame.settings.is_some());
        let previous = shared.read().clone();
        let epoch = worker.control.epoch.load(Ordering::Acquire);
        worker.control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: ALL_DEMAND,
        });
        assert_eq!(worker.control.epoch.load(Ordering::Acquire), epoch);
        assert!(producer.identity() == Some(identity));
        // One admitted sample, not a new FFT window, must be enough to publish.
        producer.push_samples_for(identity, std::iter::once([1.0, -1.0]));
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| {
            worker.control.frame_is_current(frame) && frame.settings.is_some()
        });
        {
            let frame = shared.read();
            assert_eq!(frame.generation, previous.generation);
            assert_eq!(frame.visualizer.demand, ALL_DEMAND);
            assert_ne!(frame.spectrum_db, previous.spectrum_db);
            assert!(frame.sample_time > previous.sample_time);
            assert_eq!(frame.visualizer.pcm.len(), 512);
            assert_eq!(frame.visualizer.spatial.len(), 256);
        }
        worker.control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: VisualizerDemand::default(),
        });
        producer.push_samples_for(identity, std::iter::once([0.0; 2]));
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| {
            worker.control.frame_is_current(frame) && frame.settings.is_some()
        });
        let frame = shared.read();
        assert_eq!(frame.generation, previous.generation);
        assert!(frame.visualizer.density.is_empty());
        assert!(frame.visualizer.pcm.is_empty());
        assert_eq!(frame.visualizer.spatial.capacity(), 0);
        assert_eq!(frame.visualizer.band_rms, [0.0; 3]);
    }

    #[test]
    fn worker_density_starvation_keeps_base_publications_and_recovers_without_overwrite() {
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        worker.configure(AnalysisSettings {
            fps: 60,
            fft_size: 512,
            window: AnalysisWindow::None,
            signal: VisualizationSignal::After,
        });
        worker.control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: ALL_DEMAND,
        });
        set_test_playback(&worker, true);
        let mut producer = worker.producer();
        let epoch = worker.control.epoch.load(Ordering::Acquire);
        let token = worker.control.display_token();
        let mut retained = Vec::new();
        for index in 0..DENSITY_SNAPSHOT_COUNT {
            let sequence = shared.read().sequence;
            push_window(&mut producer, epoch, token, [0.2 * (index + 1) as f32, 0.0]);
            worker.control.request_frame();
            wait_for_frame(&shared, |frame| {
                frame.sequence > sequence && frame.settings.is_some()
            });
            let frame = shared.read().clone();
            retained.push((
                frame.visualizer.density.clone(),
                frame.visualizer.density.to_vec(),
            ));
        }
        let previous = shared.read().clone();
        push_window(&mut producer, epoch, token, [0.0, 0.8]);
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| frame.sequence > previous.sequence);
        let starved_sequence = shared.read().sequence;
        {
            let frame = shared.read();
            assert!(frame.sample_time > previous.sample_time);
            assert_eq!(frame.generation, previous.generation);
            assert!(frame.visualizer.density.is_empty());
            assert_eq!(frame.visualizer.demand, ALL_DEMAND);
            assert_eq!(frame.rms_left, 0.0);
            assert!((frame.rms_right - 0.8).abs() < 0.0001);
            assert!(!frame.spectrum_db.is_empty());
            assert_eq!(frame.visualizer.spatial.len(), 256);
        }
        let released = retained.remove(0).0.as_ptr();
        push_window(&mut producer, epoch, token, [0.0, 0.8]);
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| frame.sequence > starved_sequence);
        let frame = shared.read();
        assert_eq!(frame.visualizer.density.as_ptr(), released);
        let retained_samples = 512 * 2;
        let retained_time = frame.sample_time - previous.sample_time;
        let expected_time =
            Duration::from_secs_f64(retained_samples as f64 / frame.sample_rate as f64);
        // Each nonempty ingestion prefix rounds to nanoseconds independently.
        // Even one prefix per sample must preserve the complete retained span.
        assert!(
            retained_time.abs_diff(expected_time) <= Duration::from_nanos(retained_samples),
            "retained {retained_samples} samples at {} Hz took {retained_time:?}, expected {expected_time:?}",
            frame.sample_rate,
        );
        assert_eq!(frame.generation, previous.generation);
        assert_eq!(frame.rms_left, 0.0);
        assert!((frame.rms_right - 0.8).abs() < 0.0001);
        assert_eq!(frame.visualizer.balance, Some(1.0));
        assert_eq!(frame.visualizer.demand, ALL_DEMAND);
        assert_eq!(frame.display_token, token);
        for (snapshot, cells) in &retained {
            assert_eq!(&**snapshot, cells.as_slice());
        }
        drop(frame);

        // Retained old density slots must not block a surviving spatial panel.
        worker.control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: SPATIAL_DEMAND,
        });
        let spatial_token = worker.control.display_token();
        push_window(&mut producer, epoch, token, [0.8, 0.0]);
        push_window(&mut producer, epoch, spatial_token, [0.0, 0.8]);
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| {
            worker.control.frame_is_current(frame) && frame.settings.is_some()
        });
        let frame = shared.read();
        assert_eq!(frame.display_token, spatial_token);
        assert_eq!(frame.visualizer.demand, SPATIAL_DEMAND);
        assert!(frame.visualizer.density.is_empty());
        assert_eq!(frame.visualizer.spatial.len(), 256);
        assert_eq!(frame.rms_left, 0.0);
        assert!((frame.rms_right - 0.8).abs() < 0.0001);
        for (snapshot, cells) in &retained {
            assert_eq!(&**snapshot, cells.as_slice());
        }
    }

    fn push_window(
        producer: &mut HeapProd<TapFrame>,
        epoch: u64,
        demand_token: u64,
        samples: [f32; 2],
    ) {
        use ringbuf::traits::Producer;
        for _ in 0..512 {
            assert!(
                producer
                    .try_push(TapFrame {
                        samples,
                        epoch,
                        demand_token,
                        gap: false
                    })
                    .is_ok()
            );
        }
    }

    fn set_test_playback(worker: &AnalysisWorker, active: bool) {
        worker.set_playback_active(active);
        if active {
            worker.control.set_display_enabled(true);
        }
    }

    fn wait_for_frame(shared: &RwLock<AnalysisFrame>, ready: impl Fn(&AnalysisFrame) -> bool) {
        wait_until(|| ready(&shared.read()));
    }

    fn wait_until(ready: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ready() {
            assert!(Instant::now() < deadline, "analysis transition timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn unprepared_runtime_requires_audio_and_every_admission_gate() {
        let control = AnalysisControl::new();
        assert_eq!(control.still_mode.load(Ordering::Acquire), 0);
        let mut runtime = AnalysisRuntime::new(visualizer_settings(), 5120);
        runtime.demand = ALL_DEMAND;
        let ring = Arc::new(HeapRb::new(16));
        let mut producer = HeapProd::new(ring.clone());
        let mut consumer = HeapCons::new(ring);
        for playback in [false, true] {
            for enabled in [false, true] {
                for still in [false, true] {
                    if playback && enabled && !still {
                        continue;
                    }
                    control.playback_active.store(playback, Ordering::Release);
                    control
                        .still_mode
                        .store(u64::from(still), Ordering::Release);
                    control.set_display_demand(DisplayDemand {
                        enabled,
                        visualizer: ALL_DEMAND,
                    });
                    let token = control.display_token();
                    assert!(
                        producer
                            .try_push(TapFrame {
                                demand_token: token,
                                ..TapFrame::default()
                            })
                            .is_ok()
                    );
                    assert_eq!(
                        runtime.ingest_available(
                            &mut consumer,
                            &control,
                            0,
                            token,
                            control.accepts_audio(),
                        ),
                        (0, false)
                    );
                    assert_eq!(consumer.occupied_len(), 0);
                    assert!(runtime.analyzer.is_none());
                }
            }
        }
        control.playback_active.store(true, Ordering::Release);
        control.still_mode.store(0, Ordering::Release);
        control.set_display_enabled(true);
        let token = control.display_token();
        control.request_frame();
        assert!(control.frame_requested.load(Ordering::Acquire));
        assert_eq!(
            runtime.ingest_available(&mut consumer, &control, 0, token, true),
            (0, false)
        );
        assert!(runtime.analyzer.is_none());
        for (epoch, demand_token) in [
            (2, token),
            (0, token.wrapping_sub(DisplayDemand::CAPTURE_STEP)),
        ] {
            assert!(
                producer
                    .try_push(TapFrame {
                        epoch,
                        demand_token,
                        ..TapFrame::default()
                    })
                    .is_ok()
            );
        }
        assert_eq!(
            runtime.ingest_available(&mut consumer, &control, 0, token, true),
            (0, false)
        );
        assert!(runtime.analyzer.is_none());
        assert!(
            producer
                .try_push(TapFrame {
                    demand_token: token,
                    gap: true,
                    ..TapFrame::default()
                })
                .is_ok()
        );
        assert_eq!(
            runtime.ingest_available(&mut consumer, &control, 0, token, true),
            (1, false)
        );
        let analyzer = runtime.analyzer.as_ref().unwrap();
        assert_eq!(analyzer.filled, 1);
        assert_eq!(analyzer.rolling.len(), 512);
        assert_eq!(analyzer.generation, runtime.generation);
        assert_eq!(runtime.generation, 1);
        assert_eq!(analyzer.visualizer.demand, ALL_DEMAND);
        assert!(analyzer.density.is_some());
        assert_eq!(analyzer.visualizer.spatial.capacity(), 256);
        assert!(control.frame_requested.load(Ordering::Acquire));
    }

    #[test]
    fn worker_invalidates_settings_and_display_before_any_audio_arrives() {
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        let mut generation = shared.read().generation;
        worker.configure(visualizer_settings());
        wait_for_frame(&shared, |frame| frame.generation > generation);
        generation = shared.read().generation;
        worker.reset(96_000);
        wait_for_frame(&shared, |frame| frame.generation > generation);
        generation = shared.read().generation;
        worker.control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: ALL_DEMAND,
        });
        let token = worker.control.display_token();
        wait_for_frame(&shared, |frame| {
            frame.generation == generation && frame.display_token == token
        });
        assert_eq!(shared.read().visualizer.demand, ALL_DEMAND);
        generation = shared.read().generation;
        worker.control.set_display_enabled(false);
        worker.control.set_display_enabled(true);
        let token = worker.control.display_token();
        wait_for_frame(&shared, |frame| {
            frame.generation == generation && frame.display_token == token
        });
        assert_eq!(shared.read().settings, None);
        generation = shared.read().generation;
        worker.set_still_mode(true);
        wait_for_frame(&shared, |frame| frame.generation > generation);
        generation = shared.read().generation;
        worker.configure(AnalysisSettings {
            fft_size: 32768,
            ..visualizer_settings()
        });
        wait_for_frame(&shared, |frame| frame.generation > generation);
        generation = shared.read().generation;
        worker.reset(192_000);
        wait_for_frame(&shared, |frame| frame.generation > generation);
        generation = shared.read().generation;
        let epoch = worker.control.epoch.load(Ordering::Acquire);
        worker.control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: BAND_ENERGY_DEMAND,
        });
        // Optional-only requests while suspended cannot publish or reset signal continuity.
        assert_eq!(shared.read().generation, generation);
        assert_eq!(worker.control.epoch.load(Ordering::Acquire), epoch);
        let frame = shared.read();
        assert_eq!(frame.settings, None);
        assert_eq!(frame.sample_time, Duration::ZERO);
        assert!(frame.frequencies_hz.is_empty());
        assert!(frame.spectrum_db.is_empty());
        assert_eq!(frame.visualizer.demand, VisualizerDemand::default());
        assert!(frame.visualizer.density.is_empty());
        assert_eq!(frame.visualizer.spatial.capacity(), 0);
    }

    #[test]
    fn prepared_runtime_retains_fft_and_plot_storage_across_hide_and_still_mode() {
        let control = AnalysisControl::new();
        control.playback_active.store(true, Ordering::Release);
        control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: ALL_DEMAND,
        });
        let mut runtime = AnalysisRuntime::new(visualizer_settings(), 5120);
        runtime.demand = ALL_DEMAND;
        let ring = Arc::new(HeapRb::new(1024));
        let mut producer = HeapProd::new(ring.clone());
        let mut consumer = HeapCons::new(ring);
        push_window(&mut producer, 0, control.display_token(), [0.5, -0.5]);
        assert_eq!(
            runtime.ingest_available(&mut consumer, &control, 0, control.display_token(), true),
            (512, false)
        );
        let shared = RwLock::new(AnalysisFrame::default());
        let analyzer = runtime.analyzer.as_mut().unwrap();
        let rms = analyzer.transform();
        analyzer.publish(&mut shared.write(), rms, Duration::from_secs(1));
        let fft = Arc::clone(&analyzer.fft);
        let storage = |analyzer: &SpectrumAnalyzer| {
            let density = analyzer.density.as_ref().unwrap();
            (
                analyzer.frequency.as_ptr(),
                analyzer.scratch.as_ptr(),
                analyzer.window.as_ptr(),
                analyzer.rolling.as_ptr(),
                density.cells.as_ptr(),
                density.snapshots.each_ref().map(|slot| slot.as_ptr()),
                analyzer.visualizer.spatial.as_ptr(),
                analyzer.visualizer.spatial.capacity(),
                analyzer
                    .pcm_snapshots
                    .as_ref()
                    .unwrap()
                    .each_ref()
                    .map(|slot| slot.as_ptr()),
            )
        };
        let prepared_storage = storage(analyzer);
        for still in [false, true, false, true] {
            if still {
                control.still_mode.fetch_add(3, Ordering::AcqRel);
            } else {
                control.set_display_enabled(false);
            }
            control.epoch.fetch_add(2, Ordering::AcqRel);
            if still {
                runtime.reset();
            } else {
                runtime.clear_window();
            }
            discard_analysis(
                &mut runtime,
                &shared,
                control.display_token(),
                control.epoch.load(Ordering::Acquire),
                0,
                VisualizerDemand::default(),
            );
            runtime.demand = VisualizerDemand::default();
            let analyzer = runtime.analyzer.as_ref().unwrap();
            assert!(Arc::ptr_eq(&analyzer.fft, &fft));
            assert_eq!(storage(analyzer), prepared_storage);
            assert_eq!(analyzer.filled, 0);
            assert!(shared.read().spectrum_db.is_empty());
            assert!(shared.read().visualizer.density.is_empty());
            assert_eq!(shared.read().visualizer.spatial.capacity(), 0);
            assert!(analyzer.visualizer.pcm.is_empty());
            assert!(shared.read().visualizer.pcm.is_empty());

            // Desired configuration may change repeatedly while suspended;
            // returning to the prepared configuration must not rebuild it.
            runtime.settings.fft_size = 32768;
            runtime.sample_rate = 192_000;
            assert_eq!(
                runtime.ingest_available(
                    &mut consumer,
                    &control,
                    control.epoch.load(Ordering::Acquire),
                    control.display_token(),
                    control.accepts_audio(),
                ),
                (0, false)
            );
            assert_eq!(
                storage(runtime.analyzer.as_ref().unwrap()),
                prepared_storage
            );
            runtime.settings = visualizer_settings();
            runtime.sample_rate = 5120;
            runtime.demand = ALL_DEMAND;
            if still {
                control.still_mode.fetch_add(1, Ordering::AcqRel);
            } else {
                control.set_display_enabled(true);
            }
            let epoch = control.epoch.fetch_add(2, Ordering::AcqRel) + 2;
            runtime.reset();
            push_window(&mut producer, epoch, control.display_token(), [0.0; 2]);
            assert_eq!(
                runtime.ingest_available(
                    &mut consumer,
                    &control,
                    epoch,
                    control.display_token(),
                    control.accepts_audio(),
                ),
                (512, false)
            );
            let analyzer = runtime.analyzer.as_mut().unwrap();
            assert!(Arc::ptr_eq(&analyzer.fft, &fft));
            assert_eq!(storage(analyzer), prepared_storage);
            let rms = analyzer.transform();
            assert_eq!(rms, [0.0; 2]);
            analyzer.publish(&mut shared.write(), rms, Duration::from_secs(2));
            let frame = shared.read();
            assert_eq!(frame.generation, runtime.generation);
            assert!(!frame.frequencies_hz.is_empty());
            assert_eq!(frame.frequencies_hz.len(), frame.spectrum_db.len());
            assert!(frame.visualizer.density.iter().all(|cell| *cell == 0.0));
            assert_eq!(frame.visualizer.pcm.len(), 512);
            assert!(
                frame
                    .visualizer
                    .pcm
                    .iter()
                    .all(|sample| *sample == [0.0; 2])
            );
            assert!(
                frame
                    .visualizer
                    .spatial
                    .iter()
                    .all(|bin| bin.power == 1e-14)
            );
        }
    }

    #[test]
    fn prepared_runtime_defers_changed_fft_configuration_until_current_audio() {
        let control = AnalysisControl::new();
        control.playback_active.store(true, Ordering::Release);
        control.set_display_enabled(true);
        let token = control.display_token();
        let mut runtime = AnalysisRuntime::new(visualizer_settings(), 5120);
        let ring = Arc::new(HeapRb::new(16));
        let mut producer = HeapProd::new(ring.clone());
        let mut consumer = HeapCons::new(ring);
        assert!(
            producer
                .try_push(TapFrame {
                    demand_token: token,
                    ..TapFrame::default()
                })
                .is_ok()
        );
        assert_eq!(
            runtime.ingest_available(&mut consumer, &control, 0, token, true),
            (1, false)
        );
        let fft = Arc::clone(&runtime.analyzer.as_ref().unwrap().fft);
        runtime.settings.fps = 60;
        runtime.settings.signal = VisualizationSignal::Before;
        runtime.reset();
        assert!(
            producer
                .try_push(TapFrame {
                    demand_token: token,
                    ..TapFrame::default()
                })
                .is_ok()
        );
        assert_eq!(
            runtime.ingest_available(&mut consumer, &control, 0, token, true),
            (1, false)
        );
        assert!(Arc::ptr_eq(&runtime.analyzer.as_ref().unwrap().fft, &fft));
        assert_eq!(
            runtime.analyzer.as_ref().unwrap().settings,
            runtime.settings
        );
        runtime.settings.fft_size = 1024;
        runtime.settings.window = AnalysisWindow::Hann;
        runtime.sample_rate = 96_000;
        runtime.reset();
        assert!(
            producer
                .try_push(TapFrame {
                    epoch: 2,
                    demand_token: token,
                    ..TapFrame::default()
                })
                .is_ok()
        );
        assert_eq!(
            runtime.ingest_available(&mut consumer, &control, 0, token, true),
            (0, false)
        );
        assert!(Arc::ptr_eq(&runtime.analyzer.as_ref().unwrap().fft, &fft));
        assert_eq!(runtime.analyzer.as_ref().unwrap().rolling.len(), 512);
        assert!(
            producer
                .try_push(TapFrame {
                    demand_token: token,
                    ..TapFrame::default()
                })
                .is_ok()
        );
        assert_eq!(
            runtime.ingest_available(&mut consumer, &control, 0, token, true),
            (1, false)
        );
        let analyzer = runtime.analyzer.as_ref().unwrap();
        assert!(!Arc::ptr_eq(&analyzer.fft, &fft));
        assert_eq!(analyzer.settings, runtime.settings);
        assert_eq!(analyzer.sample_rate, 96_000);
        assert_eq!(analyzer.rolling.len(), 1024);
        assert_eq!(analyzer.filled, 1);
        assert_eq!(analyzer.generation, runtime.generation);
    }

    #[test]
    fn ingestion_retains_new_epoch_and_capture_input_for_their_own_cycle() {
        let worker = AnalysisWorker::new(Arc::default()).unwrap();
        set_test_playback(&worker, true);
        let epoch = worker.control.epoch.load(Ordering::Acquire);
        let token = worker.control.display_token();
        for epoch_change in [false, true] {
            let ring = Arc::new(HeapRb::new(1024));
            let mut producer = HeapProd::new(ring.clone());
            let mut consumer = HeapCons::new(ring);
            let old_epoch = if epoch_change {
                epoch.wrapping_sub(2)
            } else {
                epoch
            };
            let old_token = if epoch_change {
                token
            } else {
                token.wrapping_sub(DisplayDemand::CAPTURE_STEP)
            };
            push_window(&mut producer, old_epoch, old_token, [0.25; 2]);
            push_window(&mut producer, epoch, token, [0.75; 2]);
            let mut runtime = AnalysisRuntime::new(visualizer_settings(), 5120);
            runtime.demand = BAND_ENERGY_DEMAND;
            assert_eq!(
                runtime.ingest_available(
                    &mut consumer,
                    &worker.control,
                    old_epoch,
                    old_token,
                    true
                ),
                (512, true)
            );
            assert_eq!(consumer.occupied_len(), 512);
            runtime.reset();
            assert_eq!(
                runtime.ingest_available(&mut consumer, &worker.control, epoch, token, true),
                (512, false)
            );
            let analyzer = runtime.analyzer.as_mut().unwrap();
            let rms = analyzer.transform();
            let mut frame = AnalysisFrame::default();
            analyzer.publish(&mut frame, rms, Duration::from_millis(100));
            assert_eq!(frame.settings, Some(analyzer.settings));
            assert!((frame.rms_left - 0.75).abs() < 0.0001);
            assert!((frame.rms_right - 0.75).abs() < 0.0001);
            assert_eq!(consumer.occupied_len(), 0);
        }
    }

    #[test]
    fn publication_permit_survives_an_incomplete_window_until_fresh_pcm_arrives() {
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        worker.configure(AnalysisSettings {
            fps: 60,
            fft_size: 512,
            window: AnalysisWindow::None,
            signal: VisualizationSignal::After,
        });
        set_test_playback(&worker, true);
        let mut producer = worker.audio_producer();
        producer.push_samples(std::iter::repeat_n([0.5, -0.25], 511));
        worker.control.request_frame();
        wait_until(|| producer.ring.occupied_len() == 0);
        assert_eq!(shared.read().settings, None);
        assert!(worker.control.frame_requested.load(Ordering::Acquire));

        // No second permit: completing the window must publish the outstanding
        // request, even if its first ingestion cycle saw only 511 samples.
        producer.push_samples(std::iter::once([0.5, -0.25]));
        wait_for_frame(&shared, |frame| frame.settings.is_some());
        let frame = shared.read();
        assert_eq!(frame.settings, Some(worker.settings()));
        assert_eq!(frame.display_token, worker.control.display_token());
        assert_eq!([frame.rms_left, frame.rms_right], [0.5, 0.25]);
        assert!(!worker.control.frame_requested.load(Ordering::Acquire));
    }

    #[test]
    fn low_fps_ingestion_preserves_consumer_continuity_without_publication_permits() {
        for sample_rate in [48_000, 192_000] {
            for fps in [5, 20] {
                let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
                let worker = AnalysisWorker::new(shared.clone()).unwrap();
                worker.configure(AnalysisSettings {
                    fps,
                    fft_size: 8192,
                    window: AnalysisWindow::None,
                    signal: VisualizationSignal::After,
                });
                worker.reset(sample_rate);
                set_test_playback(&worker, true);
                let mut producer = worker.audio_producer();
                let weights = [[1.0, 0.0], [0.0, 1.0]];
                producer.push(&[0.25; 8192 * 2], &weights);
                worker.control.request_frame();
                wait_for_frame(&shared, |frame| frame.settings.is_some());
                let previous = shared.read().clone();

                // More PCM than the whole ring, with no publication permit.
                // Each arrival must wake ingestion rather than waiting for FPS.
                for _ in 0..75 {
                    producer.push(&[0.5; 512 * 2], &weights);
                    wait_until(|| producer.ring.occupied_len() == 0);
                    let frame = shared.read();
                    assert_eq!(frame.sequence, previous.sequence);
                    assert_eq!(frame.generation, previous.generation);
                    assert_eq!(frame.settings, previous.settings);
                }
                assert!(!worker.control.tap_overflow.load(Ordering::Acquire));
                worker.control.request_frame();
                wait_for_frame(&shared, |frame| frame.sequence > previous.sequence);
                let frame = shared.read();
                assert_eq!(frame.sequence, previous.sequence + 1);
                assert_eq!(frame.generation, previous.generation);
                assert_eq!(frame.display_token, previous.display_token);
                assert_eq!(frame.settings, previous.settings);
                assert!((frame.rms_left - 0.5).abs() < 0.0001);
                assert!((frame.rms_right - 0.5).abs() < 0.0001);
                let accepted = (frame.sample_time - previous.sample_time).as_secs_f64();
                assert!((accepted - 38_400.0 / sample_rate as f64).abs() < 0.000001);
            }
        }
    }

    #[test]
    fn worker_requires_publication_permits_and_coalesces_requests_at_the_fps_cap() {
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        worker.configure(AnalysisSettings {
            fps: 5,
            fft_size: 512,
            window: AnalysisWindow::None,
            signal: VisualizationSignal::After,
        });
        set_test_playback(&worker, true);
        let mut producer = worker.audio_producer();
        let weights = [[1.0, 0.0], [0.0, 1.0]];
        producer.push(&[0.25; 512 * 2], &weights);
        wait_until(|| producer.ring.occupied_len() == 0);
        thread::sleep(Duration::from_millis(250));
        assert_eq!(shared.read().settings, None);
        assert_eq!(producer.ring.occupied_len(), 0);

        worker.control.request_frame();
        wait_for_frame(&shared, |frame| frame.settings.is_some());
        let first_sequence = shared.read().sequence;
        producer.push(&[0.5; 512 * 2], &weights);
        for _ in 0..100 {
            worker.control.request_frame();
        }
        thread::sleep(Duration::from_millis(20));
        assert_eq!(shared.read().sequence, first_sequence);
        wait_for_frame(&shared, |frame| frame.sequence > first_sequence);
        assert_eq!(shared.read().sequence, first_sequence + 1);

        // Extra requests must not remain as an accumulating job queue.
        producer.push(&[0.75; 512 * 2], &weights);
        wait_until(|| producer.ring.occupied_len() == 0);
        thread::sleep(Duration::from_millis(250));
        assert_eq!(shared.read().sequence, first_sequence + 1);
        assert_eq!(producer.ring.occupied_len(), 0);
    }

    #[test]
    fn demanded_frames_accumulate_fft_windows_larger_than_the_pcm_ring() {
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::with_capacity(shared.clone(), 512).unwrap();
        worker.configure(AnalysisSettings {
            fps: 60,
            fft_size: 1024,
            window: AnalysisWindow::None,
            signal: VisualizationSignal::After,
        });
        worker.control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: DENSITY_DEMAND,
        });
        set_test_playback(&worker, true);
        let mut producer = worker.producer();
        let epoch = worker.control.epoch.load(Ordering::Acquire);
        push_window(
            &mut producer,
            epoch,
            worker.control.display_token(),
            [1.0, 0.0],
        );
        worker.control.request_frame();
        wait_until(|| producer.occupied_len() == 0);
        assert_eq!(shared.read().settings, None);
        push_window(
            &mut producer,
            epoch,
            worker.control.display_token(),
            [0.0, 1.0],
        );
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| frame.settings.is_some());
        let frame = shared.read();
        for rms in [frame.rms_left, frame.rms_right] {
            assert!((rms - 0.5_f32.sqrt()).abs() < 0.0001);
        }
        assert!(frame.visualizer.density[64 * DENSITY_SIDE + 191] > 0.0);
        assert!(frame.visualizer.density[64 * DENSITY_SIDE + 64] > 0.0);
        assert_eq!(frame.visualizer.correlation, Some(0.0));
    }

    #[test]
    fn demanded_frame_ingests_density_samples_outside_the_latest_fft_window() {
        let control = AnalysisControl::new();
        control.playback_active.store(true, Ordering::Release);
        control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: DENSITY_DEMAND,
        });
        let ring = Arc::new(HeapRb::new(2048));
        let mut producer = HeapProd::new(ring.clone());
        let mut consumer = HeapCons::new(ring);
        let epoch = control.epoch.load(Ordering::Acquire);
        let token = control.display_token();
        push_window(&mut producer, epoch, token, [1.0, 0.0]);
        push_window(&mut producer, epoch, token, [0.0, 1.0]);
        let mut runtime = AnalysisRuntime::new(visualizer_settings(), 48_000);
        runtime.demand = DENSITY_DEMAND;
        let (accepted, controls_changed) =
            runtime.ingest_available(&mut consumer, &control, epoch, token, true);
        assert_eq!(accepted, 1024);
        assert!(!controls_changed);
        let analyzer = runtime.analyzer.as_mut().unwrap();
        let rms = analyzer.transform();
        let mut frame = AnalysisFrame::default();
        analyzer.publish(&mut frame, rms, Duration::ZERO);
        assert_eq!([frame.rms_left, frame.rms_right], [0.0, 1.0]);
        assert!(frame.visualizer.density[64 * DENSITY_SIDE + 191] > 0.0);
        assert!(frame.visualizer.density[64 * DENSITY_SIDE + 64] > 0.0);
    }

    #[test]
    fn stalled_consumer_discards_overflowed_prefix_and_restarts_at_the_gap() {
        use ringbuf::traits::Producer;
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::with_capacity(shared.clone(), 1024).unwrap();
        worker.configure(AnalysisSettings {
            fps: 60,
            fft_size: 512,
            window: AnalysisWindow::None,
            signal: VisualizationSignal::After,
        });
        worker.control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: DENSITY_DEMAND,
        });
        set_test_playback(&worker, true);
        let mut producer = worker.producer();
        let epoch = worker.control.epoch.load(Ordering::Acquire);
        push_window(
            &mut producer,
            epoch,
            worker.control.display_token(),
            [0.75; 2],
        );
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| frame.settings.is_some());
        let previous = shared.read().clone();
        push_window(
            &mut producer,
            epoch,
            worker.control.display_token(),
            [1.0; 2],
        );
        push_window(
            &mut producer,
            epoch,
            worker.control.display_token(),
            [1.0; 2],
        );
        assert!(producer.try_push(TapFrame::default()).is_err());
        worker.control.tap_overflow.store(true, Ordering::Release);
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| {
            producer.occupied_len() == 0 && frame.generation > previous.generation
        });
        assert_eq!(shared.read().sequence, previous.sequence);
        assert_eq!(shared.read().settings, None);
        for index in 0..511 {
            assert!(
                producer
                    .try_push(TapFrame {
                        samples: [0.0; 2],
                        epoch,
                        demand_token: worker.control.display_token(),
                        gap: index == 0,
                    })
                    .is_ok()
            );
        }
        worker.control.request_frame();
        wait_until(|| producer.occupied_len() == 0);
        assert_eq!(shared.read().sequence, previous.sequence);
        assert!(
            producer
                .try_push(TapFrame {
                    epoch,
                    demand_token: worker.control.display_token(),
                    ..TapFrame::default()
                })
                .is_ok()
        );
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| frame.sequence > previous.sequence);
        let frame = shared.read();
        assert_eq!([frame.rms_left, frame.rms_right], [0.0; 2]);
        assert!(frame.visualizer.density.iter().all(|value| *value == 0.0));
    }

    #[test]
    fn quick_disable_enable_cancels_in_flight_publication_and_pending_demand() {
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        worker.configure(AnalysisSettings {
            fps: 60,
            fft_size: 512,
            window: AnalysisWindow::None,
            signal: VisualizationSignal::After,
        });
        set_test_playback(&worker, true);
        let mut producer = worker.producer();
        let epoch = worker.control.epoch.load(Ordering::Acquire);
        push_window(
            &mut producer,
            epoch,
            worker.control.display_token(),
            [0.25; 2],
        );
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| frame.settings.is_some());
        let held_frame = shared.write();
        push_window(
            &mut producer,
            epoch,
            worker.control.display_token(),
            [1.0; 2],
        );
        worker.control.request_frame();
        wait_until(|| producer.occupied_len() == 0);
        worker.control.request_frame();
        set_test_playback(&worker, false);
        set_test_playback(&worker, true);
        drop(held_frame);
        let current_epoch = worker.control.epoch.load(Ordering::Acquire);
        wait_for_frame(&shared, |frame| {
            frame.audio_epoch == current_epoch && frame.settings.is_none()
        });
        assert!(!worker.control.frame_requested.load(Ordering::Acquire));
        push_window(
            &mut producer,
            epoch,
            worker.control.display_token(),
            [1.0; 2],
        );
        push_window(
            &mut producer,
            current_epoch,
            worker.control.display_token(),
            [0.0; 2],
        );
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| {
            frame.audio_epoch == current_epoch && frame.settings.is_some()
        });
        assert_eq!(shared.read().rms_left, 0.0);
        assert_eq!(shared.read().rms_right, 0.0);
    }

    #[test]
    fn parked_and_pending_workers_shutdown_without_consumer_assistance() {
        for pending in [false, true] {
            let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
            let worker = AnalysisWorker::new(shared.clone()).unwrap();
            worker.configure(AnalysisSettings {
                fps: 5,
                fft_size: 512,
                window: AnalysisWindow::None,
                signal: VisualizationSignal::After,
            });
            set_test_playback(&worker, true);
            let mut producer = worker.producer();
            push_window(
                &mut producer,
                worker.control.epoch.load(Ordering::Acquire),
                worker.control.display_token(),
                [0.0; 2],
            );
            worker.control.request_frame();
            wait_for_frame(&shared, |frame| frame.settings.is_some());
            if pending {
                worker.control.request_frame();
            }
            // Let the no-demand path park (or the pending path reach its cap).
            thread::sleep(Duration::from_millis(20));
            let (done, completed) = std::sync::mpsc::channel();
            let closer = thread::spawn(move || {
                drop(worker);
                done.send(()).unwrap();
            });
            completed.recv_timeout(Duration::from_secs(2)).unwrap();
            closer.join().unwrap();
        }
    }

    #[test]
    fn explicit_still_mode_drops_results_and_resumes_only_current_epoch_audio() {
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        let settings = AnalysisSettings {
            fps: 60,
            fft_size: 512,
            window: AnalysisWindow::None,
            signal: VisualizationSignal::After,
        };
        worker.configure(settings);
        worker.control.set_display_demand(DisplayDemand {
            enabled: true,
            visualizer: ALL_DEMAND,
        });
        set_test_playback(&worker, true);
        let mut producer = worker.producer();
        let old_epoch = worker.control.epoch.load(Ordering::Acquire);
        push_window(
            &mut producer,
            old_epoch,
            worker.control.display_token(),
            [0.5, -0.5],
        );
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| frame.settings.is_some());
        let previous = shared.read().clone();
        worker.set_still_mode(true);
        wait_for_frame(&shared, |frame| frame.sequence > previous.sequence);
        let suspended_sequence = shared.read().sequence;
        // Repeated commands and configuration changes must not republish old data.
        for fft_size in [32768, 8192, 1024, 512] {
            worker.set_still_mode(true);
            worker.configure(AnalysisSettings {
                fft_size,
                ..settings
            });
            worker.reset(96_000);
            assert!(shared.read().spectrum_db.is_empty());
            assert_eq!(shared.read().settings, None);
            assert_eq!(shared.read().visualizer.demand, VisualizerDemand::default());
            assert!(shared.read().visualizer.density.is_empty());
            assert_eq!(shared.read().visualizer.spatial.capacity(), 0);
            assert!(shared.read().visualizer.pcm.is_empty());
            worker.control.set_display_demand(DisplayDemand {
                enabled: true,
                visualizer: BAND_ENERGY_DEMAND,
            });
            worker.control.set_display_demand(DisplayDemand {
                enabled: true,
                visualizer: ALL_DEMAND,
            });
        }
        worker.set_still_mode(false);
        wait_for_frame(&shared, |frame| {
            frame.audio_epoch == worker.control.epoch.load(Ordering::Acquire)
                && frame.sequence > suspended_sequence
                && frame.settings.is_none()
        });
        assert!(shared.read().frequencies_hz.is_empty());
        // A complete stale window must not contribute to the new result.
        push_window(
            &mut producer,
            old_epoch,
            worker.control.display_token(),
            [1.0; 2],
        );
        let epoch = worker.control.epoch.load(Ordering::Acquire);
        push_window(
            &mut producer,
            epoch,
            worker.control.display_token(),
            [0.0; 2],
        );
        worker.control.request_frame();
        wait_for_frame(&shared, |frame| frame.settings.is_some());
        let frame = shared.read();
        assert_eq!(frame.settings, Some(settings));
        assert_eq!(frame.frequencies_hz.len(), frame.spectrum_db.len());
        assert!(!frame.frequencies_hz.is_empty());
        assert_eq!(frame.sample_rate, 96_000);
        assert!(frame.generation > previous.generation);
        assert!(frame.sample_time > previous.sample_time);
        assert_eq!([frame.rms_left, frame.rms_right], [0.0; 2]);
        assert!(frame.visualizer.density.iter().all(|value| *value == 0.0));
        assert_eq!(frame.visualizer.demand, ALL_DEMAND);
        assert_eq!(frame.visualizer.density.len(), DENSITY_SIDE * DENSITY_SIDE);
        assert_eq!(frame.visualizer.pcm.len(), settings.fft_size as usize);
        assert!(
            frame
                .visualizer
                .pcm
                .iter()
                .all(|sample| *sample == [0.0; 2])
        );
        assert_eq!(
            frame.visualizer.spatial.len(),
            settings.fft_size as usize / 2
        );
        assert!(
            frame
                .visualizer
                .spatial
                .iter()
                .all(|bin| bin.power == 1e-14)
        );
        assert_eq!(frame.visualizer.band_rms, [0.0; 3]);
        let published = frame.sequence;
        drop(frame);
        worker.set_still_mode(true);
        worker.set_still_mode(false);
        wait_for_frame(&shared, |frame| {
            frame.sequence > published && frame.settings.is_none()
        });
        assert!(shared.read().spectrum_db.is_empty());
    }

    #[test]
    fn worker_publishes_stream_gap_and_configuration_generations() {
        use ringbuf::traits::Producer;
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        let mut settings = AnalysisSettings {
            fps: 60,
            fft_size: 512,
            window: AnalysisWindow::None,
            signal: VisualizationSignal::After,
        };
        worker.configure(settings);
        set_test_playback(&worker, true);
        let mut producer = worker.producer();
        let mut previous = AnalysisFrame::default();
        for transition in 0..5 {
            match transition {
                1 => worker.reset(48_000),
                2 => {
                    set_test_playback(&worker, false);
                    set_test_playback(&worker, true);
                }
                4 => {
                    settings.window = AnalysisWindow::Hann;
                    worker.configure(settings);
                }
                _ => {}
            }
            let epoch = worker.control.epoch.load(Ordering::Acquire);
            let value = (transition + 1) as f32 * 0.125;
            for index in 0..512 {
                assert!(
                    producer
                        .try_push(TapFrame {
                            samples: [value, -value],
                            epoch,
                            demand_token: worker.control.display_token(),
                            gap: transition == 3 && index == 0,
                        })
                        .is_ok()
                );
            }
            worker.control.request_frame();
            let deadline = Instant::now() + Duration::from_secs(2);
            let current = loop {
                let frame = shared.read();
                if frame.sequence != previous.sequence && frame.settings.is_some() {
                    break frame.clone();
                }
                drop(frame);
                assert!(Instant::now() < deadline, "analysis publication timed out");
                thread::sleep(Duration::from_millis(1));
            };
            if transition == 2 {
                assert_eq!(current.generation, previous.generation);
            } else {
                assert!(current.generation > previous.generation);
            }
            assert_eq!(current.settings, Some(settings));
            assert_eq!(current.sample_rate, 48_000);
            assert!(current.sample_time > previous.sample_time);
            assert!((current.rms_left - value).abs() < 0.001);
            assert!((current.rms_right - value).abs() < 0.001);
            previous = current;
        }
    }

    #[test]
    fn tap_gap_requires_a_complete_new_window() {
        let settings = AnalysisSettings {
            fps: 5,
            fft_size: 512,
            ..AnalysisSettings::from(&Config::default())
        };
        let mut analyzer = SpectrumAnalyzer::new(settings, 192_000);
        for _ in 0..settings.fft_size {
            analyzer.push([1.0; 2]);
        }
        analyzer.push_tap(&TapFrame {
            samples: [0.0; 2],
            gap: true,
            ..TapFrame::default()
        });
        assert_eq!(analyzer.filled, 1);
        for _ in 1..settings.fft_size {
            analyzer.push_tap(&TapFrame::default());
        }
        assert_eq!(analyzer.filled, settings.fft_size as usize);
        assert_eq!(analyzer.transform(), [0.0; 2]);
    }

    #[test]
    fn musical_bands_follow_nyquist() {
        for rate in [48_000, 8_000, 192_000, 44_100] {
            let frequencies = musical_frequencies(rate);
            assert_eq!(frequencies.last().copied(), Some(rate as f32 * 0.5));
            assert!(frequencies.windows(2).all(|pair| pair[0] < pair[1]));
            assert!(frequencies.contains(&440.0));
            assert!(
                frequencies
                    .iter()
                    .all(|hz| *hz > 0.0 && *hz <= rate as f32 * 0.5)
            );
        }
    }

    #[test]
    fn tone_frequency_and_amplitude_are_calibrated_across_fft_windows() {
        for fft_size in [512, 1024, 2048, 4096, 8192, 16384, 32768] {
            for window in [
                AnalysisWindow::Hann,
                AnalysisWindow::BlackmanHarris,
                AnalysisWindow::None,
            ] {
                let settings = AnalysisSettings {
                    fps: 20,
                    fft_size,
                    window,
                    signal: VisualizationSignal::After,
                };
                // A1760 is both a musical center and FFT-bin aligned at every size.
                let rate = 56_320;
                let mut analyzer = SpectrumAnalyzer::new(settings, rate);
                analyzer.set_visualizer_demand(SPATIAL_DEMAND);
                for i in 0..fft_size {
                    let sample = 0.5 * (std::f32::consts::TAU * i as f32 / 32.0).sin();
                    analyzer.push([sample, -sample]);
                }
                let rms = analyzer.transform();
                let tone = analyzer
                    .frequencies
                    .iter()
                    .position(|hz| *hz == 1760.0)
                    .unwrap();
                let expected_db = 20.0 * 0.5_f32.log10();
                assert!(
                    (analyzer.levels[tone] - expected_db).abs() < 0.02,
                    "size={fft_size} window={window:?}"
                );
                assert!((rms[0] - 0.5 / 2.0_f32.sqrt()).abs() < 0.001);
                assert_eq!(rms[0], rms[1]);
                let spatial = analyzer
                    .visualizer
                    .spatial
                    .iter()
                    .find(|bin| bin.frequency_hz == 1760.0)
                    .unwrap();
                assert!((spatial.power - 0.25).abs() < 0.001);
                assert!(spatial.pan.abs() < 1e-5);
                let strongest = analyzer
                    .levels
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.total_cmp(b.1))
                    .unwrap()
                    .0;
                let tolerance = rate as f32 / fft_size as f32;
                assert!((analyzer.frequencies[strongest] - 1760.0).abs() <= tolerance + 1.0);
            }
        }
    }

    #[test]
    fn wrapped_window_preserves_asymmetric_nyquist_and_low_amplitudes() {
        for fft_size in [512, 8192, 32768] {
            let settings = AnalysisSettings {
                fps: 20,
                fft_size,
                window: AnalysisWindow::None,
                signal: VisualizationSignal::After,
            };
            for amplitude in [0.5_f32, 1e-6, 1e-8, 0.0, 2.0] {
                let mut analyzer = SpectrumAnalyzer::new(settings, 48_000);
                analyzer.set_visualizer_demand(SPATIAL_DEMAND);
                for i in 0..fft_size + 137 {
                    let sample = if i % 2 == 0 { amplitude } else { -amplitude };
                    analyzer.push([sample, -sample * 0.5]);
                }
                let rms = analyzer.transform();
                let expected_amplitude = amplitude * 0.625_f32.sqrt();
                let expected_db = 20.0 * expected_amplitude.max(1e-7).log10();
                assert!((analyzer.levels.last().unwrap() - expected_db).abs() < 0.001);
                let nyquist = analyzer.visualizer.spatial.last().unwrap();
                let expected_power = (amplitude * amplitude * 0.625).clamp(1e-14, 1.0);
                assert!((nyquist.power - expected_power).abs() <= expected_power * 0.001);
                assert!((nyquist.pan + 0.6).abs() < 1e-5 || amplitude == 0.0);
                assert!(
                    analyzer
                        .visualizer
                        .spatial
                        .iter()
                        .all(|bin| { bin.power.is_finite() && (1e-14..=1.0).contains(&bin.power) })
                );
                assert!((rms[0] - amplitude).abs() <= amplitude * 0.001);
                assert!((rms[1] - amplitude * 0.5).abs() <= amplitude * 0.001);
            }
        }
    }

    #[test]
    fn spatial_power_calibration_preserves_floor_saturation_and_overflow() {
        let mut analyzer = visualizer_analyzer(SPATIAL_DEMAND);
        let normalization_squared = analyzer.normalization * analyzer.normalization;
        for power in [f32::NAN, f32::NEG_INFINITY, -1.0, 0.0, f32::MIN_POSITIVE] {
            assert_eq!(
                calibrated_spatial_power(power, normalization_squared),
                1e-14
            );
        }
        assert_eq!(
            calibrated_spatial_power(f32::MAX, normalization_squared),
            1.0
        );
        assert_eq!(
            calibrated_spatial_power(f32::INFINITY, normalization_squared),
            f32::INFINITY,
        );
        for expected in [1e-12, 0.125, 0.5] {
            let raw = expected / normalization_squared;
            assert!(
                (calibrated_spatial_power(raw, normalization_squared) - expected).abs()
                    <= expected * 1e-6
            );
        }
        // Sanitization preserves finite over-range PCM. The FFT stays finite
        // here, but squaring its Nyquist coefficient overflows in real analysis.
        for i in 0..512 {
            analyzer.push([if i % 2 == 0 { 1e20 } else { -1e20 }, 0.0]);
        }
        analyzer.transform();
        assert_eq!(
            analyzer.visualizer.spatial.last().unwrap().power,
            f32::INFINITY
        );
    }

    #[test]
    fn reset_discards_previous_audio_and_publication_preserves_time() {
        let settings = AnalysisSettings::from(&Config::default());
        let mut analyzer = SpectrumAnalyzer::new(settings, 48_000);
        for _ in 0..settings.fft_size {
            analyzer.push([0.5, -0.5]);
        }
        analyzer.reset();
        assert_eq!(analyzer.filled, 0);
        for _ in 0..settings.fft_size {
            analyzer.push([0.0, 0.0]);
        }
        let rms = analyzer.transform();
        assert_eq!(rms, [0.0, 0.0]);
        assert!(analyzer.levels.iter().all(|db| *db == -140.0));
        let mut frame = AnalysisFrame::default();
        analyzer.publish(&mut frame, rms, Duration::from_secs(1));
        let mut changed = SpectrumAnalyzer::new(settings, 8_000);
        let rms = changed.transform();
        changed.publish(&mut frame, rms, Duration::from_secs(2));
        assert_eq!(frame.sequence, 2);
        assert_eq!(frame.sample_time, Duration::from_secs(2));
        assert_eq!(frame.sample_rate, 8_000);
        assert_eq!(frame.frequencies_hz.last(), Some(&4_000.0));
        assert_eq!(frame.spectrum_db.len(), frame.frequencies_hz.len());
    }

    #[test]
    fn settings_are_validated_and_independent_of_sample_rate() {
        let worker = AnalysisWorker::new(Arc::new(RwLock::new(AnalysisFrame::default()))).unwrap();
        let settings = AnalysisSettings {
            fps: 5,
            fft_size: 32768,
            window: AnalysisWindow::None,
            signal: VisualizationSignal::After,
        };
        worker.configure(settings);
        worker.reset(192_000);
        assert_eq!(*worker.control.settings.read(), settings);
        assert_eq!(worker.control.sample_rate.load(Ordering::Acquire), 192_000);
        assert_eq!(worker.control.epoch.load(Ordering::Acquire) % 2, 0);
        for invalid in [
            AnalysisSettings { fps: 0, ..settings },
            AnalysisSettings {
                fps: 61,
                ..settings
            },
            AnalysisSettings {
                fft_size: 1000,
                ..settings
            },
        ] {
            worker.configure(invalid);
            assert_eq!(*worker.control.settings.read(), settings);
        }
        assert!(!worker.control.accepts_audio());
    }
}
