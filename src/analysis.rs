use crate::config::{Config, SpectrumWindow};
use parking_lot::RwLock;
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer},
};
use rustfft::{Fft, FftPlanner, num_complex::Complex32};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VisualizerMode {
    #[default]
    Off,
    StereoDensity,
    SpatialSpectrum,
    Generative,
}

impl VisualizerMode {
    fn from_bits(bits: u64) -> Self {
        match bits & 3 {
            1 => Self::StereoDensity,
            2 => Self::SpatialSpectrum,
            3 => Self::Generative,
            _ => Self::Off,
        }
    }
}

pub const DENSITY_SIDE: usize = 256;

#[derive(Clone, Copy, Debug)]
pub struct SpatialBin {
    pub frequency_hz: f32,
    /// Energy balance, not a phase-derived source position.
    pub pan: f32,
    pub power_db: f32,
}

#[derive(Debug, Default)]
pub struct VisualizerFrame {
    pub mode: VisualizerMode,
    pub density: Vec<f32>,
    pub spatial: Vec<SpatialBin>,
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
        self.mode = source.mode;
        self.density.clone_from(&source.density);
        self.spatial.clone_from(&source.spatial);
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
    scale: f32,
    decay: f32,
    weight: f32,
}

impl DensityField {
    fn new(sample_rate: u32) -> Self {
        let decay = (-2.0 / sample_rate.max(1) as f32).exp();
        Self {
            cells: vec![0.0; DENSITY_SIDE * DENSITY_SIDE],
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
}

#[derive(Clone, Debug, Default)]
pub struct AnalysisFrame {
    pub sequence: u64,
    /// Changes whenever a window cannot continue the preceding signal.
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
    pub window: SpectrumWindow,
}

impl From<&Config> for AnalysisSettings {
    fn from(config: &Config) -> Self {
        Self {
            fps: config.analysis_fps,
            fft_size: config.spectrum_fft_size,
            window: config.spectrum_window,
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
pub(crate) struct TapFrame {
    pub samples: [f32; 2],
    pub epoch: u64,
    /// The preceding tap frame was dropped; never bridge this gap in an FFT.
    pub gap: bool,
}

pub(crate) struct AnalysisControl {
    pub enabled: AtomicBool,
    settings: RwLock<AnalysisSettings>,
    /// Playback/output sample rate used to map FFT bins to frequencies.
    pub sample_rate: AtomicU32,
    /// Even epochs are stable; odd epochs mark sample-rate reset in progress.
    pub epoch: AtomicU64,
    shutdown: AtomicBool,
    // Low bits encode mode; upper bits detect even a quick away/back switch.
    visualizer_mode: AtomicU64,
}

impl AnalysisControl {
    pub(crate) fn set_visualizer_mode(&self, mode: VisualizerMode) {
        let _ = self
            .visualizer_mode
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (VisualizerMode::from_bits(current) != mode)
                    .then(|| ((current & !3).wrapping_add(4)) | mode as u64)
            });
    }
}

pub(crate) struct AnalysisWorker {
    pub control: Arc<AnalysisControl>,
    pub ring: Arc<HeapRb<TapFrame>>,
    worker: Option<JoinHandle<()>>,
}

impl AnalysisWorker {
    pub fn new(shared: Arc<RwLock<AnalysisFrame>>) -> std::io::Result<Self> {
        let ring = Arc::new(HeapRb::new(32_768));
        let control = Arc::new(AnalysisControl {
            enabled: AtomicBool::new(false),
            settings: RwLock::new(AnalysisSettings::from(&Config::default())),
            sample_rate: AtomicU32::new(48_000),
            epoch: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            visualizer_mode: AtomicU64::new(0),
        });
        let consumer = HeapCons::new(ring.clone());
        let thread_control = control.clone();
        let worker = thread::Builder::new()
            .name("rivu-analysis".into())
            .spawn(move || analyze(consumer, thread_control, shared))?;
        Ok(Self {
            control,
            ring,
            worker: Some(worker),
        })
    }
    pub fn producer(&self) -> HeapProd<TapFrame> {
        HeapProd::new(self.ring.clone())
    }
    pub fn set_enabled(&self, enabled: bool) {
        if self.control.enabled.swap(enabled, Ordering::AcqRel) != enabled {
            // A quick disable/enable must invalidate queued samples even if the
            // analysis thread never observes the disabled state.
            self.control.epoch.fetch_add(2, Ordering::AcqRel);
            if let Some(worker) = &self.worker {
                worker.thread().unpark();
            }
        }
    }
    pub fn reset(&self, sample_rate: u32) {
        self.control.epoch.fetch_add(1, Ordering::AcqRel);
        self.control
            .sample_rate
            .store(sample_rate, Ordering::Release);
        self.control.epoch.fetch_add(1, Ordering::AcqRel);
        if self.control.enabled.load(Ordering::Acquire)
            && let Some(worker) = &self.worker
        {
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
            drop(current);
            if self.control.enabled.load(Ordering::Acquire)
                && let Some(worker) = &self.worker
            {
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

/// All FFT plans and frame-sized storage are allocated only on reconfiguration.
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
}

impl SpectrumAnalyzer {
    fn new(settings: AnalysisSettings, sample_rate: u32) -> Self {
        let n = settings.fft_size as usize;
        let fft = FftPlanner::<f32>::new().plan_fft_forward(n);
        let window: Vec<f32> = (0..n)
            .map(|i| {
                let phase = std::f32::consts::TAU * i as f32 / n as f32;
                match settings.window {
                    SpectrumWindow::Hann => 0.5 - 0.5 * phase.cos(),
                    SpectrumWindow::BlackmanHarris => {
                        0.35875 - 0.48829 * phase.cos() + 0.14128 * (2.0 * phase).cos()
                            - 0.01168 * (3.0 * phase).cos()
                    }
                    SpectrumWindow::None => 1.0,
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
        }
    }

    fn reset(&mut self) {
        self.head = 0;
        self.filled = 0;
        self.generation = self.generation.wrapping_add(1);
        if let Some(density) = &mut self.density {
            density.clear();
        }
        self.visualizer.density.fill(0.0);
        self.visualizer.spatial.clear();
        self.visualizer.correlation = None;
        self.visualizer.balance = None;
        self.visualizer.side_ratio = None;
        self.visualizer.band_rms = [0.0; 3];
    }

    fn set_visualizer_mode(&mut self, mode: VisualizerMode) {
        self.visualizer = VisualizerFrame {
            mode,
            ..VisualizerFrame::default()
        };
        self.density =
            (mode == VisualizerMode::StereoDensity).then(|| DensityField::new(self.sample_rate));
        match mode {
            VisualizerMode::StereoDensity => {
                self.visualizer
                    .density
                    .resize(DENSITY_SIDE * DENSITY_SIDE, 0.0);
            }
            VisualizerMode::SpatialSpectrum => {
                self.visualizer.spatial.reserve(self.rolling.len() / 2);
            }
            VisualizerMode::Off | VisualizerMode::Generative => {}
        }
    }

    fn push(&mut self, samples: [f32; 2]) {
        // Optional plots treat nonfinite channels as silence; leave the
        // existing spectrum-only sample path unchanged.
        let samples = if self.visualizer.mode == VisualizerMode::Off {
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
        let density_mode = self.visualizer.mode == VisualizerMode::StereoDensity;
        let mut energies = [0.0_f64; 3];
        for (i, samples) in self.rolling[self.head..]
            .iter()
            .chain(&self.rolling[..self.head])
            .enumerate()
        {
            rms[0] += samples[0] * samples[0];
            rms[1] += samples[1] * samples[1];
            if density_mode {
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
        if self.visualizer.mode == VisualizerMode::Generative {
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
        if density_mode {
            let [left, right, cross] = energies;
            let total = left + right;
            self.visualizer.correlation = (left > 0.0 && right > 0.0)
                .then(|| (cross / (left * right).sqrt()).clamp(-1.0, 1.0) as f32);
            self.visualizer.balance =
                (total > 0.0).then(|| ((right - left) / total).clamp(-1.0, 1.0) as f32);
            self.visualizer.side_ratio = (total > 0.0)
                .then(|| ((total - 2.0 * cross) / (2.0 * total)).clamp(0.0, 1.0) as f32);
            if let Some(density) = &self.density {
                for (target, cell) in self.visualizer.density.iter_mut().zip(&density.cells) {
                    *target = cell * density.scale;
                }
            }
        }
        if self.visualizer.mode == VisualizerMode::SpatialSpectrum {
            self.visualizer.spatial.clear();
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
                    power_db: (power.sqrt() * self.normalization).max(1e-7).log10() * 20.0,
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

    fn publish(&mut self, frame: &mut AnalysisFrame, rms: [f32; 2], sample_time: Duration) {
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

fn analyze(
    mut consumer: HeapCons<TapFrame>,
    control: Arc<AnalysisControl>,
    shared: Arc<RwLock<AnalysisFrame>>,
) {
    let mut analyzer = SpectrumAnalyzer::new(
        *control.settings.read(),
        control.sample_rate.load(Ordering::Acquire),
    );
    let mut incoming = vec![TapFrame::default(); 1024];
    let mut epoch = 0;
    let mut mode_token = u64::MAX;
    let mut sample_time = Duration::ZERO;
    loop {
        if control.shutdown.load(Ordering::Acquire) {
            break;
        }
        if !control.enabled.load(Ordering::Acquire) {
            consumer.clear();
            analyzer.reset();
            shared.write().visualizer = VisualizerFrame::default();
            thread::park();
            continue;
        }
        let cycle_start = Instant::now();
        let previous_generation = analyzer.generation;
        let current_mode_token = control.visualizer_mode.load(Ordering::Acquire);
        let mode = VisualizerMode::from_bits(current_mode_token);
        let current_epoch = control.epoch.load(Ordering::Acquire);
        let sample_rate = control.sample_rate.load(Ordering::Acquire);
        if !current_epoch.is_multiple_of(2)
            || control.epoch.load(Ordering::Acquire) != current_epoch
        {
            thread::park_timeout(Duration::from_millis(1));
            continue;
        }
        let settings = *control.settings.read();
        if sample_rate != analyzer.sample_rate
            || settings.fft_size != analyzer.settings.fft_size
            || settings.window != analyzer.settings.window
        {
            let generation = analyzer.generation.wrapping_add(1);
            analyzer = SpectrumAnalyzer::new(settings, sample_rate);
            analyzer.generation = generation;
            analyzer.set_visualizer_mode(mode);
        }
        if mode_token != current_mode_token {
            mode_token = current_mode_token;
            analyzer.set_visualizer_mode(mode);
            shared.write().visualizer = VisualizerFrame {
                mode,
                ..VisualizerFrame::default()
            };
        }
        analyzer.settings = settings;
        if current_epoch != epoch {
            epoch = current_epoch;
            analyzer.reset();
        }
        let mut accepted = 0;
        // Bound a cycle to the queued prefix; continuous production must not
        // postpone publication or shutdown indefinitely.
        let mut remaining = consumer.occupied_len();
        while remaining != 0 {
            let limit = remaining.min(incoming.len());
            let count = consumer.pop_slice(&mut incoming[..limit]);
            if count == 0 {
                break;
            }
            remaining -= count;
            for frame in &incoming[..count] {
                if frame.epoch == current_epoch {
                    accepted += 1;
                    analyzer.push_tap(frame);
                }
            }
        }
        if analyzer.generation != previous_generation {
            shared.write().visualizer = VisualizerFrame {
                mode,
                ..VisualizerFrame::default()
            };
        }
        sample_time += Duration::from_secs_f64(accepted as f64 / sample_rate.max(1) as f64);
        if accepted != 0 && analyzer.filled == analyzer.rolling.len() {
            let rms = analyzer.transform();
            if control.epoch.load(Ordering::Acquire) == current_epoch
                && control.enabled.load(Ordering::Acquire)
                && *control.settings.read() == settings
                && control.visualizer_mode.load(Ordering::Acquire) == current_mode_token
            {
                analyzer.publish(&mut shared.write(), rms, sample_time);
            }
        }
        let interval = Duration::from_secs_f64(1.0 / settings.fps as f64);
        thread::park_timeout(interval.saturating_sub(cycle_start.elapsed()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visualizer_analyzer(mode: VisualizerMode) -> SpectrumAnalyzer {
        let mut analyzer = SpectrumAnalyzer::new(
            AnalysisSettings {
                fps: 20,
                fft_size: 512,
                window: SpectrumWindow::None,
            },
            5120,
        );
        analyzer.set_visualizer_mode(mode);
        analyzer
    }

    #[test]
    fn generative_bands_integrate_power_without_phase_cancellation() {
        for window in [
            SpectrumWindow::None,
            SpectrumWindow::Hann,
            SpectrumWindow::BlackmanHarris,
        ] {
            for (frequency, band) in [(128.0, 0), (1024.0, 1), (8192.0, 2)] {
                let mut analyzer = SpectrumAnalyzer::new(
                    AnalysisSettings {
                        fps: 20,
                        fft_size: 4096,
                        window,
                    },
                    32768,
                );
                analyzer.set_visualizer_mode(VisualizerMode::Generative);
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
    fn generative_nyquist_is_not_doubled_and_silence_has_no_drive() {
        let mut analyzer = SpectrumAnalyzer::new(
            AnalysisSettings {
                fps: 20,
                fft_size: 512,
                window: SpectrumWindow::None,
            },
            8000,
        );
        analyzer.set_visualizer_mode(VisualizerMode::Generative);
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
            let mut analyzer = visualizer_analyzer(VisualizerMode::StereoDensity);
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
        let mut analyzer = visualizer_analyzer(VisualizerMode::StereoDensity);
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
        let mut analyzer = visualizer_analyzer(VisualizerMode::StereoDensity);
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
    fn gaps_and_mode_roundtrips_discard_density_history() {
        let mut analyzer = visualizer_analyzer(VisualizerMode::StereoDensity);
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
        analyzer.set_visualizer_mode(VisualizerMode::SpatialSpectrum);
        assert!(analyzer.density.is_none());
        analyzer.set_visualizer_mode(VisualizerMode::StereoDensity);
        analyzer.transform();
        assert!(
            analyzer
                .visualizer
                .density
                .iter()
                .all(|value| *value == 0.0)
        );
        analyzer.set_visualizer_mode(VisualizerMode::Off);
        assert!(analyzer.density.is_none());
        assert!(analyzer.visualizer.density.is_empty());
        assert!(analyzer.visualizer.spatial.is_empty());
    }

    #[test]
    fn spatial_channel_tones_preserve_frequency_energy_and_phase() {
        for channels in [[1.0, 0.0], [0.0, 1.0], [1.0, -1.0]] {
            let mut analyzer = visualizer_analyzer(VisualizerMode::SpatialSpectrum);
            for i in 0..512 {
                let tone = 0.5 * (std::f32::consts::TAU * i as f32 / 32.0).sin();
                analyzer.push(channels.map(|channel| channel * tone));
            }
            analyzer.transform();
            let strongest = analyzer
                .visualizer
                .spatial
                .iter()
                .max_by(|a, b| a.power_db.total_cmp(&b.power_db))
                .unwrap();
            assert_eq!(strongest.frequency_hz, 160.0);
            let expected_pan = if channels[0] == 0.0 {
                1.0
            } else if channels[1] == 0.0 {
                -1.0
            } else {
                0.0
            };
            assert!((strongest.pan - expected_pan).abs() < 1e-5);
            let expected_db = 20.0
                * (0.5
                    * ((channels[0] * channels[0] + channels[1] * channels[1]) * 0.5_f32).sqrt())
                .log10();
            assert!((strongest.power_db - expected_db).abs() < 0.001);
        }
        let mut analyzer = visualizer_analyzer(VisualizerMode::SpatialSpectrum);
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
            assert!(bin.power_db > -20.0);
        }
    }

    #[test]
    fn spatial_nyquist_uses_single_sided_endpoint_normalization() {
        let mut analyzer = visualizer_analyzer(VisualizerMode::SpatialSpectrum);
        for i in 0..512 {
            analyzer.push([if i % 2 == 0 { 0.5 } else { -0.5 }, 0.0]);
        }
        analyzer.transform();
        let nyquist = analyzer.visualizer.spatial.last().unwrap();
        assert_eq!(nyquist.frequency_hz, 2560.0);
        assert_eq!(nyquist.pan, -1.0);
        let expected_db = 20.0 * (0.5 / 2.0_f32.sqrt()).log10();
        assert!((nyquist.power_db - expected_db).abs() < 0.001);
        assert!((nyquist.power_db - analyzer.levels.last().unwrap()).abs() < 0.001);
    }

    #[test]
    fn density_panned_channel_readouts_are_correct() {
        for (samples, balance) in [([0.5, 0.0], -1.0), ([0.0, 0.5], 1.0)] {
            let mut analyzer = visualizer_analyzer(VisualizerMode::StereoDensity);
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
    fn worker_mode_roundtrip_does_not_revive_previous_density() {
        use ringbuf::traits::Producer;
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        worker.configure(AnalysisSettings {
            fps: 60,
            fft_size: 512,
            window: SpectrumWindow::None,
        });
        worker
            .control
            .set_visualizer_mode(VisualizerMode::StereoDensity);
        worker.set_enabled(true);
        let mut producer = worker.producer();
        let mut sequence = 0;
        for samples in [[0.7, -0.7], [0.5, 0.5]] {
            if sequence != 0 {
                worker.control.set_visualizer_mode(VisualizerMode::Off);
                worker
                    .control
                    .set_visualizer_mode(VisualizerMode::StereoDensity);
            }
            let epoch = worker.control.epoch.load(Ordering::Acquire);
            for _ in 0..512 {
                assert!(
                    producer
                        .try_push(TapFrame {
                            samples,
                            epoch,
                            gap: false,
                        })
                        .is_ok()
                );
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let frame = shared.read();
                if frame.sequence > sequence {
                    assert_eq!(frame.visualizer.mode, VisualizerMode::StereoDensity);
                    if sequence != 0 {
                        assert_eq!(frame.visualizer.density[128 * DENSITY_SIDE + 217], 0.0);
                        assert!(frame.visualizer.density[64 * DENSITY_SIDE + 128] > 0.0);
                        assert_eq!(frame.visualizer.correlation, Some(1.0));
                    }
                    sequence = frame.sequence;
                    break;
                }
                drop(frame);
                assert!(Instant::now() < deadline, "analysis did not publish");
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    #[test]
    fn worker_publishes_stream_gap_and_configuration_generations() {
        use ringbuf::traits::Producer;
        let shared = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = AnalysisWorker::new(shared.clone()).unwrap();
        let mut settings = AnalysisSettings {
            fps: 60,
            fft_size: 512,
            window: SpectrumWindow::None,
        };
        worker.configure(settings);
        worker.set_enabled(true);
        let mut producer = worker.producer();
        let mut previous = AnalysisFrame::default();
        for transition in 0..5 {
            match transition {
                1 => worker.reset(48_000),
                2 => {
                    worker.set_enabled(false);
                    worker.set_enabled(true);
                }
                4 => {
                    settings.window = SpectrumWindow::Hann;
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
                            gap: transition == 3 && index == 0,
                        })
                        .is_ok()
                );
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            let current = loop {
                let frame = shared.read();
                if frame.sequence != previous.sequence {
                    break frame.clone();
                }
                drop(frame);
                assert!(Instant::now() < deadline, "analysis publication timed out");
                thread::sleep(Duration::from_millis(1));
            };
            assert!(current.generation > previous.generation);
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
                SpectrumWindow::Hann,
                SpectrumWindow::BlackmanHarris,
                SpectrumWindow::None,
            ] {
                let settings = AnalysisSettings {
                    fps: 20,
                    fft_size,
                    window,
                };
                // A1760 is both a musical center and FFT-bin aligned at every size.
                let rate = 56_320;
                let mut analyzer = SpectrumAnalyzer::new(settings, rate);
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
                window: SpectrumWindow::None,
            };
            for amplitude in [0.5_f32, 1e-6] {
                let mut analyzer = SpectrumAnalyzer::new(settings, 48_000);
                for i in 0..fft_size + 137 {
                    let sample = if i % 2 == 0 { amplitude } else { -amplitude };
                    analyzer.push([sample, -sample * 0.5]);
                }
                let rms = analyzer.transform();
                let expected = 20.0 * (amplitude * 0.625_f32.sqrt()).log10();
                assert!((analyzer.levels.last().unwrap() - expected).abs() < 0.001);
                assert!((rms[0] - amplitude).abs() < amplitude * 0.001);
                assert!((rms[1] - amplitude * 0.5).abs() < amplitude * 0.001);
            }
        }
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
            window: SpectrumWindow::None,
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
        assert!(!worker.control.enabled.load(Ordering::Acquire));
    }
}
