use crate::layout::row;
use gpui::{Div, ElementId, SharedString, Stateful, Window, div, prelude::*, rgb};

pub const LIST_ROW_REMS: f32 = 2.625;
pub const DROPDOWN_ROW_REMS: f32 = 2.0;

pub fn row_background(
    theme: &'static crate::theme::Palette,
    selected: bool,
    hovered: bool,
) -> gpui::Rgba {
    rgb(match (selected, hovered) {
        (true, true) => theme.selected_hover,
        (true, false) => theme.selected,
        (false, true) => theme.highlight,
        (false, false) => theme.panel,
    })
}

pub fn tree_row(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    depth: usize,
    selected: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .h(gpui::rems(1.75))
        .px_2()
        .pr_2()
        .pl(gpui::rems(0.5 + depth as f32 * 1.25))
        .min_w_0()
        .flex()
        .items_center()
        .gap_1()
        .rounded_sm()
        .border_1()
        .border_color(gpui::transparent_black())
        .cursor_pointer()
        .bg(row_background(theme, selected, false))
        .hover(move |style| style.bg(row_background(theme, selected, true)))
}

/// Standard compact option row used by anchored dropdowns.
pub fn dropdown_row(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    selected: bool,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    list_row_surface(theme, id, selected)
        .when(!selected, |row| row.bg(rgb(theme.panel).alpha(0.0)))
        .hover(move |style| {
            style.bg(rgb(if selected {
                theme.selected_hover
            } else {
                theme.highlight
            }))
        })
        .h(gpui::rems(DROPDOWN_ROW_REMS))
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
}

pub fn list_row_height(window: &Window) -> gpui::Pixels {
    window.rem_size() * LIST_ROW_REMS
}

/// Shared row geometry and idle background; each caller owns its single hover style.
fn list_row_surface(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    selected: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .h(gpui::rems(LIST_ROW_REMS))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_2()
        .min_w_0()
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(gpui::transparent_black())
        .overflow_hidden()
        .cursor_pointer()
        .bg(row_background(theme, selected, false))
}

/// Standard selectable row shared by library, queue, playlist, and history.
pub fn list_row(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    selected: bool,
) -> Stateful<Div> {
    list_row_surface(theme, id, selected)
        .hover(move |style| style.bg(row_background(theme, selected, true)))
}

/// Two independently truncated text lines, without tooltip or input behavior.
/// The primary line inherits its color; callers can override it on the returned view.
pub fn two_line_text(
    theme: &'static crate::theme::Palette,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> Div {
    crate::layout::column()
        .flex_1()
        .gap_0()
        .overflow_hidden()
        .text_color(rgb(theme.text))
        .child(div().text_sm().truncate().child(title.into()))
        .child(crate::layout::caption(theme, detail).truncate())
}

// Out-of-flow actions leave the full text width available until the row is hovered.
pub fn hover_actions(
    theme: &'static crate::theme::Palette,
    group: &'static str,
    selected: bool,
) -> Div {
    row()
        .absolute()
        .right_0()
        .top_0()
        .h_full()
        .gap_0()
        .bg(row_background(theme, selected, true))
        .invisible()
        .group_hover(group, |style| style.visible())
}
