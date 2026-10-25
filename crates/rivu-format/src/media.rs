use serde::{Deserialize, Serialize};
/// User-selected decoder routing, applied when opening a track for playback.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecoderHint {
    #[default]
    Auto,
    Native,
    Ffmpeg,
}
