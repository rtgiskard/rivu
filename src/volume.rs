//! Conversion between normalized volume levels and linear amplitude gains.

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

/// Converts a finite, nonnegative amplitude gain to a normalized level.
/// Zero is mute; gains outside the positive curve's range clamp to `0.0..=1.0`.
/// Call outside the realtime audio callback, not once per sample.
pub(crate) fn level(gain: f32) -> f32 {
    if gain == 0.0 {
        0.0
    } else {
        (1.0 + gain.log10() / 3.0).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mute_and_unity_are_exact() {
        assert_eq!(gain(0.0), 0.0);
        assert_eq!(gain(1.0), 1.0);
        assert_eq!(level(0.0), 0.0);
        assert_eq!(level(1.0), 1.0);
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

    #[test]
    fn inverse_preserves_every_nonzero_legacy_percent_gain() {
        for percent in 1..=100 {
            let legacy = percent as f32 / 100.0;
            let restored = gain(level(legacy));
            assert!(
                ((restored - legacy) / legacy).abs() < 0.000002,
                "legacy gain {legacy}: {restored}"
            );
        }
    }

    #[test]
    fn inverse_clamps_outside_the_positive_curve() {
        assert_eq!(level(0.0001), 0.0);
        assert_eq!(level(2.0), 1.0);
    }
}
