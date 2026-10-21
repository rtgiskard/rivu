//! Parametric equalization for interleaved PCM, with no callback-time allocation.
//!
//! Coefficients follow Robert Bristow-Johnson's Audio EQ Cookbook:
//! <https://www.w3.org/TR/audio-eq-cookbook/>. Bandpass uses its constant-0-dB
//! peak variant. Shelves use the cookbook's monotonic shelf-slope parameter S.
//! Cascades are repeated identical second-order sections, **not** an implied
//! Butterworth alignment: pass-filter asymptotes are 12 dB/octave per stage;
//! bandpass has 6 dB/octave per stage on each side. Q applies to each section.
//! Every prepared section passes the strict second-order Schur stability test;
//! the reflection-coefficient criterion is described by Julius O. Smith III:
//! <https://ccrma.stanford.edu/~jos/filters/Computing_Reflection_Coefficients_Check.html>.
//!
//! Recursive coefficients never change while a bank is processing. A 10 ms
//! linear crossfade mixes complete, independently stable old/new banks. See
//! Wishnick, DAFx-14, sections 2 and 5, for why stable instantaneous poles alone
//! do not establish stability of coefficient-interpolated recursive filters:
//! <https://www.dafx14.uni-erlangen.de/papers/dafx14_aaron_wishnick_time_varying_filters_for_.pdf>.
//! Three bank slots bound memory and work: two process during a fade, and one
//! holds the latest pending update. Intermediate pending updates are coalesced.
//! Preparation/reconfiguration and destruction belong outside the device callback.
//! There is no limiter, clipping, automatic gain compensation, or source write.

use std::f64::consts::PI;
use std::fmt;

use serde::{Deserialize, Serialize};

