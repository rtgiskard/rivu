//! Linear-phase equalization with a bounded, uniformly partitioned FIR.
//!
//! The desired magnitude is the selected parametric EQ's settled response. A
//! frequency-sampled, Blackman-windowed symmetric impulse approximates that
//! magnitude; it deliberately does not preserve the IIR's nonlinear phase.
//! Preparation owns FFT planning and allocation. Processing uses fixed 256-frame
//! partitions, no allocation, no file access, and no output limiting. The causal
//! delay includes both the symmetric kernel's group delay and one partition of
//! buffering. Offline callers flush zeros, discard `latency_frames()`, and keep
//! their original frame count to obtain source-aligned, zero-extended audio.
use std::{
    f64::consts::PI,
    sync::{Arc, Mutex},
};

use rustfft::{Fft, FftPlanner, num_complex::Complex};

use super::config::{ChannelTarget, EqualizerConfig, FilterKind, MAX_CHANNELS, validate_taps};
use anyhow::{Result, ensure};

/// Causal block buffering included in FIR latency and tail lengths.
pub const PARTITION: usize = 256;
const FFT_SIZE: usize = PARTITION * 2;

pub struct FirEqualizer {
    channels: usize,
    taps: usize,
    bypass: bool,
    poisoned: bool,
    partitions: usize,
    position: usize,
    history_position: usize,
    kernels: Vec<Complex<f32>>,
    channel_kernels: Vec<usize>,
    histories: Vec<Complex<f32>>,
    output: Vec<f32>,
    overlap: Vec<f32>,
    sum: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    forward: Option<Arc<dyn Fft<f32>>>,
    inverse: Option<Arc<dyn Fft<f32>>>,
}

impl FirEqualizer {
    pub fn new(
        config: &EqualizerConfig,
        sample_rate: u32,
        channels: usize,
        taps: usize,
    ) -> Result<Self> {
        validate_taps(taps)?;
        config.validate_for_stream(sample_rate, channels)?;
        ensure!(
            channels <= MAX_CHANNELS,
            "FIR supports at most {MAX_CHANNELS} channels"
        );
        let bypass = !config.enabled;
        let (forward, inverse, scratch_len) = if bypass {
            (None, None, 0)
        } else {
            let mut planner = FftPlanner::new();
            let forward = planner.plan_fft_forward(FFT_SIZE);
            let inverse = planner.plan_fft_inverse(FFT_SIZE);
            let scratch_len = forward
                .get_inplace_scratch_len()
                .max(inverse.get_inplace_scratch_len());
            (Some(forward), Some(inverse), scratch_len)
        };
        let partitions = if bypass { 0 } else { taps.div_ceil(PARTITION) };
        let spectra_len = channels * partitions * FFT_SIZE;
        // Share immutable spectra only for identical ordered effective filters.
        // Histories, overlap, and output remain channel-local.
        let mut representatives = [0; MAX_CHANNELS];
        let mut banks = 0;
        let mut channel_kernels = Vec::with_capacity(if bypass { 0 } else { channels });
        if !bypass {
            for channel in 0..channels {
                let bank = representatives[..banks]
                    .iter()
                    .position(|&other| same_selection(config, channel, other))
                    .unwrap_or_else(|| {
                        representatives[banks] = channel;
                        banks += 1;
                        banks - 1
                    });
                channel_kernels.push(bank);
            }
        }
        let mut equalizer = Self {
            channels,
            taps,
            bypass,
            poisoned: false,
            partitions,
            position: 0,
            history_position: 0,
            kernels: vec![Complex::default(); banks * partitions * FFT_SIZE],
            channel_kernels,
            histories: vec![Complex::default(); spectra_len],
            output: vec![0.0; if bypass { 0 } else { channels * PARTITION }],
            overlap: vec![0.0; if bypass { 0 } else { channels * PARTITION }],
            sum: vec![Complex::default(); if bypass { 0 } else { FFT_SIZE }],
            scratch: vec![Complex::default(); scratch_len],
            forward,
            inverse,
        };
        if !bypass {
            KernelDesigner::with_cached(sample_rate, taps, |designer| -> Result<()> {
                for (bank, &channel) in representatives[..banks].iter().enumerate() {
                    let impulse = designer.design(config, sample_rate, channels, channel)?;
                    let bank_start = bank * partitions * FFT_SIZE;
                    let kernels =
                        &mut equalizer.kernels[bank_start..bank_start + partitions * FFT_SIZE];
                    for (kernel, coefficients) in kernels
                        .as_chunks_mut::<FFT_SIZE>()
                        .0
                        .iter_mut()
                        .zip(impulse.chunks(PARTITION))
                    {
                        // Each kernel starts zeroed, including its FFT padding.
                        for (bin, &value) in kernel.iter_mut().zip(coefficients) {
                            bin.re = value;
                        }
                        equalizer
                            .forward
                            .as_ref()
                            .expect("active FIR has FFT plans")
                            .process_with_scratch(kernel, &mut equalizer.scratch);
                        ensure!(finite_complex(kernel), "FIR kernel FFT overflow");
                    }
                }
                Ok(())
            })?;
        }
        Ok(equalizer)
    }

