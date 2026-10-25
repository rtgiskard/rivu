//! Shared EQ parameters, implementation selection, and execution-independent validation.
use std::fmt;

use serde::{Deserialize, Serialize};

pub const MAX_BANDS: usize = 32;
pub const MAX_CHANNELS: usize = 64;
pub const MAX_STAGES: u8 = 4;
/// A small, explicit guard against degenerate poles/zeros at Nyquist.
pub const MAX_FREQUENCY_RATE_RATIO: f64 = 0.499;
pub const MIN_TAPS: usize = 3;
pub const MAX_TAPS: usize = 16_385;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EqImplementation {
    #[default]
    Iir,
    LinearPhase {
        taps: usize,
    },
}

impl EqImplementation {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Self::LinearPhase { taps } = self {
            validate_taps(*taps)?;
        }
        Ok(())
    }
}

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
                "EQ output sample {sample} is not representable as finite f32; reduce gain and reset EQ before reuse"
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
pub(super) fn validate_taps(taps: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        (MIN_TAPS..=MAX_TAPS).contains(&taps) && taps % 2 == 1,
        "linear-phase FIR requires an odd tap count in {MIN_TAPS}..={MAX_TAPS}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn implementation_and_taps_validate_without_execution() {
        EqImplementation::default().validate().unwrap();
        for taps in [MIN_TAPS, 513, MAX_TAPS] {
            EqImplementation::LinearPhase { taps }.validate().unwrap();
        }
        for taps in [0, 1, 2, 1024, MAX_TAPS + 2] {
            assert!(EqImplementation::LinearPhase { taps }.validate().is_err());
        }
    }

    #[test]
    fn invalid_parameters_and_channel_selections_are_rejected() {
        let settings = EqualizerConfig {
            enabled: true,
            bands: vec![EqBand::default()],
            ..Default::default()
        };
        let mut invalid = settings.clone();
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
    }

    #[test]
    fn dormant_bands_do_not_impose_stream_constraints() {
        let mut settings = EqualizerConfig {
            bands: vec![EqBand {
                frequency_hz: 20_000.0,
                channels: ChannelTarget::Selected(vec![63]),
                ..Default::default()
            }],
            ..Default::default()
        };
        for (rate, channels) in [(8_000, 1), (4_000, 128)] {
            settings.validate_for_stream(rate, channels).unwrap();
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
}
