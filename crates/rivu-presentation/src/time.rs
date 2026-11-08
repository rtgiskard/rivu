//! Pure time labels shared by graphical and terminal presentation.

/// Floor finite nonnegative seconds and display total minutes and seconds.
pub fn format_time(seconds: f64) -> String {
    let seconds = if seconds.is_finite() {
        seconds.max(0.0) as u64
    } else {
        0
    };
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Display an absolute CUE source position at 75 frames per second.
pub fn format_cue_frame(frame: u64) -> String {
    let seconds = frame / 75;
    format!("{:02}:{:02}:{:02}", seconds / 60, seconds % 60, frame % 75)
}

#[cfg(test)]
mod tests {
    use super::{format_cue_frame, format_time};

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
        ] {
            assert_eq!(format_time(seconds), expected);
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
