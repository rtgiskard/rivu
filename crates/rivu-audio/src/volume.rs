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

/// Resolve policy before master volume. Finite dB boosts saturate at f32::MAX;
/// very negative finite gains may underflow to silence when converted to f32.
pub(crate) fn replaygain(
    info: &crate::PlaybackMetadata,
    enabled: bool,
    policy: rivu_format::config::ReplayGainPeakPolicy,
    source_peak: Option<f64>,
    processed_source: bool,
) -> (f32, rivu_protocol::model::ReplayGainStatus) {
    use rivu_format::config::ReplayGainPeakPolicy;
    let requested_gain_db = info.replaygain_track_gain.filter(|value| value.is_finite());
    let source_peak =
        source_peak.filter(|peak| !processed_source && peak.is_finite() && *peak > 0.0);
    let mut applied_gain_db = if enabled {
        requested_gain_db
            .unwrap_or(0.0)
            .min(20.0 * f64::from(f32::MAX).log10())
    } else {
        0.0
    };
    if enabled
        && requested_gain_db.is_some()
        && policy == ReplayGainPeakPolicy::PreventClipping
        && let Some(peak) = source_peak
    {
        applied_gain_db = applied_gain_db.min(-20.0 * peak.log10());
    }
    let gain = 10.0_f64
        .powf(applied_gain_db / 20.0)
        .min(f64::from(f32::MAX)) as f32;
    // Round downward when f32 conversion would otherwise exceed the peak cap.
    let gain = if enabled
        && requested_gain_db.is_some()
        && policy == ReplayGainPeakPolicy::PreventClipping
        && source_peak.is_some_and(|peak| f64::from(gain) * peak > 1.0)
    {
        gain.next_down().max(0.0)
    } else {
        gain
    };
    (
        gain,
        rivu_protocol::model::ReplayGainStatus {
            enabled,
            peak_policy: policy,
            requested_gain_db,
            applied_gain_db,
            source_peak,
            processed_source,
        },
    )
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
        assert_eq!(
            replaygain(
                &info,
                false,
                rivu_format::config::ReplayGainPeakPolicy::Off,
                None,
                false
            )
            .0,
            1.0
        );
        assert_eq!(
            replaygain(
                &info,
                true,
                rivu_format::config::ReplayGainPeakPolicy::Off,
                None,
                false
            )
            .0,
            0.5
        );
        info.replaygain_track_gain = Some(0.0);
        assert_eq!(
            replaygain(
                &info,
                true,
                rivu_format::config::ReplayGainPeakPolicy::Off,
                None,
                false
            )
            .0,
            1.0
        );
        info.replaygain_track_gain = Some(12.041199826559248);
        assert_eq!(
            replaygain(
                &info,
                true,
                rivu_format::config::ReplayGainPeakPolicy::Off,
                None,
                false
            )
            .0,
            4.0
        );
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
            assert_eq!(
                replaygain(
                    &info,
                    true,
                    rivu_format::config::ReplayGainPeakPolicy::PreventClipping,
                    Some(2.0),
                    false
                )
                .0,
                1.0
            );
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
            let (gain, status) = replaygain(
                &info,
                true,
                rivu_format::config::ReplayGainPeakPolicy::Off,
                None,
                false,
            );
            assert_eq!(gain, expected);
            assert!(status.applied_gain_db.is_finite());
        }
    }

    #[test]
    fn peak_policy_caps_only_valid_unprocessed_track_peaks() {
        use rivu_format::config::ReplayGainPeakPolicy::{Off, PreventClipping};
        let info = crate::PlaybackMetadata {
            replaygain_track_gain: Some(12.041199826559248),
            ..Default::default()
        };
        for peak in [0.25, 0.8, 1.0, 2.0, f64::MAX] {
            let (gain, status) = replaygain(&info, true, PreventClipping, Some(peak), false);
            assert!(f64::from(gain) * peak <= 1.0);
            assert_eq!(status.source_peak, Some(peak));
            assert!(status.applied_gain_db <= status.requested_gain_db.unwrap());
        }
        for peak in [
            None,
            Some(0.0),
            Some(-1.0),
            Some(f64::NAN),
            Some(f64::INFINITY),
        ] {
            let (gain, status) = replaygain(&info, true, PreventClipping, peak, false);
            assert_eq!(gain, 4.0);
            assert_eq!(status.source_peak, None);
            assert_eq!(status.applied_gain_db, info.replaygain_track_gain.unwrap());
        }
        let (gain, status) = replaygain(&info, true, PreventClipping, Some(1.0), true);
        assert_eq!(gain, 4.0);
        assert!(status.processed_source);
        assert_eq!(status.source_peak, None);
        assert_eq!(replaygain(&info, true, Off, Some(1.0), false).0, 4.0);
        assert_eq!(
            replaygain(&info, false, PreventClipping, Some(2.0), false).0,
            1.0
        );
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
