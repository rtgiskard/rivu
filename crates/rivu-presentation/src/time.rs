//! Playback duration selection and time labels shared by presentation clients.

use std::fmt::{self, Write as _};

/// Format Unix seconds in the listener's local time zone, when representable.
pub fn format_played_at(timestamp: i64) -> Option<String> {
    Some(
        chrono::DateTime::from_timestamp(timestamp, 0)?
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
    )
}

/// Prefer the engine's finite nonnegative duration, then try the track's metadata.
pub fn display_duration(engine: Option<f64>, metadata: Option<f64>) -> Option<f64> {
    let valid = |seconds: &f64| seconds.is_finite() && *seconds >= 0.0;
    engine.filter(valid).or_else(|| metadata.filter(valid))
}

/// A whole-second playback time displayed as total minutes and seconds.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TimeLabel(u64);

impl TimeLabel {
    pub fn from_seconds(seconds: f64) -> Self {
        Self(if seconds.is_finite() {
            seconds.max(0.0) as u64
        } else {
            0
        })
    }

    /// Number of ASCII cells occupied by the formatted label.
    pub fn width(self) -> u16 {
        (self.0 / 60).checked_ilog10().unwrap_or(0) as u16 + 4
    }
}

impl fmt::Display for TimeLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{:02}", self.0 / 60, self.0 % 60)
    }
}

/// Floor finite nonnegative seconds and display total minutes and seconds.
pub fn format_time(seconds: f64) -> String {
    let label = TimeLabel::from_seconds(seconds);
    let mut text = String::with_capacity(usize::from(label.width()));
    write!(text, "{label}").unwrap();
    text
}

/// Display an absolute CUE source position at 75 frames per second.
pub fn format_cue_frame(frame: u64) -> String {
    let seconds = frame / 75;
    format!("{:02}:{:02}:{:02}", seconds / 60, seconds % 60, frame % 75)
}

#[cfg(test)]
mod tests {
    use super::{TimeLabel, display_duration, format_cue_frame, format_time};

    #[test]
    fn display_duration_prefers_engine_then_valid_metadata() {
        assert_eq!(display_duration(Some(0.0), Some(12.5)), Some(0.0));
        assert_eq!(display_duration(Some(14.0), Some(12.5)), Some(14.0));
        for missing in [None, Some(-1.0), Some(f64::NAN), Some(f64::INFINITY)] {
            assert_eq!(display_duration(missing, Some(12.5)), Some(12.5));
            assert_eq!(display_duration(missing, missing), None);
        }
    }

    #[test]
    fn time_labels_preserve_total_minutes_and_floor_fractional_seconds() {
        for (seconds, expected) in [
            (-1.0, "0:00"),
            (f64::NAN, "0:00"),
            (f64::INFINITY, "0:00"),
            (f64::NEG_INFINITY, "0:00"),
            (59.999, "0:59"),
            (60.0, "1:00"),
            (3601.5, "60:01"),
            (5999.99, "99:59"),
            (6000.0, "100:00"),
        ] {
            assert_eq!(format_time(seconds), expected);
            assert_eq!(
                usize::from(TimeLabel::from_seconds(seconds).width()),
                expected.len()
            );
        }
    }

    #[test]
    fn cue_labels_keep_frame_precision_and_source_offsets() {
        for (frame, expected) in [
            (0, "00:00:00"),
            (74, "00:00:74"),
            (75, "00:01:00"),
            (4500, "01:00:00"),
            (270001, "60:00:01"),
        ] {
            assert_eq!(format_cue_frame(frame), expected);
        }
    }
}
