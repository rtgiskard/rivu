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
        "sans-serif"
    } else {
        requested
    };
    let available = cx.text_system().all_font_names();
    match system_family(requested, &available) {
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

#[cfg(target_os = "linux")]
struct Pattern(*mut fontconfig_sys::FcPattern);

#[cfg(target_os = "linux")]
impl Drop for Pattern {
    fn drop(&mut self) {
        // SAFETY: Pattern owns a live Fontconfig pattern.
        unsafe { fontconfig_sys::FcPatternDestroy(self.0) }
    }
}

#[cfg(target_os = "linux")]
fn system_family(requested: &str, available: &[String]) -> anyhow::Result<String> {
    use anyhow::{Context, ensure};
    use fontconfig_sys::*;
    use std::ffi::CString;

    struct Config(*mut FcConfig);
    impl Drop for Config {
        fn drop(&mut self) {
            // SAFETY: this guard owns a live config returned by Fontconfig.
            unsafe { FcConfigDestroy(self.0) }
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
        available_family(&matched, available)
    }
}

#[cfg(not(target_os = "linux"))]
fn system_family(requested: &str, available: &[String]) -> anyhow::Result<String> {
    available
        .iter()
        .find(|name| name.as_str() == requested)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Requested font family unavailable in GPUI"))
}

#[cfg(target_os = "linux")]
fn available_family(pattern: &Pattern, available: &[String]) -> anyhow::Result<String> {
    use fontconfig_sys::*;
    use std::{ffi::CStr, ptr};

    // Fontconfig orders localized aliases by locale; GPUI lists one name per
    // face. Match every alias before declaring the resolved font unavailable.
    for index in 0.. {
        let mut name = ptr::null_mut();
        // SAFETY: Pattern remains live and owns each returned family string.
        let result = unsafe { FcPatternGetString(pattern.0, c"family".as_ptr(), index, &mut name) };
        if result != FcResultMatch {
            break;
        }
        anyhow::ensure!(!name.is_null(), "Fontconfig returned a null family name");
        // SAFETY: a successful FcPatternGetString returns a terminated string.
        let name = unsafe { CStr::from_ptr(name.cast()) }.to_string_lossy();
        if let Some(canonical) = available
            .iter()
            .find(|candidate| candidate.as_str() == name)
        {
            return Ok(canonical.clone());
        }
    }
    anyhow::bail!("Fontconfig family unavailable in GPUI")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use fontconfig_sys::*;
    use std::ffi::CString;

    fn pattern(families: &[&str]) -> Pattern {
        // SAFETY: each string is copied into the owned pattern by Fontconfig.
        unsafe {
            let raw = FcPatternCreate();
            assert!(!raw.is_null());
            let pattern = Pattern(raw);
            for family in families {
                let family = CString::new(*family).unwrap();
                assert_ne!(
                    FcPatternAddString(pattern.0, c"family".as_ptr(), family.as_ptr().cast()),
                    0
                );
            }
            pattern
        }
    }

    #[test]
    fn localized_alias_order_does_not_hide_an_available_font() {
        let available = vec!["Example Sans".to_owned()];
        for aliases in [["示例字体", "Example Sans"], ["Example Sans", "示例字体"]] {
            assert_eq!(
                available_family(&pattern(&aliases), &available).unwrap(),
                "Example Sans"
            );
        }
        assert!(available_family(&pattern(&["示例字体"]), &available).is_err());
        assert!(available_family(&pattern(&[]), &available).is_err());
    }
}
