//! Shared, immutable GUI and TUI color presets. Visualization colors are independent.
//!
//! Tokyo Night colors: https://github.com/folke/tokyonight.nvim/tree/main/lua/tokyonight/colors
//! Day uses the upstream generated extras/lua/tokyonight_day.lua palette.

use rivu_ui::theme::Palette;
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

// Integer sRGB interpolation is only used in static initializers, never while rendering.
const fn blend(base: u32, tint: u32, percent: u32) -> u32 {
    let r = (((base >> 16) & 255) * (100 - percent) + ((tint >> 16) & 255) * percent) / 100;
    let g = (((base >> 8) & 255) * (100 - percent) + ((tint >> 8) & 255) * percent) / 100;
    let b = ((base & 255) * (100 - percent) + (tint & 255) * percent) / 100;
    (r << 16) | (g << 8) | b
}

// Rivu Dark keeps its blue-gray identity. Neutral hover never doubles as selection;
// only selection and primary actions use an accent-tinted surface. Auxiliary text
// stays below secondary text but above disabled controls in contrast.
static RIVU_DARK: Palette = Palette {
    bg: 0x1a1b26,
    panel: 0x16161e,
    control: 0x1f2231,
    input: 0x13131a,
    structure_border: 0x252938,
    border: 0x3b4261,
    text: 0xc0caf5,
    secondary: 0x9aa5ce,
    muted: 0x737aa2,
    disabled: 0x565f89,
    accent: 0x7aa2f7,
    accent_hover: blend(0x7aa2f7, 0xffffff, 12),
    on_accent: 0x1a1b26,
    highlight: 0x252938,
    selected: blend(0x16161e, 0x7aa2f7, 14),
    selected_hover: blend(0x16161e, 0x7aa2f7, 22),
    error: 0xf7768e,
    error_bg: 0x3b2330,
    warning: 0xe0af68,
};

// Night: bg_dark frames the normal bg and backs popup/control surfaces;
// fg/fg_dark/dark5 carry text hierarchy. bg_highlight is neutral hover, and blue0
// anchors upstream visual selection. Hover variants are evaluated at compile time.
static TOKYO_NIGHT: Palette = Palette {
    bg: 0x16161e,
    panel: 0x1a1b26,
    control: 0x16161e,
    input: 0x16161e,
    structure_border: 0x292e42,
    border: 0x414868,
    text: 0xc0caf5,
    secondary: 0xa9b1d6,
    muted: 0x737aa2,
    disabled: 0x565f89,
    accent: 0x7aa2f7,
    accent_hover: blend(0x7aa2f7, 0xffffff, 12),
    on_accent: 0x16161e,
    highlight: 0x292e42,
    selected: blend(0x1a1b26, 0x3d59a1, 40),
    selected_hover: blend(blend(0x1a1b26, 0x3d59a1, 40), 0x7aa2f7, 8),
    error: 0xdb4b4b,
    error_bg: blend(0x1a1b26, 0xdb4b4b, 12),
    warning: 0xe0af68,
};

// Day retains upstream fg, fg_dark, blue and bg_visual hues. Small black/white
// blends adapt editor colors to persistent GUI labels: darker blue permits white
// primary-action text; lighter hover/selection keeps secondary text readable.
// Auxiliary text interpolates comment toward fg_dark, never beyond secondary text.
static TOKYO_NIGHT_DAY: Palette = Palette {
    bg: 0xd0d5e3,
    panel: 0xe1e2e7,
    control: 0xd0d5e3,
    input: 0xd0d5e3,
    structure_border: 0xc4c8da,
    border: 0x8990b3,
    text: blend(0x3760bf, 0x000000, 8),
    secondary: 0x6172b0,
    muted: blend(0x848cb5, 0x6172b0, 90),
    disabled: 0x8990b3,
    accent: blend(0x2e7de9, 0x000000, 15),
    accent_hover: blend(0x2e7de9, 0x000000, 23),
    on_accent: 0xffffff,
    highlight: blend(0xc4c8da, 0xe1e2e7, 50),
    selected: blend(0xb7c1e3, 0xe1e2e7, 60),
    selected_hover: blend(0xb7c1e3, 0xe1e2e7, 50),
    error: 0xc64343,
    error_bg: 0xdababe,
    warning: 0x8c6c3e,
};

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
            let text_levels = [
                palette.text,
                palette.secondary,
                palette.muted,
                palette.disabled,
            ];
            for pair in text_levels.windows(2) {
                assert!(
                    contrast(pair[0], palette.panel) > contrast(pair[1], palette.panel),
                    "{theme:?} text hierarchy"
                );
            }
            assert!(contrast(palette.text, palette.bg) >= 4.5, "{theme:?} text");
            assert!(
                contrast(palette.muted, palette.panel) >= 3.0,
                "{theme:?} muted"
            );
            for surface in [
                palette.panel,
                palette.control,
                palette.highlight,
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
