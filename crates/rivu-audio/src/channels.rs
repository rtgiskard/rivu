//! Speaker positions, lossless device mapping, and logical analysis downmix.

use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    FrontLeft,
    FrontRight,
    FrontCenter,
    Lfe,
    RearLeft,
    RearRight,
    FrontLeftCenter,
    FrontRightCenter,
    RearCenter,
    SideLeft,
    SideRight,
    TopCenter,
    TopFrontLeft,
    TopFrontCenter,
    TopFrontRight,
    TopRearLeft,
    TopRearCenter,
    TopRearRight,
}

impl Channel {
    /// WAVE speaker-mask bit index, also shared by FFmpeg's first 18 AVChannel IDs.
    pub fn standard_index(self) -> u32 {
        match self {
            Self::FrontLeft => 0,
            Self::FrontRight => 1,
            Self::FrontCenter => 2,
            Self::Lfe => 3,
            Self::RearLeft => 4,
            Self::RearRight => 5,
            Self::FrontLeftCenter => 6,
            Self::FrontRightCenter => 7,
            Self::RearCenter => 8,
            Self::SideLeft => 9,
            Self::SideRight => 10,
            Self::TopCenter => 11,
            Self::TopFrontLeft => 12,
            Self::TopFrontCenter => 13,
            Self::TopFrontRight => 14,
            Self::TopRearLeft => 15,
            Self::TopRearCenter => 16,
            Self::TopRearRight => 17,
        }
    }

    /// Resolves a standard speaker bit without guessing from the channel count.
    pub fn from_standard_index(index: u32) -> Option<Self> {
        Some(match index {
            0 => Self::FrontLeft,
            1 => Self::FrontRight,
            2 => Self::FrontCenter,
            3 => Self::Lfe,
            4 => Self::RearLeft,
            5 => Self::RearRight,
            6 => Self::FrontLeftCenter,
            7 => Self::FrontRightCenter,
            8 => Self::RearCenter,
            9 => Self::SideLeft,
            10 => Self::SideRight,
            11 => Self::TopCenter,
            12 => Self::TopFrontLeft,
            13 => Self::TopFrontCenter,
            14 => Self::TopFrontRight,
            15 => Self::TopRearLeft,
            16 => Self::TopRearCenter,
            17 => Self::TopRearRight,
            _ => return None,
        })
    }
}

pub(super) fn mapping(source: &[Channel], device: &[Channel]) -> Result<Vec<Option<usize>>> {
    if source.is_empty() || device.is_empty() {
        bail!("Empty source or device speaker layout");
    }
    for (index, channel) in source.iter().enumerate() {
        if source[..index].contains(channel) {
            bail!("Duplicate source speaker: {channel:?}");
        }
    }
    for (index, channel) in device.iter().enumerate() {
        if device[..index].contains(channel) {
            bail!("Duplicate device speaker: {channel:?}");
        }
    }
    // Preserve the established mono-on-stereo behavior, explicitly duplicating
    // a single center signal. No multichannel signal is ever downmixed here.
    if source == [Channel::FrontCenter] && device == [Channel::FrontLeft, Channel::FrontRight] {
        return Ok(vec![Some(0), Some(0)]);
    }
    for channel in source {
        if !device.contains(channel) {
            bail!(
                "Device layout {device:?} lacks source speaker {channel:?} (source {source:?}); refusing downmix"
            );
        }
    }
    Ok(device
        .iter()
        .map(|channel| source.iter().position(|s| s == channel))
        .collect())
}

// Visualization paths fold decoded channels into two logical sides. Real
// output still uses the original permutation with independent speakers.
pub(super) fn analysis_weights(layout: &[Channel]) -> Vec<[f32; 2]> {
    use Channel::*;
    let mut weights: Vec<_> = layout
        .iter()
        .map(|channel| match channel {
            FrontLeft | RearLeft | SideLeft | FrontLeftCenter | TopFrontLeft | TopRearLeft => {
                [1.0, 0.0]
            }
            FrontRight | RearRight | SideRight | FrontRightCenter | TopFrontRight
            | TopRearRight => [0.0, 1.0],
            _ => [1.0, 1.0],
        })
        .collect();
    let total = weights.iter().fold([0.0_f32; 2], |sum, weight| {
        [sum[0] + weight[0], sum[1] + weight[1]]
    });
    for weight in &mut weights {
        weight[0] /= total[0].max(1.0);
        weight[1] /= total[1].max(1.0);
    }
    weights
}

#[cfg(test)]
mod tests {
    use super::*;
    use Channel::*;

    #[test]
    fn standard_speaker_bits_round_trip_without_count_inference() {
        for index in 0..18 {
            let speaker = Channel::from_standard_index(index).unwrap();
            assert_eq!(speaker.standard_index(), index);
        }
        assert_eq!(Channel::from_standard_index(18), None);
        assert_eq!(Channel::from_standard_index(u32::MAX), None);
    }

    #[test]
    fn analysis_downmix_is_normalized_and_independent_of_speaker_mapping() {
        let layout = [FrontLeft, FrontRight, FrontCenter, Lfe, SideLeft, SideRight];
        let weights = analysis_weights(&layout);
        assert_eq!(weights[0], [0.25, 0.0]);
        assert_eq!(weights[1], [0.0, 0.25]);
        assert_eq!(weights[2], [0.25, 0.25]);
        assert_eq!(weights[3], [0.25, 0.25]);
        assert_eq!(weights[4], [0.25, 0.0]);
        assert_eq!(weights[5], [0.0, 0.25]);
        assert!(mapping(&layout, &[FrontLeft, FrontRight]).is_err());
        assert_eq!(analysis_weights(&[FrontCenter]), [[1.0, 1.0]]);
        assert_eq!(analysis_weights(&[FrontLeft]), [[1.0, 0.0]]);
        assert!(analysis_weights(&[]).is_empty());
    }

    #[test]
    fn mapping_preserves_every_speaker_and_never_guesses_from_count() {
        for source in [
            vec![FrontLeft, FrontRight, FrontCenter, Lfe, RearLeft, RearRight],
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                Lfe,
                RearCenter,
                SideLeft,
                SideRight,
            ],
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                Lfe,
                RearLeft,
                RearRight,
                SideLeft,
                SideRight,
            ],
        ] {
            let mut device = source.clone();
            device.rotate_left(2);
            let map = mapping(&source, &device).unwrap();
            for (destination, index) in map.iter().enumerate() {
                assert_eq!(source[index.unwrap()], device[destination]);
            }
        }
        assert!(
            mapping(
                &[
                    FrontLeft,
                    FrontRight,
                    FrontCenter,
                    Lfe,
                    RearCenter,
                    SideLeft,
                    SideRight
                ],
                &[
                    FrontLeft,
                    FrontRight,
                    FrontCenter,
                    Lfe,
                    RearLeft,
                    RearRight,
                    TopCenter
                ]
            )
            .is_err()
        );
        assert!(mapping(&[FrontLeft, FrontRight, Lfe], &[FrontLeft, FrontRight]).is_err());
        assert_eq!(
            mapping(&[FrontCenter], &[FrontLeft, FrontRight]).unwrap(),
            [Some(0), Some(0)]
        );
    }
}
