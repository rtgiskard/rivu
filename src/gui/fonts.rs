use gpui::{App, Font, font};

#[cfg(not(feature = "embed-nerd-font"))]
pub(super) const SYMBOLS_FAMILY: &str = "Symbols Nerd Font Mono";
#[cfg(feature = "embed-nerd-font")]
pub(super) const SYMBOLS_FAMILY: &str = env!("RIVU_EMBEDDED_SYMBOLS_FAMILY");

#[cfg(feature = "embed-nerd-font")]
pub(super) fn register_symbols(cx: &App) -> anyhow::Result<()> {
    cx.text_system().add_fonts(vec![std::borrow::Cow::Borrowed(
        include_bytes!(env!("RIVU_EMBEDDED_SYMBOLS_FONT")),
    )])
}

/// Resolve only at startup or when the saved setting changes, never while painting.
pub(super) fn resolve(requested: &str, cx: &App) -> Font {
    let requested = requested.trim();
    let requested = if requested.is_empty() { "sans-serif" } else { requested };
    match system_family(requested) {
        Ok(primary) => {
            // Only select the primary family. Explicit fallback lists make GPUI
            // eagerly load every candidate; leave missing glyphs to its backend.
            if cx.text_system().all_font_names().contains(&primary) {
                return font(primary);
            }
            tracing::warn!(requested, primary, "Fontconfig family unavailable in GPUI");
        }
        Err(error) => tracing::warn!(requested, %error, "Cannot resolve system UI font"),
    }
    font("sans-serif")
}

#[cfg(target_os = "linux")]
fn system_family(requested: &str) -> anyhow::Result<String> {
    use anyhow::{Context, ensure};
    use fontconfig_sys::*;
    use std::{
        ffi::{CStr, CString},
        ptr,
    };

    struct Config(*mut FcConfig);
    impl Drop for Config {
        fn drop(&mut self) {
            // SAFETY: this guard owns a live config returned by Fontconfig.
            unsafe { FcConfigDestroy(self.0) }
        }
    }
    struct Pattern(*mut FcPattern);
    impl Drop for Pattern {
        fn drop(&mut self) {
            // SAFETY: this guard owns a live pattern returned by Fontconfig.
            unsafe { FcPatternDestroy(self.0) }
        }
    }

    let requested = CString::new(requested).context("Font family contains a NUL byte")?;
    // SAFETY: all C objects are checked before use and owned by guards. The
    // returned family is copied before its owning match pattern is destroyed.
    unsafe {
        let config = FcInitLoadConfigAndFonts();
        ensure!(!config.is_null(), "Cannot load Fontconfig configuration");
        let config = Config(config);
        let pattern = FcPatternCreate();
        ensure!(!pattern.is_null(), "Cannot allocate Fontconfig pattern");
        let pattern = Pattern(pattern);
        ensure!(
            FcPatternAddString(pattern.0, c"family".as_ptr(), requested.as_ptr().cast()) != 0,
            "Cannot set requested font family"
        );
        ensure!(
            FcConfigSubstitute(config.0, pattern.0, FcMatchPattern) != 0,
            "Cannot apply Fontconfig substitutions"
        );
        FcDefaultSubstitute(pattern.0);
        let mut result = FcResultNoMatch;
        let matched = FcFontMatch(config.0, pattern.0, &mut result);
        ensure!(!matched.is_null(), "Fontconfig returned no matching font");
        let matched = Pattern(matched);
        let mut name = ptr::null_mut();
        ensure!(
            FcPatternGetString(matched.0, c"family".as_ptr(), 0, &mut name) == FcResultMatch
                && !name.is_null(),
            "Fontconfig returned no family name"
        );
        Ok(CStr::from_ptr(name.cast()).to_string_lossy().into_owned())
    }
}

#[cfg(not(target_os = "linux"))]
fn system_family(requested: &str) -> anyhow::Result<String> {
    Ok(requested.to_owned())
}
