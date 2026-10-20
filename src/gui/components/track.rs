use crate::gui::appearance::fonts;
use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, rgb};
use rivu_ui::{list_row, row_tooltip, two_line_text};
/// Numbered track row shared by album, playlist, and queue views.
pub(crate) fn track_list_row(
    theme: &'static rivu_ui::theme::Palette,
    id: impl Into<gpui::ElementId>,
    text_id: impl Into<gpui::ElementId>,
    (title, detail): (String, String),
    index: usize,
    playing: bool,
    selected: bool,
) -> gpui::Stateful<Div> {
    list_row(theme, id, selected)
        .child(
            div()
                .w_8()
                .flex_shrink_0()
                .text_xs()
                .whitespace_nowrap()
                .font_features(fonts::tabular_numbers())
                .text_color(rgb(if playing {
                    theme.accent
                } else {
                    theme.secondary
                }))
                .when(playing, |view| {
                    view.font_family(fonts::SYMBOLS_FAMILY).font_size_scale(1.)
                })
                .child(if playing {
                    "".to_owned()
                } else {
                    (index + 1).to_string()
                }),
        )
        .child(track_row_text(theme, text_id, title, detail, playing))
}

/// Standard two-line row text with a full-content hover tooltip.
pub(crate) fn row_text(
    theme: &'static rivu_ui::theme::Palette,
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> Stateful<Div> {
    track_row_text(theme, id, title, detail, false)
}

fn track_row_text(
    theme: &'static rivu_ui::theme::Palette,
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    playing: bool,
) -> Stateful<Div> {
    let title = title.into();
    let detail = detail.into();
    let view = two_line_text(theme, title.clone(), detail.clone())
        .id(id)
        .when(playing, |view| view.text_color(rgb(theme.accent)));
    row_tooltip(view, title, detail)
}

/// Favorite action; the caller owns visibility, commands, and current state.
pub(in crate::gui) fn track_favorite_toggle<T: 'static>(
    id: impl Into<ElementId>,
    favorite: bool,
    text: impl Fn(&str) -> SharedString,
    cx: &mut gpui::Context<T>,
    action: impl Fn(&mut T, &mut gpui::Window, &mut gpui::Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = rivu_ui::theme::palette(cx);
    rivu_ui::quiet_icon_button(
        theme,
        rivu_ui::icon_button(
            id,
            if favorite { "󰓎" } else { "󰓐" },
            text(if favorite {
                "gl-unfavorite"
            } else {
                "gl-favorite"
            }),
            cx,
            action,
        ),
    )
    .when(favorite, |view| view.text_color(rgb(theme.accent)))
    .aria_toggled(if favorite {
        gpui::Toggled::True
    } else {
        gpui::Toggled::False
    })
    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
}
