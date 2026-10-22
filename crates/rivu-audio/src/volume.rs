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

#[cfg(test)]
mod tests {
    use super::*;

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
