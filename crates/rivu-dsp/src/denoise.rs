//! Offline, bounded-memory classical denoising.
//!
//! Stationary mode subtracts a calibrated, fixed white-noise power spectrum;
//! Profile mode uses a frozen, explicitly captured colored-noise spectrum and a
//! Wiener power gain. Adaptive mode estimates noise with minima-controlled
//! recursive averaging (MCRA), then applies a decision-directed Wiener gain.
//! They are deliberately different estimators, not names for strength presets.
//! Stationary's white-noise calibration is not a colored/fan-noise estimator;
//! use an explicit matching Profile or identified hum notches for that target.
//!
//! Primary algorithm references:
//! - S. Boll, *Suppression of Acoustic Noise in Speech Using Spectral Subtraction*,
//!   IEEE ASSP 27(2), 1979, https://doi.org/10.1109/TASSP.1979.1163209.
//! - I. Cohen and B. Berdugo, *Noise Estimation by Minima Controlled Recursive
//!   Averaging for Robust Speech Enhancement*, IEEE SPL 9(1), 2002,
//!   https://doi.org/10.1109/97.988717 (author's paper:
//!   https://israelcohen.com/wp-content/uploads/2018/05/SPL_Jan2002.pdf).
//!
//! This implementation uses the MCRA estimator structure, not the paper's
//! OM-LSA enhancement stage or an assertion of equivalent perceptual quality.
//! For music, an additional local spectral-contrast guard prevents a sustained
//! narrowband component from becoming noise merely because the minimum-search
//! window expires. Ambiguous tonal interference is deliberately preserved by
//! adaptive mode; identify it explicitly with Profile mode or hum notches.
//! The adaptive estimate is bounded by its rolling local-power minimum to avoid
//! absorbing speech during uncertain presence decisions. Frequency smoothing
//! fills suppression holes but cannot lower a bin's own signal-preserving gain;
//! averaging noise-bin gains into strong harmonics would attenuate clean speech.
//! A high-confidence instantaneous SNR bound and transient-dependent gain recovery
//! avoid decision-directed lag on changing pitch/formants. This preserves more
//! content at the cost of less aggressive noise-only suppression, not a dry-path
//! fallback: spectral gains still follow the estimated per-bin signal/noise.
//!
//! Real nonnegative gains preserve each STFT bin's phase. One gain is estimated
//! from mean channel power and applied to every channel, preserving interchannel
//! phase and level ratios within each bin (not independent stereo gating).
//! Square-root periodic Hann analysis/synthesis with explicit overlap-weight
//! normalization handles boundaries. Leading delay is discarded and `finish`
//! emits only actual source frames. No limiter, normalization or output clamp is
//! applied. Isolated impulse repair is conservative local interpolation; it is
//! not a restoration algorithm for long dropouts, clipping or dense crackle.
//!
//! Preparation and processing belong on a scheduling/file-job thread, never a
//! device callback. Internal storage is bounded by FFT size, channels and repair
//! width, independent of source duration. Full-wet processing retains no dry copy;
//! partial-wet processing retains only the aligned lookahead. The caller owns
//! output capacity and must drain it between calls. Cancellation invalidates a
//! partial job until `reset`; the caller must discard its partial result. No
//! source file is opened or modified.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const MAX_BLOCK_FRAMES: usize = 65_536;
pub const MAX_CHANNELS: usize = 32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenoiseMode {
    #[default]
    Off,
    Stationary,
    Profile,
    Adaptive,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HumConfig {
    pub enabled: bool,
    pub fundamental_hz: f32,
    /// Includes the fundamental. Each harmonic has a separate identical-per-channel notch.
    pub harmonics: usize,
    pub q: f32,
}

impl Default for HumConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fundamental_hz: 50.0,
            harmonics: 6,
            q: 30.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImpulseConfig {
    pub enabled: bool,
    /// Maximum contiguous damaged samples eligible for interpolation (1..=8).
    pub max_width: usize,
    /// Robust local difference threshold multiplier (3..=40).
    pub threshold: f32,
    /// Absolute detection floor; avoids treating roundoff in silence as an impulse.
    pub floor_dbfs: f32,
    /// 0..=1; requires increasingly similar pre/post context to protect onsets.
    pub transient_protection: f32,
}

impl ImpulseConfig {
    fn radius(&self) -> usize {
        self.max_width + 2
    }
}

impl Default for ImpulseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_width: 3,
            threshold: 8.0,
            floor_dbfs: -60.0,
            transient_protection: 0.9,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DenoiseConfig {
    pub mode: DenoiseMode,
    /// Noise subtraction / noise-power multiplier (0..=4).
    pub strength: f32,
    /// Minimum spectral amplitude gain in dB (-100..=0), not an input gate.
    pub spectral_gain_floor_db: f32,
    /// Gain-reduction time constant. Zero means immediate reduction.
    pub attack_ms: f32,
    /// Gain-recovery time constant. Zero means immediate recovery.
    pub release_ms: f32,
    /// Radius of triangular smoothing in frequency bins (0..=16).
    pub smoothing_bins: usize,
    /// Positive spectral-flux protection, 0..=1.
    pub transient_protection: f32,
    /// Global linear blend against the unmodified source, including hum/repair.
    pub wet: f32,
    /// Power of two, 256..=8192. Hop is fft_size / 4.
    pub fft_size: usize,
    /// Stationary white-noise RMS relative to full scale. Not a noise gate.
    pub stationary_noise_dbfs: f32,
    pub hum: HumConfig,
    pub impulse: ImpulseConfig,
}

impl Default for DenoiseConfig {
    fn default() -> Self {
        Self {
            mode: DenoiseMode::Off,
            strength: 1.0,
            spectral_gain_floor_db: -24.0,
            attack_ms: 8.0,
            release_ms: 70.0,
            smoothing_bins: 2,
            transient_protection: 0.7,
            wet: 1.0,
            fft_size: 2048,
            stationary_noise_dbfs: -45.0,
            hum: HumConfig::default(),
            impulse: ImpulseConfig::default(),
        }
    }
}

fn in_range(value: f32, low: f32, high: f32, name: &str) -> Result<()> {
    ensure!(
        value.is_finite() && value >= low && value <= high,
        "{name} must be finite and in {low}..={high}"
    );
    Ok(())
}

fn validate_format(sample_rate: u32, channels: usize, fft_size: usize) -> Result<()> {
    ensure!(
        (8000..=384_000).contains(&sample_rate),
        "unsupported denoise sample rate"
    );
    ensure!(
        (1..=MAX_CHANNELS).contains(&channels),
        "unsupported denoise channel count"
    );
    ensure!(
        (256..=8192).contains(&fft_size) && fft_size.is_power_of_two(),
        "denoise FFT size must be a power of two in 256..=8192"
    );
    Ok(())
}

impl DenoiseConfig {
    /// Whether valid controls leave samples unchanged; does not validate them.
    pub fn is_bypassed(&self) -> bool {
        self.wet == 0.0
            || (self.mode == DenoiseMode::Off && !self.hum.enabled && !self.impulse.enabled)
    }

    /// Maximum withheld source frames for valid controls, without preparing DSP state.
    pub fn lookahead_frames(&self) -> usize {
        if self.is_bypassed() {
            0
        } else {
            let spectral = if self.mode == DenoiseMode::Off {
                0
            } else {
                self.fft_size
            };
            spectral
                + if self.impulse.enabled {
                    self.impulse.radius()
                } else {
                    0
                }
        }
    }

    /// Validate persisted controls before a source format is available.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (256..=8192).contains(&self.fft_size) && self.fft_size.is_power_of_two(),
            "denoise FFT size must be a power of two in 256..=8192"
        );
        in_range(self.strength, 0.0, 4.0, "denoise strength")?;
        in_range(
            self.spectral_gain_floor_db,
            -100.0,
            0.0,
            "spectral gain floor",
        )?;
        in_range(self.attack_ms, 0.0, 2000.0, "denoise attack")?;
        in_range(self.release_ms, 0.0, 10_000.0, "denoise release")?;
        ensure!(
            self.smoothing_bins <= 16,
            "frequency smoothing radius exceeds 16"
        );
        in_range(self.transient_protection, 0.0, 1.0, "transient protection")?;
        in_range(self.wet, 0.0, 1.0, "wet blend")?;
        in_range(
            self.stationary_noise_dbfs,
            -120.0,
            0.0,
            "stationary noise level",
        )?;
        in_range(self.hum.fundamental_hz, 20.0, 1000.0, "hum fundamental")?;
        in_range(self.hum.q, 2.0, 200.0, "hum Q")?;
        ensure!(
            (1..=32).contains(&self.hum.harmonics),
            "hum harmonic count must be 1..=32"
        );
        ensure!(
            (1..=8).contains(&self.impulse.max_width),
            "impulse width must be 1..=8"
        );
        in_range(self.impulse.threshold, 3.0, 40.0, "impulse threshold")?;
        in_range(
            self.impulse.floor_dbfs,
            -120.0,
            -6.0,
            "impulse detection floor",
        )?;
        in_range(
            self.impulse.transient_protection,
            0.0,
            1.0,
            "impulse transient protection",
        )
    }

    pub fn validate_for_format(&self, sample_rate: u32, channels: usize) -> Result<()> {
        self.validate()?;
        validate_format(sample_rate, channels, self.fft_size)?;
        if self.hum.enabled {
            ensure!(
                self.hum.fundamental_hz * (self.hum.harmonics as f32) < 0.49 * sample_rate as f32,
                "hum harmonic reaches the Nyquist boundary"
            );
        }
        Ok(())
    }

    /// Validate every constructor precondition without allocating DSP state.
    /// Profile identity remains required even for a zero-wet bypass.
    pub fn validate_for_processor(
        &self,
        sample_rate: u32,
        channels: usize,
        profile: Option<&NoiseProfile>,
    ) -> Result<()> {
        self.validate_for_format(sample_rate, channels)?;
        if self.mode == DenoiseMode::Profile {
            let Some(profile) = profile else {
                anyhow::bail!("profile denoise requires an explicitly captured noise segment");
            };
            profile.validate(sample_rate, channels, self.fft_size)?;
        }
        Ok(())
    }
}

