//! Shared, immutable GUI and TUI color presets. Visualization colors are independent.
//!
//! Tokyo Night colors: https://github.com/folke/tokyonight.nvim/tree/main/lua/tokyonight/colors
//! Day uses the upstream generated extras/lua/tokyonight_day.lua palette.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    RivuDark,
    TokyoNight,
    TokyoNightDay,
}

impl Theme {
    pub const ALL: [Self; 3] = [Self::RivuDark, Self::TokyoNight, Self::TokyoNightDay];

    pub fn label(self) -> &'static str {
        match self {
            Self::RivuDark => "Rivu Dark",
            Self::TokyoNight => "Tokyo Night",
            Self::TokyoNightDay => "Tokyo Night Day",
        }
    }

    pub fn palette(self) -> &'static Palette {
        match self {
            Self::RivuDark => &RIVU_DARK,
            Self::TokyoNight => &TOKYO_NIGHT,
            Self::TokyoNightDay => &TOKYO_NIGHT_DAY,
        }
    }
}

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

// Integer sRGB interpolation is only used in static initializers, never while rendering.
const fn blend(base: u32, tint: u32, percent: u32) -> u32 {
    let r = (((base >> 16) & 255) * (100 - percent) + ((tint >> 16) & 255) * percent) / 100;
    let g = (((base >> 8) & 255) * (100 - percent) + ((tint >> 8) & 255) * percent) / 100;
    let b = ((base & 255) * (100 - percent) + (tint & 255) * percent) / 100;
    (r << 16) | (g << 8) | b
}

const fn surfaces(mut palette: Palette, hover_tint: u32) -> Palette {
    // Light themes darken the accent on hover and need lighter selection surfaces.
    let light = hover_tint == 0;
    palette.accent_hover = blend(palette.accent, hover_tint, 12);
    palette.selected = blend(palette.panel, palette.accent, if light { 15 } else { 20 });
    palette.selected_hover = blend(palette.panel, palette.accent, if light { 20 } else { 28 });
    palette.error_bg = blend(palette.panel, palette.error, 12);
    palette
}

// Rivu's original Tokyo Night surfaces and blue-purple text remain the default.
// Lift only the old comment color for readable muted text; reserve it for disabled controls.
static RIVU_DARK: Palette = Palette {
    bg: 0x1a1b26,
    panel: 0x16161e,
    control: 0x1f2231,
    structure_border: 0x292e42,
    border: 0x3b4261,
    text: 0xc0caf5,
    secondary: 0x9aa5ce,
    muted: blend(0x565f89, 0x9aa5ce, 40),
    disabled: 0x565f89,
    accent: 0x7aa2f7,
    accent_hover: blend(0x7aa2f7, 0xffffff, 12),
    on_accent: 0x1a1b26,
    highlight: 0x292e42,
    selected: blend(0x16161e, 0x7aa2f7, 20),
    selected_hover: blend(0x16161e, 0x7aa2f7, 28),
    error: 0xf7768e,
    error_bg: 0x3b2330,
    warning: 0xe0af68,
};

static TOKYO_NIGHT: Palette = surfaces(
    Palette {
        bg: 0x16161e,
        panel: 0x1a1b26,
        control: 0x292e42,
        structure_border: 0x292e42,
        border: 0x414868,
        text: 0xc0caf5,
        secondary: 0xa9b1d6,
        muted: blend(0xa9b1d6, 0xc0caf5, 20),
        disabled: 0x737aa2,
        accent: 0x7aa2f7,
        accent_hover: 0,
        on_accent: 0x16161e,
        highlight: 0x292e42,
        selected: 0,
        selected_hover: 0,
        error: 0xf7768e,
        error_bg: 0,
        warning: 0xe0af68,
    },
    0xffffff,
);

// Day uses upstream dark5 for muted text and red1 for errors. Blend blue/fg
// toward blue6 for readable text and white-labeled accent buttons on light surfaces.
static TOKYO_NIGHT_DAY: Palette = surfaces(
    Palette {
        bg: 0xd0d5e3,
        panel: 0xe1e2e7,
        control: blend(0xc4c8da, 0xe1e2e7, 50),
        structure_border: 0xc4c8da,
        border: 0x8990b3,
        text: blend(0x3760bf, 0x2e5857, 50),
        secondary: blend(0x6172b0, 0x2e5857, 40),
        muted: 0x68709a,
        disabled: 0x8990b3,
        accent: blend(0x2e7de9, 0x2e5857, 60),
        accent_hover: 0,
        on_accent: 0xffffff,
        highlight: 0xc4c8da,
        selected: 0,
        selected_hover: 0,
        error: 0xc64343,
        error_bg: 0,
        warning: 0x8c6c3e,
    },
    0x000000,
);

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(rgb: u32) -> f64 {
        fn channel(value: u32) -> f64 {
            let value = f64::from(value & 255) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(rgb >> 16) + 0.7152 * channel(rgb >> 8) + 0.0722 * channel(rgb)
    }

    fn contrast(a: u32, b: u32) -> f64 {
        let a = luminance(a);
        let b = luminance(b);
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn presets_keep_text_and_accent_labels_readable() {
        for theme in Theme::ALL {
            let palette = theme.palette();
            assert!(contrast(palette.text, palette.bg) >= 4.5, "{theme:?} text");
            assert!(
                contrast(palette.muted, palette.panel) >= 3.0,
                "{theme:?} muted"
            );
            for surface in [
                palette.panel,
                palette.control,
                palette.selected,
                palette.selected_hover,
            ] {
                assert!(
                    contrast(palette.text, surface) >= 3.0,
                    "{theme:?} surface text"
                );
                assert!(
                    contrast(palette.secondary, surface) >= 3.0,
                    "{theme:?} secondary text"
                );
            }
            for surface in [palette.selected, palette.selected_hover] {
                assert!(
                    contrast(palette.accent, surface) >= 3.0,
                    "{theme:?} selected accent"
                );
            }
            for surface in [palette.accent, palette.accent_hover] {
                assert!(
                    contrast(palette.on_accent, surface) >= 4.5,
                    "{theme:?} accent label"
                );
            }
            let light = matches!(theme, Theme::TokyoNightDay);
            assert_eq!(luminance(palette.bg) > luminance(palette.text), light);
        }
    }
}
