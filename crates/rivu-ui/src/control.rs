use crate::overlay::text_tooltip;
use gpui::{
    Context, Div, ElementId, Role, SharedString, Stateful, Toggled, Window, div, prelude::*, rgb,
};

/// Keep keyboard, pointer, and GPUI's accessible Click on the same action path.
pub fn activate_control<T: 'static>(
    control: Stateful<Div>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    control.on_click(cx.listener(move |this, _, window, cx| {
        action(this, window, cx);
        cx.stop_propagation();
    }))
}

/// Selected appearance only; callers supply role-specific selected/toggled semantics.
pub fn active_control(
    theme: &'static crate::theme::Palette,
    control: Stateful<Div>,
    active: bool,
) -> Stateful<Div> {
    control.when(active, |view| {
        view.bg(rgb(theme.selected))
            .text_color(rgb(theme.accent))
            .group_hover("control", |style| style.bg(rgb(theme.selected_hover)))
    })
}

/// Shared geometry and interaction surface for buttons and select triggers.
/// Variants replace the shared group's hover material; focus retains its own border.
fn control_surface(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
) -> Stateful<Div> {
    div()
        .id(id)
        .group("control")
        .tab_index(0)
        .focus(|style| style.border_color(rgb(theme.accent)))
        .flex()
        .items_center()
        .min_w_0()
        .px_2()
        .py_1()
        .rounded_sm()
        .min_h(gpui::rems(2.0))
        .text_sm()
        .text_color(rgb(theme.text))
        .cursor_pointer()
        .bg(rgb(theme.control))
        .border_1()
        .border_color(rgb(theme.border))
        .group_hover("control", |style| style.bg(rgb(theme.highlight)))
}

/// Reusable visual button primitive. It has no application-specific behavior.
pub fn button_style(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    let label = label.into();
    control_surface(theme, id)
        .role(Role::Button)
        .aria_label(label.clone())
        .justify_center()
        .child(label)
}

/// Text button with shared application event handling.
pub fn button<T: 'static>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = crate::theme::palette(cx);
    activate_control(button_style(theme, id, label), cx, action)
}

/// Sortable list header control with shared active-direction styling.
pub fn sortable_header<T: 'static>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    descending: bool,
    width: Option<gpui::Pixels>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = crate::theme::palette(cx);
    let label = label.into();
    let mut control = div()
        .id(id)
        .role(Role::ColumnHeader)
        .aria_label(label.clone())
        .a11y_synthetic_children(move |builder| {
            if active {
                builder.parent_node().set_sort_direction(if descending {
                    gpui::accesskit::SortDirection::Descending
                } else {
                    gpui::accesskit::SortDirection::Ascending
                });
            }
        })
        .tab_index(0)
        .focus(|style| style.border_color(rgb(theme.accent)))
        .flex()
        .items_center()
        .gap_1()
        .h_full()
        .min_w_0()
        .overflow_hidden()
        .border_1()
        .border_color(gpui::transparent_black())
        .text_xs()
        .text_color(rgb(if active { theme.accent } else { theme.muted }))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme.highlight)).text_color(rgb(theme.text)))
        .on_click(cx.listener(move |this, _, window, cx| {
            action(this, window, cx);
            cx.stop_propagation();
        }))
        .child(div().min_w_0().truncate().child(label))
        .when(active, |control| {
            control.child(
                div()
                    .flex_shrink_0()
                    .font_family(crate::theme::icon_family(cx))
                    .font_size_scale(1.)
                    .child(if descending { "󰁅" } else { "󰁝" }),
            )
        });
    if let Some(width) = width {
        control = control.w(width).flex_shrink_0();
    } else {
        control = control.flex_1().min_w_0();
    }
    control
}

/// Shared outer height for toolbar actions and collapsed or expanded search.
pub(crate) fn toolbar_control_height(cx: &gpui::App) -> gpui::Rems {
    gpui::rems((1.25 * crate::theme::font_size_scale(cx)).max(2.))
}

/// Icon button with the Nerd symbol font and shared tooltip behavior.
/// Control text sizing is overridable; the glyph resets the base-text scale.
pub fn icon_button<T: 'static>(
    id: impl Into<ElementId>,
    icon: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let hint = hint.into();
    let theme = crate::theme::palette(cx);
    activate_control(
        control_surface(theme, id)
            .role(Role::Button)
            .aria_label(hint.clone())
            .child(self::icon(icon, cx)),
        cx,
        action,
    )
    .w(gpui::rems(2.))
    .h(toolbar_control_height(cx))
    .text_size(gpui::rems(1.))
    .p_0()
    .flex()
    .items_center()
    .justify_center()
    .flex_shrink_0()
    .tooltip(text_tooltip(hint))
}