/// Frozen linked-channel noise PSD. Capture only an explicitly selected noise
/// segment; persist source/range identity separately with the application's job.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoiseProfile {
    pub sample_rate: u32,
    pub channels: usize,
    pub fft_size: usize,
    pub captured_frames: u64,
    /// Raw (unnormalized FFT) bin power for the periodic sqrt-Hann window.
    pub power: Vec<f32>,
}

impl NoiseProfile {
    pub fn validate(&self, sample_rate: u32, channels: usize, fft_size: usize) -> Result<()> {
        validate_format(sample_rate, channels, fft_size)?;
        ensure!(
            self.sample_rate == sample_rate
                && self.channels == channels
                && self.fft_size == fft_size,
            "noise profile format does not match the job"
        );
        ensure!(
            self.captured_frames > 0,
            "noise profile contains no captured frames"
        );
        ensure!(
            self.power.len() == fft_size / 2 + 1,
            "noise profile has wrong bin count"
        );
        ensure!(
            self.power.iter().all(|p| p.is_finite() && *p >= 0.0),
            "noise profile contains invalid power"
        );
        Ok(())
    }
}

#[cfg(feature = "execution")]
mod processor;

#[cfg(feature = "execution")]
pub use processor::{Denoiser, ProfileCollector};

/// Fixed-block graph adapter with declared lookahead prefill and bounded staging.
/// Only `input_samples` are source audio; the rest of a block drains graph delay.
#[cfg(feature = "execution")]
pub struct BlockDenoiser {
    denoiser: Denoiser,
    output: Vec<f32>,
    queue: std::collections::VecDeque<f32>,
    delay_samples: usize,
    max_block_samples: usize,
    finished: bool,
}

