use crate::{layout::column, menu::menu_item, theme::Palette};
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

    /// A padded, viewport-bounded scroll card; the caller chooses its width.
    pub fn scroll_card(
        theme: &'static Palette,
        id: impl Into<ElementId>,
        window: &Window,
    ) -> Stateful<Div> {
        Self::card(theme)
            .id(id)
            .max_w(relative(0.9))
            .max_h(window.viewport_size().height * 0.9)
            .overflow_y_scroll()
            .p_2()
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
        let inset = window.rem_size() * 0.5;
        Self::surface(theme, id)
            .role(Role::Menu)
            .max_w((window.viewport_size().width - inset * 2.).max(px(0.)))
            .max_h((window.viewport_size().height - inset * 2.).max(px(0.)))
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
        Self::surface(theme, id)
            .role(Role::ListBox)
            .w(width)
            .gap_0()
            .p_1()
            .rounded_sm()
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
    label: impl Into<SharedString>,
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
    menu_item(id, label, cx, action)
        .role(match indicator {
            MenuIndicator::Check => Role::MenuItemCheckBox,
            MenuIndicator::Radio => Role::MenuItemRadio,
        })
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .child(
            div()
                .ml_auto()
                .w(rems(crate::theme::font_size_scale(cx)))
                .flex_shrink_0()
                .flex()
                .justify_center()
                .text_color(rgb(if checked { theme.accent } else { theme.muted }))
                .child(mark),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use std::{cell::Cell, rc::Rc};

    struct CheckedMenus {
        bounds: Rc<Cell<[Bounds<Pixels>; 4]>>,
    }

    impl Render for CheckedMenus {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let mut menus = column()
                .items_start()
                .font_size_scale(crate::theme::font_size_scale(cx));
            for (index, (indicator, checked)) in [
                (MenuIndicator::Check, false),
                (MenuIndicator::Check, true),
                (MenuIndicator::Radio, false),
                (MenuIndicator::Radio, true),
            ]
            .into_iter()
            .enumerate()
            {
                let bounds = self.bounds.clone();
                menus = menus.child(
                    column()
                        .relative()
                        .child(checked_menu_item(
                            ("choice", index),
                            "Keep the same label",
                            checked,
                            indicator,
                            cx,
                            |_, _, _| {},
                        ))
                        .child(
                            canvas(
                                move |value, _, _| {
                                    let mut measured = bounds.get();
                                    measured[index] = value;
                                    bounds.set(measured);
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        ),
                );
            }
            menus
        }
    }

    #[gpui::test]
    fn checked_menu_indicators_keep_intrinsic_width_across_states_and_scales(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::theme::init_test);
        let bounds = Rc::new(Cell::new([Bounds::default(); 4]));
        let window = cx.add_window({
            let bounds = bounds.clone();
            move |_, _| CheckedMenus { bounds }
        });
        for interface_scale in [0.75, 1., 1.5, 2.] {
            for font_size in [8, 11, 18] {
                cx.update(|cx| {
                    crate::theme::set_typography(
                        gpui::font("sans-serif"),
                        "sans-serif",
                        font_size,
                        cx,
                    )
                });
                cx.update_window(window.into(), |_, window, cx| {
                    window.set_rem_size(px(16. * interface_scale));
                    window.draw(cx).clear(cx);
                })
                .unwrap();
                let measured = bounds.get();
                assert!(measured[0].size.width > px(0.));
                for menu in &measured[1..] {
                    assert_eq!(menu.size, measured[0].size);
                }
            }
        }
    }
}
