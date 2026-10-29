use std::collections::VecDeque;
use std::f32::consts::PI;
use std::sync::Arc;

use anyhow::bail;
use rustfft::{Fft, FftPlanner, num_complex::Complex32};

use super::*;

const MIN_POWER: f32 = 1.0e-30;

fn window(size: usize) -> Vec<f32> {
    (0..size)
        .map(|i| (0.5 - 0.5 * (2.0 * PI * i as f32 / size as f32).cos()).sqrt())
        .collect()
}

fn validate_block(input: &[f32], channels: usize) -> Result<usize> {
    ensure!(
        input.len().is_multiple_of(channels),
        "partial interleaved audio frame"
    );
    let frames = input.len() / channels;
    ensure!(
        frames <= MAX_BLOCK_FRAMES,
        "audio block exceeds {MAX_BLOCK_FRAMES} frames"
    );
    ensure!(
        input.iter().all(|x| x.is_finite()),
        "audio block contains non-finite samples"
    );
    Ok(frames)
}

/// Incremental selected-segment capture. max_frames is a required explicit upper
/// bound; crossing it errors before consuming the block. No full-clip retention.
pub struct ProfileCollector {
    sample_rate: u32,
    channels: usize,
    size: usize,
    max_frames: u64,
    captured_frames: u64,
    input: Vec<f32>,
    filled: usize,
    window: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    spectrum: Vec<Complex32>,
    scratch: Vec<Complex32>,
    power: Vec<f64>,
    observations: usize,
    poisoned: bool,
}

impl ProfileCollector {
    pub fn new(
        sample_rate: u32,
        channels: usize,
        fft_size: usize,
        max_frames: u64,
    ) -> Result<Self> {
        validate_format(sample_rate, channels, fft_size)?;
        ensure!(
            max_frames > 0,
            "profile capture needs an explicit nonzero frame bound"
        );
        let fft = FftPlanner::new().plan_fft_forward(fft_size);
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        Ok(Self {
            sample_rate,
            channels,
            size: fft_size,
            max_frames,
            captured_frames: 0,
            input: vec![0.0; fft_size * channels],
            filled: 0,
            window: window(fft_size),
            fft,
            spectrum: vec![Complex32::default(); fft_size],
            scratch,
            power: vec![0.0; fft_size / 2 + 1],
            observations: 0,
            poisoned: false,
        })
    }

    pub fn push_block(&mut self, input: &[f32], cancel: &mut impl FnMut() -> bool) -> Result<()> {
        ensure!(!self.poisoned, "noise profile failure requires reset");
        let frames = validate_block(input, self.channels)?;
        ensure!(
            frames as u64 <= self.max_frames - self.captured_frames,
            "selected noise segment exceeds its explicit frame bound"
        );
        if cancel() {
            self.poisoned = true;
            bail!("noise profile capture cancelled");
        }
        for frame in input.chunks_exact(self.channels) {
            let start = self.filled * self.channels;
            self.input[start..start + self.channels].copy_from_slice(frame);
            self.filled += 1;
            self.captured_frames += 1;
            if self.filled == self.size {
                if cancel() {
                    self.poisoned = true;
                    bail!("noise profile capture cancelled");
                }
                self.observe(self.size)?;
                let hop = self.size / 2;
                self.input.copy_within(hop * self.channels.., 0);
                self.filled -= hop;
            }
        }
        Ok(())
    }

    fn observe(&mut self, valid: usize) -> Result<()> {
        // Center partial selections in the window; normalize using only actual
        // selected samples, never the zero-padding support.
        let offset = (self.size - valid) / 2;
        let energy: f32 = self.window[offset..offset + valid]
            .iter()
            .map(|x| x * x)
            .sum();
        if energy <= MIN_POWER {
            return Ok(());
        }
        self.poisoned = true;
        let scale = (self.size as f32 / 2.0) / energy / self.channels as f32;
        for ch in 0..self.channels {
            for i in 0..self.size {
                self.spectrum[i] = Complex32::new(
                    if (offset..offset + valid).contains(&i) {
                        self.input[(i - offset) * self.channels + ch] * self.window[i]
                    } else {
                        0.0
                    },
                    0.0,
                );
            }
            self.fft
                .process_with_scratch(&mut self.spectrum, &mut self.scratch);
            ensure!(
                self.spectrum
                    .iter()
                    .all(|bin| bin.re.is_finite() && bin.im.is_finite()),
                "noise profile FFT overflow"
            );
            for (sum, bin) in self.power.iter_mut().zip(&self.spectrum) {
                let power = bin.norm_sqr() * scale;
                ensure!(power.is_finite(), "noise profile power overflow");
                *sum += f64::from(power);
                ensure!(sum.is_finite(), "noise profile accumulation overflow");
            }
        }
        self.observations += 1;
        self.poisoned = false;
        Ok(())
    }

    pub fn finish(mut self) -> Result<NoiseProfile> {
        ensure!(!self.poisoned, "failed noise profile cannot be published");
        ensure!(
            self.captured_frames > 0,
            "select a nonempty noise-only segment first"
        );
        if self.observations == 0 || self.filled > self.size / 2 {
            self.observe(self.filled)?;
        }
        ensure!(
            self.observations > 0,
            "noise segment has no measurable window support"
        );
        let profile = NoiseProfile {
            sample_rate: self.sample_rate,
            channels: self.channels,
            fft_size: self.size,
            captured_frames: self.captured_frames,
            power: self
                .power
                .iter()
                .map(|x| (*x / self.observations as f64) as f32)
                .collect(),
        };
        profile.validate(self.sample_rate, self.channels, self.size)?;
        Ok(profile)
    }

    pub fn reset(&mut self) {
        self.input.fill(0.0);
        self.power.fill(0.0);
        self.filled = 0;
        self.captured_frames = 0;
        self.observations = 0;
        self.poisoned = false;
    }
}

struct HumNotch {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    state: Vec<[f64; 2]>,
}

impl HumNotch {
    fn new(rate: u32, channels: usize, hz: f32, q: f32) -> Self {
        let omega = 2.0 * std::f64::consts::PI * f64::from(hz) / f64::from(rate);
        let alpha = omega.sin() / (2.0 * f64::from(q));
        let a0 = 1.0 + alpha;
        Self {
            b0: 1.0 / a0,
            b1: -2.0 * omega.cos() / a0,
            b2: 1.0 / a0,
            a1: -2.0 * omega.cos() / a0,
            a2: (1.0 - alpha) / a0,
            state: vec![[0.0; 2]; channels],
        }
    }

    fn process(&mut self, ch: usize, input: f32) -> f32 {
        let x = f64::from(input);
        let state = &mut self.state[ch];
        let y = self.b0 * x + state[0];
        state[0] = self.b1 * x - self.a1 * y + state[1];
        state[1] = self.b2 * x - self.a2 * y;
        y as f32
    }
}

/// Bounded centered context. Repaired runs replace their pending samples, so a
/// later center sees repaired left history rather than repeatedly detecting them.
struct ImpulseRepair {
    config: ImpulseConfig,
    channels: usize,
    radius: usize,
    floor: f32,
    data: Vec<f32>,
    filled: usize,
    processed: u64,
    frame: Vec<f32>,
}

impl ImpulseRepair {
    fn new(config: ImpulseConfig, channels: usize) -> Self {
        let radius = config.max_width + 2;
        let floor = 10.0_f32.powf(config.floor_dbfs / 20.0);
        Self {
            config,
            channels,
            radius,
            floor,
            data: vec![0.0; (2 * radius + 1) * channels],
            filled: radius,
            processed: 0,
            frame: vec![0.0; channels],
        }
    }

    fn push(&mut self, frame: &[f32], final_frames: Option<u64>) -> Result<bool> {
        let c = self.channels;
        self.data[self.filled * c..(self.filled + 1) * c].copy_from_slice(frame);
        self.filled += 1;
        if self.filled < 2 * self.radius + 1 {
            return Ok(false);
        }
        let interior = self.processed >= self.radius as u64
            && final_frames.is_none_or(|total| self.processed + (self.radius as u64) < total);
        if interior {
            self.repair()?;
        }
        self.frame
            .copy_from_slice(&self.data[self.radius * c..(self.radius + 1) * c]);
        self.data.copy_within(c.., 0);
        self.filled -= 1;
        self.processed += 1;
        Ok(true)
    }