#[cfg(feature = "execution")]
impl BlockDenoiser {
    /// Conservative retained bytes for a boxed adapter, without constructing DSP.
    /// Includes the adapter object, all staging and processor buffers, and an FFT
    /// backend allowance; excludes allocator metadata and caller-owned audio.
    /// Graphs that replace a bypassed node with a pass-through need not charge it.
    pub fn estimated_state_bytes(
        config: &DenoiseConfig,
        sample_rate: u32,
        channels: usize,
        max_block_frames: usize,
    ) -> Result<usize> {
        config.validate_for_format(sample_rate, channels)?;
        ensure!(
            (1..=MAX_BLOCK_FRAMES).contains(&max_block_frames),
            "invalid denoise adapter block size"
        );
        let delay_samples = config.lookahead_frames() * channels;
        let block_samples = max_block_frames * channels;
        // Output burst D+B, queue prefill plus burst 2D+B; the embedded
        // Denoiser and its inline repair state are already in size_of::<Self>().
        Ok(size_of::<Self>()
            + (3 * delay_samples + 2 * block_samples) * size_of::<f32>()
            + Denoiser::estimated_heap_bytes(config, channels))
    }

    pub fn new(denoiser: Denoiser, max_block_frames: usize) -> Result<Self> {
        ensure!(
            (1..=MAX_BLOCK_FRAMES).contains(&max_block_frames),
            "invalid denoise adapter block size"
        );
        let channels = denoiser.channels();
        let delay_samples = denoiser.lookahead_frames() * channels;
        let max_block_samples = max_block_frames * channels;
        // Before a call at most lookahead source samples remain withheld.
        // Processing plus finish can emit only those and this call's real input.
        let burst = delay_samples + max_block_samples;
        let mut queue = std::collections::VecDeque::with_capacity(delay_samples + burst);
        queue.resize(delay_samples, 0.0);
        Ok(Self {
            denoiser,
            output: Vec::with_capacity(burst),
            queue,
            delay_samples,
            max_block_samples,
            finished: false,
        })
    }

