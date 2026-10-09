//! Pure speaker mapping for native output with verified speaker positions.

use super::Channel;
use anyhow::{Result, bail};

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

#[cfg(test)]
mod tests {
    use super::*;
    use Channel::*;

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
