use gpui::{App, Font, FontFeatures, font};
use std::sync::{Arc, LazyLock};

pub(in crate::gui) fn tabular_numbers() -> FontFeatures {
    static FEATURES: LazyLock<FontFeatures> =
        LazyLock::new(|| FontFeatures(Arc::new(vec![("tnum".into(), 1)])));
    FEATURES.clone()
}

#[cfg(not(feature = "embed-nerd-font"))]
pub(in crate::gui) const SYMBOLS_FAMILY: &str = "Symbols Nerd Font Mono";
#[cfg(feature = "embed-nerd-font")]
pub(in crate::gui) const SYMBOLS_FAMILY: &str = env!("RIVU_EMBEDDED_SYMBOLS_FAMILY");

#[cfg(feature = "embed-nerd-font")]
pub(in crate::gui) fn register_symbols(cx: &App) -> anyhow::Result<()> {
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(env!(
            "RIVU_EMBEDDED_SYMBOLS_FONT"
        )))])
}

pub(in crate::gui) fn publish(font: &Font, font_size: u32, cx: &mut App) {
    rivu_ui::theme::set_typography(font.clone(), SYMBOLS_FAMILY, font_size, cx);
}

/// Resolve only at startup or when the saved setting changes, never while painting.
pub(in crate::gui) fn resolve(requested: &str, cx: &App) -> Font {
    let requested = requested.trim();
    let requested = if requested.is_empty() {
        "monospace"
    } else {
        requested
    };
    match cx.text_system().resolve_system_font_family(requested) {
        Ok(primary) => {
            let resolved = font(primary);
            #[cfg(target_os = "linux")]
            let resolved = Font {
                fallbacks: Some(gpui::FontFallbacks::from_system_family(
                    requested.to_owned(),
                    resolved.family.clone(),
                )),
                ..resolved
            };
            return resolved;
        }
        Err(error) => tracing::warn!(requested, %error, "Cannot resolve system UI font"),
    }
    font("sans-serif")
}