    pub fn lookahead_frames(&self) -> usize {
        self.denoiser.lookahead_frames()
    }

    /// Finish the source once at EOF, then drain its prefilled playback delay.
    /// Cancellation invalidates the stream until reset, just like `Denoiser`.
    pub fn process(
        &mut self,
        samples: &mut [f32],
        input_samples: usize,
        eof: bool,
        cancel: &mut impl FnMut() -> bool,
    ) -> Result<()> {
        let channels = self.denoiser.channels();
        ensure!(
            samples.len() <= self.max_block_samples
                && samples.len().is_multiple_of(channels)
                && input_samples <= samples.len()
                && input_samples.is_multiple_of(channels),
            "invalid denoise adapter block"
        );
        self.output.clear();
        if input_samples > 0 {
            self.denoiser
                .process_block(&samples[..input_samples], &mut self.output, cancel)?;
        }
        if eof && !self.finished {
            self.denoiser.finish(&mut self.output, cancel)?;
            self.finished = true;
        }
        ensure!(
            self.queue.len() + self.output.len() <= self.queue.capacity(),
            "streaming denoise exceeded its admitted output capacity"
        );
        ensure!(
            self.queue.len() + self.output.len() >= samples.len(),
            "streaming denoise exceeded declared lookahead"
        );
        let queued = self.queue.len().min(samples.len());
        let direct = samples.len() - queued;
        for sample in &mut samples[..queued] {
            *sample = self.queue.pop_front().expect("checked queue length");
        }
        samples[queued..].copy_from_slice(&self.output[..direct]);
        self.queue.extend(self.output[direct..].iter().copied());
        Ok(())
    }

