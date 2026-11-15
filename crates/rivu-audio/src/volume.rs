//! Conversion from normalized volume levels to linear amplitude gains.

/// Converts a finite normalized level in `0.0..=1.0` to an amplitude gain.
/// Zero is mute; positive levels span approximately -60 dB through unity (0 dB).
/// Call outside the realtime audio callback, not once per sample.
pub(crate) fn gain(level: f32) -> f32 {
    if level == 0.0 {
        0.0
    } else {
        10.0_f32.powf(3.0 * (level - 1.0))
    }
}

/// Playback-only track gain, independent of master volume.
/// Missing or invalid tags leave unity; finite gains saturate at PCM representability.
pub(crate) fn replaygain(info: &crate::PlaybackMetadata, enabled: bool) -> f32 {
    if !enabled {
        return 1.0;
    }
    let Some(decibels) = info.replaygain_track_gain.filter(|value| value.is_finite()) else {
        return 1.0;
    };
    10.0_f64.powf(decibels / 20.0).min(f64::from(f32::MAX)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaygain_is_opt_in_and_applies_track_gain_without_limiting_boosts() {
        let mut info = crate::PlaybackMetadata {
            replaygain_track_gain: Some(-6.020599913279624),
            ..Default::default()
        };
        assert_eq!(replaygain(&info, false), 1.0);
        assert_eq!(replaygain(&info, true), 0.5);
        info.replaygain_track_gain = Some(0.0);
        assert_eq!(replaygain(&info, true), 1.0);
        info.replaygain_track_gain = Some(12.041199826559248);
        assert_eq!(replaygain(&info, true), 4.0);
    }

    #[test]
    fn replaygain_uses_unity_for_missing_or_invalid_track_gain() {
        for track_gain in [
            None,
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(f64::NEG_INFINITY),
        ] {
            let info = crate::PlaybackMetadata {
                replaygain_track_gain: track_gain,
                ..Default::default()
            };
            assert_eq!(replaygain(&info, true), 1.0);
        }
    }

    #[test]
    fn extreme_finite_gains_saturate_without_reversing_attenuation() {
        for (decibels, expected) in [
            (-1_000.0, 0.0),
            (-f64::MAX, 0.0),
            (1_000.0, f32::MAX),
            (f64::MAX, f32::MAX),
        ] {
            let info = crate::PlaybackMetadata {
                replaygain_track_gain: Some(decibels),
                ..Default::default()
            };
            assert_eq!(replaygain(&info, true), expected);
        }
    }

    #[test]
    fn mute_and_unity_are_exact() {
        assert_eq!(gain(0.0), 0.0);
        assert_eq!(gain(1.0), 1.0);
    }

    #[test]
    fn normalized_levels_have_strictly_increasing_gains() {
        let mut previous = gain(0.0);
        for step in 1..=1000 {
            let current = gain(step as f32 / 1000.0);
            assert!(current > previous, "step {step}: {current} <= {previous}");
            previous = current;
        }
    }

    #[test]
    fn equal_level_steps_are_six_decibels() {
        for step in 1..10 {
            let lower = gain(step as f32 / 10.0);
            let upper = gain((step + 1) as f32 / 10.0);
            let decibels = 20.0 * (upper / lower).log10();
            assert!((decibels - 6.0).abs() < 0.00002, "step {step}: {decibels}");
        }
    }
}