    fn repair(&mut self) -> Result<()> {
        let r = self.radius;
        let c = self.channels;
        let floor = self.floor;
        for ch in 0..c {
            let left = self.data[(r - 1) * c + ch];
            let center = self.data[r * c + ch];
            let mut differences = [0.0_f32; 32];
            let mut count = 0;
            for i in 1..r {
                differences[count] = (self.data[i * c + ch] - self.data[(i - 1) * c + ch]).abs();
                count += 1;
            }
            for i in r + self.config.max_width + 1..2 * r + 1 {
                differences[count] = (self.data[i * c + ch] - self.data[(i - 1) * c + ch]).abs();
                count += 1;
            }
            ensure!(
                differences[..count].iter().all(|value| value.is_finite()),
                "denoise repair difference overflow"
            );
            differences[..count].sort_unstable_by(f32::total_cmp);
            let scale = differences[count / 2].max(floor);
            let jump = (center - left).abs();
            ensure!(
                jump.is_finite() && (self.config.threshold * scale).is_finite(),
                "denoise repair threshold overflow"
            );
            if jump <= self.config.threshold * scale {
                continue;
            }
            for width in 1..=self.config.max_width {
                let right = self.data[(r + width) * c + ch];
                let allowed_drift =
                    self.config.threshold * scale + (1.0 - self.config.transient_protection) * jump;
                ensure!(
                    allowed_drift.is_finite() && (right - left).is_finite(),
                    "denoise repair drift overflow"
                );
                if (right - left).abs() > allowed_drift {
                    continue;
                }
                // A sustained step or broad onset does not return to its local
                // baseline within max_width and therefore cannot enter here.
                let following = self.data[(r + width + 1) * c + ch];
                ensure!(
                    (following - right).is_finite(),
                    "denoise repair context overflow"
                );
                if (following - right).abs() > self.config.threshold * scale {
                    continue;
                }
                for i in 0..width {
                    let t = (i + 1) as f32 / (width + 1) as f32;
                    self.data[(r + i) * c + ch] = left + t * (right - left);
                    ensure!(
                        self.data[(r + i) * c + ch].is_finite(),
                        "denoise repair interpolation overflow"
                    );
                }
                break;
            }
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.data.fill(0.0);
        self.frame.fill(0.0);
        self.filled = self.radius;
        self.processed = 0;
    }
}

/// Streaming finite-file denoiser. `process_block` appends zero or more frames;
/// output is delayed until sufficient analysis/repair lookahead is available.
/// Call `finish` once at EOF, then reset before another source or seek.
pub struct Denoiser {
    config: DenoiseConfig,
    sample_rate: u32,
    channels: usize,
    size: usize,
    hop: usize,
    bypass: bool,
    received: u64,
    emitted: u64,
    finished: bool,
    poisoned: bool,
    dry: VecDeque<f32>,
    hum: Vec<HumNotch>,
    impulse: Option<ImpulseRepair>,
    frame: Vec<f32>,
    input: Vec<f32>,
    filled: usize,
    frame_start: i64,
    window: Vec<f32>,
    forward: Option<Arc<dyn Fft<f32>>>,
    inverse: Option<Arc<dyn Fft<f32>>>,
    spectra: Vec<Complex32>,
    scratch: Vec<Complex32>,
    overlap: Vec<f32>,
    weights: Vec<f32>,
    power: Vec<f32>,
    smoothed_power: Vec<f32>,
    noise: Vec<f32>,
    initial_noise: Vec<f32>,
    local_energy: Vec<f32>,
    minimum: Vec<f32>,
    temporary_minimum: Vec<f32>,
    presence: Vec<f32>,
    previous_power: Vec<f32>,
    previous_clean: Vec<f32>,
    gain: Vec<f32>,
    target_gain: Vec<f32>,
    analyzed: usize,
    minimum_period: usize,
    attack: f32,
    release: f32,
    floor: f32,
    adaptive_smoothing: f32,
    adaptive_base: f32,
}

impl Denoiser {
    pub fn new(
        sample_rate: u32,
        channels: usize,
        config: DenoiseConfig,
        profile: Option<NoiseProfile>,
    ) -> Result<Self> {
        config.validate_for_format(sample_rate, channels)?;
        if config.mode == DenoiseMode::Profile {
            let Some(profile) = &profile else {
                bail!("profile denoise requires an explicitly captured noise segment");
            };
            profile.validate(sample_rate, channels, config.fft_size)?;
        }
        let size = config.fft_size;
        let hop = size / 4;
        let bypass = config.wet == 0.0
            || (config.mode == DenoiseMode::Off && !config.hum.enabled && !config.impulse.enabled);
        let spectral = !bypass && config.mode != DenoiseMode::Off;
        let spectral_size = if spectral { size } else { 0 };
        let bins = if spectral { size / 2 + 1 } else { 0 };
        let (forward, inverse, scratch_len) = if spectral {
            let mut planner = FftPlanner::new();
            let forward = planner.plan_fft_forward(size);
            let inverse = planner.plan_fft_inverse(size);
            let scratch_len = forward
                .get_inplace_scratch_len()
                .max(inverse.get_inplace_scratch_len());
            (Some(forward), Some(inverse), scratch_len)
        } else {
            (None, None, 0)
        };
        let initial_noise = if !spectral {
            Vec::new()
        } else if config.mode == DenoiseMode::Profile {
            profile.expect("profile validated above").power
        } else {
            let stationary_power =
                10.0_f32.powf(config.stationary_noise_dbfs / 10.0) * size as f32 / 2.0;
            vec![stationary_power; bins]
        };
        let mut hum = Vec::with_capacity(if !bypass && config.hum.enabled {
            config.hum.harmonics
        } else {
            0
        });
        if !bypass && config.hum.enabled {
            for harmonic in 1..=config.hum.harmonics {
                hum.push(HumNotch::new(
                    sample_rate,
                    channels,
                    config.hum.fundamental_hz * harmonic as f32,
                    config.hum.q,
                ));
            }
        }
        let impulse = (!bypass && config.impulse.enabled)
            .then(|| ImpulseRepair::new(config.impulse.clone(), channels));
        let coefficient = |ms: f32| {
            if !spectral || ms == 0.0 {
                0.0
            } else {
                (-(hop as f32) / (sample_rate as f32 * ms * 0.001)).exp()
            }
        };
        let attack = coefficient(config.attack_ms);
        let release = coefficient(config.release_ms);
        let (floor, adaptive_smoothing, adaptive_base) = if spectral {
            (
                10.0_f32.powf(config.noise_floor_db / 20.0),
                (-(hop as f32) / (0.08 * sample_rate as f32)).exp(),
                (-(hop as f32) / (0.25 * sample_rate as f32)).exp(),
            )
        } else {
            (0.0, 0.0, 0.0)
        };
        // Retain dry samples only when a partial blend actually has lookahead.
        let delay = spectral_size + impulse.as_ref().map_or(0, |repair| repair.radius);
        let dry_capacity = if !bypass && config.wet < 1.0 && delay > 0 {
            (delay + 1) * channels
        } else {
            0
        };
        Ok(Self {
            config,
            sample_rate,
            channels,
            size,
            hop,
            bypass,
            received: 0,
            emitted: 0,
            finished: false,
            poisoned: false,
            dry: VecDeque::with_capacity(dry_capacity),
            hum,
            impulse,
            frame: vec![0.0; if bypass { 0 } else { channels }],
            input: vec![0.0; spectral_size * channels],
            filled: spectral_size / 2,
            frame_start: -(size as i64 / 2),
            window: if spectral { window(size) } else { Vec::new() },
            forward,
            inverse,
            spectra: vec![Complex32::default(); spectral_size * channels],
            scratch: vec![Complex32::default(); scratch_len],
            overlap: vec![0.0; spectral_size * channels],
            weights: vec![0.0; spectral_size],
            power: vec![0.0; bins],
            smoothed_power: vec![0.0; bins],
            noise: initial_noise.clone(),
            initial_noise,
            local_energy: vec![0.0; bins],
            minimum: vec![0.0; bins],
            temporary_minimum: vec![0.0; bins],
            presence: vec![0.0; bins],
            previous_power: vec![0.0; bins],
            previous_clean: vec![0.0; bins],
            gain: vec![1.0; bins],
            target_gain: vec![1.0; bins],
            analyzed: 0,
            minimum_period: ((sample_rate as usize) / hop).max(1),
            attack,
            release,
            floor,
            adaptive_smoothing,
            adaptive_base,
        })
    }