    /// Causal latency and tail for validated controls, without preparing FFT state.
    pub fn timing_frames(config: &EqualizerConfig, taps: usize) -> (usize, usize) {
        Self::timing(!config.enabled, taps)
    }

    pub fn latency_frames(&self) -> usize {
        Self::timing(self.bypass, self.taps).0
    }

    /// Zeros required after the final input to exhaust the complete causal tail.
    pub fn tail_frames(&self) -> usize {
        Self::timing(self.bypass, self.taps).1
    }

    fn timing(bypass: bool, taps: usize) -> (usize, usize) {
        if bypass {
            (0, 0)
        } else {
            ((taps - 1) / 2 + PARTITION, taps - 1 + PARTITION)
        }
    }

    /// Plot the realized zero-phase magnitude of the kernel `new` prepares for
    /// `channel`, excluding the constant group delay. Unlike the parametric
    /// response this includes truncation and windowing error. Call outside
    /// audio callbacks; work is bounded by `taps * frequencies_hz.len()`.
    #[allow(clippy::too_many_arguments)]
    pub fn sample_response_db(
        config: &EqualizerConfig,
        sample_rate: u32,
        channels: usize,
        channel: usize,
        taps: usize,
        frequencies_hz: &[f64],
        decibels: &mut [f64],
    ) -> Result<()> {
        validate_taps(taps)?;
        config.validate_for_stream(sample_rate, channels)?;
        ensure!(
            frequencies_hz.len() == decibels.len(),
            "FIR response buffers differ in length"
        );
        ensure!(channel < channels, "FIR response channel is out of range");
        let nyquist = f64::from(sample_rate) * 0.5;
        ensure!(
            frequencies_hz
                .iter()
                .all(|frequency| frequency.is_finite() && (0.0..=nyquist).contains(frequency)),
            "FIR response frequency must be between DC and Nyquist"
        );
        if !config.enabled {
            // Disabled FIR processing is a bit-exact bypass, not a unity kernel.
            decibels.fill(0.0);
            return Ok(());
        }
        KernelDesigner::with_cached(sample_rate, taps, |designer| {
            let impulse = designer.design(config, sample_rate, channels, channel)?;
            let center = (taps - 1) / 2;
            for (&frequency, decibel) in frequencies_hz.iter().zip(decibels.iter_mut()) {
                // Symmetric kernel: A(w) = h[c] + 2 sum h[c-k] cos(kw). The
                // Chebyshev recurrence avoids one trigonometric call per tap.
                let omega = 2.0 * PI * frequency / f64::from(sample_rate);
                let twice_cos = 2.0 * omega.cos();
                let (mut previous, mut current) = (1.0, omega.cos());
                let mut amplitude = f64::from(impulse[center]);
                for offset in 1..=center {
                    amplitude += 2.0 * f64::from(impulse[center - offset]) * current;
                    (previous, current) = (current, twice_cos * current - previous);
                }
                *decibel = 20.0 * amplitude.abs().log10();
            }
            Ok(())
        })
    }

    pub fn reset(&mut self) {
        self.poisoned = false;
        self.position = 0;
        self.history_position = 0;
        self.histories.fill(Complex::default());
        self.output.fill(0.0);
        self.overlap.fill(0.0);
    }

