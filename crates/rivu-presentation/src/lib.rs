pub mod history;
pub mod i18n;
pub mod keyboard;
pub mod processing;
pub mod theme;
pub mod time;
pub mod track;

/// Application icon and fallback artwork shared by presentation owners.
pub static DEFAULT_ARTWORK_SVG: &[u8] = include_bytes!("../data/icons/rivu.svg");
