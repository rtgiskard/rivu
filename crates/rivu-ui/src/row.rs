use crate::layout::row;
use gpui::{Div, ElementId, SharedString, Stateful, Window, div, prelude::*, rgb};

pub const LIST_ROW_REMS: f32 = 2.625;
pub const DROPDOWN_ROW_REMS: f32 = 2.0;

/// A transparent half-row drop target with no ordinary pointer handlers.
/// Keep it attached across drag starts; only valid drag-over styles reveal its border.
pub fn list_drop_target(id: impl Into<ElementId>, after: bool) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .left_0()
        .right_0()
        .top(gpui::relative(if after { 0.5 } else { 0.0 }))
        .h(gpui::relative(0.5))
        .border_color(gpui::transparent_black())
        .when(after, |zone| zone.border_b_2())
        .when(!after, |zone| zone.border_t_2())
}

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
        .h_7()
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

/// Uniform two-line row height, growing only when text no longer fits the geometry.
pub fn list_row_height(window: &Window, cx: &gpui::App) -> gpui::Pixels {
    let scale = crate::theme::font_size_scale(cx);
    let line_height = |font_rems| {
        (gpui::phi().to_pixels(gpui::rems(font_rems).into(), window.rem_size()) * scale).round()
    };
    (window.rem_size() * LIST_ROW_REMS).max(line_height(0.875) + line_height(0.75) + gpui::px(2.))
}

/// Uniform single-line list and option height; borders remain physical layout pixels.
pub fn single_line_row_height(window: &Window, cx: &gpui::App) -> gpui::Pixels {
    (window.rem_size() * DROPDOWN_ROW_REMS).max(
        (gpui::phi().to_pixels(gpui::rems(0.875).into(), window.rem_size())
            * crate::theme::font_size_scale(cx))
        .round()
            + gpui::px(2.),
    )
}

/// Compact tree height follows text, not the size of standalone action buttons.
pub fn tree_row_height(window: &Window, cx: &gpui::App) -> gpui::Pixels {
    (window.rem_size() * 1.75).max(
        (gpui::phi().to_pixels(gpui::rems(0.875).into(), window.rem_size())
            * crate::theme::font_size_scale(cx))
        .round()
            + gpui::px(2.),
    )
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
        .line_height(gpui::relative(1.28))
        .overflow_hidden()
        .text_color(rgb(theme.text))
        .child(div().text_sm().truncate().child(title.into()))
        .child(crate::layout::caption(theme, detail).truncate())
}

/// Isolate an action area's presses from row selection and drag handlers.
/// The barrier belongs on an ancestor so native child focus and Click run first.
pub fn row_action_region(id: impl Into<ElementId>) -> Stateful<Div> {
    row()
        .id(id)
        .flex_shrink_0()
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// Reveal actions on row hover or descendant keyboard focus, preserving child hover styles.
pub fn hover_reveal(
    id: impl Into<ElementId>,
    group: &'static str,
    content: impl IntoElement,
) -> Stateful<Div> {
    hover_reveal_surface(id, group).child(content)
}

fn hover_reveal_surface(id: impl Into<ElementId>, group: &'static str) -> Stateful<Div> {
    row_action_region(id)
        .tab_index(0)
        .tab_stop(false)
        .opacity(0.)
        .group_hover(group, |style| style.opacity(1.))
        .focus_visible_within(|style| style.opacity(1.))
}

// Out-of-flow actions leave the full text width available until the row is hovered.
pub fn hover_actions(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    group: &'static str,
    selected: bool,
) -> Stateful<Div> {
    hover_reveal_surface(id, group)
        .absolute()
        .right_0()
        .top_0()
        .h_full()
        .gap_0()
        .bg(row_background(theme, selected, false))
        .group_hover(group, move |style| {
            style.opacity(1.).bg(row_background(theme, selected, true))
        })
}
