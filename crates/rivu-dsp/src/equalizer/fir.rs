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
use std::{f64::consts::PI, sync::Arc};

use rustfft::{Fft, FftPlanner, num_complex::Complex};

use super::config::{EqualizerConfig, MAX_CHANNELS, validate_taps};
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
    histories: Vec<Complex<f32>>,
    input: Vec<f32>,
    output: Vec<f32>,
    overlap: Vec<f32>,
    work: Vec<Complex<f32>>,
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
        let mut equalizer = Self {
            channels,
            taps,
            bypass,
            poisoned: false,
            partitions,
            position: 0,
            history_position: 0,
            kernels: vec![Complex::default(); spectra_len],
            histories: vec![Complex::default(); spectra_len],
            input: vec![0.0; if bypass { 0 } else { channels * PARTITION }],
            output: vec![0.0; if bypass { 0 } else { channels * PARTITION }],
            overlap: vec![0.0; if bypass { 0 } else { channels * PARTITION }],
            work: vec![Complex::default(); if bypass { 0 } else { FFT_SIZE }],
            sum: vec![Complex::default(); if bypass { 0 } else { FFT_SIZE }],
            scratch: vec![Complex::default(); scratch_len],
            forward,
            inverse,
        };
        if !bypass {
            let mut designer = KernelDesigner::new(sample_rate, taps);
            for channel in 0..channels {
                let impulse = designer.design(config, sample_rate, channels, channel)?;
                for partition in 0..partitions {
                    equalizer.work.fill(Complex::default());
                    for (bin, &value) in equalizer
                        .work
                        .iter_mut()
                        .zip(impulse[partition * PARTITION..].iter().take(PARTITION))
                    {
                        bin.re = value;
                    }
                    equalizer
                        .forward
                        .as_ref()
                        .expect("active FIR has FFT plans")
                        .process_with_scratch(&mut equalizer.work, &mut equalizer.scratch);
                    ensure!(finite_complex(&equalizer.work), "FIR kernel FFT overflow");
                    let start = (channel * partitions + partition) * FFT_SIZE;
                    equalizer.kernels[start..start + FFT_SIZE].copy_from_slice(&equalizer.work);
                }
            }
        }
        Ok(equalizer)
    }

    pub fn latency_frames(&self) -> usize {
        if self.bypass {
            0
        } else {
            (self.taps - 1) / 2 + PARTITION
        }
    }

    /// Zeros required after the final input to exhaust the complete causal tail.
    pub fn tail_frames(&self) -> usize {
        if self.bypass {
            0
        } else {
            self.taps - 1 + PARTITION
        }
    }

    pub fn reset(&mut self) {
        self.poisoned = false;
        self.position = 0;
        self.history_position = 0;
        self.histories.fill(Complex::default());
        self.input.fill(0.0);
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
                let index = channel * PARTITION + self.position;
                self.input[index] = *value;
                *value = self.output[index];
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
            self.work.fill(Complex::default());
            for (bin, &sample) in self
                .work
                .iter_mut()
                .zip(&self.input[channel * PARTITION..(channel + 1) * PARTITION])
            {
                bin.re = sample;
            }
            self.forward
                .as_ref()
                .expect("active FIR has FFT plans")
                .process_with_scratch(&mut self.work, &mut self.scratch);
            ensure!(finite_complex(&self.work), "FIR input FFT overflow");
            let start = (channel * self.partitions + self.history_position) * FFT_SIZE;
            self.histories[start..start + FFT_SIZE].copy_from_slice(&self.work);
            self.sum.fill(Complex::default());
            for partition in 0..self.partitions {
                let history =
                    (self.history_position + self.partitions - partition) % self.partitions;
                let input = (channel * self.partitions + history) * FFT_SIZE;
                let kernel = (channel * self.partitions + partition) * FFT_SIZE;
                for ((sum, signal), filter) in self
                    .sum
                    .iter_mut()
                    .zip(&self.histories[input..input + FFT_SIZE])
                    .zip(&self.kernels[kernel..kernel + FFT_SIZE])
                {
                    *sum += *signal * *filter;
                    ensure!(
                        sum.re.is_finite() && sum.im.is_finite(),
                        "FIR convolution overflow"
                    );
                }
            }
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

/// Shared only by the channels of one construction, then released before use.
struct KernelDesigner {
    frequencies: Vec<f64>,
    decibels: Vec<f64>,
    spectrum: Vec<Complex<f64>>,
    inverse: Arc<dyn Fft<f64>>,
    scratch: Vec<Complex<f64>>,
    window: Vec<f64>,
    impulse: Vec<f32>,
}

impl KernelDesigner {
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
            decibels: vec![0.0; frequencies.len()],
            frequencies,
            spectrum: vec![Complex::default(); size],
            inverse,
            scratch,
            window,
            impulse: vec![0.0; taps],
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
    fn unity_has_exact_declared_delay_and_reset_discards_history() {
        let config = EqualizerConfig {
            enabled: true,
            ..Default::default()
        };
        let mut fir = FirEqualizer::new(&config, 48_000, 2, 513).unwrap();
        let delay = fir.latency_frames();
        let mut input = vec![0.0; (fir.tail_frames() + 4) * 2];
        input[0] = 0.75;
        input[1] = -0.5;
        fir.process_interleaved(&mut input).unwrap();
        for (frame, pair) in input.as_chunks::<2>().0.iter().enumerate() {
            let expected = if frame == delay {
                [0.75, -0.5]
            } else {
                [0.0, 0.0]
            };
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
            .map(|i| {
                if i < 1024 {
                    (i as f32 * 0.13).sin()
                } else {
                    0.0
                }
            })
            .collect();
        let mut actual: Vec<_> = input.iter().flat_map(|&x| [x; 3]).collect();
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
            let mut mono = FirEqualizer::new(&independent, 48_000, 1, 513).unwrap();
            let mut expected = input.clone();
            mono.process_interleaved(&mut expected).unwrap();
            for (frame, expected) in actual.as_chunks::<3>().0.iter().zip(expected) {
                assert_eq!(frame[channel], expected);
            }
        }
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
        assert!(FirEqualizer::new(&config, 48_000, 1, 1024).is_err());
    }

    #[test]
    fn disabled_fir_prepares_no_plans_or_convolution_storage_and_preserves_bits() {
        let mut fir = FirEqualizer::new(&EqualizerConfig::default(), 48_000, 2, 513).unwrap();
        assert!(fir.forward.is_none() && fir.inverse.is_none());
        let capacity = fir.kernels.capacity()
            + fir.histories.capacity()
            + fir.input.capacity()
            + fir.output.capacity()
            + fir.overlap.capacity()
            + fir.work.capacity()
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
