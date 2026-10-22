use serde::{Deserialize, Serialize};

/// Decoded file metadata shared by scanning, persistence, and playback.
///
/// This value contains no decoder or audio-output state; probing supplies it and
/// the library caches it independently of the playback engine.
/// This carries no library identity or user-maintained state.
#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Option<f64>,
    /// Encoded audio bitrate in bits/s; a scan estimate unless `bitrate_exact` is true.
    pub bitrate_bps: Option<u64>,
    /// Reliable fixed-encoding calculation or complete-playback measured average;
    /// decoder overlap/delay/padding handling can introduce small backend differences.
    pub bitrate_exact: bool,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    /// Source PCM/lossless precision, never the decoder's output sample width.
    pub bits_per_sample: Option<u32>,
    /// Original date precision is retained (e.g. year, year-month, or full date).
    pub release_date: Option<String>,
    pub codec: String,
    pub channels: u16,
    pub sample_rate: u32,
}

/// User-selected decoder routing, applied when opening a track for playback.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecoderHint {
    #[default]
    Auto,
    Native,
    Ffmpeg,
}