/// Low-emphasis inline action; preserve the shared icon hit area and tooltip.
pub fn quiet_icon_button(
    theme: &'static crate::theme::Palette,
    control: Stateful<Div>,
) -> Stateful<Div> {
    control
        .border_color(gpui::transparent_black())
        .bg(rgb(theme.panel).alpha(0.0))
        .text_color(rgb(theme.secondary))
}

/// Unavailable actions retain owner guards and suppress all native activation paths.
pub fn disabled_control(
    theme: &'static crate::theme::Palette,
    control: Stateful<Div>,
    disabled: bool,
) -> Stateful<Div> {
    control.when(disabled, |view| {
        view.text_color(rgb(theme.disabled))
            .border_color(rgb(theme.structure_border))
            .group_hover("control", |style| style.bg(rgb(theme.control)))
            .cursor_default()
            .tab_stop(false)
            .a11y_synthetic_children(|builder| {
                let node = builder.parent_node();
                node.set_disabled();
                node.remove_action(gpui::AccessibleAction::Click);
                node.remove_action(gpui::AccessibleAction::Focus);
            })
            .capture_key_down(|event, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    window.prevent_default();
                    cx.stop_propagation();
                }
            })
            .capture_key_up(|event, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    window.prevent_default();
                    cx.stop_propagation();
                }
            })
            .capture_any_mouse_down(|_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .capture_any_mouse_up(|_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
    })
}

/// Destructive text remains visible on hover; keep the shared focus treatment.
pub fn danger_control(
    theme: &'static crate::theme::Palette,
    control: Stateful<Div>,
) -> Stateful<Div> {
    control
        .text_color(rgb(theme.error))
        .border_color(rgb(theme.error))
}

/// Compact toggle button used by visual and system settings.
pub fn toggle_button<T: 'static>(
    id: impl Into<ElementId>,
    icon: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    enabled: bool,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = crate::theme::palette(cx);
    active_control(
        theme,
        icon_button(id, icon, hint, cx, move |this, _, cx| {
            action(this, cx);
            cx.notify();
        })
        .role(Role::Switch)
        .aria_toggled(if enabled {
            Toggled::True
        } else {
            Toggled::False
        }),
        enabled,
    )
}

/// Shared trigger for settings and other anchored dropdowns.
pub fn dropdown_trigger(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    expanded: bool,
    cx: &gpui::App,
) -> Stateful<Div> {
    let label = label.into();
    control_surface(theme, id)
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_value(label.clone())
        .aria_expanded(expanded)
        .w_full()
        .gap_2()
        .child(div().flex_1().min_w_0().truncate().child(label))
        .child(
            div()
                .flex_shrink_0()
                .text_color(rgb(theme.muted))
                .font_family(crate::theme::icon_family(cx))
                .font_size_scale(1.)
                .text_size(gpui::rems(0.875))
                .child("󰅀"),
        )
}

pub fn compact_icon_button(
    theme: &'static crate::theme::Palette,
    control: gpui::Stateful<Div>,
) -> gpui::Stateful<Div> {
    quiet_icon_button(theme, control).w_6()
}

/// Decorative Nerd Font glyph; the owning control supplies its accessible name.
/// Size is inherited so callers can use the normal GPUI text-size builders.
pub fn icon(glyph: impl Into<SharedString>, cx: &gpui::App) -> Div {
    div()
        .font_family(crate::theme::icon_family(cx))
        .font_size_scale(1.)
        .flex_shrink_0()
        .child(glyph.into())
}