pub const MAX_BANDS: usize = 32;
pub const MAX_CHANNELS: usize = 64;
pub const MAX_STAGES: u8 = 4;
pub const TRANSITION_MS: u32 = 10;
/// A small, explicit guard against degenerate poles/zeros at Nyquist.
pub const MAX_FREQUENCY_RATE_RATIO: f64 = 0.499;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterKind {
    #[default]
    Peak,
    LowShelf,
    HighShelf,
    LowPass,
    HighPass,
    BandPass,
    Notch,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelTarget {
    #[default]
    All,
    /// Zero-based interleaved channel indices; nonempty, unique, and in range.
    Selected(Vec<usize>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EqBand {
    pub enabled: bool,
    /// Enabled solo bands exclude all enabled non-solo bands, globally.
    pub solo: bool,
    pub kind: FilterKind,
    /// Center, cutoff, or shelf midpoint in Hz, depending on filter kind.
    pub frequency_hz: f64,
    /// Used by peak and shelves only; [-24, 24] dB.
    pub gain_db: f64,
    /// [0.1, 20], per section; ignored for shelves (which use shelf_slope).
    pub q: f64,
    /// Cookbook S in [0.1, 1]; ignored except for shelves.
    pub shelf_slope: f64,
    /// [1, 4] for pass/bandpass/notch; must be 1 for peak and shelves.
    pub stages: u8,
    pub channels: ChannelTarget,
}

impl Default for EqBand {
    fn default() -> Self {
        Self {
            enabled: true,
            solo: false,
            kind: FilterKind::Peak,
            frequency_hz: 1_000.0,
            gain_db: 0.0,
            q: std::f64::consts::FRAC_1_SQRT_2,
            shelf_slope: 1.0,
            stages: 1,
            channels: ChannelTarget::All,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EqualizerConfig {
    pub enabled: bool,
    /// [-24, 24] dB, before the filters on every channel.
    pub preamp_db: f64,
    /// [-24, 24] dB, after the filters on every channel.
    pub output_gain_db: f64,
    pub bands: Vec<EqBand>,
}

impl Default for EqualizerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            preamp_db: 0.0,
            output_gain_db: 0.0,
            bands: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum EqualizerError {
    InvalidSampleRate(u32),
    InvalidChannelCount(usize),
    TooManyBands(usize),
    InvalidParameter {
        band: Option<usize>,
        parameter: &'static str,
    },
    UnsupportedStages {
        band: usize,
    },
    FrequencyExceedsRate {
        band: usize,
        maximum_hz: f64,
    },
    InvalidChannel {
        channel: usize,
        channels: usize,
    },
    EmptyChannelSelection {
        band: usize,
    },
    DuplicateChannel {
        band: usize,
        channel: usize,
    },
    UnstableCoefficients {
        band: usize,
    },
    InvalidResponseFrequency,
    ResponseLengthMismatch,
    IncompleteFrame {
        samples: usize,
        channels: usize,
    },
    NonFiniteInput {
        sample: usize,
    },
    NonFiniteOutput {
        sample: usize,
    },
}

impl fmt::Display for EqualizerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate(rate) => {
                write!(f, "EQ sample rate {rate} is outside 8000..=768000 Hz")
            }
            Self::InvalidChannelCount(count) => {
                write!(f, "EQ channel count {count} is outside 1..={MAX_CHANNELS}")
            }
            Self::TooManyBands(count) => write!(f, "EQ has {count} bands; maximum is {MAX_BANDS}"),
            Self::InvalidParameter { band, parameter } => {
                write!(f, "invalid EQ {parameter} (band {band:?})")
            }
            Self::UnsupportedStages { band } => {
                write!(f, "EQ band {band} cannot cascade this filter kind")
            }
            Self::FrequencyExceedsRate { band, maximum_hz } => write!(
                f,
                "EQ band {band} exceeds the rate-dependent maximum {maximum_hz} Hz"
            ),
            Self::InvalidChannel { channel, channels } => write!(
                f,
                "EQ channel {channel} is outside the {channels}-channel stream"
            ),
            Self::EmptyChannelSelection { band } => {
                write!(f, "EQ band {band} has an empty channel selection")
            }
            Self::DuplicateChannel { band, channel } => {
                write!(f, "EQ band {band} selects channel {channel} twice")
            }
            Self::UnstableCoefficients { band } => write!(
                f,
                "EQ band {band} did not produce finite, strictly stable coefficients"
            ),
            Self::InvalidResponseFrequency => {
                f.write_str("EQ response frequency must be finite and between DC and Nyquist")
            }
            Self::ResponseLengthMismatch => f.write_str("EQ response input/output lengths differ"),
            Self::IncompleteFrame { samples, channels } => write!(
                f,
                "EQ buffer of {samples} samples does not contain whole {channels}-channel frames"
            ),
            Self::NonFiniteInput { sample } => write!(f, "EQ input sample {sample} is not finite"),
            Self::NonFiniteOutput { sample } => write!(
                f,
                "EQ output sample {sample} is not representable as finite f32; reduce gain"
            ),
        }
    }
}

impl std::error::Error for EqualizerError {}

impl EqualizerConfig {
    /// Validate persistent parameters independently of a particular stream.
    /// Disabled and non-solo bands are also validated, so invalid latent settings
    /// cannot become active merely by toggling a control.
    pub fn validate(&self) -> Result<(), EqualizerError> {
        if self.bands.len() > MAX_BANDS {
            return Err(EqualizerError::TooManyBands(self.bands.len()));
        }
        validate_number(self.preamp_db, -24.0, 24.0, None, "preamp_db")?;
        validate_number(self.output_gain_db, -24.0, 24.0, None, "output_gain_db")?;
        for (index, band) in self.bands.iter().enumerate() {
            validate_number(
                band.frequency_hz,
                1.0,
                96_000.0,
                Some(index),
                "frequency_hz",
            )?;
            validate_number(band.gain_db, -24.0, 24.0, Some(index), "gain_db")?;
            validate_number(band.q, 0.1, 20.0, Some(index), "q")?;
            validate_number(band.shelf_slope, 0.1, 1.0, Some(index), "shelf_slope")?;
            if !(1..=MAX_STAGES).contains(&band.stages) {
                return Err(EqualizerError::InvalidParameter {
                    band: Some(index),
                    parameter: "stages",
                });
            }
            if band.stages != 1
                && matches!(
                    band.kind,
                    FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf
                )
            {
                return Err(EqualizerError::UnsupportedStages { band: index });
            }
            if let ChannelTarget::Selected(channels) = &band.channels {
                if channels.is_empty() {
                    return Err(EqualizerError::EmptyChannelSelection { band: index });
                }
                let mut mask = 0_u64;
                for &channel in channels {
                    if channel >= MAX_CHANNELS {
                        return Err(EqualizerError::InvalidChannel {
                            channel,
                            channels: MAX_CHANNELS,
                        });
                    }
                    let bit = 1_u64 << channel;
                    if mask & bit != 0 {
                        return Err(EqualizerError::DuplicateChannel {
                            band: index,
                            channel,
                        });
                    }
                    mask |= bit;
                }
            }
        }
        Ok(())
    }

    /// Applied frequencies above 0.499 * sample_rate are rejected, never
    /// clamped, dropped, or retuned. Dormant bands (disabled or excluded by
    /// solo) retain their stored values without imposing stream constraints.
    /// Disabled EQ accepts any nonzero stream format and remains exact bypass.
    pub fn validate_for_stream(
        &self,
        sample_rate: u32,
        channels: usize,
    ) -> Result<(), EqualizerError> {
        self.validate()?;
        if sample_rate == 0 {
            return Err(EqualizerError::InvalidSampleRate(sample_rate));
        }
        if channels == 0 {
            return Err(EqualizerError::InvalidChannelCount(channels));
        }
        if !self.enabled {
            return Ok(());
        }
        if !(8_000..=768_000).contains(&sample_rate) {
            return Err(EqualizerError::InvalidSampleRate(sample_rate));
        }
        if channels > MAX_CHANNELS {
            return Err(EqualizerError::InvalidChannelCount(channels));
        }
        let maximum_hz = f64::from(sample_rate) * MAX_FREQUENCY_RATE_RATIO;
        let solo = self.bands.iter().any(|band| band.enabled && band.solo);
        for (index, band) in self.bands.iter().enumerate() {
            if !band.enabled || (solo && !band.solo) {
                continue;
            }
            if band.frequency_hz > maximum_hz {
                return Err(EqualizerError::FrequencyExceedsRate {
                    band: index,
                    maximum_hz,
                });
            }
            if let ChannelTarget::Selected(selected) = &band.channels {
                for &channel in selected {
                    if channel >= channels {
                        return Err(EqualizerError::InvalidChannel { channel, channels });
                    }
                }
            }
        }
        Ok(())
    }

    /// Plot this config without constructing PCM histories or a runtime. Output
    /// is settled dB including bypass, solo, channel targeting, and both gains.
    /// Only bounded coefficient storage is prepared; call outside render/audio
    /// hot paths and cache until config or stream format changes. True zeros
    /// produce negative infinity. Errors leave the output buffer unchanged.
    pub fn sample_response_db(
        &self,
        sample_rate: u32,
        channels: usize,
        channel: usize,
        frequencies_hz: &[f64],
        decibels: &mut [f64],
    ) -> Result<(), EqualizerError> {
        if frequencies_hz.len() != decibels.len() {
            return Err(EqualizerError::ResponseLengthMismatch);
        }
        let bank = Bank::prepare_response(self, sample_rate, channels)?;
        if channel >= channels {
            return Err(EqualizerError::InvalidChannel { channel, channels });
        }
        for &frequency in frequencies_hz {
            if !frequency.is_finite() || !(0.0..=f64::from(sample_rate) * 0.5).contains(&frequency)
            {
                return Err(EqualizerError::InvalidResponseFrequency);
            }
        }
        for (&frequency, decibel) in frequencies_hz.iter().zip(decibels) {
            *decibel = 20.0
                * bank
                    .magnitude(2.0 * PI * frequency / f64::from(sample_rate), channel)
                    .log10();
        }
        Ok(())
    }
}

fn validate_number(
    value: f64,
    min: f64,
    max: f64,
    band: Option<usize>,
    parameter: &'static str,
) -> Result<(), EqualizerError> {
    if !value.is_finite() || !(min..=max).contains(&value) {
        return Err(EqualizerError::InvalidParameter { band, parameter });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct Coefficients {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Coefficients {
    fn prepare(band: &EqBand, sample_rate: u32, index: usize) -> Result<Self, EqualizerError> {
        let omega = 2.0 * PI * band.frequency_hz / f64::from(sample_rate);
        let (sin, cos) = omega.sin_cos();
        let a = 10.0_f64.powf(band.gain_db / 40.0);
        let alpha = if matches!(band.kind, FilterKind::LowShelf | FilterKind::HighShelf) {
            sin * 0.5 * ((a + a.recip()) * (band.shelf_slope.recip() - 1.0) + 2.0).sqrt()
        } else {
            sin / (2.0 * band.q)
        };
        let shelf = 2.0 * a.sqrt() * alpha;
        let (b0, b1, b2, a0, a1, a2) = match band.kind {
            FilterKind::Peak => (
                1.0 + alpha * a,
                -2.0 * cos,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos,
                1.0 - alpha / a,
            ),
            FilterKind::LowPass => (
                (1.0 - cos) * 0.5,
                1.0 - cos,
                (1.0 - cos) * 0.5,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            FilterKind::HighPass => (
                (1.0 + cos) * 0.5,
                -(1.0 + cos),
                (1.0 + cos) * 0.5,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            FilterKind::BandPass => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            FilterKind::Notch => (1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            FilterKind::LowShelf => (
                a * ((a + 1.0) - (a - 1.0) * cos + shelf),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - shelf),
                (a + 1.0) + (a - 1.0) * cos + shelf,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - shelf,
            ),
            FilterKind::HighShelf => (
                a * ((a + 1.0) + (a - 1.0) * cos + shelf),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - shelf),
                (a + 1.0) - (a - 1.0) * cos + shelf,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - shelf,
            ),
        };
        let result = Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        };
        // Strict second-order Schur/Jury conditions for 1 + a1 z^-1 + a2 z^-2.
        // Reject numerical degeneracy instead of hiding it with state clamping.
        if ![result.b0, result.b1, result.b2, result.a1, result.a2]
            .iter()
            .all(|v| v.is_finite())
            || result.a2.abs() >= 1.0
            || 1.0 + result.a1 + result.a2 <= 0.0
            || 1.0 - result.a1 + result.a2 <= 0.0
        {
            return Err(EqualizerError::UnstableCoefficients { band: index });
        }
        Ok(result)
    }

    fn magnitude(self, omega: f64) -> f64 {
        let (s1, c1) = omega.sin_cos();
        let (s2, c2) = (2.0 * omega).sin_cos();
        let nr = self.b0 + self.b1 * c1 + self.b2 * c2;
        let ni = -self.b1 * s1 - self.b2 * s2;
        let dr = 1.0 + self.a1 * c1 + self.a2 * c2;
        let di = -self.a1 * s1 - self.a2 * s2;
        nr.hypot(ni) / dr.hypot(di)
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
struct State {
    z1: f64,
    z2: f64,
}

#[cfg(test)]
impl State {
    fn process(&mut self, input: f64, c: Coefficients) -> f64 {
        let output = c.b0 * input + self.z1;
        self.z1 = c.b1 * input - c.a1 * output + self.z2;
        self.z2 = c.b2 * input - c.a2 * output;
        output
    }
}

struct Section {
    coefficients: Coefficients,
    channel_mask: u64,
}

struct Bank {
    enabled: bool,
    preamp: f64,
    output_gain: f64,
    sections: Vec<Section>,
    // Per section: contiguous channel z1 plane, then channel z2 plane.
    // A single allocation retains the original two-f64-per-channel footprint,
    // while independent channel lanes can vectorize without state shuffles.
    states: Vec<f64>,
}

impl Bank {
    fn empty() -> Self {
        Self {
            enabled: false,
            preamp: 1.0,
            output_gain: 1.0,
            sections: Vec::new(),
            states: Vec::new(),
        }
    }

    fn prepare(
        config: &EqualizerConfig,
        sample_rate: u32,
        channels: usize,
    ) -> Result<Self, EqualizerError> {
        let mut bank = Self::prepare_response(config, sample_rate, channels)?;
        bank.states.resize(bank.sections.len() * channels * 2, 0.0);
        Ok(bank)
    }

    fn prepare_response(
        config: &EqualizerConfig,
        sample_rate: u32,
        channels: usize,
    ) -> Result<Self, EqualizerError> {
        config.validate_for_stream(sample_rate, channels)?;
        let mut bank = Self::empty();
        bank.enabled = config.enabled;
        bank.preamp = 10.0_f64.powf(config.preamp_db / 20.0);
        bank.output_gain = 10.0_f64.powf(config.output_gain_db / 20.0);
        let solo = config.bands.iter().any(|band| band.enabled && band.solo);
        for (index, band) in config.bands.iter().enumerate() {
            if !config.enabled || !band.enabled || (solo && !band.solo) {
                continue;
            }
            let coefficients = Coefficients::prepare(band, sample_rate, index)?;
            // A zero-gain peak/shelf is mathematically the identity; omit it to
            // preserve exact identity and avoid unnecessary recursive state.
            if band.gain_db == 0.0
                && matches!(
                    band.kind,
                    FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf
                )
            {
                continue;
            }
            let mask = match &band.channels {
                ChannelTarget::All => u64::MAX,
                ChannelTarget::Selected(selected) => {
                    selected.iter().fold(0, |mask, &ch| mask | (1_u64 << ch))
                }
            };
            for _ in 0..band.stages {
                bank.sections.push(Section {
                    coefficients,
                    channel_mask: mask,
                });
            }
        }
        Ok(bank)
    }

    #[inline]
    fn process_sample(&mut self, input: f64, channel: usize, channels: usize) -> f64 {
        if !self.enabled {
            return input;
        }
        let mut output = input * self.preamp;
        for (section, states) in self
            .sections
            .iter()
            .zip(self.states.chunks_exact_mut(channels * 2))
        {
            if section.channel_mask & (1_u64 << channel) != 0 {
                let input = output;
                let c = section.coefficients;
                output = c.b0 * input + states[channel];
                states[channel] = c.b1 * input - c.a1 * output + states[channels + channel];
                states[channels + channel] = c.b2 * input - c.a2 * output;
            }
        }
        output * self.output_gain
    }

    #[inline]
    fn process_frame(&mut self, frame: &mut [f64]) {
        if !self.enabled {
            return;
        }
        let channels = frame.len();
        for sample in &mut *frame {
            *sample *= self.preamp;
        }
        let all_channels = u64::MAX >> (MAX_CHANNELS - channels);
        for (section, states) in self
            .sections
            .iter()
            .zip(self.states.chunks_exact_mut(channels * 2))
        {
            let (z1, z2) = states.split_at_mut(channels);
            let c = section.coefficients;
            if section.channel_mask & all_channels == all_channels {
                // Branchless independent f64 lanes; deliberately retain the
                // original multiply/add order, without fast-math or f32 state.
                for ((sample, z1), z2) in frame.iter_mut().zip(z1).zip(z2) {
                    let input = *sample;
                    let output = c.b0 * input + *z1;
                    *z1 = c.b1 * input - c.a1 * output + *z2;
                    *z2 = c.b2 * input - c.a2 * output;
                    *sample = output;
                }
            } else {
                let mut selected = section.channel_mask & all_channels;
                while selected != 0 {
                    let channel = selected.trailing_zeros() as usize;
                    selected &= selected - 1;
                    let input = frame[channel];
                    let output = c.b0 * input + z1[channel];
                    z1[channel] = c.b1 * input - c.a1 * output + z2[channel];
                    z2[channel] = c.b2 * input - c.a2 * output;
                    frame[channel] = output;
                }
            }
        }
        for sample in frame {
            *sample *= self.output_gain;
        }
    }

    fn magnitude(&self, omega: f64, channel: usize) -> f64 {
        if !self.enabled {
            return 1.0;
        }
        let mut magnitude = self.preamp * self.output_gain;
        for section in &self.sections {
            if section.channel_mask & (1_u64 << channel) != 0 {
                magnitude *= section.coefficients.magnitude(omega);
            }
        }
        magnitude
    }
}

/// Prepared, fixed-format equalizer. Maximum processing cost is two banks of
/// MAX_BANDS * MAX_STAGES sections per sample; maximum state storage is three
/// such banks times MAX_CHANNELS for active filtering. New disabled instances
/// allocate no PCM histories; bypass accepts any nonzero channel count/rate.
/// The format never changes; construct a new instance for a format change.
pub struct Equalizer {
    config: EqualizerConfig,
    sample_rate: u32,
    channels: usize,
    banks: [Bank; 3],
    active: usize,
    retiring: Option<usize>,
    pending: Option<usize>,
    fade_position: usize,
    fade_frames: usize,
}

impl Equalizer {
    pub fn new(
        config: &EqualizerConfig,
        sample_rate: u32,
        channels: usize,
    ) -> Result<Self, EqualizerError> {
        let bank = Bank::prepare(config, sample_rate, channels)?;
        Ok(Self {
            config: config.clone(),
            sample_rate,
            channels,
            banks: [bank, Bank::empty(), Bank::empty()],
            active: 0,
            retiring: None,
            pending: None,
            fade_position: 0,
            fade_frames: (sample_rate as usize * TRANSITION_MS as usize / 1_000).max(2),
        })
    }

    pub fn config(&self) -> &EqualizerConfig {
        &self.config
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Prepare a complete bank outside the callback. Errors leave the current
    /// configuration and all histories unchanged. During a transition, retain
    /// only the latest pending bank; never interrupt a fade or interpolate poles.
    /// Without further updates, the target is reached within at most two fades.
    pub fn reconfigure(&mut self, config: &EqualizerConfig) -> Result<(), EqualizerError> {
        if *config == self.config {
            return Ok(());
        }
        let bank = Bank::prepare(config, self.sample_rate, self.channels)?;
        let slot = (0..3)
            .find(|&slot| slot != self.active && Some(slot) != self.retiring)
            .expect("three bank slots");
        self.banks[slot] = bank;
        self.config = config.clone();
        if self.is_bypassed() && !config.enabled {
            self.active = slot;
            return Ok(());
        }
        if self.retiring.is_some() {
            self.pending = Some(slot);
        } else {
            self.retiring = Some(self.active);
            self.active = slot;
            self.fade_position = 0;
        }
        Ok(())
    }

    /// Clear all histories, cancel transitions, and immediately select the most
    /// recently requested config. Use at seek/discontinuity before feeding new
    /// audio; audio from before reset can never leak into the new timeline.
    pub fn reset(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.active = pending;
        }
        self.retiring = None;
        self.fade_position = 0;
        for bank in &mut self.banks {
            bank.states.fill(0.0);
        }
    }

    pub fn is_transitioning(&self) -> bool {
        self.retiring.is_some()
    }

    /// True only when bypass can safely borrow original PCM without processing
    /// or advancing a transition. A disable request fades before this is true.
    pub fn is_bypassed(&self) -> bool {
        self.retiring.is_none() && !self.banks[self.active].enabled
    }

    /// Process whole interleaved frames in place, without allocating, freeing,
    /// locking, or changing coefficients. Settled disabled bypass preserves
    /// every sample bit (including signed zero and non-finite payloads).
    /// Invalid lengths and non-finite active inputs fail before mutation.
    /// Output overflow is reported, never limited or replaced with silence;
    /// the already processed prefix/state may have advanced on that error.
    pub fn process(&mut self, samples: &mut [f32]) -> Result<(), EqualizerError> {
        if !samples.len().is_multiple_of(self.channels) {
            return Err(EqualizerError::IncompleteFrame {
                samples: samples.len(),
                channels: self.channels,
            });
        }
        if self.retiring.is_none() && !self.banks[self.active].enabled {
            return Ok(());
        }
        if let Some(sample) = samples.iter().position(|sample| !sample.is_finite()) {
            return Err(EqualizerError::NonFiniteInput { sample });
        }
        // Stereo/mono have too few independent lanes to repay frame staging.
        // Retain channel-major scheduling for those common configurations.
        if self.channels <= 2 {
            return self.process_scalar_channels(samples);
        }
        // Only active filtering reaches here; it validates at most 64 channels.
        // Scratch is fixed-size stack storage, reused across every frame/bank.
        let mut target_frame = [0.0; MAX_CHANNELS];
        let mut previous_frame = [0.0; MAX_CHANNELS];
        for (frame_index, frame) in samples.chunks_exact_mut(self.channels).enumerate() {
            let target_frame = &mut target_frame[..self.channels];
            for (target, &sample) in target_frame.iter_mut().zip(&*frame) {
                *target = f64::from(sample);
            }
            if let Some(old) = self.retiring {
                previous_frame[..self.channels].copy_from_slice(target_frame);
                self.banks[old].process_frame(&mut previous_frame[..self.channels]);
            }
            self.banks[self.active].process_frame(target_frame);
            let weight = if self.retiring.is_some() {
                self.fade_position as f64 / (self.fade_frames - 1) as f64
            } else {
                0.0
            };
            for (channel, sample) in frame.iter_mut().enumerate() {
                let target = target_frame[channel];
                let output = if self.retiring.is_some() {
                    let previous = previous_frame[channel];
                    if previous == target {
                        target
                    } else {
                        previous * (1.0 - weight) + target * weight
                    }
                } else {
                    target
                };
                let output = output as f32;
                if !output.is_finite() {
                    return Err(EqualizerError::NonFiniteOutput {
                        sample: frame_index * self.channels + channel,
                    });
                }
                *sample = output;
            }
            self.advance_frame();
        }
        Ok(())
    }

    fn process_scalar_channels(&mut self, samples: &mut [f32]) -> Result<(), EqualizerError> {
        for (frame_index, frame) in samples.chunks_exact_mut(self.channels).enumerate() {
            let weight = if self.retiring.is_some() {
                self.fade_position as f64 / (self.fade_frames - 1) as f64
            } else {
                0.0
            };
            for (channel, sample) in frame.iter_mut().enumerate() {
                let input = f64::from(*sample);
                let target = self.banks[self.active].process_sample(input, channel, self.channels);
                let output = if let Some(old) = self.retiring {
                    let previous = self.banks[old].process_sample(input, channel, self.channels);
                    if previous == target {
                        target
                    } else {
                        previous * (1.0 - weight) + target * weight
                    }
                } else {
                    target
                };
                let output = output as f32;
                if !output.is_finite() {
                    return Err(EqualizerError::NonFiniteOutput {
                        sample: frame_index * self.channels + channel,
                    });
                }
                *sample = output;
            }
            self.advance_frame();
        }
        Ok(())
    }

    #[inline]
    fn advance_frame(&mut self) {
        if self.retiring.is_some() {
            self.fade_position += 1;
            if self.fade_position == self.fade_frames {
                self.retiring = None;
                self.fade_position = 0;
                if let Some(next) = self.pending.take() {
                    self.retiring = Some(self.active);
                    self.active = next;
                }
            }
        }
    }

    /// Magnitude of the latest requested **settled** configuration, including
    /// solo, targeting, preamp, and output gain; not an instantaneous fade plot.
    /// DC and Nyquist are valid response queries. True zeros return zero.
    pub fn response_magnitude(
        &self,
        frequency_hz: f64,
        channel: usize,
    ) -> Result<f64, EqualizerError> {
        self.validate_response_query(frequency_hz, channel)?;
        let bank = self.pending.unwrap_or(self.active);
        Ok(self.banks[bank].magnitude(
            2.0 * PI * frequency_hz / f64::from(self.sample_rate),
            channel,
        ))
    }

    /// True response zeros are represented as negative infinity, not hidden by
    /// a display floor. A GUI may apply its own explicit plotting floor.
    pub fn response_db(&self, frequency_hz: f64, channel: usize) -> Result<f64, EqualizerError> {
        Ok(20.0 * self.response_magnitude(frequency_hz, channel)?.log10())
    }

    /// Sample a GUI-owned frequency grid into a caller-owned linear-magnitude
    /// buffer. Input validation is atomic and neither buffer is allocated here.
    pub fn sample_response(
        &self,
        frequencies_hz: &[f64],
        channel: usize,
        magnitudes: &mut [f64],
    ) -> Result<(), EqualizerError> {
        if frequencies_hz.len() != magnitudes.len() {
            return Err(EqualizerError::ResponseLengthMismatch);
        }
        self.validate_response_query(0.0, channel)?;
        for &frequency in frequencies_hz {
            self.validate_response_query(frequency, channel)?;
        }
        let bank = &self.banks[self.pending.unwrap_or(self.active)];
        for (&frequency, magnitude) in frequencies_hz.iter().zip(magnitudes) {
            *magnitude =
                bank.magnitude(2.0 * PI * frequency / f64::from(self.sample_rate), channel);
        }
        Ok(())
    }

    fn validate_response_query(
        &self,
        frequency_hz: f64,
        channel: usize,
    ) -> Result<(), EqualizerError> {
        if channel >= self.channels {
            return Err(EqualizerError::InvalidChannel {
                channel,
                channels: self.channels,
            });
        }
        if !frequency_hz.is_finite()
            || !(0.0..=f64::from(self.sample_rate) * 0.5).contains(&frequency_hz)
        {
            return Err(EqualizerError::InvalidResponseFrequency);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(kind: FilterKind) -> EqualizerConfig {
        EqualizerConfig {
            enabled: true,
            bands: vec![EqBand {
                kind,
                gain_db: 12.0,
                ..EqBand::default()
            }],
            ..EqualizerConfig::default()
        }
    }

    fn close(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "actual {actual}, expected {expected}, tolerance {tolerance}"
        );
    }

    // Independent analog prototypes with bilinear prewarping, not evaluation
    // of the production digital coefficients. See the cookbook's H(s) forms.
    fn prototype_db(band: &EqBand, frequency: f64, sample_rate: u32) -> f64 {
        let u = (PI * frequency / f64::from(sample_rate)).tan()
            / (PI * band.frequency_hz / f64::from(sample_rate)).tan();
        let a = 10.0_f64.powf(band.gain_db / 40.0);
        let one_minus_u2 = 1.0 - u * u;
        let shelf_damping = ((a + a.recip()) * (band.shelf_slope.recip() - 1.0) + 2.0).sqrt();
        let (nr, ni, dr, di) = match band.kind {
            FilterKind::Peak => (one_minus_u2, u * a / band.q, one_minus_u2, u / (a * band.q)),
            FilterKind::LowPass => (1.0, 0.0, one_minus_u2, u / band.q),
            FilterKind::HighPass => (-u * u, 0.0, one_minus_u2, u / band.q),
            FilterKind::BandPass => (0.0, u / band.q, one_minus_u2, u / band.q),
            FilterKind::Notch => (one_minus_u2, 0.0, one_minus_u2, u / band.q),
            FilterKind::LowShelf => (
                a * (a - u * u),
                a * a.sqrt() * u * shelf_damping,
                1.0 - a * u * u,
                a.sqrt() * u * shelf_damping,
            ),
            FilterKind::HighShelf => (
                a * (1.0 - a * u * u),
                a * a.sqrt() * u * shelf_damping,
                a - u * u,
                a.sqrt() * u * shelf_damping,
            ),
        };
        20.0 * (nr.hypot(ni) / dr.hypot(di)).log10() * f64::from(band.stages)
    }

    #[test]
    fn plotted_response_matches_independent_prewarped_prototypes() {
        for kind in [
            FilterKind::Peak,
            FilterKind::LowShelf,
            FilterKind::HighShelf,
            FilterKind::LowPass,
            FilterKind::HighPass,
            FilterKind::BandPass,
            FilterKind::Notch,
        ] {
            for gain in [-24.0, 6.0, 24.0] {
                for q in [0.1, 0.707, 20.0] {
                    let mut settings = config(kind);
                    settings.bands[0].gain_db = gain;
                    settings.bands[0].q = q;
                    settings.bands[0].shelf_slope = 0.3;
                    let eq = Equalizer::new(&settings, 48_000, 1).unwrap();
                    for frequency in [20.0, 100.0, 800.0, 1_500.0, 4_000.0, 12_000.0, 23_000.0] {
                        close(
                            eq.response_db(frequency, 0).unwrap(),
                            prototype_db(&settings.bands[0], frequency, 48_000),
                            0.1,
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn textbook_center_and_endpoint_responses() {
        let peak = Equalizer::new(&config(FilterKind::Peak), 48_000, 2).unwrap();
        close(peak.response_db(1_000.0, 0).unwrap(), 12.0, 1e-9);
        close(peak.response_db(0.0, 0).unwrap(), 0.0, 1e-9);
        close(peak.response_db(24_000.0, 0).unwrap(), 0.0, 1e-9);
        for (kind, dc, nyquist) in [
            (FilterKind::LowShelf, 12.0, 0.0),
            (FilterKind::HighShelf, 0.0, 12.0),
        ] {
            let eq = Equalizer::new(&config(kind), 48_000, 1).unwrap();
            close(eq.response_db(0.0, 0).unwrap(), dc, 1e-9);
            close(eq.response_db(1_000.0, 0).unwrap(), 6.0, 1e-9);
            close(eq.response_db(24_000.0, 0).unwrap(), nyquist, 1e-9);
        }
        for kind in [FilterKind::LowPass, FilterKind::HighPass] {
            let eq = Equalizer::new(&config(kind), 48_000, 1).unwrap();
            close(eq.response_db(1_000.0, 0).unwrap(), -3.010_299_956_64, 1e-9);
        }
        let bandpass = Equalizer::new(&config(FilterKind::BandPass), 48_000, 1).unwrap();
        close(bandpass.response_db(1_000.0, 0).unwrap(), 0.0, 1e-9);
        let notch = Equalizer::new(&config(FilterKind::Notch), 48_000, 1).unwrap();
        assert!(notch.response_db(1_000.0, 0).unwrap() < -200.0);
    }

    #[test]
    fn boost_cut_pairs_are_inverse() {
        for kind in [
            FilterKind::Peak,
            FilterKind::LowShelf,
            FilterKind::HighShelf,
        ] {
            let mut settings = config(kind);
            let mut inverse = settings.bands[0].clone();
            inverse.gain_db = -inverse.gain_db;
            settings.bands.push(inverse);
            let eq = Equalizer::new(&settings, 48_000, 1).unwrap();
            for frequency in [0.0, 10.0, 100.0, 1_000.0, 8_000.0, 23_900.0] {
                close(eq.response_db(frequency, 0).unwrap(), 0.0, 1e-8);
            }
        }
    }

    #[test]
    fn cascades_multiply_response_not_gain_settings() {
        for kind in [
            FilterKind::LowPass,
            FilterKind::HighPass,
            FilterKind::BandPass,
            FilterKind::Notch,
        ] {
            let mut settings = config(kind);
            let single = Equalizer::new(&settings, 48_000, 1).unwrap();
            settings.bands[0].stages = 4;
            let cascade = Equalizer::new(&settings, 48_000, 1).unwrap();
            close(
                cascade.response_db(2_000.0, 0).unwrap(),
                4.0 * single.response_db(2_000.0, 0).unwrap(),
                1e-9,
            );
        }
    }

    #[test]
    fn measured_sine_gain_matches_response() {
        for kind in [
            FilterKind::Peak,
            FilterKind::LowShelf,
            FilterKind::HighShelf,
            FilterKind::LowPass,
            FilterKind::HighPass,
            FilterKind::BandPass,
            FilterKind::Notch,
        ] {
            let mut eq = Equalizer::new(&config(kind), 48_000, 1).unwrap();
            let frequency = 1_500.0;
            let mut samples: Vec<f32> = (0..48_000)
                .map(|n| (0.01 * (2.0 * PI * frequency * n as f64 / 48_000.0).sin()) as f32)
                .collect();
            eq.process(&mut samples).unwrap();
            let power: f64 = samples[24_000..]
                .iter()
                .map(|&v| f64::from(v).powi(2))
                .sum::<f64>()
                / 24_000.0;
            let measured = (power * 2.0).sqrt() / 0.01;
            close(measured, eq.response_magnitude(frequency, 0).unwrap(), 1e-6);
            close(
                20.0 * measured.log10(),
                prototype_db(&config(kind).bands[0], frequency, 48_000),
                0.1,
            );
        }
    }

    #[test]
    fn disabled_bypass_is_bit_exact() {
        let mut settings = config(FilterKind::LowPass);
        settings.enabled = false;
        settings.preamp_db = 24.0;
        let mut eq = Equalizer::new(&settings, 48_000, 2).unwrap();
        let bits = [
            0_u32,
            0x8000_0000,
            0x7fc0_0123,
            f32::INFINITY.to_bits(),
            0x3f12_3456,
            0xbf12_3456,
        ];
        let mut samples = bits.map(f32::from_bits);
        eq.process(&mut samples).unwrap();
        assert_eq!(samples.map(f32::to_bits), bits);
        close(eq.response_magnitude(1_000.0, 0).unwrap(), 1.0, 0.0);
    }

    #[test]
    fn selected_channels_and_histories_are_isolated() {
        let mut settings = config(FilterKind::Peak);
        settings.bands[0].channels = ChannelTarget::Selected(vec![1]);
        let mut eq = Equalizer::new(&settings, 48_000, 3).unwrap();
        let mut samples = vec![0.0; 3 * 1_024];
        samples[0] = 0.3;
        samples[1] = 0.2;
        eq.process(&mut samples).unwrap();
        assert_eq!(samples[0], 0.3);
        assert!(
            samples
                .as_chunks::<3>()
                .0
                .iter()
                .skip(1)
                .all(|frame| frame[0] == 0.0)
        );
        assert!(
            samples
                .as_chunks::<3>()
                .0
                .iter()
                .all(|frame| frame[2] == 0.0)
        );
        assert!(
            samples
                .as_chunks::<3>()
                .0
                .iter()
                .skip(1)
                .any(|frame| frame[1] != 0.0)
        );
        close(eq.response_db(1_000.0, 0).unwrap(), 0.0, 0.0);
        close(eq.response_db(1_000.0, 1).unwrap(), 12.0, 1e-9);
    }

    #[test]
    fn maximum_channel_index_is_supported() {
        let mut settings = config(FilterKind::Peak);
        settings.bands[0].channels = ChannelTarget::Selected(vec![63]);
        let mut eq = Equalizer::new(&settings, 48_000, 64).unwrap();
        let mut samples = [0.25; 64];
        eq.process(&mut samples).unwrap();
        assert!(samples[..63].iter().all(|&sample| sample == 0.25));
        assert_ne!(samples[63], 0.25);
    }

    #[test]
    fn solo_ignores_disabled_solo_and_excludes_non_solo() {
        let mut settings = config(FilterKind::Peak);
        let mut solo = settings.bands[0].clone();
        solo.solo = true;
        solo.gain_db = -6.0;
        settings.bands.push(solo);
        let eq = Equalizer::new(&settings, 48_000, 1).unwrap();
        close(eq.response_db(1_000.0, 0).unwrap(), -6.0, 1e-9);
        settings.bands[1].enabled = false;
        let eq = Equalizer::new(&settings, 48_000, 1).unwrap();
        close(eq.response_db(1_000.0, 0).unwrap(), 12.0, 1e-9);
    }

    #[test]
    fn reset_clears_history_and_selects_latest_pending_config() {
        let mut settings = config(FilterKind::LowPass);
        let mut eq = Equalizer::new(&settings, 48_000, 1).unwrap();
        eq.process(&mut [1.0]).unwrap();
        settings.bands[0].frequency_hz = 2_000.0;
        eq.reconfigure(&settings).unwrap();
        settings.bands[0].frequency_hz = 4_000.0;
        eq.reconfigure(&settings).unwrap();
        eq.reset();
        assert!(!eq.is_transitioning());
        let mut silence = [0.0; 1_024];
        eq.process(&mut silence).unwrap();
        assert!(silence.iter().all(|&sample| sample == 0.0));
        let mut fresh = Equalizer::new(&settings, 48_000, 1).unwrap();
        let mut a = [0.0; 1_024];
        a[0] = 1.0;
        let mut b = a;
        eq.process(&mut a).unwrap();
        fresh.process(&mut b).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn gain_transition_is_continuous_and_finishes_exactly() {
        let mut settings = EqualizerConfig {
            enabled: true,
            ..EqualizerConfig::default()
        };
        let mut eq = Equalizer::new(&settings, 48_000, 2).unwrap();
        settings.output_gain_db = -12.0;
        eq.reconfigure(&settings).unwrap();
        let mut samples = vec![1.0; 2 * 600];
        eq.process(&mut samples).unwrap();
        assert_eq!(samples[0], 1.0);
        assert_eq!(samples[1], 1.0);
        let target = 10.0_f32.powf(-12.0 / 20.0);
        close(f64::from(samples[2 * 479]), f64::from(target), 1e-7);
        assert!(
            samples
                .as_chunks::<2>()
                .0
                .iter()
                .all(|frame| frame[0] == frame[1])
        );
        assert!(
            samples
                .as_chunks::<2>()
                .0
                .iter()
                .map(|frame| frame[0])
                .collect::<Vec<_>>()
                .windows(2)
                .all(|pair| pair[1] <= pair[0])
        );
        assert!(!eq.is_transitioning());
    }

    #[test]
    fn pending_updates_are_latest_wins_and_bounded() {
        let mut settings = config(FilterKind::Peak);
        let mut eq = Equalizer::new(&settings, 48_000, 1).unwrap();
        for index in 0..100 {
            settings.bands[0].gain_db = -24.0 + index as f64 * 0.4;
            eq.reconfigure(&settings).unwrap();
        }
        close(
            eq.response_db(1_000.0, 0).unwrap(),
            settings.bands[0].gain_db,
            1e-8,
        );
        let mut silence = [0.0; 960];
        eq.process(&mut silence).unwrap();
        assert!(!eq.is_transitioning());
        assert!(eq.pending.is_none());
        assert!(
            eq.banks
                .iter()
                .all(|bank| bank.sections.len() <= MAX_BANDS * MAX_STAGES as usize)
        );
    }

    #[test]
    fn chunk_boundaries_do_not_change_transition_output() {
        for channels in [2, 3, 8, 31] {
            let settings = config(FilterKind::Peak);
            let mut a = Equalizer::new(&settings, 48_000, channels).unwrap();
            let mut b = Equalizer::new(&settings, 48_000, channels).unwrap();
            let mut changed = settings;
            changed.bands[0].frequency_hz = 9_000.0;
            changed.bands[0].gain_db = -24.0;
            changed.bands[0].channels = ChannelTarget::Selected(vec![0]);
            a.reconfigure(&changed).unwrap();
            b.reconfigure(&changed).unwrap();
            let mut whole: Vec<f32> = (0..channels * 2_000)
                .map(|index| (index as f32 * 0.03).sin() * 0.01)
                .collect();
            let mut chunks = whole.clone();
            a.process(&mut whole).unwrap();
            for chunk in chunks.chunks_mut(channels * 37) {
                b.process(chunk).unwrap();
            }
            assert_eq!(whole, chunks);
        }
    }

    #[test]
    fn disabling_eventually_restores_exact_bypass() {
        let mut settings = config(FilterKind::Peak);
        let mut eq = Equalizer::new(&settings, 48_000, 1).unwrap();
        eq.process(&mut [1.0]).unwrap();
        settings.enabled = false;
        eq.reconfigure(&settings).unwrap();
        eq.process(&mut [0.0; 480]).unwrap();
        let mut samples = [-0.0, 0.125, -0.875, f32::MAX];
        let expected = samples.map(f32::to_bits);
        eq.process(&mut samples).unwrap();
        assert_eq!(samples.map(f32::to_bits), expected);
    }

    #[test]
    fn extreme_coefficients_and_impulses_remain_finite() {
        for kind in [
            FilterKind::Peak,
            FilterKind::LowShelf,
            FilterKind::HighShelf,
            FilterKind::LowPass,
            FilterKind::HighPass,
            FilterKind::BandPass,
            FilterKind::Notch,
        ] {
            for rate in [8_000, 44_100, 192_000, 768_000] {
                for frequency in [
                    1.0,
                    (f64::from(rate) * MAX_FREQUENCY_RATE_RATIO).min(96_000.0),
                ] {
                    for q in [0.1, 20.0] {
                        for gain_db in [-24.0, 24.0] {
                            let mut settings = config(kind);
                            settings.bands[0].frequency_hz = frequency;
                            settings.bands[0].q = q;
                            settings.bands[0].gain_db = gain_db;
                            settings.bands[0].shelf_slope = 0.1;
                            if matches!(
                                kind,
                                FilterKind::LowPass
                                    | FilterKind::HighPass
                                    | FilterKind::BandPass
                                    | FilterKind::Notch
                            ) {
                                settings.bands[0].stages = MAX_STAGES;
                            }
                            let mut eq = Equalizer::new(&settings, rate, 1).unwrap();
                            let mut impulse = [0.0; 4_096];
                            impulse[0] = 0.001;
                            eq.process(&mut impulse).unwrap();
                            assert!(impulse.iter().all(|sample| sample.is_finite()));
                            for bank in &eq.banks {
                                assert!(bank.states.iter().all(|state| state.is_finite()));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn repeated_extreme_transitions_stay_finite() {
        let mut settings = config(FilterKind::Peak);
        settings.bands = vec![settings.bands[0].clone(); MAX_BANDS];
        let mut eq = Equalizer::new(&settings, 48_000, 2).unwrap();
        for index in 0..32 {
            for (band_index, band) in settings.bands.iter_mut().enumerate() {
                band.frequency_hz = if (index + band_index) % 2 == 0 {
                    1.0
                } else {
                    23_952.0
                };
                band.q = if index % 2 == 0 { 0.1 } else { 20.0 };
                band.gain_db = if band_index % 2 == 0 { -24.0 } else { 24.0 };
            }
            eq.reconfigure(&settings).unwrap();
            let mut samples = [0.000_001; 128];
            eq.process(&mut samples).unwrap();
            assert!(samples.iter().all(|sample| sample.is_finite()));
        }
    }

    #[test]
    fn invalid_config_is_rejected_transactionally() {
        let settings = config(FilterKind::Peak);
        let mut eq = Equalizer::new(&settings, 48_000, 2).unwrap();
        let mut invalid = settings.clone();
        invalid.bands[0].frequency_hz = 24_000.0;
        assert!(matches!(
            eq.reconfigure(&invalid),
            Err(EqualizerError::FrequencyExceedsRate { .. })
        ));
        assert_eq!(eq.config(), &settings);
        assert!(!eq.is_transitioning());
        for invalid_value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 0.0] {
            invalid.bands[0].frequency_hz = invalid_value;
            assert!(invalid.validate().is_err());
        }
        invalid = settings.clone();
        invalid.bands[0].channels = ChannelTarget::Selected(vec![1, 1]);
        assert!(matches!(
            invalid.validate(),
            Err(EqualizerError::DuplicateChannel { .. })
        ));
        invalid.bands[0].channels = ChannelTarget::Selected(vec![]);
        assert!(invalid.validate().is_err());
        invalid.bands[0].channels = ChannelTarget::Selected(vec![2]);
        assert!(invalid.validate_for_stream(48_000, 2).is_err());
        invalid = settings.clone();
        invalid.bands[0].stages = 2;
        assert!(matches!(
            invalid.validate(),
            Err(EqualizerError::UnsupportedStages { .. })
        ));
        invalid = settings.clone();
        invalid.bands = vec![EqBand::default(); MAX_BANDS + 1];
        assert!(matches!(
            invalid.validate(),
            Err(EqualizerError::TooManyBands(_))
        ));
        assert!(Equalizer::new(&settings, 0, 2).is_err());
        assert!(Equalizer::new(&settings, 48_000, 0).is_err());
        assert!(Equalizer::new(&settings, 48_000, MAX_CHANNELS + 1).is_err());
    }

    #[test]
    fn invalid_audio_and_response_requests_are_explicit() {
        let mut eq = Equalizer::new(&config(FilterKind::Peak), 48_000, 2).unwrap();
        let mut incomplete = [1.0, 2.0, 3.0];
        assert!(matches!(
            eq.process(&mut incomplete),
            Err(EqualizerError::IncompleteFrame { .. })
        ));
        assert_eq!(incomplete, [1.0, 2.0, 3.0]);
        let mut nonfinite = [0.25, f32::NAN];
        assert!(matches!(
            eq.process(&mut nonfinite),
            Err(EqualizerError::NonFiniteInput { sample: 1 })
        ));
        assert_eq!(nonfinite[0], 0.25);
        assert!(eq.response_db(f64::NAN, 0).is_err());
        assert!(eq.response_db(24_001.0, 0).is_err());
        assert!(eq.response_db(1_000.0, 2).is_err());
        let mut values = [7.0, 7.0];
        assert!(
            eq.sample_response(&[1_000.0, f64::NAN], 0, &mut values)
                .is_err()
        );
        assert_eq!(values, [7.0, 7.0]);
        eq.sample_response(&[100.0, 1_000.0], 0, &mut values)
            .unwrap();
        close(values[1], 10.0_f64.powf(12.0 / 20.0), 1e-9);
    }

    #[test]
    fn gains_are_not_clipped_and_overflow_is_not_hidden() {
        let mut settings = EqualizerConfig {
            enabled: true,
            preamp_db: 6.0,
            output_gain_db: 6.0,
            ..EqualizerConfig::default()
        };
        let mut eq = Equalizer::new(&settings, 48_000, 1).unwrap();
        let mut audio = [1.0];
        eq.process(&mut audio).unwrap();
        assert!(audio[0] > 3.9);
        close(eq.response_db(1_000.0, 0).unwrap(), 12.0, 1e-9);
        settings.preamp_db = 24.0;
        settings.output_gain_db = 24.0;
        let mut eq = Equalizer::new(&settings, 48_000, 1).unwrap();
        assert!(matches!(
            eq.process(&mut [f32::MAX]),
            Err(EqualizerError::NonFiniteOutput { sample: 0 })
        ));
    }

    #[test]
    fn dormant_presets_do_not_constrain_bypassed_streams() {
        let mut settings = config(FilterKind::Peak);
        settings.enabled = false;
        settings.bands[0].frequency_hz = 20_000.0;
        settings.bands[0].channels = ChannelTarget::Selected(vec![63]);
        for (rate, channels) in [(8_000, 1), (4_000, 128)] {
            let mut eq = Equalizer::new(&settings, rate, channels).unwrap();
            assert!(eq.is_bypassed());
            let mut audio = vec![0.125; channels * 16];
            let expected = audio.clone();
            eq.process(&mut audio).unwrap();
            assert_eq!(audio, expected);
            let mut db = [99.0; 2];
            settings
                .sample_response_db(rate, channels, 0, &[0.0, 1_000.0], &mut db)
                .unwrap();
            assert_eq!(db, [0.0, 0.0]);
        }
        settings.enabled = true;
        assert!(matches!(
            settings.validate_for_stream(8_000, 1),
            Err(EqualizerError::FrequencyExceedsRate { .. })
        ));
        settings.bands[0].enabled = false;
        settings.validate_for_stream(8_000, 1).unwrap();
        let mut active = EqBand {
            solo: true,
            frequency_hz: 500.0,
            ..EqBand::default()
        };
        settings.bands[0].enabled = true;
        settings.bands.push(active.clone());
        settings.validate_for_stream(8_000, 1).unwrap();
        active.solo = false;
        settings.bands[1] = active;
        assert!(settings.validate_for_stream(8_000, 1).is_err());
    }

    #[test]
    fn coefficient_only_plot_matches_runtime_without_pcm_history() {
        let mut settings = config(FilterKind::Peak);
        settings.preamp_db = -3.0;
        settings.output_gain_db = 1.0;
        settings.bands[0].channels = ChannelTarget::Selected(vec![1]);
        let eq = Equalizer::new(&settings, 48_000, 2).unwrap();
        let bank = Bank::prepare_response(&settings, 48_000, 2).unwrap();
        assert!(bank.states.is_empty());
        let frequencies = [0.0, 100.0, 1_000.0, 12_000.0, 24_000.0];
        for channel in 0..2 {
            let mut db = [0.0; 5];
            settings
                .sample_response_db(48_000, 2, channel, &frequencies, &mut db)
                .unwrap();
            for (frequency, actual) in frequencies.into_iter().zip(db) {
                close(actual, eq.response_db(frequency, channel).unwrap(), 1e-12);
            }
        }
    }

    #[test]
    fn processing_keeps_prepared_storage_through_pending_fades() {
        let mut settings = config(FilterKind::LowPass);
        let mut eq = Equalizer::new(&settings, 48_000, 8).unwrap();
        settings.bands[0].frequency_hz = 2_000.0;
        eq.reconfigure(&settings).unwrap();
        settings.bands[0].frequency_hz = 4_000.0;
        eq.reconfigure(&settings).unwrap();
        let storage = eq.banks.each_ref().map(|bank| {
            (
                bank.sections.as_ptr(),
                bank.sections.capacity(),
                bank.states.as_ptr(),
                bank.states.capacity(),
            )
        });
        let mut audio = [0.001; 16_384];
        eq.process(&mut audio).unwrap();
        assert!(!eq.is_transitioning());
        assert_eq!(
            storage,
            eq.banks.each_ref().map(|bank| (
                bank.sections.as_ptr(),
                bank.sections.capacity(),
                bank.states.as_ptr(),
                bank.states.capacity()
            ))
        );
    }

    #[test]
    fn channel_vector_kernel_matches_scalar_cascade_exactly() {
        for channels in [1, 2, 3, 4, 7, 8, 16, 31, 32, 63, 64] {
            let mut settings = EqualizerConfig {
                enabled: true,
                preamp_db: -3.0,
                output_gain_db: 1.0,
                ..EqualizerConfig::default()
            };
            for index in 0..8 {
                settings.bands.push(EqBand {
                    kind: if index % 2 == 0 {
                        FilterKind::Peak
                    } else {
                        FilterKind::Notch
                    },
                    stages: if index % 2 == 0 { 1 } else { 4 },
                    frequency_hz: 80.0 * 1.7_f64.powi(index),
                    gain_db: if index % 3 == 0 { 6.0 } else { -3.0 },
                    channels: if index % 3 == 0 {
                        ChannelTarget::Selected(
                            (0..channels).filter(|channel| channel % 2 == 0).collect(),
                        )
                    } else {
                        ChannelTarget::All
                    },
                    ..EqBand::default()
                });
            }
            let reference_bank = Bank::prepare_response(&settings, 48_000, channels).unwrap();
            let mut reference_states =
                vec![State::default(); reference_bank.sections.len() * channels];
            let mut expected: Vec<f32> = (0..channels * 300)
                .map(|sample| (sample as f32 * 0.13).sin() * 0.01)
                .collect();
            let mut actual = expected.clone();
            // Original channel-major cascade, with independent scalar states.
            for frame in expected.chunks_exact_mut(channels) {
                for (channel, sample) in frame.iter_mut().enumerate() {
                    let mut output = f64::from(*sample) * reference_bank.preamp;
                    for (index, section) in reference_bank.sections.iter().enumerate() {
                        if section.channel_mask & (1_u64 << channel) != 0 {
                            output = reference_states[index * channels + channel]
                                .process(output, section.coefficients);
                        }
                    }
                    *sample = (output * reference_bank.output_gain) as f32;
                }
            }
            let mut eq = Equalizer::new(&settings, 48_000, channels).unwrap();
            for block in actual.chunks_mut(channels * 41) {
                eq.process(block).unwrap();
            }
            assert_eq!(
                actual, expected,
                "scalar equivalence for {channels} channels"
            );
        }
    }
}
