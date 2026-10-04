use parking_lot::RwLock;
use ringbuf::{HeapCons, HeapProd, HeapRb, traits::Consumer};
use rustfft::{FftPlanner, num_complex::Complex32};
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
    pub frequencies_hz: Vec<f32>,
    pub spectrum_db: Vec<f32>,
    pub rms_left: f32,
    pub rms_right: f32,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct TapFrame {
    pub samples: [f32; 2],
    pub epoch: u64,
}

pub(crate) struct AnalysisControl {
    pub enabled: AtomicBool,
    /// Requested analyzer publication rate, in frames per second.
    pub rate: AtomicU32,
    /// Playback/output sample rate used to map FFT bins to frequencies.
    pub sample_rate: AtomicU32,
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
            rate: AtomicU32::new(30),
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
        self.control
            .sample_rate
            .store(sample_rate, Ordering::Release);
        self.control.epoch.fetch_add(1, Ordering::AcqRel);
    }
    pub fn set_rate(&self, rate: u32) {
        if (5..=60).contains(&rate) {
            self.control.rate.store(rate, Ordering::Release);
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

fn musical_frequencies(sample_rate: u32, frequencies: &mut Vec<f32>) {
    frequencies.clear();
    let nyquist = sample_rate as f32 * 0.5;
    if nyquist <= 0.0 {
        return;
    }
    for midi in 0.. {
        let center = 440.0 * 2.0_f32.powf((midi as f32 - 69.0) / 12.0);
        if center >= nyquist {
            break;
        }
        frequencies.push(center);
    }
    frequencies.push(nyquist);
}

fn analyze(
    mut consumer: HeapCons<TapFrame>,
    control: Arc<AnalysisControl>,
    shared: Arc<RwLock<AnalysisFrame>>,
) {
    const N: usize = 8192;
    let fft = FftPlanner::<f32>::new().plan_fft_forward(N);
    let mut scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
    let mut frequency = vec![Complex32::default(); N];
    let hann: Vec<f32> = (0..N)
        .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / N as f32).cos())
        .collect();
    let normalization = 2.0 / hann.iter().sum::<f32>();
    let mut frequencies = Vec::with_capacity(160);
    let mut bands = Vec::new();
    let mut levels = Vec::new();
    let mut rolling = vec![[0.0_f32; 2]; N];
    let mut incoming = vec![TapFrame::default(); N];
    let mut head = 0;
    let mut filled = 0;
    let mut epoch = 0;
    let mut sample_rate = 0;
    loop {
        if control.shutdown.load(Ordering::Acquire) {
            break;
        }
        if !control.enabled.load(Ordering::Acquire) {
            consumer.clear();
            filled = 0;
            head = 0;
            thread::park();
            continue;
        }
        let cycle_start = Instant::now();
        let new_sample_rate = control.sample_rate.load(Ordering::Acquire);
        if new_sample_rate != sample_rate {
            sample_rate = new_sample_rate;
            filled = 0;
            head = 0;
            musical_frequencies(sample_rate, &mut frequencies);
            bands.resize(frequencies.len(), (0, 0));
            levels.resize(frequencies.len(), -70.0);
            for (band, center) in bands.iter_mut().zip(&frequencies) {
                let lo = center * 2.0_f32.powf(-1.0 / 24.0);
                let hi = center * 2.0_f32.powf(1.0 / 24.0);
                let first = (lo * N as f32 / sample_rate as f32).floor().max(1.0) as usize;
                let last = (hi * N as f32 / sample_rate as f32).ceil() as usize;
                *band = (first.min(N / 2), last.max(first + 1).min(N / 2 + 1));
            }
        }
        let analysis_rate = control.rate.load(Ordering::Acquire);
        let mut changed = false;
        loop {
            let count = consumer.pop_slice(&mut incoming);
            if count == 0 {
                break;
            }
            for frame in &incoming[..count] {
                if frame.epoch != epoch {
                    epoch = frame.epoch;
                    filled = 0;
                    head = 0;
                }
                rolling[head] = frame.samples;
                head = (head + 1) % N;
                filled = (filled + 1).min(N);
            }
            changed = true;
        }
        if changed && filled == N {
            let mut rms = [0.0_f32; 2];
            for i in 0..N {
                let samples = rolling[(head + i) % N];
                rms[0] += samples[0] * samples[0];
                rms[1] += samples[1] * samples[1];
                // Pack both real channels into one FFT. Recover their independent
                // powers below so opposite-phase stereo does not vanish.
                frequency[i] = Complex32::new(samples[0] * hann[i], samples[1] * hann[i]);
            }
            fft.process_with_scratch(&mut frequency, &mut scratch);
            for (level, &(first, last)) in levels.iter_mut().zip(&bands) {
                let mut power = 0.0_f32;
                for bin in first..last {
                    let packed = frequency[bin];
                    let mirrored = frequency[(N - bin) % N].conj();
                    let left = (packed + mirrored) * 0.5;
                    let right = (packed - mirrored) * Complex32::new(0.0, -0.5);
                    power = power.max((left.norm_sqr() + right.norm_sqr()) * 0.5);
                }
                *level = (power.sqrt() * normalization).max(1e-7).log10() * 20.0;
            }
            let mut frame = shared.write();
            if frame.sample_rate != sample_rate || frame.frequencies_hz.len() != frequencies.len() {
                frame.frequencies_hz.clone_from(&frequencies);
                frame.spectrum_db.resize(frequencies.len(), -70.0);
                frame.sample_rate = sample_rate;
            }
            frame.spectrum_db.copy_from_slice(&levels);
            frame.rms_left = (rms[0] / N as f32).sqrt();
            frame.rms_right = (rms[1] / N as f32).sqrt();
            frame.sequence = frame.sequence.wrapping_add(1);
        }
        let interval = Duration::from_secs_f64(1.0 / analysis_rate as f64);
        thread::park_timeout(interval.saturating_sub(cycle_start.elapsed()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn musical_bands_follow_nyquist_across_rate_changes() {
        let mut frequencies = Vec::new();
        for rate in [48_000, 8_000, 192_000, 44_100] {
            musical_frequencies(rate, &mut frequencies);
            assert_eq!(frequencies.last().copied(), Some(rate as f32 * 0.5));
            assert!(frequencies.windows(2).all(|pair| pair[0] < pair[1]));
            assert!(frequencies.contains(&440.0));
            assert!(frequencies.iter().all(|hz| *hz > 0.0 && *hz <= rate as f32 * 0.5));
        }
    }

    #[test]
    fn publication_rate_is_validated_and_independent_of_sample_rate() {
        let worker = AnalysisWorker::new(Arc::new(RwLock::new(AnalysisFrame::default()))).unwrap();
        worker.set_rate(5);
        worker.reset(192_000);
        assert_eq!(worker.control.rate.load(Ordering::Acquire), 5);
        assert_eq!(worker.control.sample_rate.load(Ordering::Acquire), 192_000);
        worker.set_rate(60);
        worker.set_rate(0);
        worker.set_rate(61);
        assert_eq!(worker.control.rate.load(Ordering::Acquire), 60);
        assert!(!worker.control.enabled.load(Ordering::Acquire));
    }
}
