use gpui::App;
use rivu_presentation::theme::Theme;

pub(in crate::gui) fn set_theme(theme: Theme, cx: &mut App) {
    rivu_ui::theme::set_palette(rivu_presentation::theme::palette(theme), cx);
}
