use crate::{
    layout::{UI_INSET, column},
    menu::{MENU_WIDTH, menu_item},
    theme::Palette,
};
use gpui::{prelude::*, *};

/// Opaque elevated material for menus, dropdowns, and tooltips only.
pub(crate) fn floating_surface(theme: &'static Palette) -> Div {
    div()
        .bg(rgb(theme.control))
        .border_1()
        .border_color(rgb(theme.border))
        .shadow_sm()
}

/// Modal material and pointer barrier. Dismissal and focus remain host policy.
pub struct Dialog;

impl Dialog {
    pub fn backdrop(id: impl Into<ElementId>) -> Stateful<Div> {
        div()
            .id(id)
            .occlude()
            .absolute()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x000000).alpha(0.58))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
    }

    pub fn card(theme: &'static Palette) -> Div {
        column()
            .bg(rgb(theme.panel))
            .border_1()
            .border_color(rgb(theme.border))
            .rounded_md()
    }
}

/// Elevated surface shells; none implicitly dismiss or change focus.
pub struct Popover;

impl Popover {
    pub fn surface(theme: &'static Palette, id: impl Into<ElementId>) -> Stateful<Div> {
        floating_surface(theme).id(id).occlude()
    }

    pub fn menu(
        theme: &'static Palette,
        id: impl Into<ElementId>,
        window: &Window,
    ) -> Stateful<Div> {
        Self::surface(theme, id)
            .w(rems(MENU_WIDTH / 16.))
            .max_w((window.viewport_size().width - px(UI_INSET * 2.)).max(px(0.)))
            .max_h((window.viewport_size().height - px(UI_INSET * 2.)).max(px(0.)))
            .overflow_y_scroll()
            .p_1()
            .gap_0()
            .rounded_md()
    }

    pub fn choices(
        theme: &'static Palette,
        id: impl Into<ElementId>,
        width: Pixels,
    ) -> Stateful<Div> {
        Self::surface(theme, id).w(width).gap_0().p_1().rounded_sm()
    }
}

#[derive(Clone, Copy)]
pub enum MenuIndicator {
    Check,
    Radio,
}

/// A normal actionable menu row with a shared checked/radio indicator.
pub fn checked_menu_item<T: 'static>(
    id: impl Into<ElementId>,
    label: impl IntoElement,
    checked: bool,
    indicator: MenuIndicator,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = crate::theme::palette(cx);
    let mark = match (indicator, checked) {
        (MenuIndicator::Check, true) => "✓",
        (MenuIndicator::Check, false) => "",
        (MenuIndicator::Radio, true) => "●",
        (MenuIndicator::Radio, false) => "○",
    };
    menu_item(id, label, cx, action).child(
        div()
            .ml_auto()
            .text_color(rgb(if checked { theme.accent } else { theme.muted }))
            .child(mark),
    )
}