    pub fn reset(&mut self) {
        self.denoiser.reset();
        self.output.clear();
        self.queue.clear();
        self.queue.resize(self.delay_samples, 0.0);
        self.finished = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "execution")]
    mod budget {
        use super::*;
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        // Thread-local accounting keeps these regressions independent of other
        // tests and measures retained requested bytes, not allocator/RSS noise.
        struct Counted;
        thread_local! {
            static BYTES: Cell<Option<(usize, isize)>> = const { Cell::new(None) };
        }
        fn record(allocated: usize, released: usize) {
            let _ = BYTES.try_with(|bytes| {
                if let Some((total, live)) = bytes.get() {
                    bytes.set(Some((
                        total + allocated,
                        live + allocated as isize - released as isize,
                    )));
                }
            });
        }
        // SAFETY: all storage operations delegate unchanged to System; the
        // allocation-free TLS counter neither reads nor mutates that storage.
        unsafe impl GlobalAlloc for Counted {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                let ptr = unsafe { System.alloc(layout) };
                if !ptr.is_null() {
                    record(layout.size(), 0);
                }
                ptr
            }
            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                let ptr = unsafe { System.alloc_zeroed(layout) };
                if !ptr.is_null() {
                    record(layout.size(), 0);
                }
                ptr
            }
            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                record(0, layout.size());
                unsafe { System.dealloc(ptr, layout) };
            }
            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
                let result = unsafe { System.realloc(ptr, layout, size) };
                if !result.is_null() {
                    record(size, layout.size());
                }
                result
            }
        }
        #[global_allocator]
        static ALLOCATOR: Counted = Counted;

        fn measured<T>(f: impl FnOnce() -> T) -> (T, usize, isize) {
            BYTES.with(|bytes| {
                assert!(bytes.replace(Some((0, 0))).is_none());
            });
            let value = f();
            let (total, live) = BYTES.with(|bytes| bytes.replace(None).unwrap());
            (value, total, live)
        }

        #[test]
        fn denoise_state_estimate_covers_retained_allocations_and_staging() {
            const BLOCK: usize = 256;
            for channels in [1, 2, MAX_CHANNELS] {
                for fft_size in [256, 512, 1024, 2048, 4096, 8192] {
                    for mode in [
                        DenoiseMode::Off,
                        DenoiseMode::Stationary,
                        DenoiseMode::Profile,
                        DenoiseMode::Adaptive,
                    ] {
                        for wet in [0.0, 0.5, 1.0] {
                            for hum in [false, true] {
                                for repair in [false, true] {
                                    let mut config = DenoiseConfig {
                                        mode,
                                        fft_size,
                                        wet,
                                        ..Default::default()
                                    };
                                    config.hum.enabled = hum;
                                    config.hum.harmonics = 32;
                                    config.impulse.enabled = repair;
                                    config.impulse.max_width = 8;
                                    let (estimate, allocated, _) = measured(|| {
                                        BlockDenoiser::estimated_state_bytes(
                                            &config, 48_000, channels, BLOCK,
                                        )
                                        .unwrap()
                                    });
                                    assert_eq!(allocated, 0, "estimation must not prepare DSP");
                                    let (mut adapter, _, retained) = measured(|| {
                                        let profile =
                                            (mode == DenoiseMode::Profile).then(|| NoiseProfile {
                                                sample_rate: 48_000,
                                                channels,
                                                fft_size,
                                                captured_frames: 1,
                                                power: vec![0.0; fft_size / 2 + 1],
                                            });
                                        Box::new(
                                            BlockDenoiser::new(
                                                Denoiser::new(
                                                    48_000,
                                                    channels,
                                                    config.clone(),
                                                    profile,
                                                )
                                                .unwrap(),
                                                BLOCK,
                                            )
                                            .unwrap(),
                                        )
                                    });
                                    assert!(
                                        retained >= 0 && estimate >= retained as usize,
                                        "{config:?}, {channels}ch: estimate {estimate}, retained {retained}"
                                    );
                                    if mode == DenoiseMode::Off || config.is_bypassed() {
                                        assert_eq!(
                                            estimate, retained as usize,
                                            "time-domain storage must be counted once, without padding"
                                        );
                                    }
                                    let mut block = vec![0.0; BLOCK * channels];
                                    let (_, allocated, _) = measured(|| {
                                        adapter
                                            .process(
                                                &mut block,
                                                BLOCK * channels,
                                                true,
                                                &mut || false,
                                            )
                                            .unwrap();
                                        adapter.reset();
                                    });
                                    assert_eq!(allocated, 0, "admitted staging must not grow");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(feature = "execution")]
    #[test]
    fn block_adapter_preserves_real_input_eof_reset_and_capacity() {
        const BLOCK: usize = 256;
        for mode in [
            DenoiseMode::Off,
            DenoiseMode::Stationary,
            DenoiseMode::Adaptive,
        ] {
            let mut config = DenoiseConfig {
                mode,
                fft_size: 256,
                wet: 0.5,
                ..Default::default()
            };
            config.impulse.enabled = true;
            config.impulse.max_width = 8;
            config.hum.enabled = true;
            for frames in [0, 1, 255, 256, 257, 1025] {
                let input: Vec<_> = (0..frames * 2)
                    .map(|index| (index as f32 * 0.031).sin() * 0.2)
                    .collect();
                let mut reference = Denoiser::new(48_000, 2, config.clone(), None).unwrap();
                let delay = reference.lookahead_frames() * 2;
                let mut expected = vec![0.0; delay];
                for chunk in input.chunks(74) {
                    reference
                        .process_block(chunk, &mut expected, &mut || false)
                        .unwrap();
                }
                reference.finish(&mut expected, &mut || false).unwrap();
                let mut adapter = BlockDenoiser::new(
                    Denoiser::new(48_000, 2, config.clone(), None).unwrap(),
                    BLOCK,
                )
                .unwrap();
                let capacities = (adapter.output.capacity(), adapter.queue.capacity());
                for _ in 0..2 {
                    let mut actual = Vec::new();
                    let mut position = 0;
                    while position < expected.len() {
                        let count = (expected.len() - position).min(BLOCK * 2);
                        let real = input.len().saturating_sub(position).min(count);
                        // Padded graph samples must never reach the denoiser.
                        let mut block = vec![f32::NAN; count];
                        let start = position.min(input.len());
                        block[..real].copy_from_slice(&input[start..start + real]);
                        adapter
                            .process(
                                &mut block,
                                real,
                                position + count >= input.len(),
                                &mut || false,
                            )
                            .unwrap();
                        actual.extend_from_slice(&block);
                        position += count;
                        assert_eq!(
                            (adapter.output.capacity(), adapter.queue.capacity()),
                            capacities
                        );
                    }
                    // Repeated graph EOF must not finish the source twice.
                    adapter.process(&mut [], 0, true, &mut || false).unwrap();
                    assert_eq!(adapter.denoiser.input_frames(), frames as u64);
                    assert_eq!(adapter.denoiser.output_frames(), frames as u64);
                    assert_eq!(actual, expected);
                    adapter.reset();
                }
                assert!(
                    adapter
                        .process(&mut [0.0; 2], 2, false, &mut || true)
                        .is_err()
                );
                assert!(adapter.process(&mut [], 0, true, &mut || false).is_err());
                adapter.reset();
                adapter.process(&mut [], 0, true, &mut || false).unwrap();
            }
        }
    }

    #[cfg(feature = "execution")]
    #[test]
    fn block_adapter_hum_only_direct_prefix_has_no_delay() {
        const BLOCK: usize = 256;
        let input: Vec<_> = (0..513 * 2)
            .map(|index| (index as f32 * 0.031).sin() * 0.2)
            .collect();
        for wet in [0.5, 1.0] {
            let mut config = DenoiseConfig {
                mode: DenoiseMode::Off,
                wet,
                ..Default::default()
            };
            config.hum.enabled = true;
            let mut reference = Denoiser::new(48_000, 2, config.clone(), None).unwrap();
            let mut expected = Vec::new();
            reference
                .process_block(&input, &mut expected, &mut || false)
                .unwrap();
            reference.finish(&mut expected, &mut || false).unwrap();
            assert_ne!(expected, input);
            let mut adapter =
                BlockDenoiser::new(Denoiser::new(48_000, 2, config, None).unwrap(), BLOCK).unwrap();
            assert_eq!(adapter.lookahead_frames(), 0);
            let mut actual = Vec::new();
            for (index, chunk) in input.chunks(BLOCK * 2).enumerate() {
                let mut block = chunk.to_vec();
                adapter
                    .process(
                        &mut block,
                        chunk.len(),
                        (index + 1) * BLOCK * 2 >= input.len(),
                        &mut || false,
                    )
                    .unwrap();
                actual.extend_from_slice(&block);
            }
            assert_eq!(actual, expected);
            adapter.process(&mut [], 0, true, &mut || false).unwrap();
            let mut excess = [0.25, -0.5];
            assert!(
                adapter
                    .process(&mut excess, 0, true, &mut || false)
                    .is_err()
            );
            assert_eq!(excess, [0.25, -0.5]);
        }
    }

    #[cfg(feature = "execution")]
    #[test]
    fn block_adapter_orders_backlog_direct_prefix_and_empty_eof_burst() {
        const BLOCK: usize = 256;
        let input: Vec<_> = (0..(BLOCK + 193) * 2)
            .map(|index| (index as f32 * 0.031).sin() * 0.2)
            .collect();
        let config = DenoiseConfig {
            mode: DenoiseMode::Stationary,
            fft_size: BLOCK,
            wet: 0.5,
            ..Default::default()
        };
        let mut reference = Denoiser::new(48_000, 2, config.clone(), None).unwrap();
        let mut expected = vec![0.0; reference.lookahead_frames() * 2];
        reference
            .process_block(&input, &mut expected, &mut || false)
            .unwrap();
        reference.finish(&mut expected, &mut || false).unwrap();
        for finish_with_input in [false, true] {
            let mut adapter = BlockDenoiser::new(
                Denoiser::new(48_000, 2, config.clone(), None).unwrap(),
                BLOCK,
            )
            .unwrap();
            let mut actual = Vec::new();
            for (index, chunk) in input.chunks(BLOCK * 2).enumerate() {
                let mut block = chunk.to_vec();
                adapter
                    .process(
                        &mut block,
                        chunk.len(),
                        finish_with_input && index == 1,
                        &mut || false,
                    )
                    .unwrap();
                actual.extend_from_slice(&block);
            }
            // EOF may emit a burst even when the caller requests no playback samples.
            adapter.process(&mut [], 0, true, &mut || false).unwrap();
            adapter.process(&mut [], 0, true, &mut || false).unwrap();
            for frames in [1, 127, 128] {
                let mut block = vec![f32::NAN; frames * 2];
                adapter.process(&mut block, 0, true, &mut || false).unwrap();
                actual.extend_from_slice(&block);
            }
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn validation_rejects_invalid_parameters() {
        let config = DenoiseConfig {
            spectral_gain_floor_db: -18.5,
            ..Default::default()
        };
        for spectral_gain_floor_db in [f32::NAN, f32::INFINITY, -100.1, 0.1] {
            let mut bad = config.clone();
            bad.spectral_gain_floor_db = spectral_gain_floor_db;
            assert!(bad.validate().is_err());
        }
        let mut bad = config.clone();
        bad.strength = f32::NAN;
        assert!(bad.validate().is_err());
        bad = config.clone();
        bad.fft_size = 1000;
        assert!(bad.validate().is_err());
        assert!(config.validate_for_format(7999, 2).is_err());
        assert!(config.validate_for_format(48_000, 0).is_err());
        let mut hum = config;
        hum.hum.enabled = true;
        hum.hum.fundamental_hz = 1000.0;
        hum.hum.harmonics = 5;
        assert!(hum.validate_for_format(8000, 1).is_err());
    }

    #[test]
    fn noise_profile_validation_is_independent_of_execution() {
        let profile = NoiseProfile {
            sample_rate: 48_000,
            channels: 2,
            fft_size: 256,
            captured_frames: 1,
            power: vec![0.0; 129],
        };
        profile.validate(48_000, 2, 256).unwrap();
        assert!(profile.validate(16_000, 2, 256).is_err());
        assert!(profile.validate(48_000, 1, 256).is_err());
        assert!(profile.validate(48_000, 2, 512).is_err());
        let mut invalid = profile.clone();
        invalid.captured_frames = 0;
        assert!(invalid.validate(48_000, 2, 256).is_err());
        invalid = profile.clone();
        invalid.power.pop();
        assert!(invalid.validate(48_000, 2, 256).is_err());
        for power in [f32::NAN, f32::INFINITY, -1.0] {
            invalid = profile.clone();
            invalid.power[0] = power;
            assert!(invalid.validate(48_000, 2, 256).is_err());
        }
    }
}
