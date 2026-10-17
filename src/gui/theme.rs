use crate::theme::{Palette, Theme};
use gpui::{App, Global};

struct ActiveTheme(Theme);

impl Global for ActiveTheme {}

pub(super) fn palette(cx: &App) -> &'static Palette {
    cx.try_global::<ActiveTheme>()
        .map_or_else(|| Theme::default().palette(), |active| active.0.palette())
}

pub(super) fn set_theme(theme: Theme, cx: &mut App) {
    if cx
        .try_global::<ActiveTheme>()
        .is_some_and(|active| active.0 == theme)
    {
        return;
    }
    cx.set_global(ActiveTheme(theme));
    cx.refresh_windows();
}
