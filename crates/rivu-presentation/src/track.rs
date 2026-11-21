use crate::time::{format_cue_frame, format_time};
use rivu_protocol::model::{CueSegment, Track};
use std::{borrow::Cow, path::Path};

/// Display missing titles without changing valid metadata or search semantics.
pub fn track_title<'a>(title: &'a str, path: &'a Path, cue_number: Option<u32>) -> Cow<'a, str> {
    if title.trim().is_empty() {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        match cue_number {
            Some(number) => Cow::Owned(format!("{stem} · #{number:02}")),
            None => stem,
        }
    } else {
        Cow::Borrowed(title)
    }
}

/// Audio labels before the frontend applies its missing-value treatment.
#[derive(Default)]
pub struct AudioDetails {
    pub codec: Option<String>,
    pub sample_rate: Option<String>,
    pub channels: Option<String>,
    pub duration: Option<String>,
    pub bitrate: Option<String>,
    pub bits_per_sample: Option<String>,
}

impl AudioDetails {
    /// The approximation prefix includes any spacing required by the frontend.
    pub fn format(track: &Track, approximate_prefix: &str) -> Self {
        Self {
            codec: (!track.codec.is_empty()).then(|| track.codec.to_ascii_lowercase()),
            sample_rate: (track.sample_rate != 0).then(|| format!("{} Hz", track.sample_rate)),
            channels: (track.channels != 0).then(|| track.channels.to_string()),
            duration: track.duration.map(format_time),
            bitrate: track.bitrate_bps.map(|bps| {
                let prefix = if track.bitrate_exact {
                    ""
                } else {
                    approximate_prefix
                };
                format!("{prefix}{:.0} kbps", bps as f64 / 1000.0)
            }),
            bits_per_sample: track.bits_per_sample.map(|bits| bits.to_string()),
        }
    }
}

pub struct CueDetails {
    pub sheet: String,
    pub number: String,
    pub start: String,
    /// None means the segment ends at the source file's end.
    pub end: Option<String>,
}

impl CueDetails {
    pub fn format(cue: &CueSegment) -> Self {
        Self {
            sheet: cue.sheet.to_string_lossy().into_owned(),
            number: cue.number.to_string(),
            start: format_cue_frame(cue.start_frame),
            end: cue.end_frame.map(format_cue_frame),
        }
    }
}

pub fn track_kind_key(track: &Track) -> &'static str {
    if track.cue.is_some() {
        "gl-cue-segment"
    } else {
        "gl-whole-file"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_missing_titles_use_the_stem_and_optional_cue_number() {
        let path = Path::new("/music/night.live.flac");
        for title in ["", " \t\n", "\u{2003}"] {
            assert_eq!(track_title(title, path, None), "night.live");
            assert_eq!(track_title(title, path, Some(3)), "night.live · #03");
        }
        for cue in [None, Some(3)] {
            assert_eq!(track_title("  Valid title  ", path, cue), "  Valid title  ");
        }
    }
}