    pub fn config(&self) -> &DenoiseConfig {
        &self.config
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn channels(&self) -> usize {
        self.channels
    }
    pub fn input_frames(&self) -> u64 {
        self.received
    }
    pub fn output_frames(&self) -> u64 {
        self.emitted
    }
    /// Maximum withheld source frames before EOF (not a playback delay).
    pub fn lookahead_frames(&self) -> usize {
        if self.bypass {
            0
        } else {
            (if self.config.mode == DenoiseMode::Off {
                0
            } else {
                self.size
            }) + self.impulse.as_ref().map_or(0, |repair| repair.radius)
        }
    }

    fn check_cancel(&mut self, cancel: &mut impl FnMut() -> bool) -> Result<()> {
        if cancel() {
            self.poisoned = true;
            bail!("denoise job cancelled; discard partial output");
        }
        Ok(())
    }

    pub fn process_block(
        &mut self,
        input: &[f32],
        output: &mut Vec<f32>,
        cancel: &mut impl FnMut() -> bool,
    ) -> Result<()> {
        ensure!(
            !self.finished && !self.poisoned,
            "denoise job must be reset before reuse"
        );
        let frames = validate_block(input, self.channels)?;
        ensure!(
            self.received <= (i64::MAX as u64) - frames as u64 - self.size as u64,
            "audio frame counter overflow"
        );
        self.check_cancel(cancel)?;
        if self.bypass {
            output.extend_from_slice(input);
            self.received += frames as u64;
            self.emitted += frames as u64;
            return Ok(());
        }
        // Any failure after consuming source invalidates the timeline until reset.
        self.poisoned = true;
        for (index, frame) in input.chunks_exact(self.channels).enumerate() {
            if index % self.hop == 0 {
                self.check_cancel(cancel)?;
            }
            if self.dry.capacity() > 0 {
                self.dry.extend(frame.iter().copied());
            }
            self.received += 1;
            if let Some(repair) = &mut self.impulse {
                if !repair.push(frame, None)? {
                    continue;
                }
                self.frame.copy_from_slice(&repair.frame);
            } else {
                self.frame.copy_from_slice(frame);
            }
            self.push_prepared(output, cancel, self.impulse.is_none().then_some(frame))?;
        }
        self.poisoned = false;
        Ok(())
    }

    fn emit(
        &mut self,
        wet_frame: usize,
        output: &mut Vec<f32>,
        immediate_dry: Option<&[f32]>,
    ) -> Result<()> {
        let wet = self.config.wet;
        let mut frame = [0.0; MAX_CHANNELS];
        for (ch, sample) in frame[..self.channels].iter_mut().enumerate() {
            let processed = if self.config.mode == DenoiseMode::Off {
                self.frame[ch]
            } else {
                self.overlap[wet_frame * self.channels + ch] / self.weights[wet_frame]
            };
            ensure!(processed.is_finite(), "denoise output overflow");
            *sample = if wet == 1.0 {
                processed
            } else {
                let dry = if let Some(dry) = immediate_dry {
                    dry[ch]
                } else {
                    self.dry
                        .pop_front()
                        .expect("source precedes aligned output")
                };
                // A convex blend must not overflow merely because its f32
                // difference would. This does not limit either signal.
                (f64::from(dry) * (1.0 - f64::from(wet)) + f64::from(processed) * f64::from(wet))
                    as f32
            };
            ensure!(sample.is_finite(), "denoise wet blend overflow");
        }
        output.extend_from_slice(&frame[..self.channels]);
        self.emitted += 1;
        Ok(())
    }

    fn push_prepared(
        &mut self,
        output: &mut Vec<f32>,
        cancel: &mut impl FnMut() -> bool,
        immediate_dry: Option<&[f32]>,
    ) -> Result<()> {
        for ch in 0..self.channels {
            for notch in &mut self.hum {
                self.frame[ch] = notch.process(ch, self.frame[ch]);
                ensure!(self.frame[ch].is_finite(), "denoise hum filter overflow");
            }
        }
        ensure!(
            self.frame.iter().all(|sample| sample.is_finite()),
            "denoise repair overflow"
        );
        if self.config.mode == DenoiseMode::Off {
            self.emit(0, output, immediate_dry)?;
            return Ok(());
        }
        self.input[self.filled * self.channels..(self.filled + 1) * self.channels]
            .copy_from_slice(&self.frame);
        self.filled += 1;
        if self.filled == self.size {
            self.analyze(output, cancel)?;
        }
        Ok(())
    }

    fn analyze(&mut self, output: &mut Vec<f32>, cancel: &mut impl FnMut() -> bool) -> Result<()> {
        self.check_cancel(cancel)?;
        self.power.fill(0.0);
        for ch in 0..self.channels {
            let spectrum = &mut self.spectra[ch * self.size..(ch + 1) * self.size];
            for (i, bin) in spectrum.iter_mut().enumerate() {
                *bin = Complex32::new(self.input[i * self.channels + ch] * self.window[i], 0.0);
            }
            self.forward
                .as_ref()
                .expect("spectral denoise has FFT plans")
                .process_with_scratch(spectrum, &mut self.scratch);
            ensure!(
                spectrum
                    .iter()
                    .all(|bin| bin.re.is_finite() && bin.im.is_finite()),
                "denoise FFT overflow"
            );
            for (power, bin) in self.power.iter_mut().zip(spectrum.iter()) {
                *power += bin.norm_sqr() / self.channels as f32;
                ensure!(power.is_finite(), "denoise spectral power overflow");
            }
        }
        self.estimate_gains()?;
        for ch in 0..self.channels {
            let spectrum = &mut self.spectra[ch * self.size..(ch + 1) * self.size];
            for (bin, value) in spectrum.iter_mut().enumerate() {
                *value *= self.gain[bin.min(self.size - bin)];
                ensure!(
                    value.re.is_finite() && value.im.is_finite(),
                    "denoise spectral gain overflow"
                );
            }
            self.inverse
                .as_ref()
                .expect("spectral denoise has FFT plans")
                .process_with_scratch(spectrum, &mut self.scratch);
            ensure!(
                spectrum
                    .iter()
                    .all(|bin| bin.re.is_finite() && bin.im.is_finite()),
                "denoise inverse FFT overflow"
            );
            for (i, bin) in spectrum.iter().enumerate() {
                self.overlap[i * self.channels + ch] += bin.re * self.window[i] / self.size as f32;
                ensure!(
                    self.overlap[i * self.channels + ch].is_finite(),
                    "denoise overlap-add overflow"
                );
            }
        }
        for (weight, window) in self.weights.iter_mut().zip(&self.window) {
            *weight += window * window;
        }
        for i in 0..self.hop {
            let position = self.frame_start + i as i64;
            if position >= 0 && (position as u64) < self.received && self.emitted < self.received {
                self.emit(i, output, None)?;
            }
        }
        let samples = self.hop * self.channels;
        self.input.copy_within(samples.., 0);
        self.filled -= self.hop;
        self.overlap.copy_within(samples.., 0);
        self.overlap[(self.size - self.hop) * self.channels..].fill(0.0);
        self.weights.copy_within(self.hop.., 0);
        self.weights[self.size - self.hop..].fill(0.0);
        self.frame_start += self.hop as i64;
        self.analyzed += 1;
        Ok(())
    }

    fn estimate_gains(&mut self) -> Result<()> {
        let bins = self.power.len();
        // Frequency-smoothed power is used for adaptation and flux only. Gain
        // estimation keeps narrow strong harmonics from being diluted first.
        for k in 0..bins {
            let lo = k.saturating_sub(1);
            let hi = (k + 1).min(bins - 1);
            self.smoothed_power[k] = self.power[lo..=hi].iter().sum::<f32>() / (hi - lo + 1) as f32;
            ensure!(
                self.smoothed_power[k].is_finite(),
                "denoise power smoothing overflow"
            );
        }
        let first = self.analyzed == 0;
        if self.config.mode == DenoiseMode::Adaptive {
            let smoothing = self.adaptive_smoothing;
            let base = self.adaptive_base;
            let rotate_minimum = self.analyzed.is_multiple_of(self.minimum_period);
            for k in 0..bins {
                let p = self.smoothed_power[k].max(MIN_POWER);
                let mut background = 0.0;
                let mut neighbors = 0;
                for j in k.saturating_sub(8)..=(k + 8).min(bins - 1) {
                    if k.abs_diff(j) > 2 {
                        background += self.smoothed_power[j];
                        neighbors += 1;
                    }
                }
                let background = background / neighbors.max(1) as f32;
                ensure!(background.is_finite(), "denoise background power overflow");
                if first {
                    self.local_energy[k] = p;
                    self.minimum[k] = p;
                    self.temporary_minimum[k] = p;
                    // A first frame may already contain speech. Its harmonic
                    // peaks are not a valid captured noise profile; initialize
                    // them from flanking power instead of learning the signal.
                    self.noise[k] = self.power[k].min(background).max(MIN_POWER);
                } else {
                    self.local_energy[k] = smoothing * self.local_energy[k] + (1.0 - smoothing) * p;
                    ensure!(
                        self.local_energy[k].is_finite(),
                        "denoise local energy overflow"
                    );
                    self.minimum[k] = self.minimum[k].min(self.local_energy[k]);
                    self.temporary_minimum[k] = self.temporary_minimum[k].min(self.local_energy[k]);
                    if rotate_minimum {
                        self.minimum[k] = self.temporary_minimum[k];
                        self.temporary_minimum[k] = self.local_energy[k];
                    }
                    // MCRA's rolling minimum alone eventually calls any long
                    // held musical note "noise". Distinguish a spectral ridge
                    // from a broadband noise-level change using flanking bins,
                    // excluding the main lobe and its immediate neighbors.
                    let tonal_signal = f64::from(p) > 6.0 * f64::from(background.max(MIN_POWER))
                        && f64::from(p) > 4.0 * f64::from(self.noise[k].max(MIN_POWER));
                    let signal_present = tonal_signal
                        || f64::from(p) > 2.5 * f64::from(self.minimum[k].max(MIN_POWER))
                        || f64::from(self.local_energy[k])
                            > 2.5 * f64::from(self.minimum[k].max(MIN_POWER));
                    let present = if signal_present { 1.0 } else { 0.0 };
                    self.presence[k] = 0.8 * self.presence[k] + 0.2 * present;
                    // A probabilistic ramp would absorb a fraction of a strong
                    // onset before presence converges, polluting the frozen
                    // noise estimate. Confirmed signal never enters that
                    // estimate; broadband changes can adapt after the rolling
                    // minimum advances to the new persistent noise level.
                    if !signal_present {
                        let alpha = base + (1.0 - base) * self.presence[k];
                        self.noise[k] = alpha * self.noise[k] + (1.0 - alpha) * self.power[k];
                        ensure!(self.noise[k].is_finite(), "denoise noise estimate overflow");
                    }
                    // Presence estimation is uncertain around quiet phonemes.
                    // Do not let accumulated speech energy push the estimate
                    // above the measured noise-floor envelope. This intentionally
                    // favors content preservation over maximally deep gating.
                    self.noise[k] = self.noise[k].min(self.minimum[k]);
                }
            }
        }
        let floor = self.floor;
        for k in 0..bins {
            let power = self.power[k].max(MIN_POWER);
            let noise = (self.noise[k] * self.config.strength).max(MIN_POWER);
            ensure!(noise.is_finite(), "denoise noise strength overflow");
            ensure!((noise / power).is_finite(), "denoise noise ratio overflow");
            let mut target = match self.config.mode {
                DenoiseMode::Off => 1.0,
                DenoiseMode::Stationary => (1.0 - noise / power).max(0.0).sqrt(),
                DenoiseMode::Profile => (1.0 - noise / power).max(0.0),
                DenoiseMode::Adaptive => {
                    let posterior_signal = (power / noise - 1.0).max(0.0);
                    ensure!(
                        posterior_signal.is_finite(),
                        "denoise posterior SNR overflow"
                    );
                    let decision_directed =
                        0.96 * self.previous_clean[k] / noise + 0.04 * posterior_signal;
                    ensure!(decision_directed.is_finite(), "denoise prior SNR overflow");
                    // Decision-directed SNR alone lags moving formants and
                    // pitch changes: the previous bin may contain no signal.
                    // Strong current evidence gets an instantaneous estimate;
                    // the 3x-noise uncertainty margin keeps ordinary stochastic
                    // excursions on the conservative decision-directed path.
                    let prior = decision_directed.max((posterior_signal - 3.0).max(0.0));
                    prior / (1.0 + prior)
                }
            };
            ensure!(target.is_finite(), "denoise target gain overflow");
            if self.config.strength == 0.0 {
                target = 1.0;
            }
            let prior_power = self.previous_power[k];
            let flux = if prior_power > MIN_POWER {
                let flux =
                    (self.smoothed_power[k] - prior_power) / self.smoothed_power[k].max(MIN_POWER);
                ensure!(flux.is_finite(), "denoise spectral flux overflow");
                flux.max(0.0)
            } else {
                0.0
            };
            target += (1.0 - target) * self.config.transient_protection * flux;
            ensure!(target.is_finite(), "denoise transient gain overflow");
            self.target_gain[k] = target.max(floor);
            self.previous_power[k] = self.smoothed_power[k];
        }
        // One-sided triangular smoothing fills suppression holes without pulling
        // a strong signal bin toward adjacent noise-only gains. Radius zero is
        // the genuine unsmoothed path; no global blend/bypass is substituted.
        let radius = self.config.smoothing_bins;
        for k in 0..bins {
            let mut total = 0.0;
            let mut weights = 0.0;
            for j in k.saturating_sub(radius)..=(k + radius).min(bins - 1) {
                let weight = (radius + 1 - k.abs_diff(j)) as f32;
                total += weight * self.target_gain[j];
                weights += weight;
            }
            let target = if self.config.mode == DenoiseMode::Adaptive {
                self.target_gain[k].max(total / weights)
            } else {
                total / weights
            };
            let alpha = if target < self.gain[k] {
                self.attack
            } else if self.config.mode == DenoiseMode::Adaptive {
                // A fixed slow recovery smears every new formant/pitch bin
                // even after the estimator identifies it as signal. The
                // transient control accelerates recovery in proportion to
                // new power absent from the preceding clean estimate.
                let new_signal =
                    (self.power[k] - self.previous_clean[k]) / self.power[k].max(MIN_POWER);
                ensure!(new_signal.is_finite(), "denoise recovery ratio overflow");
                let new_signal = new_signal.max(0.0);
                self.release * (1.0 - self.config.transient_protection * new_signal)
            } else {
                self.release
            };
            self.gain[k] = alpha * self.gain[k] + (1.0 - alpha) * target;
            self.previous_clean[k] = self.gain[k] * self.gain[k] * self.power[k];
            ensure!(
                self.gain[k].is_finite() && self.previous_clean[k].is_finite(),
                "denoise gain history overflow"
            );
        }
        Ok(())
    }

    /// Flush delayed repair and overlap-add output. Emits exactly the source
    /// frame count in total, with no padding frames or shifted timestamps.
    /// Repeated finish is an error; empty clips finish without emitting samples.
    pub fn finish(
        &mut self,
        output: &mut Vec<f32>,
        cancel: &mut impl FnMut() -> bool,
    ) -> Result<()> {
        ensure!(
            !self.finished && !self.poisoned,
            "denoise job must be reset before reuse"
        );
        self.check_cancel(cancel)?;
        self.poisoned = true;
        if !self.bypass {
            // Flush only real delayed repair frames; zero lookahead is context,
            // not source and is never included in the dry queue or counters.
            while self
                .impulse
                .as_ref()
                .is_some_and(|repair| repair.processed < self.received)
            {
                self.check_cancel(cancel)?;
                self.frame.fill(0.0);
                let repair = self
                    .impulse
                    .as_mut()
                    .expect("repair exists in loop condition");
                if repair.push(&self.frame, Some(self.received))? {
                    self.frame.copy_from_slice(&repair.frame);
                    self.push_prepared(output, cancel, None)?;
                }
            }
            while self.emitted < self.received {
                self.check_cancel(cancel)?;
                // This is STFT zero padding, not hum/impulse source input.
                self.input[self.filled * self.channels..(self.filled + 1) * self.channels]
                    .fill(0.0);
                self.filled += 1;
                if self.filled == self.size {
                    self.analyze(output, cancel)?;
                }
            }
        }
        self.finished = true;
        self.poisoned = false;
        Ok(())
    }

    /// Reset all histories after seek/source replacement; retains preparation and
    /// the explicit captured profile, never captures a replacement implicitly.
    pub fn reset(&mut self) {
        self.received = 0;
        self.emitted = 0;
        self.finished = false;
        self.poisoned = false;
        self.dry.clear();
        for notch in &mut self.hum {
            notch.state.fill([0.0; 2]);
        }
        if let Some(repair) = &mut self.impulse {
            repair.reset();
        }
        self.frame.fill(0.0);
        self.input.fill(0.0);
        self.filled = self.input.len() / self.channels / 2;
        self.frame_start = -(self.size as i64 / 2);
        self.overlap.fill(0.0);
        self.weights.fill(0.0);
        self.power.fill(0.0);
        self.smoothed_power.fill(0.0);
        self.noise.copy_from_slice(&self.initial_noise);
        self.local_energy.fill(0.0);
        self.minimum.fill(0.0);
        self.temporary_minimum.fill(0.0);
        self.presence.fill(0.0);
        self.previous_power.fill(0.0);
        self.previous_clean.fill(0.0);
        self.gain.fill(1.0);
        self.target_gain.fill(1.0);
        self.analyzed = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spectral_capacity(processor: &Denoiser) -> usize {
        processor.input.capacity()
            + processor.window.capacity()
            + processor.spectra.capacity()
            + processor.scratch.capacity()
            + processor.overlap.capacity()
            + processor.weights.capacity()
            + processor.power.capacity()
            + processor.smoothed_power.capacity()
            + processor.noise.capacity()
            + processor.initial_noise.capacity()
            + processor.local_energy.capacity()
            + processor.minimum.capacity()
            + processor.temporary_minimum.capacity()
            + processor.presence.capacity()
            + processor.previous_power.capacity()
            + processor.previous_clean.capacity()
            + processor.gain.capacity()
            + processor.target_gain.capacity()
    }

    #[test]
    fn bypass_and_time_domain_paths_prepare_only_their_required_state() {
        let mut bypass_config = DenoiseConfig {
            mode: DenoiseMode::Adaptive,
            wet: 0.0,
            ..Default::default()
        };
        bypass_config.hum.enabled = true;
        bypass_config.impulse.enabled = true;
        for config in [DenoiseConfig::default(), bypass_config] {
            let mut processor = Denoiser::new(48_000, 2, config, None).unwrap();
            assert_eq!(spectral_capacity(&processor), 0);
            assert!(processor.forward.is_none() && processor.inverse.is_none());
            assert_eq!(processor.frame.capacity(), 0);
            assert_eq!(processor.hum.capacity(), 0);
            assert!(processor.impulse.is_none());
            assert_eq!(processor.dry.capacity(), 0);
            assert_eq!(processor.lookahead_frames(), 0);
            let input = [-0.0, 0.0, f32::MAX, -f32::MAX];
            let mut output = Vec::new();
            processor
                .process_block(&input, &mut output, &mut || false)
                .unwrap();
            processor.finish(&mut output, &mut || false).unwrap();
            assert_eq!(
                input.map(f32::to_bits).as_slice(),
                output.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
            );
            assert_eq!(
                (processor.input_frames(), processor.output_frames()),
                (2, 2)
            );
            processor.reset();
            assert_eq!(
                (processor.input_frames(), processor.output_frames()),
                (0, 0)
            );
        }
        let config = DenoiseConfig {
            wet: 0.5,
            hum: HumConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let processor = Denoiser::new(48_000, 2, config.clone(), None).unwrap();
        assert_eq!(spectral_capacity(&processor), 0);
        assert!(processor.forward.is_none() && processor.inverse.is_none());
        assert_eq!(processor.frame.len(), 2);
        assert_eq!(
            processor
                .hum
                .iter()
                .map(|notch| notch.state.len())
                .sum::<usize>(),
            2 * config.hum.harmonics
        );
        assert_eq!(processor.dry.capacity(), 0);
        assert_eq!(processor.lookahead_frames(), 0);
        let repair_config = DenoiseConfig {
            impulse: ImpulseConfig {
                enabled: true,
                ..Default::default()
            },
            ..config
        };
        let mut processor = Denoiser::new(48_000, 2, repair_config, None).unwrap();
        assert_eq!(spectral_capacity(&processor), 0);
        let radius = processor.impulse.as_ref().unwrap().radius;
        assert_eq!(processor.lookahead_frames(), radius);
        assert_eq!(processor.dry.capacity(), (radius + 1) * 2);
        let capacity = processor.dry.capacity();
        let mut output = Vec::new();
        processor
            .process_block(&[0.125; 2000], &mut output, &mut || false)
            .unwrap();
        processor.finish(&mut output, &mut || false).unwrap();
        assert_eq!(output.len(), 2000);
        assert!(processor.dry.is_empty());
        assert_eq!(processor.dry.capacity(), capacity);
        assert_eq!(processor.analyzed, 0);
    }

    #[test]
    fn hum_only_is_immediate_and_matches_the_time_domain_reference() {
        let config = DenoiseConfig {
            wet: 0.25,
            hum: HumConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut processor = Denoiser::new(48_000, 2, config.clone(), None).unwrap();
        let mut reference: Vec<_> = (1..=config.hum.harmonics)
            .map(|harmonic| {
                HumNotch::new(
                    48_000,
                    2,
                    config.hum.fundamental_hz * harmonic as f32,
                    config.hum.q,
                )
            })
            .collect();
        let input = noise(2000, 135, 4.0);
        let expected: Vec<_> = input
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|frame| {
                frame
                    .iter()
                    .enumerate()
                    .map(|(ch, &dry)| {
                        let mut wet = dry;
                        for notch in &mut reference {
                            wet = notch.process(ch, wet);
                        }
                        (f64::from(dry) * 0.75 + f64::from(wet) * 0.25) as f32
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut output = Vec::new();
        for block in input.chunks(74) {
            let before = output.len();
            processor
                .process_block(block, &mut output, &mut || false)
                .unwrap();
            assert_eq!(output.len() - before, block.len());
        }
        assert_eq!(output, expected);
        processor.finish(&mut output, &mut || false).unwrap();
        assert_eq!(output, expected);
        assert_eq!(processor.analyzed, 0);
        processor.reset();
        output.clear();
        processor
            .process_block(&[0.0; 4], &mut output, &mut || false)
            .unwrap();
        assert_eq!(output, [0.0; 4]);
    }

    #[test]
    fn hum_overflow_stops_at_the_last_finite_frame_and_reset_recovers() {
        let config = DenoiseConfig {
            hum: HumConfig {
                enabled: true,
                harmonics: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut processor = Denoiser::new(48_000, 1, config, None).unwrap();
        let mut output = Vec::new();
        assert!(
            processor
                .process_block(&[f32::MAX, -f32::MAX], &mut output, &mut || false)
                .is_err()
        );
        assert_eq!(output.len(), 1);
        assert!(output[0].is_finite());
        assert!(processor.finish(&mut output, &mut || false).is_err());
        assert_eq!(output.len(), 1);
        processor.reset();
        output.clear();
        processor
            .process_block(&[0.0; 2], &mut output, &mut || false)
            .unwrap();
        processor.finish(&mut output, &mut || false).unwrap();
        assert_eq!(output, [0.0; 2]);
    }

    #[test]
    fn partial_wet_convex_blend_does_not_overflow_its_difference() {
        let config = DenoiseConfig {
            wet: 0.5,
            hum: HumConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut processor = Denoiser::new(48_000, 2, config, None).unwrap();
        processor.frame.copy_from_slice(&[f32::MAX, -f32::MAX]);
        let mut output = Vec::new();
        processor
            .emit(0, &mut output, Some(&[-f32::MAX, f32::MAX]))
            .unwrap();
        assert_eq!(output, [0.0; 2]);
        assert_eq!(processor.dry.capacity(), 0);
    }

    #[test]
    fn zero_wet_still_requires_a_valid_explicit_profile() {
        let config = DenoiseConfig {
            mode: DenoiseMode::Profile,
            wet: 0.0,
            fft_size: 256,
            ..Default::default()
        };
        assert!(Denoiser::new(48_000, 1, config.clone(), None).is_err());
        let mut captured = NoiseProfile {
            sample_rate: 48_000,
            channels: 1,
            fft_size: 256,
            captured_frames: 1,
            power: vec![0.0; 129],
        };
        captured.power[0] = f32::INFINITY;
        assert!(Denoiser::new(48_000, 1, config.clone(), Some(captured.clone())).is_err());
        captured.power[0] = 0.0;
        let processor = Denoiser::new(48_000, 1, config, Some(captured)).unwrap();
        assert_eq!(spectral_capacity(&processor), 0);
        assert!(processor.forward.is_none() && processor.inverse.is_none());
    }

    #[test]
    fn numerical_errors_never_publish_invalid_audio_and_require_reset() {
        for mode in [DenoiseMode::Stationary, DenoiseMode::Adaptive] {
            for amplitude in [1.0e20, f32::MAX] {
                let config = DenoiseConfig {
                    mode,
                    fft_size: 256,
                    ..Default::default()
                };
                let mut processor = Denoiser::new(48_000, 2, config, None).unwrap();
                let mut output = vec![0.125];
                assert!(
                    processor
                        .process_block(&vec![amplitude; 512], &mut output, &mut || false)
                        .is_err()
                );
                assert_eq!(output, [0.125]);
                assert!(
                    processor
                        .process_block(&[0.0; 2], &mut output, &mut || false)
                        .is_err()
                );
                assert!(processor.finish(&mut output, &mut || false).is_err());
                processor.reset();
                output.clear();
                processor
                    .process_block(&[0.0; 14], &mut output, &mut || false)
                    .unwrap();
                processor.finish(&mut output, &mut || false).unwrap();
                assert_eq!(output, [0.0; 14]);
                assert_eq!(
                    (processor.input_frames(), processor.output_frames()),
                    (7, 7)
                );
            }
        }
        let config = DenoiseConfig {
            impulse: ImpulseConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut processor = Denoiser::new(48_000, 1, config, None).unwrap();
        let input: Vec<_> = (0..64)
            .map(|i| if i % 2 == 0 { f32::MAX } else { -f32::MAX })
            .collect();
        let mut output = Vec::new();
        assert!(
            processor
                .process_block(&input, &mut output, &mut || false)
                .is_err()
        );
        assert!(output.iter().all(|sample| sample.is_finite()));
        let published = output.clone();
        assert!(processor.finish(&mut output, &mut || false).is_err());
        assert_eq!(output, published);
        processor.reset();
        output.clear();
        processor
            .process_block(&[0.0; 3], &mut output, &mut || false)
            .unwrap();
        processor.finish(&mut output, &mut || false).unwrap();
        assert_eq!(output, [0.0; 3]);
    }

    #[test]
    fn invalid_input_is_atomic_and_short_tail_overflow_is_reported_at_finish() {
        let config = DenoiseConfig {
            mode: DenoiseMode::Stationary,
            fft_size: 256,
            ..Default::default()
        };
        let mut processor = Denoiser::new(48_000, 2, config, None).unwrap();
        let mut output = vec![0.25];
        assert!(
            processor
                .process_block(&[1.0, 0.0, f32::NAN, 0.0], &mut output, &mut || false)
                .is_err()
        );
        assert_eq!(
            (processor.received, processor.emitted, processor.filled),
            (0, 0, 128)
        );
        assert!(!processor.poisoned);
        assert_eq!(output, [0.25]);
        processor
            .process_block(&[1.0e20; 2], &mut output, &mut || false)
            .unwrap();
        assert!(processor.finish(&mut output, &mut || false).is_err());
        assert_eq!(output, [0.25]);
        assert!(processor.poisoned);
    }

    #[test]
    fn profile_fft_power_and_finish_overflow_cannot_publish_and_reset_recovers() {
        for amplitude in [1.0e20, f32::MAX] {
            let mut capture = ProfileCollector::new(48_000, 1, 256, 512).unwrap();
            assert!(
                capture
                    .push_block(&[amplitude; 256], &mut || false)
                    .is_err()
            );
            assert!(capture.push_block(&[0.0], &mut || false).is_err());
            capture.reset();
            capture.push_block(&[0.125; 256], &mut || false).unwrap();
            capture.finish().unwrap().validate(48_000, 1, 256).unwrap();
            let mut capture = ProfileCollector::new(48_000, 1, 256, 512).unwrap();
            assert!(
                capture
                    .push_block(&[amplitude; 256], &mut || false)
                    .is_err()
            );
            assert!(capture.finish().is_err());
        }
        let mut capture = ProfileCollector::new(48_000, 1, 256, 512).unwrap();
        capture.push_block(&[1.0e20], &mut || false).unwrap();
        assert!(capture.finish().is_err());
        let mut capture = ProfileCollector::new(48_000, 1, 256, 512).unwrap();
        assert!(capture.push_block(&[0.0, f32::NAN], &mut || false).is_err());
        assert_eq!((capture.captured_frames, capture.filled), (0, 0));
        assert!(!capture.poisoned);
    }

    fn noise(frames: usize, seed: u32, amplitude: f32) -> Vec<f32> {
        let mut state = seed;
        (0..frames)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32 * amplitude
            })
            .collect()
    }

    fn run(
        config: DenoiseConfig,
        rate: u32,
        channels: usize,
        input: &[f32],
        profile: Option<NoiseProfile>,
        block: usize,
    ) -> Vec<f32> {
        let mut denoiser = Denoiser::new(rate, channels, config, profile).unwrap();
        let mut output = Vec::new();
        for chunk in input.chunks(block * channels) {
            denoiser
                .process_block(chunk, &mut output, &mut || false)
                .unwrap();
        }
        denoiser.finish(&mut output, &mut || false).unwrap();
        assert_eq!(output.len(), input.len());
        assert!(output.iter().all(|x| x.is_finite()));
        output
    }

    fn energy(input: &[f32]) -> f64 {
        input.iter().map(|x| f64::from(*x).powi(2)).sum()
    }

    fn profile(input: &[f32], rate: u32, channels: usize, size: usize) -> NoiseProfile {
        let mut capture =
            ProfileCollector::new(rate, channels, size, (input.len() / channels) as u64).unwrap();
        for chunk in input.chunks(713 * channels) {
            capture.push_block(chunk, &mut || false).unwrap();
        }
        capture.finish().unwrap()
    }

    #[test]
    fn missing_or_mismatched_profiles_are_rejected() {
        let config = DenoiseConfig::default();
        let required = DenoiseConfig {
            mode: DenoiseMode::Profile,
            ..config.clone()
        };
        assert!(Denoiser::new(48_000, 2, required.clone(), None).is_err());
        let wrong = profile(&noise(2048, 5, 0.01), 16_000, 1, 2048);
        assert!(Denoiser::new(48_000, 2, required, Some(wrong)).is_err());
    }

    #[test]
    fn bypass_is_sample_exact_including_signed_zero() {
        let input = [0.0, -0.0, 0.3, -0.9, 4.0, -4.0];
        let output = run(DenoiseConfig::default(), 48_000, 2, &input, None, 1);
        assert!(
            input
                .iter()
                .zip(output)
                .all(|(a, b)| a.to_bits() == b.to_bits())
        );
        let config = DenoiseConfig {
            mode: DenoiseMode::Adaptive,
            wet: 0.0,
            ..Default::default()
        };
        assert_eq!(run(config, 48_000, 2, &input, None, 2), input);
    }

    #[test]
    fn unity_stft_preserves_alignment_edges_and_arbitrary_block_boundaries() {
        let config = DenoiseConfig {
            mode: DenoiseMode::Stationary,
            strength: 0.0,
            fft_size: 256,
            ..Default::default()
        };
        for rate in [8000, 44_100, 48_000, 192_000, 384_000] {
            for channels in [1, 2, 6, 32] {
                for frames in [0, 1, 2, 63, 127, 128, 255, 256, 257, 713] {
                    let input = noise(frames * channels, 12, 0.5);
                    let output = run(config.clone(), rate, channels, &input, None, 37);
                    for (x, y) in input.iter().zip(&output) {
                        assert!((x - y).abs() < 2.0e-6);
                    }
                    let other = run(config.clone(), rate, channels, &input, None, 113);
                    assert_eq!(output, other);
                }
            }
        }
    }

    #[test]
    fn stationary_reduces_noise_and_preserves_a_sustained_tone() {
        let rate = 16_000;
        let frames = rate as usize * 2;
        let background = noise(frames, 0x1234, 0.06);
        let clean: Vec<_> = (0..frames)
            .map(|i| 0.3 * (2.0 * PI * 1000.0 * i as f32 / rate as f32).sin())
            .collect();
        let mixed: Vec<_> = clean.iter().zip(&background).map(|(a, b)| a + b).collect();
        let config = DenoiseConfig {
            mode: DenoiseMode::Stationary,
            fft_size: 512,
            stationary_noise_dbfs: 20.0 * (0.06_f32 / 3.0_f32.sqrt()).log10(),
            transient_protection: 0.0,
            attack_ms: 0.0,
            release_ms: 0.0,
            smoothing_bins: 0,
            ..Default::default()
        };
        let suppressed = run(config.clone(), rate, 1, &background, None, 137);
        let output = run(config, rate, 1, &mixed, None, 137);
        assert!(energy(&suppressed[1024..]) < 0.5 * energy(&background[1024..]));
        let input_error: Vec<_> = mixed.iter().zip(&clean).map(|(x, y)| x - y).collect();
        let output_error: Vec<_> = output.iter().zip(&clean).map(|(x, y)| x - y).collect();
        assert!(energy(&output_error[1024..]) < 0.6 * energy(&input_error[1024..]));
        let projection = output
            .iter()
            .zip(&clean)
            .map(|(a, b)| f64::from(a * b))
            .sum::<f64>()
            / energy(&clean);
        assert!((projection - 1.0).abs() < 0.04);
    }

    #[test]
    fn captured_colored_profile_suppresses_noise_without_moving_stereo_image() {
        let rate = 16_000;
        let mut background = noise(32_000, 81, 0.06);
        for i in 1..background.len() {
            background[i] += 0.8 * background[i - 1];
        }
        let captured = profile(&background, rate, 1, 512);
        let config = DenoiseConfig {
            mode: DenoiseMode::Profile,
            fft_size: 512,
            transient_protection: 0.0,
            smoothing_bins: 0,
            attack_ms: 0.0,
            release_ms: 0.0,
            ..Default::default()
        };
        let residual = run(config.clone(), rate, 1, &background, Some(captured), 233);
        assert!(energy(&residual[1024..]) < 0.45 * energy(&background[1024..]));
        let stereo: Vec<_> = background.iter().flat_map(|x| [*x, -*x * 0.37]).collect();
        let captured = profile(&stereo, rate, 2, 512);
        let output = run(config, rate, 2, &stereo, Some(captured), 113);
        for pair in output.as_chunks::<2>().0 {
            assert!((pair[1] + 0.37 * pair[0]).abs() < 1.0e-6);
        }
    }

    #[test]
    fn adaptive_tracks_changing_noise_and_is_not_a_stationary_alias() {
        let rate = 16_000;
        let mut input = noise(96_000, 985, 0.02);
        for x in &mut input[32_000..] {
            *x *= 3.0;
        }
        let adaptive = DenoiseConfig {
            mode: DenoiseMode::Adaptive,
            fft_size: 512,
            transient_protection: 0.0,
            smoothing_bins: 1,
            noise_floor_db: -30.0,
            attack_ms: 0.0,
            release_ms: 30.0,
            ..Default::default()
        };
        let output = run(adaptive.clone(), rate, 1, &input, None, 231);
        assert!(energy(&output[80_000..]) < 0.45 * energy(&input[80_000..]));
        let fixed = run(
            DenoiseConfig {
                mode: DenoiseMode::Stationary,
                stationary_noise_dbfs: -60.0,
                ..adaptive
            },
            rate,
            1,
            &input,
            None,
            231,
        );
        assert!(energy(&output[80_000..]) < 0.7 * energy(&fixed[80_000..]));
    }

    fn tone_projection(input: &[f32], rate: u32, hz: f32) -> f64 {
        let mut real = 0.0;
        let mut imag = 0.0;
        for (i, x) in input.iter().enumerate() {
            let phase = 2.0 * std::f64::consts::PI * f64::from(hz) * i as f64 / f64::from(rate);
            real += f64::from(*x) * phase.cos();
            imag += f64::from(*x) * phase.sin();
        }
        (real * real + imag * imag).sqrt() * 2.0 / input.len() as f64
    }

    #[test]
    fn hum_harmonics_are_independent_of_spectral_mode_and_do_not_leak_channels() {
        let rate = 8000;
        let mono: Vec<_> = (0..32_000)
            .map(|i| {
                let t = 2.0 * PI * i as f32 / rate as f32;
                0.2 * (50.0 * t).sin() + 0.1 * (100.0 * t).sin() + 0.2 * (997.0 * t).sin()
            })
            .collect();
        let stereo: Vec<_> = mono.iter().flat_map(|x| [*x, 0.0]).collect();
        let config = DenoiseConfig {
            hum: HumConfig {
                enabled: true,
                harmonics: 2,
                q: 20.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let output = run(config, rate, 2, &stereo, None, 97);
        assert!(output.as_chunks::<2>().0.iter().all(|x| x[1] == 0.0));
        let left: Vec<_> = output.as_chunks::<2>().0.iter().map(|x| x[0]).collect();
        assert!(tone_projection(&left[16_000..], rate, 50.0) < 0.003);
        assert!(tone_projection(&left[16_000..], rate, 100.0) < 0.003);
        assert!((tone_projection(&left[16_000..], rate, 997.0) - 0.2).abs() < 0.003);
    }

    #[test]
    fn impulse_repair_interpolates_short_runs_but_protects_a_sustained_onset() {
        let mut input = vec![0.1; 2000];
        input[500] = 0.9;
        input[800] = -0.7;
        input[801] = -0.8;
        input[802] = -0.6;
        input[1200..].fill(0.4);
        let config = DenoiseConfig {
            impulse: ImpulseConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let output = run(config, 48_000, 1, &input, None, 17);
        assert!((output[500] - 0.1).abs() < 1.0e-6);
        assert!(output[800..803].iter().all(|x| (*x - 0.1).abs() < 1.0e-6));
        assert_eq!(&output[1190..1210], &input[1190..1210]);
        assert_eq!(&output[..10], &input[..10]);
        assert_eq!(&output[1990..], &input[1990..]);
    }

    #[test]
    fn wet_blend_uses_aligned_original_and_reset_removes_all_history() {
        let input = noise(3001, 16, 0.1);
        let full = DenoiseConfig {
            mode: DenoiseMode::Adaptive,
            fft_size: 256,
            hum: HumConfig {
                enabled: true,
                ..Default::default()
            },
            impulse: ImpulseConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let processed = run(full.clone(), 48_000, 1, &input, None, 33);
        let half = run(
            DenoiseConfig {
                wet: 0.5,
                ..full.clone()
            },
            48_000,
            1,
            &input,
            None,
            33,
        );
        for ((dry, wet), mixed) in input.iter().zip(&processed).zip(&half) {
            assert!((*mixed - (*dry + 0.5 * (*wet - *dry))).abs() < 1.0e-7);
        }
        let mut processor = Denoiser::new(48_000, 1, full, None).unwrap();
        let mut first = Vec::new();
        processor
            .process_block(&input, &mut first, &mut || false)
            .unwrap();
        processor.finish(&mut first, &mut || false).unwrap();
        processor.reset();
        let mut second = Vec::new();
        processor
            .process_block(&input, &mut second, &mut || false)
            .unwrap();
        processor.finish(&mut second, &mut || false).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn partial_wet_blends_remain_aligned_through_short_multichannel_tails() {
        let channels = 6;
        for mode in [
            DenoiseMode::Off,
            DenoiseMode::Stationary,
            DenoiseMode::Profile,
            DenoiseMode::Adaptive,
        ] {
            for frames in [1, 5, 9, 63, 127, 128, 255, 257, 713] {
                let input = noise(frames * channels, 261, 0.1);
                let config = DenoiseConfig {
                    mode,
                    fft_size: 256,
                    hum: HumConfig {
                        enabled: true,
                        ..Default::default()
                    },
                    impulse: ImpulseConfig {
                        enabled: true,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let captured = (mode == DenoiseMode::Profile)
                    .then(|| profile(&vec![0.0; 64 * channels], 48_000, channels, 256));
                let full = run(
                    config.clone(),
                    48_000,
                    channels,
                    &input,
                    captured.clone(),
                    17,
                );
                for wet in [0.25, 0.75] {
                    let blended = run(
                        DenoiseConfig {
                            wet,
                            ..config.clone()
                        },
                        48_000,
                        channels,
                        &input,
                        captured.clone(),
                        113,
                    );
                    for ((dry, processed), mixed) in input.iter().zip(&full).zip(&blended) {
                        assert_eq!(
                            *mixed,
                            (f64::from(*dry) * (1.0 - f64::from(wet))
                                + f64::from(*processed) * f64::from(wet))
                                as f32
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn bounded_input_cancellation_empty_profile_and_invalid_frames_are_explicit() {
        let mut processor = Denoiser::new(48_000, 2, DenoiseConfig::default(), None).unwrap();
        let mut output = Vec::new();
        assert!(
            processor
                .process_block(&[0.0], &mut output, &mut || false)
                .is_err()
        );
        assert!(
            processor
                .process_block(&[f32::NAN, 0.0], &mut output, &mut || false)
                .is_err()
        );
        assert!(
            processor
                .process_block(
                    &vec![0.0; (MAX_BLOCK_FRAMES + 1) * 2],
                    &mut output,
                    &mut || false
                )
                .is_err()
        );
        assert!(
            processor
                .process_block(&[0.0, 0.0], &mut output, &mut || true)
                .is_err()
        );
        assert!(processor.finish(&mut output, &mut || false).is_err());
        processor.reset();
        processor.finish(&mut output, &mut || false).unwrap();
        assert!(output.is_empty());
        assert!(ProfileCollector::new(48_000, 1, 256, 0).is_err());
        assert!(
            ProfileCollector::new(48_000, 1, 256, 100)
                .unwrap()
                .finish()
                .is_err()
        );
        let mut capture = ProfileCollector::new(48_000, 1, 256, 3).unwrap();
        assert!(capture.push_block(&[0.0; 4], &mut || false).is_err());
        capture.push_block(&[0.0; 3], &mut || false).unwrap();
        assert_eq!(capture.finish().unwrap().captured_frames, 3);
    }

    #[test]
    fn every_mode_handles_silence_short_clips_and_repair_tail_without_padding() {
        for mode in [
            DenoiseMode::Off,
            DenoiseMode::Stationary,
            DenoiseMode::Profile,
            DenoiseMode::Adaptive,
        ] {
            for frames in [0, 1, 2, 5, 9, 63, 255, 257] {
                let input = vec![0.0; frames * 2];
                let config = DenoiseConfig {
                    mode,
                    fft_size: 256,
                    impulse: ImpulseConfig {
                        enabled: true,
                        ..Default::default()
                    },
                    hum: HumConfig {
                        enabled: true,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let captured =
                    (mode == DenoiseMode::Profile).then(|| profile(&[0.0, 0.0], 48_000, 2, 256));
                let output = run(config, 48_000, 2, &input, captured, 1);
                assert!(output.iter().all(|x| *x == 0.0));
            }
        }
    }

    #[test]
    fn adaptive_preserves_sustained_content_after_noise_only_context() {
        let rate = 16_000;
        let mut input = noise(64_000, 76, 0.025);
        let clean: Vec<_> = (0..input.len())
            .map(|i| {
                if i < 16_000 {
                    0.0
                } else {
                    0.2 * (2.0 * PI * 1000.0 * i as f32 / rate as f32).sin()
                }
            })
            .collect();
        for (sample, tone) in input.iter_mut().zip(&clean) {
            *sample += tone;
        }
        let config = DenoiseConfig {
            mode: DenoiseMode::Adaptive,
            fft_size: 512,
            smoothing_bins: 0,
            ..Default::default()
        };
        let output = run(config, rate, 1, &input, None, 179);
        assert!(energy(&output[4000..12_000]) < 0.5 * energy(&input[4000..12_000]));
        let gain = output[20_000..]
            .iter()
            .zip(&clean[20_000..])
            .map(|(x, y)| f64::from(x * y))
            .sum::<f64>()
            / energy(&clean[20_000..]);
        let noise_ratio = energy(&output[4000..12_000]) / energy(&input[4000..12_000]);
        let late_gain = output[48_000..]
            .iter()
            .zip(&clean[48_000..])
            .map(|(x, y)| f64::from(x * y))
            .sum::<f64>()
            / energy(&clean[48_000..]);
        println!(
            "adaptive preservation: tone_gain={gain:.8}, late_tone_gain={late_gain:.8}, noise_energy_ratio={noise_ratio:.8}"
        );
        assert!((late_gain - 1.0).abs() < 0.08);
        assert!((gain - 1.0).abs() < 0.08);
    }

    #[test]
    fn transient_control_and_attack_are_real_parameters() {
        let mut input = noise(16_000, 587, 0.01);
        for (i, value) in input[8000..8128].iter_mut().enumerate() {
            *value += 0.15 * (2.0 * PI * 0.13 * i as f32).sin();
        }
        let config = DenoiseConfig {
            mode: DenoiseMode::Stationary,
            fft_size: 256,
            stationary_noise_dbfs: -24.0,
            smoothing_bins: 0,
            attack_ms: 0.0,
            release_ms: 0.0,
            transient_protection: 0.0,
            ..Default::default()
        };
        let unprotected = run(config.clone(), 16_000, 1, &input, None, 97);
        let protected = run(
            DenoiseConfig {
                transient_protection: 1.0,
                ..config.clone()
            },
            16_000,
            1,
            &input,
            None,
            97,
        );
        let dry_energy = energy(&input[7970..8170]);
        assert!(energy(&protected[7970..8170]) > energy(&unprotected[7970..8170]));
        assert!(
            (dry_energy - energy(&protected[7970..8170])).abs()
                < (dry_energy - energy(&unprotected[7970..8170])).abs()
        );
        let slow = run(
            DenoiseConfig {
                attack_ms: 1000.0,
                ..config
            },
            16_000,
            1,
            &input,
            None,
            97,
        );
        assert!(energy(&slow[..1000]) > energy(&unprotected[..1000]));
    }

    #[test]
    fn mid_job_cancellation_poisoning_and_profile_reset_are_deterministic() {
        let config = DenoiseConfig {
            mode: DenoiseMode::Adaptive,
            fft_size: 256,
            ..Default::default()
        };
        let mut processor = Denoiser::new(16_000, 1, config, None).unwrap();
        let mut calls = 0;
        let mut output = Vec::new();
        assert!(
            processor
                .process_block(&[0.1; 2048], &mut output, &mut || {
                    calls += 1;
                    calls > 8
                })
                .is_err()
        );
        assert!(processor.input_frames() < 2048);
        assert!(
            processor
                .process_block(&[0.0], &mut output, &mut || false)
                .is_err()
        );
        processor.reset();
        output.clear();
        processor
            .process_block(&[0.0; 7], &mut output, &mut || false)
            .unwrap();
        processor.finish(&mut output, &mut || false).unwrap();
        assert_eq!(output, vec![0.0; 7]);
        let mut capture = ProfileCollector::new(16_000, 1, 256, 1024).unwrap();
        assert!(capture.push_block(&[0.1; 1024], &mut || true).is_err());
        capture.reset();
        capture.push_block(&[0.02; 1024], &mut || false).unwrap();
        let actual = capture.finish().unwrap();
        let expected = profile(&[0.02; 1024], 16_000, 1, 256);
        assert_eq!(actual.power, expected.power);
    }

    #[test]
    fn adaptive_default_preserves_modulated_harmonics_and_improves_high_snr() {
        // Multi-harmonic, varying-pitch/envelope content starts immediately;
        // unlike the single-tone test there is no supplied noise-only lead-in,
        // and the actual default FFT size and smoothing remain enabled.
        let rate = 16_000;
        let mut phase = 0.0_f32;
        let clean: Vec<_> = (0..64_000)
            .map(|i| {
                let t = i as f32 / rate as f32;
                phase += 2.0 * PI * (143.0 + 12.0 * (2.0 * PI * 1.7 * t).sin()) / rate as f32;
                let envelope = 0.15 + 0.85 * (2.0 * PI * 2.3 * t).sin().powi(2);
                let voice = (1..=16)
                    .map(|h| (phase * h as f32).sin() / h as f32)
                    .sum::<f32>();
                0.12 * envelope * voice
            })
            .collect();
        let config = DenoiseConfig {
            mode: DenoiseMode::Adaptive,
            ..Default::default()
        };
        let processed_clean = run(config.clone(), rate, 1, &clean, None, 179);
        let clean_error: Vec<_> = processed_clean
            .iter()
            .zip(&clean)
            .map(|(x, y)| x - y)
            .collect();
        let clean_gain = processed_clean
            .iter()
            .zip(&clean)
            .map(|(x, y)| f64::from(x * y))
            .sum::<f64>()
            / energy(&clean);
        let clean_snr = 10.0 * (energy(&clean) / energy(&clean_error)).log10();
        let mut background = noise(clean.len(), 735, 1.0);
        let scale = (energy(&clean) / (10.0_f64.powf(1.5) * energy(&background))).sqrt() as f32;
        for x in &mut background {
            *x *= scale;
        }
        let input: Vec<_> = clean.iter().zip(&background).map(|(x, y)| x + y).collect();
        let output = run(config, rate, 1, &input, None, 179);
        let error: Vec<_> = output.iter().zip(&clean).map(|(x, y)| x - y).collect();
        let snr = 10.0 * (energy(&clean) / energy(&error)).log10();
        let noisy_gain = output
            .iter()
            .zip(&clean)
            .map(|(x, y)| f64::from(x * y))
            .sum::<f64>()
            / energy(&clean);
        println!(
            "adaptive default modulated content: clean_gain={clean_gain:.8}, clean_snr_db={clean_snr:.5}, input_snr_db=15, output_snr_db={snr:.5}, noisy_gain={noisy_gain:.8}"
        );
        assert!((clean_gain - 1.0).abs() < 0.05);
        assert!(clean_snr > 24.0);
        assert!(snr > 15.0);
        assert!((noisy_gain - 1.0).abs() < 0.06);
    }
}