    /// Whole frames only. Invalid active input is rejected before mutation.
    /// Disabled EQ preserves every input bit, including non-finite payloads.
    /// Numerical failure may advance a finite prefix and requires reset; no
    /// invalid FFT, convolution, or overlap-add sample is published.
    pub fn process_interleaved(&mut self, samples: &mut [f32]) -> Result<()> {
        ensure!(
            samples.len().is_multiple_of(self.channels),
            "incomplete FIR frame"
        );
        ensure!(!self.poisoned, "FIR numerical failure requires reset");
        if self.bypass {
            return Ok(());
        }
        ensure!(
            samples.iter().all(|value| value.is_finite()),
            "FIR input is not finite"
        );
        for frame in samples.chunks_exact_mut(self.channels) {
            for (channel, value) in frame.iter_mut().enumerate() {
                // The next write slot holds the partition that just aged out.
                let start = (channel * self.partitions + self.history_position) * FFT_SIZE;
                self.histories[start + self.position] = Complex::new(*value, 0.0);
                *value = self.output[channel * PARTITION + self.position];
            }
            self.position += 1;
            if self.position == PARTITION {
                if let Err(error) = self.convolve_partition() {
                    self.poisoned = true;
                    return Err(error);
                }
                self.position = 0;
            }
        }
        Ok(())
    }

    fn convolve_partition(&mut self) -> Result<()> {
        for channel in 0..self.channels {
            let start = (channel * self.partitions + self.history_position) * FFT_SIZE;
            let slot = &mut self.histories[start..start + FFT_SIZE];
            slot[PARTITION..].fill(Complex::default());
            self.forward
                .as_ref()
                .expect("active FIR has FFT plans")
                .process_with_scratch(slot, &mut self.scratch);
            ensure!(finite_complex(slot), "FIR input FFT overflow");
            self.sum.fill(Complex::default());
            for partition in 0..self.partitions {
                let history =
                    (self.history_position + self.partitions - partition) % self.partitions;
                let input = (channel * self.partitions + history) * FFT_SIZE;
                let kernel =
                    (self.channel_kernels[channel] * self.partitions + partition) * FFT_SIZE;
                for ((sum, signal), filter) in self
                    .sum
                    .iter_mut()
                    .zip(&self.histories[input..input + FFT_SIZE])
                    .zip(&self.kernels[kernel..kernel + FFT_SIZE])
                {
                    *sum += *signal * *filter;
                }
            }
            ensure!(finite_complex(&self.sum), "FIR convolution overflow");
            self.inverse
                .as_ref()
                .expect("active FIR has FFT plans")
                .process_with_scratch(&mut self.sum, &mut self.scratch);
            ensure!(finite_complex(&self.sum), "FIR inverse FFT overflow");
            for index in 0..PARTITION {
                let output = channel * PARTITION + index;
                self.output[output] = self.sum[index].re / FFT_SIZE as f32 + self.overlap[output];
                self.overlap[output] = self.sum[index + PARTITION].re / FFT_SIZE as f32;
                ensure!(
                    self.output[output].is_finite() && self.overlap[output].is_finite(),
                    "FIR overlap-add overflow"
                );
            }
        }
        self.history_position = (self.history_position + 1) % self.partitions;
        Ok(())
    }
}

fn finite_complex(values: &[Complex<f32>]) -> bool {
    values
        .iter()
        .all(|value| value.re.is_finite() && value.im.is_finite())
}

fn same_selection(config: &EqualizerConfig, left: usize, right: usize) -> bool {
    let selected = |channel| {
        config
            .effective_bands()
            .map(|(_, band)| band)
            .filter(move |band| match &band.channels {
                ChannelTarget::All => true,
                ChannelTarget::Selected(channels) => channels.contains(&channel),
            })
            .map(|band| {
                let (gain, q, slope) = match band.kind {
                    FilterKind::Peak => (band.gain_db, band.q, 0.0),
                    FilterKind::LowShelf | FilterKind::HighShelf => {
                        (band.gain_db, 0.0, band.shelf_slope)
                    }
                    _ => (0.0, band.q, 0.0),
                };
                (
                    band.kind,
                    band.frequency_hz.to_bits(),
                    gain.to_bits(),
                    q.to_bits(),
                    slope.to_bits(),
                    band.stages,
                )
            })
    };
    selected(left).eq(selected(right))
}

// A checked-out designer is exclusively owned by one construction. Only one
// idle workspace survives, independent of how many workers have constructed EQ.
// Validated taps bound its arrays and the single retained FFT plan; convolution
// histories and the finished kernels remain owned by each FirEqualizer.
static IDLE_KERNEL_DESIGNER: Mutex<Option<KernelDesigner>> = Mutex::new(None);

