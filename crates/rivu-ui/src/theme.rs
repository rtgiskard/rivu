//! Host-provided immutable colors and live resolved typography.

use gpui::{App, Font, Global, SharedString};

/// Semantic 24-bit RGB colors; no alpha or runtime color conversion is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: u32,
    pub panel: u32,
    pub control: u32,
    pub structure_border: u32,
    pub border: u32,
    pub text: u32,
    pub secondary: u32,
    pub muted: u32,
    pub disabled: u32,
    pub accent: u32,
    pub accent_hover: u32,
    pub on_accent: u32,
    pub highlight: u32,
    pub selected: u32,
    pub selected_hover: u32,
    pub error: u32,
    pub error_bg: u32,
    pub warning: u32,
}

struct ActivePalette(&'static Palette);
impl Global for ActivePalette {}

struct Typography {
    font: Font,
    icons: SharedString,
}
impl Global for Typography {}

/// Install the host's palette before rendering controls. Presets remain host-owned.
pub fn set_palette(palette: &'static Palette, cx: &mut App) {
    if cx
        .try_global::<ActivePalette>()
        .is_some_and(|active| std::ptr::eq(active.0, palette))
    {
        return;
    }
    cx.set_global(ActivePalette(palette));
    cx.refresh_windows();
}

pub fn palette(cx: &App) -> &'static Palette {
    cx.global::<ActivePalette>().0
}

/// Publish resolved fonts at startup and whenever host typography changes.
/// Independent tooltip and drag roots read this current value on every render.
pub fn set_typography(font: Font, icons: impl Into<SharedString>, cx: &mut App) {
    let icons = icons.into();
    if cx
        .try_global::<Typography>()
        .is_some_and(|current| current.font == font && current.icons == icons)
    {
        return;
    }
    cx.set_global(Typography { font, icons });
    cx.refresh_windows();
}

pub fn font(cx: &App) -> &Font {
    &cx.global::<Typography>().font
}

pub fn icon_family(cx: &App) -> SharedString {
    cx.global::<Typography>().icons.clone()
}

#[cfg(test)]
pub(crate) fn init_test(cx: &mut App) {
    // Deliberately neutral fixture, not an application preset.
    static PALETTE: Palette = Palette {
        bg: 0x101010,
        panel: 0x202020,
        control: 0x303030,
        structure_border: 0x404040,
        border: 0x505050,
        text: 0xffffff,
        secondary: 0xdddddd,
        muted: 0xaaaaaa,
        disabled: 0x777777,
        accent: 0x80aaff,
        accent_hover: 0xaaccff,
        on_accent: 0x000000,
        highlight: 0x404040,
        selected: 0x304060,
        selected_hover: 0x405070,
        error: 0xff8080,
        error_bg: 0x402020,
        warning: 0xffdd80,
    };
    set_palette(&PALETTE, cx);
    set_typography(gpui::font("sans-serif"), "sans-serif", cx);
}
