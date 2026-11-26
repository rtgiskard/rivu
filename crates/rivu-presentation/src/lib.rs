pub mod audio_output;
pub mod history;
pub mod i18n;
pub mod keyboard;
pub mod navigation;
pub mod processing;
pub mod refresh;
pub mod theme;
pub mod time;
pub mod track;

/// Square fallback cover shared by GUI and MPRIS, separate from the application icon.
pub static DEFAULT_ARTWORK_SVG: &[u8] = include_bytes!("../data/artwork/default.svg");