/// Reusable preparation storage, never used by the audio processing path.
struct KernelDesigner {
    sample_rate: u32,
    frequencies: Vec<f64>,
    decibels: Vec<f64>,
    spectrum: Vec<Complex<f64>>,
    inverse: Arc<dyn Fft<f64>>,
    scratch: Vec<Complex<f64>>,
    window: Vec<f64>,
    impulse: Vec<f32>,
}

impl KernelDesigner {
    fn with_cached<T>(sample_rate: u32, taps: usize, design: impl FnOnce(&mut Self) -> T) -> T {
        // Poisoning disables this optional optimization, not FIR construction.
        // Both planning and design happen after the short checkout lock ends.
        let cached = IDLE_KERNEL_DESIGNER
            .lock()
            .ok()
            .and_then(|mut idle| idle.take());
        let mut designer = if let Some(mut designer) = cached {
            designer.prepare(sample_rate, taps);
            designer
        } else {
            Self::new(sample_rate, taps)
        };
        // A fallible design returns its workspace too. Unwinding instead drops
        // the exclusive workspace without ever poisoning the cache's mutex.
        let result = design(&mut designer);
        if let Ok(mut idle) = IDLE_KERNEL_DESIGNER.lock()
            && idle.is_none()
        {
            *idle = Some(designer);
        }
        // If another constructor filled the slot, drop this designer only after
        // releasing the lock, including its potentially large arrays and plan.
        result
    }

    fn new(sample_rate: u32, taps: usize) -> Self {
        let size = (taps * 8).next_power_of_two();
        let frequencies: Vec<_> = (0..=size / 2)
            .map(|bin| bin as f64 * f64::from(sample_rate) / size as f64)
            .collect();
        let inverse = FftPlanner::new().plan_fft_inverse(size);
        let scratch = vec![Complex::default(); inverse.get_inplace_scratch_len()];
        let window = (0..=(taps - 1) / 2)
            .map(|index| {
                let phase = 2.0 * PI * index as f64 / (taps - 1) as f64;
                0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos()
            })
            .collect();
        Self {
            sample_rate,
            decibels: vec![0.0; frequencies.len()],
            frequencies,
            spectrum: vec![Complex::default(); size],
            inverse,
            scratch,
            window,
            impulse: vec![0.0; taps],
        }
    }

    fn prepare(&mut self, sample_rate: u32, taps: usize) {
        let size = (taps * 8).next_power_of_two();
        if self.spectrum.len() != size {
            // Replace rather than shrink in place: idle retention follows the
            // latest design size, not the largest one ever prepared.
            *self = Self::new(sample_rate, taps);
            return;
        }
        if self.sample_rate != sample_rate {
            for (bin, frequency) in self.frequencies.iter_mut().enumerate() {
                *frequency = bin as f64 * f64::from(sample_rate) / size as f64;
            }
            self.sample_rate = sample_rate;
        }
        if self.impulse.len() != taps {
            self.impulse.resize(taps, 0.0);
            self.window.resize((taps - 1) / 2 + 1, 0.0);
            for (index, window) in self.window.iter_mut().enumerate() {
                let phase = 2.0 * PI * index as f64 / (taps - 1) as f64;
                *window = 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos();
            }
        }
    }

