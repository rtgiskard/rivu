use crate::theme::Theme;
use gpui::App;

pub(in crate::gui) fn set_theme(theme: Theme, cx: &mut App) {
    rivu_ui::theme::set_palette(theme.palette(), cx);
}
