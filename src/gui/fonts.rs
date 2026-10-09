use gpui::{App, Font, FontFallbacks, font};

/// Resolve only at startup or when the saved setting changes, never while painting.
pub(super) fn resolve(requested: &str, cx: &App) -> Font {
    let requested = requested.trim();
    if requested.is_empty() {
        return font(".SystemUIFont");
    }
    match system_families(requested) {
        Ok(families) => {
            // GPUI accepts family names, not file/face identities. Exclude names its
            // database cannot load so one unavailable fallback cannot break the run.
            let available = cx.text_system().all_font_names();
            let mut families = families
                .into_iter()
                .filter(|family| available.iter().any(|name| name == family));
            if let Some(primary) = families.next() {
                let mut resolved = font(primary);
                resolved.fallbacks = Some(FontFallbacks::from_fonts(families.collect()));
                return resolved;
            }
            tracing::warn!(requested, "No Fontconfig candidates available in GPUI");
        }
        Err(error) => tracing::warn!(requested, %error, "Cannot resolve system UI font"),
    }
    font(".SystemUIFont")
}

#[cfg(target_os = "linux")]
fn system_families(requested: &str) -> anyhow::Result<Vec<String>> {
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
    struct FontSet(*mut FcFontSet);
    impl Drop for FontSet {
        fn drop(&mut self) {
            // SAFETY: FcFontSort transfers ownership of this set to the caller.
            unsafe { FcFontSetDestroy(self.0) }
        }
    }

    let requested = CString::new(requested).context("Font family contains a NUL byte")?;
    // SAFETY: all C objects are checked before use and owned by guards. Returned
    // strings remain borrowed only until copied, while their font set is alive.
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
        // trim=true removes candidates that add no character coverage. We retain
        // metadata only; GPUI loads faces through its existing font database.
        let fonts = FcFontSort(config.0, pattern.0, 1, ptr::null_mut(), &mut result);
        ensure!(!fonts.is_null(), "Fontconfig returned no font set");
        let fonts = FontSet(fonts);
        let mut families = Vec::new();
        for index in 0..(*fonts.0).nfont as usize {
            let face = *(*fonts.0).fonts.add(index);
            let mut name = ptr::null_mut();
            if FcPatternGetString(face, c"family".as_ptr(), 0, &mut name) == FcResultMatch
                && !name.is_null()
            {
                let name = CStr::from_ptr(name.cast()).to_string_lossy().into_owned();
                if !families.contains(&name) {
                    families.push(name);
                }
            }
        }
        ensure!(!families.is_empty(), "Fontconfig returned no font families");
        Ok(families)
    }
}

#[cfg(not(target_os = "linux"))]
fn system_families(requested: &str) -> anyhow::Result<Vec<String>> {
    Ok(vec![requested.to_owned()])
}