    fn design(
        &mut self,
        config: &EqualizerConfig,
        sample_rate: u32,
        channels: usize,
        channel: usize,
    ) -> Result<&[f32]> {
        config.sample_response_db(
            sample_rate,
            channels,
            channel,
            &self.frequencies,
            &mut self.decibels,
        )?;
        let size = self.spectrum.len();
        // The previous inverse FFT leaves imaginary components as well as real
        // values; each channel starts from the same real, even target spectrum.
        self.spectrum.fill(Complex::default());
        for (bin, &db) in self.decibels.iter().enumerate() {
            let magnitude = 10.0_f64.powf(db / 20.0);
            ensure!(magnitude.is_finite(), "FIR target response is not finite");
            self.spectrum[bin].re = magnitude;
            if bin != 0 && bin != size / 2 {
                self.spectrum[size - bin].re = magnitude;
            }
        }
        self.inverse
            .process_with_scratch(&mut self.spectrum, &mut self.scratch);
        let taps = self.impulse.len();
        let center = (taps - 1) / 2;
        for (index, &window) in self.window.iter().enumerate() {
            let lag = (index + size - center) % size;
            let value = (self.spectrum[lag].re * window / size as f64) as f32;
            ensure!(value.is_finite(), "FIR impulse is not finite");
            self.impulse[index] = value;
            self.impulse[taps - 1 - index] = value;
        }
        Ok(&self.impulse)
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::{ChannelTarget, EqBand};
    use super::*;

    #[test]
    fn realized_response_matches_processed_tones_and_reveals_short_kernel_error() {
        // A narrow notch is the representative case where the finite kernel
        // departs from its parametric target, and more taps track it better.
        let config = EqualizerConfig {
            enabled: true,
            bands: vec![EqBand {
                kind: FilterKind::Notch,
                frequency_hz: 1_000.0,
                q: 8.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let frequencies = [100.0, 900.0, 1_000.0, 4_000.0];
        let mut target = [0.0; 4];
        config
            .sample_response_db(48_000, 1, 0, &frequencies, &mut target)
            .unwrap();
        let depth = |taps| {
            let mut realized = [0.0; 4];
            FirEqualizer::sample_response_db(
                &config,
                48_000,
                1,
                0,
                taps,
                &frequencies,
                &mut realized,
            )
            .unwrap();
            realized
        };
        let (short, long) = (depth(257), depth(8_193));
        assert!(target[2] < -100.0, "IIR notch is a true null");
        assert!(
            short[2] > -6.0,
            "257 taps cannot resolve a Q=8 notch: {short:?}"
        );
        assert!(long[2] < short[2] - 20.0, "{long:?} vs {short:?}");
        assert!(
            (short[0] - target[0]).abs() < 0.1,
            "passband stays on target"
        );

        // Independent evidence: steady-state tone gain through the processor.
        let taps = 1_025;
        let mut realized = [0.0; 4];
        FirEqualizer::sample_response_db(&config, 48_000, 1, 0, taps, &frequencies, &mut realized)
            .unwrap();
        let mut fir = FirEqualizer::new(&config, 48_000, 1, taps).unwrap();
        for (&frequency, &expected) in frequencies.iter().zip(&realized) {
            fir.reset();
            let frames = fir.tail_frames() + 48_000;
            let mut signal: Vec<f32> = (0..frames)
                .map(|i| (2.0 * PI * frequency * i as f64 / 48_000.0).sin() as f32)
                .collect();
            fir.process_interleaved(&mut signal).unwrap();
            let settled = &signal[fir.tail_frames()..];
            let rms = (settled.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>()
                / settled.len() as f64)
                .sqrt();
            let measured = 20.0 * (rms * 2.0_f64.sqrt()).log10();
            assert!(
                (measured - expected).abs() < 0.1,
                "{frequency} Hz: {measured} vs {expected}"
            );
        }

        let mut disabled = [1.0; 4];
        FirEqualizer::sample_response_db(
            &EqualizerConfig::default(),
            48_000,
            1,
            0,
            taps,
            &frequencies,
            &mut disabled,
        )
        .unwrap();
        assert_eq!(disabled, [0.0; 4]);
    }

    #[test]
    fn kernel_design_refreshes_rate_taps_and_response_between_uses() {
        let mut config = EqualizerConfig {
            enabled: true,
            bands: vec![EqBand {
                gain_db: 9.0,
                frequency_hz: 1_700.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut reused = KernelDesigner::new(48_000, 513);
        // The rate-only and 513 -> 769 changes keep the FFT size unchanged.
        // Later transitions shrink and grow the transform as well.
        for (index, (sample_rate, taps)) in [
            (48_000, 513),
            (96_000, 513),
            (96_000, 769),
            (44_100, 769),
            (44_100, 257),
            (48_000, 513),
        ]
        .into_iter()
        .enumerate()
        {
            config.preamp_db = index as f64 - 3.0;
            config.bands[0].gain_db = if index % 2 == 0 { 9.0 } else { -6.0 };
            reused.prepare(sample_rate, taps);
            let mut fresh = KernelDesigner::new(sample_rate, taps);
            let expected = fresh.design(&config, sample_rate, 1, 0).unwrap();
            assert_eq!(
                reused.design(&config, sample_rate, 1, 0).unwrap(),
                expected,
                "{sample_rate} Hz, {taps} taps"
            );
        }
    }

    #[test]
    fn concurrent_constructors_preserve_independent_audio_and_timing() {
        let render = |sample_rate, taps, gain_db| {
            let config = EqualizerConfig {
                enabled: true,
                bands: vec![EqBand {
                    gain_db,
                    frequency_hz: 1_700.0,
                    channels: ChannelTarget::Selected(vec![0]),
                    ..Default::default()
                }],
                ..Default::default()
            };
            let mut fir = FirEqualizer::new(&config, sample_rate, 2, taps).unwrap();
            let mut samples: Vec<_> = (0..fir.tail_frames() + 1024)
                .flat_map(|frame| {
                    let value = if frame < 1024 {
                        (frame as f32 * 0.071).sin()
                    } else {
                        0.0
                    };
                    [value, -value * 0.25]
                })
                .collect();
            fir.process_interleaved(&mut samples).unwrap();
            (fir.latency_frames(), fir.tail_frames(), samples)
        };
        let cases = [
            (48_000, 513, 6.0),
            (96_000, 513, -9.0),
            (44_100, 769, 12.0),
            (48_000, 1025, -6.0),
        ];
        let expected: Vec<_> = cases
            .iter()
            .map(|&(rate, taps, gain)| render(rate, taps, gain))
            .collect();
        let barrier = std::sync::Barrier::new(cases.len());
        std::thread::scope(|scope| {
            for (&(rate, taps, gain), expected) in cases.iter().zip(&expected) {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..2 {
                        assert_eq!(&render(rate, taps, gain), expected);
                    }
                });
            }
        });
    }

    #[test]
    fn unity_has_exact_declared_delay_and_reset_discards_history() {
        let config = EqualizerConfig {
            enabled: true,
            ..Default::default()
        };
        let mut fir = FirEqualizer::new(&config, 48_000, 2, 513).unwrap();
        let delay = fir.latency_frames();
        // Cross ring wraps with different channel data and leave a partial
        // partition staged when resetting.
        let source = |frame: usize| {
            [
                if frame.is_multiple_of(509) { 0.75 } else { 0.0 },
                if frame.is_multiple_of(257) { -0.5 } else { 0.0 },
            ]
        };
        let mut input: Vec<_> = (0..(8 * PARTITION + 17)).flat_map(source).collect();
        fir.process_interleaved(&mut input).unwrap();
        for (frame, pair) in input.as_chunks::<2>().0.iter().enumerate() {
            let expected = frame.checked_sub(delay).map_or([0.0; 2], source);
            assert!((pair[0] - expected[0]).abs() < 2e-6);
            assert!((pair[1] - expected[1]).abs() < 2e-6);
        }
        fir.reset();
        input.fill(0.0);
        fir.process_interleaved(&mut input).unwrap();
        assert!(input.iter().all(|sample| sample.abs() < 1e-7));
    }

    #[test]
    fn actual_impulse_is_symmetric_and_channel_targeted() {
        let config = EqualizerConfig {
            enabled: true,
            bands: vec![EqBand {
                gain_db: 12.0,
                frequency_hz: 2_000.0,
                channels: ChannelTarget::Selected(vec![0]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut fir = FirEqualizer::new(&config, 48_000, 2, 2049).unwrap();
        let mut signal = vec![0.0; (fir.tail_frames() + 2) * 2];
        signal[0] = 1.0;
        signal[1] = 1.0;
        fir.process_interleaved(&mut signal).unwrap();
        for index in 0..2049 {
            assert!(
                (signal[(PARTITION + index) * 2] - signal[(PARTITION + 2048 - index) * 2]).abs()
                    < 2e-6
            );
        }
        let frequency = 2_000.0 / 48_000.0;
        let response: Complex<f64> = signal
            .as_chunks::<2>()
            .0
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                Complex::from_polar(f64::from(frame[0]), -2.0 * PI * frequency * index as f64)
            })
            .sum();
        assert!((20.0 * response.norm().log10() - 12.0).abs() < 0.1);
        assert!((signal[fir.latency_frames() * 2 + 1] - 1.0).abs() < 2e-6);
    }

    fn assert_mono_reference<const CHANNELS: usize>(
        config: &EqualizerConfig,
        channel: usize,
        input: &[f32],
        actual: &[f32],
    ) {
        assert_eq!(input.len(), actual.len());
        assert!(input.len().is_multiple_of(CHANNELS));
        let mut expected: Vec<_> = input
            .iter()
            .skip(channel)
            .step_by(CHANNELS)
            .copied()
            .collect();
        let mut mono = FirEqualizer::new(config, 48_000, 1, 513).unwrap();
        mono.process_interleaved(&mut expected).unwrap();
        for (frame, sample) in actual.as_chunks::<CHANNELS>().0.iter().zip(expected) {
            assert_eq!(frame[channel], sample);
        }
    }
    #[test]
    fn channel_targets_and_global_solo_match_independent_streams() {
        let selected = EqBand {
            solo: true,
            gain_db: 9.0,
            channels: ChannelTarget::Selected(vec![0, 2]),
            ..Default::default()
        };
        let config = EqualizerConfig {
            enabled: true,
            preamp_db: -3.0,
            bands: vec![
                selected.clone(),
                EqBand {
                    gain_db: -12.0,
                    ..Default::default()
                },
                EqBand {
                    enabled: false,
                    solo: true,
                    gain_db: 18.0,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut multi = FirEqualizer::new(&config, 48_000, 3, 513).unwrap();
        let frames = multi.tail_frames() + 1024;
        let input: Vec<_> = (0..frames)
            .flat_map(|i| {
                let x = if i < 1024 {
                    (i as f32 * 0.13).sin()
                } else {
                    0.0
                };
                [x, -x * 0.5, x * 0.25]
            })
            .collect();
        let mut actual = input.clone();
        multi.process_interleaved(&mut actual).unwrap();
        for channel in 0..3 {
            let mut independent = config.clone();
            independent.bands = if channel == 1 {
                Vec::new()
            } else {
                vec![EqBand {
                    channels: ChannelTarget::All,
                    ..selected.clone()
                }]
            };
            assert_mono_reference::<3>(&independent, channel, &input, &actual);
        }
    }

    #[test]
    fn channel_kernel_sharing_ignores_only_kind_irrelevant_controls() {
        for kind in crate::equalizer::FILTER_KINDS {
            let left = EqBand {
                kind,
                gain_db: 6.0,
                channels: ChannelTarget::Selected(vec![0]),
                ..Default::default()
            };
            let mut right = EqBand {
                channels: ChannelTarget::Selected(vec![1]),
                ..left.clone()
            };
            match kind {
                FilterKind::Peak => right.shelf_slope = 0.1,
                FilterKind::LowShelf | FilterKind::HighShelf => right.q = 10.0,
                _ => {
                    right.gain_db = -12.0;
                    right.shelf_slope = 0.1;
                }
            }
            let config = EqualizerConfig {
                enabled: true,
                bands: vec![left, right],
                ..Default::default()
            };
            let mut stereo = FirEqualizer::new(&config, 48_000, 2, 513).unwrap();
            assert_eq!(stereo.channel_kernels, [0, 0], "{kind:?}");
            let input: Vec<_> = (0..2048)
                .flat_map(|i| [(i as f32 * 0.071).sin(), (i as f32 * 0.029).cos()])
                .collect();
            let mut actual = input.clone();
            stereo.process_interleaved(&mut actual).unwrap();
            for channel in 0..2 {
                let mono_config = EqualizerConfig {
                    bands: vec![EqBand {
                        channels: ChannelTarget::All,
                        ..config.bands[channel].clone()
                    }],
                    ..config.clone()
                };
                assert_mono_reference::<2>(&mono_config, channel, &input, &actual);
            }
            let mut different = config;
            different.bands[1].frequency_hz += 1.0;
            let distinct = FirEqualizer::new(&different, 48_000, 2, 513).unwrap();
            assert_eq!(distinct.channel_kernels, [0, 1], "{kind:?}");
        }
    }

    #[test]
    fn matching_channel_filters_preserve_independent_signals_and_reset() {
        let config = EqualizerConfig {
            enabled: true,
            bands: vec![
                EqBand {
                    gain_db: 6.0,
                    channels: ChannelTarget::Selected(vec![0, 1]),
                    ..Default::default()
                },
                EqBand {
                    gain_db: 6.0,
                    channels: ChannelTarget::Selected(vec![2]),
                    ..Default::default()
                },
                EqBand {
                    gain_db: 6.0 + 1e-12,
                    channels: ChannelTarget::Selected(vec![3]),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut multi = FirEqualizer::new(&config, 48_000, 4, 513).unwrap();
        let frames = multi.tail_frames() + 1024;
        let original: Vec<_> = (0..frames)
            .flat_map(|frame| {
                std::array::from_fn::<_, 4, _>(|channel| {
                    if frame < 1024 {
                        (frame as f32 * (0.03 + channel as f32 * 0.017)).sin()
                    } else {
                        0.0
                    }
                })
            })
            .collect();
        let mut actual = original.clone();
        multi.process_interleaved(&mut actual).unwrap();
        for channel in 0..4 {
            let band = config.bands[if channel < 2 { 0 } else { channel - 1 }].clone();
            let mono_config = EqualizerConfig {
                bands: vec![EqBand {
                    channels: ChannelTarget::All,
                    ..band
                }],
                ..config.clone()
            };
            assert_mono_reference::<4>(&mono_config, channel, &original, &actual);
        }
        multi.reset();
        let mut silence = vec![0.0; frames * 4];
        multi.process_interleaved(&mut silence).unwrap();
        assert!(silence.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn chunk_boundaries_do_not_change_audio_and_errors_are_atomic() {
        let config = EqualizerConfig {
            enabled: true,
            preamp_db: 3.0,
            ..Default::default()
        };
        let mut one = FirEqualizer::new(&config, 48_000, 1, 1025).unwrap();
        let mut many = FirEqualizer::new(&config, 48_000, 1, 1025).unwrap();
        let mut input: Vec<_> = (0..4000).map(|i| (i as f32 * 0.071).sin()).collect();
        let mut split = input.clone();
        one.process_interleaved(&mut input).unwrap();
        for chunk in split.chunks_mut(37) {
            many.process_interleaved(chunk).unwrap();
        }
        assert_eq!(input, split);
        let mut invalid = [1.0, f32::NAN];
        assert!(many.process_interleaved(&mut invalid).is_err());
        assert_eq!(invalid[0], 1.0);
        let mut expected: Vec<_> = (0..2048).map(|i| (i as f32 * 0.037).cos()).collect();
        let mut actual = expected.clone();
        one.process_interleaved(&mut expected).unwrap();
        many.process_interleaved(&mut actual).unwrap();
        assert_eq!(actual, expected);
        assert!(FirEqualizer::new(&config, 48_000, 1, 1024).is_err());
    }

    #[test]
    fn disabled_fir_prepares_no_plans_or_convolution_storage_and_preserves_bits() {
        let mut fir = FirEqualizer::new(&EqualizerConfig::default(), 48_000, 2, 513).unwrap();
        assert!(fir.forward.is_none() && fir.inverse.is_none());
        let capacity = fir.kernels.capacity()
            + fir.histories.capacity()
            + fir.output.capacity()
            + fir.overlap.capacity()
            + fir.sum.capacity()
            + fir.scratch.capacity();
        assert_eq!(capacity, 0);
        assert_eq!((fir.latency_frames(), fir.tail_frames()), (0, 0));
        let mut samples = [
            f32::from_bits(0x7fc0_1234),
            f32::NEG_INFINITY,
            -0.0,
            f32::MAX,
        ];
        let bits = samples.map(f32::to_bits);
        fir.process_interleaved(&mut samples).unwrap();
        assert_eq!(samples.map(f32::to_bits), bits);
    }

    #[test]
    fn finite_fft_and_convolution_overflows_require_reset_without_invalid_publication() {
        for (gain, constant) in [(0.0, true), (48.0, false)] {
            let config = EqualizerConfig {
                enabled: true,
                preamp_db: gain / 2.0,
                output_gain_db: gain / 2.0,
                ..Default::default()
            };
            let mut fir = FirEqualizer::new(&config, 48_000, 1, 513).unwrap();
            let mut input = vec![0.0; PARTITION * 2];
            if constant {
                input.fill(f32::MAX);
            } else {
                input[0] = f32::MAX / 4.0;
            }
            assert!(fir.process_interleaved(&mut input).is_err());
            assert!(input.iter().all(|sample| sample.is_finite()));
            let mut next = [0.25; PARTITION];
            assert!(fir.process_interleaved(&mut next).is_err());
            assert_eq!(next, [0.25; PARTITION]);
            fir.reset();
            let mut fresh = FirEqualizer::new(&config, 48_000, 1, 513).unwrap();
            let mut expected = vec![0.125; 2048];
            let mut actual = expected.clone();
            fir.process_interleaved(&mut actual).unwrap();
            fresh.process_interleaved(&mut expected).unwrap();
            assert_eq!(actual, expected);
            if gain > 0.0 {
                assert!(actual.iter().any(|sample| *sample > 1.0));
            }
        }
    }
}
