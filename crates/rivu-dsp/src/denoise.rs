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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_and_validation_reject_invalid_parameters() {
        let config = DenoiseConfig {
            spectral_gain_floor_db: -18.5,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(config, serde_json::from_str(&json).unwrap());
        assert!(serde_json::from_str::<DenoiseConfig>(r#"{"noise_floor_db":-18.5}"#).is_err());
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
