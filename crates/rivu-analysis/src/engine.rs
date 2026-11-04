use crate::{
    AnalysisFrame, AnalysisSettings, DENSITY_SIDE, DensitySnapshot, DisplayDemand, PcmSnapshot,
    SpatialBin, SpectrumSnapshot, VisualizerDemand, VisualizerFrame,
};
use rivu_format::config::AnalysisWindow;
use rivu_protocol::pcm::{PcmHeader, PcmState};
use rustfft::{Fft, FftPlanner, num_complex::Complex32};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

// One accumulation field plus three immutable publication slots. Three slots
// cover a GUI-held frame, the latest publication, and the next worker result.
const DENSITY_SNAPSHOT_COUNT: usize = 3;

// At FFT32768, three reusable slots retain at most 192 KiB of sample storage.
// Retained publications never cause this optional pool to grow.
const SPECTRUM_SNAPSHOT_COUNT: usize = 3;

// Each prepared analyzer owns at most 160 KiB of reusable PCM sample storage.
// Slow consumers omit optional PCM instead of growing this pool.
const PCM_SNAPSHOT_COUNT: usize = 5;
const PCM_SNAPSHOT_FRAMES: usize = 4096;

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
                return DensitySnapshot::from_shared(Arc::clone(slot));
            }
        }
        // A slow consumer may retain every slot. Omit this optional payload
        // rather than allocate, block, or delay the base spectrum publication.
        DensitySnapshot::default()
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
    spectrogram_snapshots: Option<[Arc<Vec<f32>>; SPECTRUM_SNAPSHOT_COUNT]>,
    // Unnormalized conjugate-pair power, shared only with additional consumers.
    // Spatial pan still needs the independently reconstructed channel powers.
    bin_powers: Vec<f32>,
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
            spectrogram_snapshots: None,
            bin_powers: Vec::new(),
        }
    }

    #[cfg(test)]
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
        self.visualizer.spectrogram = SpectrumSnapshot::default();
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
        if demand.band_energy || demand.spectrogram {
            self.bin_powers.resize(self.rolling.len() / 2, 0.0);
        } else {
            self.bin_powers = Vec::new();
        }
        self.set_spectrogram_demand(demand.spectrogram);
        self.visualizer.demand = demand;
    }

    fn set_spectrogram_demand(&mut self, demanded: bool) {
        if demanded {
            self.spectrogram_snapshots.get_or_insert_with(|| {
                std::array::from_fn(|_| Arc::new(vec![-140.0; self.rolling.len() / 2]))
            });
        } else {
            self.visualizer.spectrogram = SpectrumSnapshot::default();
            self.spectrogram_snapshots = None;
        }
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
        let cached_bins = if self.visualizer.demand.spectrogram && self.snapshot_spectrogram() {
            1..n / 2 + 1
        } else if self.visualizer.demand.band_energy {
            // The three drive bands are contiguous. Do not scan unused bins
            // when band energy is the only additional power consumer.
            let bins = self.drive_bins[0].0..self.drive_bins[2].1;
            for bin in bins.clone() {
                self.bin_powers[bin - 1] =
                    self.frequency[bin].norm_sqr() + self.frequency[n - bin].norm_sqr();
            }
            bins
        } else {
            0..0
        };
        if self.visualizer.demand.band_energy {
            for (target, &(first, last)) in
                self.visualizer.band_rms.iter_mut().zip(&self.drive_bins)
            {
                let mut power = 0.0;
                for bin in first..last {
                    let endpoint = if bin == n / 2 { 0.5 } else { 1.0 };
                    power += self.bin_powers[bin - 1] * 0.5 * endpoint;
                }
                *target = (power * self.power_normalization).sqrt();
            }
        }
        for (level, &(first, last)) in self.levels.iter_mut().zip(&self.bands) {
            let mut power = 0.0_f32;
            for bin in first..last {
                let pair_power = if cached_bins.contains(&bin) {
                    self.bin_powers[bin - 1]
                } else {
                    self.frequency[bin].norm_sqr() + self.frequency[n - bin].norm_sqr()
                };
                // Nyquist has no negative-frequency partner to double.
                let endpoint_gain = if bin == n / 2 { 0.25 } else { 1.0 };
                power = power.max(pair_power * 0.25 * endpoint_gain);
            }
            *level = (power.sqrt() * self.normalization).max(1e-7).log10() * 20.0;
        }
        [(rms[0] / n as f32).sqrt(), (rms[1] / n as f32).sqrt()]
    }

    fn snapshot_spectrogram(&mut self) -> bool {
        // Only mutate a uniquely owned slot, after releasing our old handle.
        self.visualizer.spectrogram = SpectrumSnapshot::default();
        let Some(slots) = &mut self.spectrogram_snapshots else {
            return false;
        };
        for slot in slots {
            if let Some(levels) = Arc::get_mut(slot) {
                let n = self.frequency.len();
                for (bin, level) in (1..=n / 2).zip(levels) {
                    // The packed FFT's conjugate pair yields mean channel
                    // power even for opposite-phase stereo. Match the base
                    // spectrum's coherent gain and single-sided Nyquist gain.
                    let endpoint_gain = if bin == n / 2 { 0.25 } else { 1.0 };
                    let pair_power =
                        self.frequency[bin].norm_sqr() + self.frequency[n - bin].norm_sqr();
                    self.bin_powers[bin - 1] = pair_power;
                    let power = pair_power * 0.25 * endpoint_gain;
                    *level = (power.sqrt() * self.normalization).max(1e-7).log10() * 20.0;
                }
                self.visualizer.spectrogram = SpectrumSnapshot::from_shared(Arc::clone(slot));
                return true;
            }
        }
        // Slow consumers retain every slot: omit only the optional full FFT.
        false
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
                self.visualizer.pcm = PcmSnapshot::from_shared(Arc::clone(slot));
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

/// Synchronous worker-owned DSP. No work is performed on display request threads.
/// PCM ingestion is independent of publication cadence and always visits every sample.
pub struct AnalysisEngine {
    analyzer: Option<SpectrumAnalyzer>,
    settings: AnalysisSettings,
    demand: DisplayDemand,
    state: PcmState,
    display_token: u64,
    generation: u64,
    expected: Option<(u64, u64)>,
    sample_time: Duration,
    fresh: bool,
    requested: bool,
    last_frame_at: Option<Instant>,
}
impl AnalysisEngine {
    pub fn new(settings: AnalysisSettings) -> Self {
        assert!(settings.is_valid());
        Self {
            analyzer: None,
            settings,
            demand: DisplayDemand::default(),
            state: PcmState::default(),
            display_token: 0,
            generation: 0,
            expected: None,
            sample_time: Duration::ZERO,
            fresh: false,
            requested: false,
            last_frame_at: None,
        }
    }

    /// Control updates invalidate before admitting any further PCM. Cadence-only
    /// changes preserve partial windows; capture restarts preserve signal continuity.
    pub fn configure(
        &mut self,
        settings: AnalysisSettings,
        demand: DisplayDemand,
        state: PcmState,
        display_token: u64,
        frame: &mut AnalysisFrame,
    ) {
        assert!(settings.is_valid());
        let semantic = !self.settings.signal_compatible_with(settings)
            || self.state.generation != state.generation
            || self.state.sample_rate != state.sample_rate;
        let capture = self.state.epoch != state.epoch
            || self.state.intent != state.intent
            || self.state.applied_intent != state.applied_intent
            || self.state.capture != state.capture
            || self.state.active != state.active
            || self.demand.enabled != demand.enabled;
        let revoked = display_token != self.display_token;
        if semantic {
            self.generation = self.generation.wrapping_add(1);
        }
        if semantic || capture {
            if let Some(analyzer) = &mut self.analyzer {
                analyzer.clear_window();
            }
            self.expected = None;
            self.fresh = false;
        }
        self.settings = settings;
        self.demand = demand;
        self.state = state;
        self.display_token = display_token;
        if let Some(analyzer) = &mut self.analyzer {
            analyzer.set_visualizer_demand(if demand.enabled {
                demand.visualizer
            } else {
                VisualizerDemand::default()
            });
            if analyzer.settings.signal_compatible_with(settings) {
                analyzer.settings = settings;
            }
        }
        if semantic || capture || revoked {
            self.clear_frame(frame);
        }
        if !demand.enabled {
            self.requested = false;
        }
    }

    fn clear_frame(&mut self, frame: &mut AnalysisFrame) {
        if let Some(analyzer) = &mut self.analyzer {
            analyzer.metadata_dirty = true;
        }
        frame.sequence = frame.sequence.wrapping_add(1);
        frame.display_token = self.display_token;
        frame.audio_epoch = self.state.epoch;
        frame.audio_intent = self.state.intent;
        frame.generation = self.generation;
        frame.sample_rate = self.state.sample_rate;
        frame.sample_time = self.sample_time;
        frame.settings = None;
        frame.frequencies_hz.clear();
        frame.spectrum_db.clear();
        frame.rms_left = 0.0;
        frame.rms_right = 0.0;
        let demand = self.demand.visualizer;
        let mut spatial = std::mem::take(&mut frame.visualizer.spatial);
        spatial.clear();
        frame.visualizer = VisualizerFrame {
            demand,
            ..VisualizerFrame::default()
        };
        if demand.spatial_spectrum {
            frame.visualizer.spatial = spatial;
        }
    }

    /// Revoke a local capture without inventing a source discontinuity.
    pub fn restart_capture(&mut self, frame: &mut AnalysisFrame) {
        if let Some(analyzer) = &mut self.analyzer {
            analyzer.clear_window();
        }
        self.expected = None;
        self.fresh = false;
        self.requested = false;
        self.clear_frame(frame);
    }

    pub fn request_frame(&mut self) {
        self.requested |= self.demand.enabled;
    }

    /// Reject retired streams before touching rolling windows or density history.
    pub fn ingest(
        &mut self,
        header: PcmHeader,
        samples: impl ExactSizeIterator<Item = [f32; 2]>,
        frame: &mut AnalysisFrame,
    ) -> bool {
        let count = samples.len();
        if !self.demand.enabled || !self.state.accepts(header) || count == 0 {
            return false;
        }
        let discontinuity = header.gap
            || self.expected.is_some_and(|(sequence, start)| {
                header.sequence != sequence || header.start_frame != start
            });
        if discontinuity {
            self.generation = self.generation.wrapping_add(1);
            if let Some(analyzer) = &mut self.analyzer {
                analyzer.clear_window();
            }
            self.fresh = false;
            self.clear_frame(frame);
        }
        let replan = self.analyzer.as_ref().is_none_or(|analyzer| {
            analyzer.sample_rate != header.sample_rate
                || !analyzer.settings.signal_compatible_with(self.settings)
        });
        if replan {
            self.analyzer = Some(SpectrumAnalyzer::new(self.settings, header.sample_rate));
        }
        let analyzer = self.analyzer.as_mut().unwrap();
        analyzer.settings = self.settings;
        analyzer.generation = self.generation;
        analyzer.set_visualizer_demand(self.demand.visualizer);
        for sample in samples {
            analyzer.push(sample);
        }
        self.expected = Some((
            header.sequence.wrapping_add(1),
            header.start_frame.wrapping_add(count as u64),
        ));
        self.sample_time +=
            Duration::from_secs_f64(count as f64 / header.sample_rate.max(1) as f64);
        self.fresh = true;
        true
    }

    /// A timer exists only for a coalesced permit with a complete fresh window.
    pub fn publication_delay(&self, now: Instant) -> Option<Duration> {
        if !self.requested
            || !self.fresh
            || !self.state.active
            || !self.demand.enabled
            || !self
                .analyzer
                .as_ref()
                .is_some_and(|analyzer| analyzer.filled == analyzer.rolling.len())
        {
            return None;
        }
        let interval = Duration::from_secs_f64(1.0 / self.settings.fps as f64);
        Some(self.last_frame_at.map_or(Duration::ZERO, |last| {
            (last + interval).saturating_duration_since(now)
        }))
    }

    pub fn publish(&mut self, frame: &mut AnalysisFrame, now: Instant) -> bool {
        if self.publication_delay(now) != Some(Duration::ZERO) {
            return false;
        }
        self.requested = false;
        self.fresh = false;
        self.last_frame_at = Some(now);
        let analyzer = self.analyzer.as_mut().unwrap();
        let rms = analyzer.transform();
        analyzer.publish(frame, rms, self.sample_time);
        frame.display_token = self.display_token;
        frame.audio_epoch = self.state.epoch;
        frame.audio_intent = self.state.intent;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivu_format::config::{Config, VisualizationSignal};
    const DENSITY_DEMAND: VisualizerDemand = VisualizerDemand {
        stereo_density: true,
        spatial_spectrum: false,
        band_energy: false,
        pcm: false,
        spectrogram: false,
    };
    const SPATIAL_DEMAND: VisualizerDemand = VisualizerDemand {
        stereo_density: false,
        spatial_spectrum: true,
        band_energy: false,
        pcm: false,
        spectrogram: false,
    };
    const BAND_ENERGY_DEMAND: VisualizerDemand = VisualizerDemand {
        stereo_density: false,
        spatial_spectrum: false,
        band_energy: true,
        pcm: false,
        spectrogram: false,
    };
    const PCM_DEMAND: VisualizerDemand = VisualizerDemand {
        stereo_density: false,
        spatial_spectrum: false,
        band_energy: false,
        pcm: true,
        spectrogram: false,
    };
    const ALL_DEMAND: VisualizerDemand = VisualizerDemand {
        stereo_density: true,
        spatial_spectrum: true,
        band_energy: true,
        pcm: true,
        spectrogram: true,
    };

    const SPECTROGRAM_DEMAND: VisualizerDemand = VisualizerDemand {
        stereo_density: false,
        spatial_spectrum: false,
        band_energy: false,
        pcm: false,
        spectrogram: true,
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
    fn shared_power_preserves_sum_max_and_nyquist_across_demand_changes() {
        let mut analyzer = SpectrumAnalyzer::new(
            AnalysisSettings {
                fft_size: 32768,
                window: AnalysisWindow::None,
                ..visualizer_settings()
            },
            32768,
        );
        let musical_band = analyzer
            .bands
            .iter()
            .position(|&(first, last)| first <= 1000 && 1010 < last)
            .unwrap();
        let demands = [
            ALL_DEMAND,
            BAND_ENERGY_DEMAND,
            VisualizerDemand::default(),
            SPATIAL_DEMAND,
            SPECTROGRAM_DEMAND,
            ALL_DEMAND,
        ];
        let mut retained = Vec::new();
        for (index, demand) in demands.into_iter().enumerate() {
            analyzer.set_visualizer_demand(demand);
            let gain = (index + 1) as f32 / 8.0;
            for i in 0..32768 {
                let phase = std::f32::consts::TAU * i as f32 / 32768.0;
                let nyquist = if i % 2 == 0 { 0.125 } else { -0.125 };
                let sample = gain
                    * (0.125 * (1000.0 * phase).sin() + 0.25 * (1010.0 * phase).sin() + nyquist);
                analyzer.push([sample, -sample]);
            }
            analyzer.transform();
            let expected_peak_db = 20.0 * (gain * 0.25).log10();
            assert!((analyzer.levels[musical_band] - expected_peak_db).abs() < 0.01);
            let expected_nyquist_db = 20.0 * (gain * 0.125).log10();
            assert!((analyzer.levels.last().unwrap() - expected_nyquist_db).abs() < 0.01);
            if demand.band_energy {
                let expected = gain * ((0.125_f32.powi(2) + 0.25_f32.powi(2)) * 0.5).sqrt();
                assert!((analyzer.visualizer.band_rms[1] - expected).abs() < 0.0001);
                assert!((analyzer.visualizer.band_rms[2] - gain * 0.125).abs() < 0.0001);
            } else {
                assert_eq!(analyzer.visualizer.band_rms, [0.0; 3]);
            }
            if demand.spectrogram {
                assert!((analyzer.visualizer.spectrogram[1009] - expected_peak_db).abs() < 0.01);
                assert!(
                    (analyzer.visualizer.spectrogram[16383] - expected_nyquist_db).abs() < 0.01
                );
                retained.push((analyzer.visualizer.spectrogram.clone(), expected_peak_db));
            } else {
                assert!(analyzer.visualizer.spectrogram.is_empty());
            }
            if demand.spatial_spectrum {
                let nyquist = analyzer.visualizer.spatial.last().unwrap();
                assert!((nyquist.power - (gain * 0.125).powi(2)).abs() < 0.0001);
                assert!(nyquist.pan.abs() < 0.0001);
            }
            for (snapshot, expected_db) in &retained {
                assert!((snapshot[1009] - expected_db).abs() < 0.01);
            }
        }
        // Retain the third slot, then change the window while no full-bin
        // publication is available. Band energy must not reuse stale power.
        analyzer.transform();
        assert!(!analyzer.visualizer.spectrogram.is_empty());
        retained.push((
            analyzer.visualizer.spectrogram.clone(),
            20.0 * (0.75_f32 * 0.25).log10(),
        ));
        for _ in 0..32768 {
            analyzer.push([0.0; 2]);
        }
        analyzer.transform();
        assert!(analyzer.visualizer.spectrogram.is_empty());
        assert_eq!(analyzer.visualizer.band_rms, [0.0; 3]);
        assert!(analyzer.levels.iter().all(|db| *db <= -139.0));
        for (snapshot, expected_db) in &retained {
            assert!((snapshot[1009] - expected_db).abs() < 0.01);
        }
    }

    #[test]
    fn spectrogram_resolves_two_tones_inside_one_musical_band() {
        let settings = AnalysisSettings {
            fft_size: 32768,
            window: AnalysisWindow::Hann,
            ..visualizer_settings()
        };
        let mut analyzer = SpectrumAnalyzer::new(settings, 32768);
        analyzer.set_visualizer_demand(SPECTROGRAM_DEMAND);
        let bins = [1000, 1010];
        assert!(
            analyzer
                .bands
                .iter()
                .any(|&(first, last)| { first <= bins[0] && bins[1] < last })
        );
        for i in 0..settings.fft_size {
            let tone = bins
                .iter()
                .map(|bin| {
                    0.25 * (std::f32::consts::TAU * *bin as f32 * i as f32
                        / settings.fft_size as f32)
                        .sin()
                })
                .sum::<f32>();
            analyzer.push([tone, tone]);
        }
        let rms = analyzer.transform();
        let mut frame = AnalysisFrame::default();
        analyzer.publish(&mut frame, rms, Duration::from_secs(1));
        let full = &frame.visualizer.spectrogram;
        assert_eq!(full.len(), 16384);
        let expected_db = 20.0 * 0.25_f32.log10();
        for bin in bins {
            assert!((full[bin - 1] - expected_db).abs() < 0.01);
            assert!(full[bin - 1] > full[bin - 2]);
            assert!(full[bin - 1] > full[bin]);
        }
        assert!(full[1005 - 1] < -80.0);
        assert_eq!(frame.sample_rate, 32768);
        assert_eq!(frame.settings, Some(settings));
    }

    #[test]
    fn spectrogram_calibrates_stereo_phase_windows_and_fft_sizes() {
        for fft_size in [512, 32768] {
            for window in [
                AnalysisWindow::None,
                AnalysisWindow::Hann,
                AnalysisWindow::BlackmanHarris,
            ] {
                for channels in [[1.0, 1.0], [1.0, -1.0], [1.0, 0.0], [0.0, 1.0]] {
                    let mut analyzer = SpectrumAnalyzer::new(
                        AnalysisSettings {
                            fft_size,
                            window,
                            ..visualizer_settings()
                        },
                        48000,
                    );
                    analyzer.set_visualizer_demand(SPECTROGRAM_DEMAND);
                    for i in 0..fft_size {
                        let tone =
                            0.5 * (std::f32::consts::TAU * 32.0 * i as f32 / fft_size as f32).sin();
                        analyzer.push(channels.map(|gain| gain * tone));
                    }
                    analyzer.transform();
                    let mean_power = (channels[0] * channels[0] + channels[1] * channels[1]) * 0.5;
                    let expected_db = 20.0 * (0.5 * mean_power.sqrt()).log10();
                    assert!((analyzer.visualizer.spectrogram[31] - expected_db).abs() < 0.01);
                    assert_eq!(analyzer.visualizer.spectrogram.len(), fft_size as usize / 2);
                }
            }
        }
        let mut analyzer = visualizer_analyzer(SPECTROGRAM_DEMAND);
        for i in 0..512 {
            let sample = if i % 2 == 0 { 0.25 } else { -0.25 };
            analyzer.push([sample, -sample]);
        }
        analyzer.transform();
        assert!((analyzer.visualizer.spectrogram[255] - 20.0 * 0.25_f32.log10()).abs() < 0.01);
        for _ in 0..512 {
            analyzer.push([0.25, -0.25]);
        }
        analyzer.transform();
        assert!(
            analyzer
                .visualizer
                .spectrogram
                .iter()
                .all(|db| *db <= -139.0)
        );
    }

    #[test]
    fn spectrogram_pool_exhaustion_preserves_base_analysis_and_reuses_slots() {
        let mut analyzer = SpectrumAnalyzer::new(
            AnalysisSettings {
                fft_size: 32768,
                ..visualizer_settings()
            },
            32768,
        );
        assert!(analyzer.spectrogram_snapshots.is_none());
        analyzer.set_visualizer_demand(SPECTROGRAM_DEMAND);
        let pointers = analyzer
            .spectrogram_snapshots
            .as_ref()
            .unwrap()
            .each_ref()
            .map(|slot| slot.as_ptr());
        let fft = Arc::clone(&analyzer.fft);
        let frequency = analyzer.frequency.as_ptr();
        let mut frame = AnalysisFrame::default();
        let mut retained = Vec::new();
        for (index, pointer) in pointers.iter().enumerate() {
            let amplitude = 0.125 * (index + 1) as f32;
            for i in 0..32768 {
                let sample = amplitude * (std::f32::consts::TAU * i as f32 / 32.0).sin();
                analyzer.push([sample, -sample]);
            }
            let rms = analyzer.transform();
            analyzer.publish(&mut frame, rms, Duration::from_secs(index as u64 + 1));
            assert_eq!(frame.visualizer.spectrogram.as_ptr(), *pointer);
            let staged = frame.clone();
            assert!(Arc::ptr_eq(
                frame.visualizer.spectrogram.shared_storage().unwrap(),
                staged.visualizer.spectrogram.shared_storage().unwrap()
            ));
            retained.push((staged, 20.0 * amplitude.log10()));
        }
        let previous_sequence = frame.sequence;
        for _ in 0..32768 {
            analyzer.push([0.75, -0.5]);
        }
        let rms = analyzer.transform();
        analyzer.publish(&mut frame, rms, Duration::from_secs(4));
        assert!(frame.visualizer.spectrogram.is_empty());
        assert_eq!(frame.visualizer.demand, SPECTROGRAM_DEMAND);
        assert_eq!(frame.sequence, previous_sequence + 1);
        assert_eq!(frame.sample_time, Duration::from_secs(4));
        assert_eq!(frame.spectrum_db, analyzer.levels);
        assert!(!frame.spectrum_db.is_empty());
        assert_eq!([frame.rms_left, frame.rms_right], [0.75, 0.5]);
        for (snapshot, expected_db) in &retained {
            assert!((snapshot.visualizer.spectrogram[1023] - expected_db).abs() < 0.01);
        }
        let released = retained.remove(0).0.visualizer.spectrogram.as_ptr();
        let rms = analyzer.transform();
        analyzer.publish(&mut frame, rms, Duration::from_secs(5));
        assert_eq!(frame.visualizer.spectrogram.as_ptr(), released);
        assert!(frame.visualizer.spectrogram.iter().all(|db| *db <= -139.0));
        assert_eq!(
            analyzer
                .spectrogram_snapshots
                .as_ref()
                .unwrap()
                .each_ref()
                .map(|slot| slot.as_ptr()),
            pointers
        );
        assert!(Arc::ptr_eq(&analyzer.fft, &fft));
        assert_eq!(analyzer.frequency.as_ptr(), frequency);

        let weak = analyzer
            .spectrogram_snapshots
            .as_ref()
            .unwrap()
            .each_ref()
            .map(Arc::downgrade);
        analyzer.reset();
        assert!(analyzer.visualizer.spectrogram.is_empty());
        assert_eq!(analyzer.filled, 0);
        for (snapshot, expected_db) in &retained {
            assert!((snapshot.visualizer.spectrogram[1023] - expected_db).abs() < 0.01);
        }
        analyzer.set_visualizer_demand(VisualizerDemand::default());
        frame.visualizer.clone_from(&analyzer.visualizer);
        assert!(frame.visualizer.spectrogram.is_empty());
        assert!(analyzer.spectrogram_snapshots.is_none());
        retained.clear();
        assert!(weak.iter().all(|slot| slot.upgrade().is_none()));

        // A staging clone cannot retain data belonging to a disabled style.
        let stale = VisualizerFrame {
            spectrogram: vec![-12.0; 256].into(),
            ..VisualizerFrame::default()
        };
        frame.visualizer.clone_from(&stale);
        assert!(frame.visualizer.spectrogram.is_empty());
        analyzer.set_visualizer_demand(SPECTROGRAM_DEMAND);
        assert_eq!(analyzer.spectrogram_snapshots.as_ref().unwrap().len(), 3);
        assert_eq!(analyzer.filled, 0);
        assert!(analyzer.visualizer.spectrogram.is_empty());
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
            SPECTROGRAM_DEMAND,
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
        assert_eq!(
            frame.visualizer.spectrogram,
            separate[4].visualizer.spectrogram
        );
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
            assert!(analyzer.spectrogram_snapshots.is_none());
            for payload in [&analyzer.visualizer, &frame.visualizer] {
                assert!(payload.density.is_empty());
                assert!(payload.spectrogram.is_empty());
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
        analyzer.reset();
        analyzer.push([0.0; 2]);
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
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod boundary_tests;
