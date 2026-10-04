use crate::config::{Config, SpectrumWindow};
use parking_lot::RwLock;
use ringbuf::{HeapCons, HeapProd, HeapRb, traits::Consumer};
use rustfft::{Fft, FftPlanner, num_complex::Complex32};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default)]
pub struct AnalysisFrame {
    pub sequence: u64,
    pub sample_rate: u32,
    /// Monotonic amount of accepted audio represented by this frame.
    pub sample_time: Duration,
    pub frequencies_hz: Vec<f32>,
    pub spectrum_db: Vec<f32>,
    pub rms_left: f32,
    pub rms_right: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnalysisSettings {
    pub fps: u32,
    pub fft_size: u32,
    pub window: SpectrumWindow,
    pub bands_per_octave: u32,
}

impl From<&Config> for AnalysisSettings {
    fn from(config: &Config) -> Self {
        Self {
            fps: config.analysis_fps,
            fft_size: config.spectrum_fft_size,
            window: config.spectrum_window,
            bands_per_octave: config.spectrum_bands_per_octave,
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
            && (1..=48).contains(&self.bands_per_octave)
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
        self.control.enabled.store(enabled, Ordering::Release);
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }
    pub fn reset(&self, sample_rate: u32) {
        self.control.epoch.fetch_add(1, Ordering::AcqRel);
        self.control
            .sample_rate
            .store(sample_rate, Ordering::Release);
        self.control.epoch.fetch_add(1, Ordering::AcqRel);
    }
    pub fn configure(&self, settings: AnalysisSettings) {
        if settings.valid() {
            *self.control.settings.write() = settings;
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

fn musical_frequencies(sample_rate: u32, bands_per_octave: u32) -> Vec<f32> {
    let nyquist = sample_rate as f32 * 0.5;
    let mut frequencies = Vec::with_capacity(bands_per_octave as usize * 16);
    if nyquist <= 0.0 {
        return frequencies;
    }
    // Keep A440 exactly on a center for every density, starting near MIDI 0.
    let first = (-69.0 * bands_per_octave as f32 / 12.0).ceil() as i32;
    for band in first.. {
        let center = 440.0 * 2.0_f32.powf(band as f32 / bands_per_octave as f32);
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
    frequencies: Vec<f32>,
    bands: Vec<(usize, usize)>,
    levels: Vec<f32>,
    rolling: Vec<[f32; 2]>,
    head: usize,
    filled: usize,
    metadata_dirty: bool,
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
        let frequencies = musical_frequencies(sample_rate, settings.bands_per_octave);
        let half_band = 2.0_f32.powf(0.5 / settings.bands_per_octave as f32);
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
            levels: vec![-140.0; frequencies.len()],
            frequencies,
            bands,
            rolling: vec![[0.0; 2]; n],
            head: 0,
            filled: 0,
            metadata_dirty: true,
        }
    }

    fn reset(&mut self) {
        self.head = 0;
        self.filled = 0;
    }

    fn push(&mut self, samples: [f32; 2]) {
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
        for i in 0..n {
            let samples = self.rolling[(self.head + i) % n];
            rms[0] += samples[0] * samples[0];
            rms[1] += samples[1] * samples[1];
            // Pack both real channels into one FFT. Recover independent powers
            // below so opposite-phase stereo does not vanish.
            self.frequency[i] =
                Complex32::new(samples[0] * self.window[i], samples[1] * self.window[i]);
        }
        self.fft
            .process_with_scratch(&mut self.frequency, &mut self.scratch);
        for (level, &(first, last)) in self.levels.iter_mut().zip(&self.bands) {
            let mut power = 0.0_f32;
            for bin in first..last {
                let packed = self.frequency[bin];
                let mirrored = self.frequency[(n - bin) % n].conj();
                let left = (packed + mirrored) * 0.5;
                let right = (packed - mirrored) * Complex32::new(0.0, -0.5);
                // Nyquist has no negative-frequency partner to double.
                let endpoint_gain = if bin == n / 2 { 0.25 } else { 1.0 };
                power = power.max((left.norm_sqr() + right.norm_sqr()) * 0.5 * endpoint_gain);
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
        frame.sample_rate = self.sample_rate;
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
    let mut incoming = vec![TapFrame::default(); 32768];
    let mut epoch = 0;
    let mut sample_time = Duration::ZERO;
    loop {
        if control.shutdown.load(Ordering::Acquire) {
            break;
        }
        if !control.enabled.load(Ordering::Acquire) {
            consumer.clear();
            analyzer.reset();
            thread::park();
            continue;
        }
        let cycle_start = Instant::now();
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
            || settings.bands_per_octave != analyzer.settings.bands_per_octave
        {
            analyzer = SpectrumAnalyzer::new(settings, sample_rate);
        }
        if current_epoch != epoch {
            epoch = current_epoch;
            analyzer.reset();
        }
        let mut accepted = 0;
        loop {
            let count = consumer.pop_slice(&mut incoming);
            if count == 0 {
                break;
            }
            for frame in &incoming[..count] {
                if frame.epoch == current_epoch {
                    accepted += 1;
                    analyzer.push_tap(frame);
                }
            }
        }
        sample_time += Duration::from_secs_f64(accepted as f64 / sample_rate.max(1) as f64);
        if accepted != 0 && analyzer.filled == analyzer.rolling.len() {
            let rms = analyzer.transform();
            if control.epoch.load(Ordering::Acquire) == current_epoch {
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
    fn musical_bands_follow_nyquist_and_density() {
        for density in [1, 12, 24, 48] {
            for rate in [48_000, 8_000, 192_000, 44_100] {
                let frequencies = musical_frequencies(rate, density);
                assert_eq!(frequencies.last().copied(), Some(rate as f32 * 0.5));
                assert!(frequencies.windows(2).all(|pair| pair[0] < pair[1]));
                assert!(frequencies.contains(&440.0));
                assert!(
                    frequencies
                        .iter()
                        .all(|hz| *hz > 0.0 && *hz <= rate as f32 * 0.5)
                );
                let a = frequencies.iter().position(|hz| *hz == 440.0).unwrap();
                assert!((frequencies[a + density as usize] - 880.0).abs() < 0.001);
            }
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
                for bands_per_octave in [1, 24, 48] {
                    let settings = AnalysisSettings {
                        fps: 20,
                        fft_size,
                        window,
                        bands_per_octave,
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
                        "size={fft_size} window={window:?} density={bands_per_octave}"
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
                    let tolerance = (rate as f32 / fft_size as f32)
                        .max(1760.0 * (2.0_f32.powf(0.5 / bands_per_octave as f32) - 1.0));
                    assert!((analyzer.frequencies[strongest] - 1760.0).abs() <= tolerance + 1.0);
                }
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
        let mut changed = SpectrumAnalyzer::new(
            AnalysisSettings {
                bands_per_octave: 48,
                ..settings
            },
            8_000,
        );
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
            bands_per_octave: 48,
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
            AnalysisSettings {
                bands_per_octave: 0,
                ..settings
            },
            AnalysisSettings {
                bands_per_octave: 49,
                ..settings
            },
        ] {
            worker.configure(invalid);
            assert_eq!(*worker.control.settings.read(), settings);
        }
        assert!(!worker.control.enabled.load(Ordering::Acquire));
    }
}
