use crate::{control::activate_control, layout::UI_INSET, overlay::text_tooltip};
use gpui::{Context, Div, ElementId, Role, SharedString, Stateful, Window, div, prelude::*, rgb};

pub const MENU_WIDTH: f32 = 260.0;
pub const POPOVER_MAX_REMS: f32 = 15.0;

/// Menu item with shared application event handling.
pub fn menu_item<T: 'static>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = crate::theme::palette(cx);
    let label = label.into();
    activate_control(menu_item_style(theme, id, label.clone(), label), cx, action)
}

pub fn menu_separator(theme: &'static crate::theme::Palette) -> Div {
    div().h(gpui::px(1.)).mx_2().my_1().bg(rgb(theme.border))
}

pub fn menu_title(theme: &'static crate::theme::Palette, title: impl Into<SharedString>) -> Div {
    div()
        .px(gpui::px(UI_INSET))
        .py_1()
        .text_xs()
        .text_color(rgb(theme.muted))
        .child(title.into())
}

pub fn menu_back<T: 'static>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let label = label.into();
    menu_item(id, "", cx, action)
        .aria_label(label.clone())
        .font_family(crate::theme::icon_family(cx))
        .font_size_scale(1.)
        .tooltip(text_tooltip(label))
}

/// Destructive menu rows use text, not a button border, to identify the action.
pub fn danger_menu_item(
    theme: &'static crate::theme::Palette,
    control: Stateful<Div>,
) -> Stateful<Div> {
    control.text_color(rgb(theme.error))
}

/// Custom menu content must supply its actual accessible text, never a glyph or debug name.
pub fn menu_item_style(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    content: impl IntoElement,
) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::MenuItem)
        .aria_label(label)
        .tab_index(0)
        .focus(|style| style.bg(rgb(theme.selected)))
        .w_full()
        .min_h(gpui::rems(2.0))
        .px(gpui::px(UI_INSET))
        .py_0()
        .flex()
        .gap_2()
        .items_center()
        .text_sm()
        .cursor_pointer()
        .border_1()
        .border_color(gpui::transparent_black())
        .bg(rgb(theme.panel).alpha(0.0))
        .rounded_sm()
        .hover(|style| style.bg(rgb(theme.highlight)))
        .child(div().flex_1().min_w_0().whitespace_normal().child(content))
}