/// Controlled labeled checkbox. The owner commits the requested next value.
/// Its hit area uses normal control sizing; disabled controls never request changes.
pub fn checkbox<T: 'static>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    checked: bool,
    enabled: bool,
    cx: &mut Context<T>,
    on_change: impl Fn(&mut T, bool, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = crate::theme::palette(cx);
    let label = label.into();
    let control = control_surface(theme, id)
        .role(Role::CheckBox)
        .aria_label(label.clone())
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .gap_2()
        .child(icon(if checked { "󰄲" } else { "󰄱" }, cx))
        .child(label);
    let control = activate_control(control, cx, move |this, _, cx| {
        if enabled {
            on_change(this, !checked, cx);
            cx.notify();
        }
    });
    disabled_control(theme, control, !enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Element, FocusHandle, InputEvent, Render, TestAppContext};

    struct Controls {
        focus: FocusHandle,
        disabled: bool,
        clicks: usize,
    }

    impl Render for Controls {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = crate::theme::palette(cx);
            div().size_full().child(disabled_control(
                theme,
                button("action", "Apply", cx, |this, _, cx| {
                    this.clicks += 1;
                    cx.notify();
                })
                .track_focus(&self.focus)
                .w(gpui::px(160.)),
                self.disabled,
            ))
        }
    }

    #[gpui::test]
    fn disabled_controls_block_pointer_and_native_keyboard_clicks(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        let focus = cx.update(|cx| cx.focus_handle());
        let window = cx.add_window(|_, _| Controls {
            focus: focus.clone(),
            disabled: true,
            clicks: 0,
        });
        let controls = window.root(cx).unwrap();
        let handle = window.into();
        for (disabled, expected) in [(true, 0), (false, 3), (true, 3)] {
            controls.update(cx, |this, cx| {
                this.disabled = disabled;
                cx.notify();
            });
            cx.update_window(handle, |_, window, cx| window.focus(&focus, cx))
                .unwrap();
            cx.run_until_parked();
            cx.update_window(handle, |_, window, cx| {
                window.draw(cx).clear(cx);
                let position = gpui::point(gpui::px(12.), gpui::px(12.));
                window.dispatch_event(
                    gpui::MouseDownEvent {
                        button: gpui::MouseButton::Left,
                        position,
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
                window.dispatch_event(
                    gpui::MouseUpEvent {
                        button: gpui::MouseButton::Left,
                        position,
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
            })
            .unwrap();
            for key in ["enter", "space"] {
                cx.update_window(handle, |_, window, cx| {
                    window.draw(cx).clear(cx);
                    let keystroke = gpui::Keystroke::parse(key).unwrap();
                    window.dispatch_event(
                        gpui::KeyDownEvent {
                            keystroke: keystroke.clone(),
                            is_held: false,
                            prefer_character_input: false,
                        }
                        .to_platform_input(),
                        cx,
                    );
                    window.dispatch_event(gpui::KeyUpEvent { keystroke }.to_platform_input(), cx);
                })
                .unwrap();
                cx.run_until_parked();
            }
            assert_eq!(controls.read_with(cx, |this, _| this.clicks), expected);
        }
    }

    #[gpui::test]
    fn icon_names_and_boolean_roles_are_not_visual_glyphs(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        let entity = cx.new(|cx| Controls {
            focus: cx.focus_handle(),
            disabled: false,
            clicks: 0,
        });
        entity.update(cx, |_, cx| {
            let icon = icon_button("save", "󰆓", "Save changes", cx, |_, _, _| {});
            let mut node = gpui::accesskit::Node::new(icon.a11y_role().unwrap());
            icon.write_a11y_info(&mut node);
            assert_eq!(node.role(), Role::Button);
            assert_eq!(node.label(), Some("Save changes"));
            assert!(node.supports_action(gpui::AccessibleAction::Click));

            let toggle = toggle_button("enabled", "󰄲", "Enabled", true, cx, |_, _| {});
            let mut node = gpui::accesskit::Node::new(toggle.a11y_role().unwrap());
            toggle.write_a11y_info(&mut node);
            assert_eq!(node.role(), Role::Switch);
            assert_eq!(node.toggled(), Some(Toggled::True));
            assert_eq!(node.is_selected(), None);

            let check = checkbox("check", "Keep enabled", false, true, cx, |_, _, _| {});
            let mut node = gpui::accesskit::Node::new(check.a11y_role().unwrap());
            check.write_a11y_info(&mut node);
            assert_eq!(node.role(), Role::CheckBox);
            assert_eq!(node.label(), Some("Keep enabled"));
            assert_eq!(node.toggled(), Some(Toggled::False));
            assert!(node.supports_action(gpui::AccessibleAction::Click));

            for (indicator, role) in [
                (crate::surface::MenuIndicator::Check, Role::MenuItemCheckBox),
                (crate::surface::MenuIndicator::Radio, Role::MenuItemRadio),
            ] {
                let item = crate::surface::checked_menu_item(
                    "menu-choice",
                    "Choose item",
                    true,
                    indicator,
                    cx,
                    |_, _, _| {},
                );
                let mut node = gpui::accesskit::Node::new(item.a11y_role().unwrap());
                item.write_a11y_info(&mut node);
                assert_eq!(node.role(), role);
                assert_eq!(node.label(), Some("Choose item"));
                assert_eq!(node.toggled(), Some(Toggled::True));
                assert!(node.supports_action(gpui::AccessibleAction::Click));
            }
        });
    }
}
