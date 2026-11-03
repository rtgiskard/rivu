pub mod i18n;
pub mod keyboard;
pub mod theme;

/// Application icon and fallback artwork shared by presentation owners.
pub static DEFAULT_IMAGE: &[u8] = include_bytes!("../data/icons/rivu.png");
