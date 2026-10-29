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
        .focus_visible(|style| style.border_color(rgb(theme.accent)))
        .flex()
        .items_center()
        .min_w_0()
        .px_2()
        .py_1()
        .rounded_sm()
        .min_h_8()
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
        .max_w_full()
        .tooltip(text_tooltip(label.clone()))
        .child(div().min_w_0().truncate().child(label))
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

// Uniform tree rows must reserve the default button's natural line, padding and border.
pub(crate) fn icon_button_height(window: &Window, cx: &gpui::App) -> gpui::Pixels {
    let rem = window.rem_size();
    (rem * 2.).max((rem * crate::theme::font_size_scale(cx)).round() + rem + gpui::px(2.))
}

fn icon_button_surface(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    icon: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    cx: &gpui::App,
) -> Stateful<Div> {
    let hint = hint.into();
    let font_scale = crate::theme::font_size_scale(cx);
    let glyph = self::icon(icon, cx)
        .font_size_scale(font_scale)
        .size(gpui::rems(font_scale))
        .flex()
        .items_center()
        .justify_center();
    control_surface(theme, id)
        .role(Role::Button)
        .aria_label(hint.clone())
        .child(glyph)
        .w_auto()
        .h_auto()
        .min_w_8()
        .min_h_8()
        .text_size(gpui::rems(1.))
        .line_height(gpui::relative(1.))
        .p_2()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .tooltip(text_tooltip(hint))
}

/// Material shared by every icon button in a group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IconButtonStyle {
    Surface,
    Quiet,
    Bare,
}

/// Natural icon-button padding and minimum hit area, independent of glyph size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IconButtonDensity {
    Regular,
    Compact,
}

#[derive(Clone, Copy)]
enum IconButtonSemantics {
    Action,
    Toggled(bool),
    Switch(bool),
}

/// A controlled icon action. Its owner supplies all business state and callbacks.
pub struct IconButton {
    id: ElementId,
    icon: SharedString,
    hint: SharedString,
    enabled: bool,
    selected: bool,
    selected_fill: bool,
    semantics: IconButtonSemantics,
}

impl IconButton {
    pub fn new(
        id: impl Into<ElementId>,
        icon: impl Into<SharedString>,
        hint: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            icon: icon.into(),
            hint: hint.into(),
            enabled: true,
            selected: false,
            selected_fill: true,
            semantics: IconButtonSemantics::Action,
        }
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Appearance only; does not imply selected or toggled accessibility semantics.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Keep selected accent glyphs without selected background material when false.
    pub fn selected_fill(mut self, selected_fill: bool) -> Self {
        self.selected_fill = selected_fill;
        self
    }

    /// A toggle button, such as Favorite; remains a Button, not a Switch.
    pub fn toggled(mut self, toggled: bool) -> Self {
        self.semantics = IconButtonSemantics::Toggled(toggled);
        self.selected = toggled;
        self
    }

    /// A boolean setting with Switch semantics and matching selected appearance.
    pub fn switch(mut self, on: bool) -> Self {
        self.semantics = IconButtonSemantics::Switch(on);
        self.selected = on;
        self
    }

    /// Build a standalone item with the same styling and behavior as a group item.
    pub fn build<T: 'static>(
        self,
        style: IconButtonStyle,
        density: IconButtonDensity,
        cx: &mut Context<T>,
        action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
    ) -> Stateful<Div> {
        self.build_with(style, density, cx, action, |control| control)
    }

    /// Customize geometry, focus tracking or emphasis before disabled barriers apply.
    pub fn build_with<T: 'static>(
        self,
        style: IconButtonStyle,
        density: IconButtonDensity,
        cx: &mut Context<T>,
        action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
        customize: impl FnOnce(Stateful<Div>) -> Stateful<Div>,
    ) -> Stateful<Div> {
        let theme = crate::theme::palette(cx);
        let mut control = icon_button_surface(theme, self.id, self.icon, self.hint, cx);
        if density == IconButtonDensity::Compact {
            control = control.px_1().py_2().min_w_6().min_h_8();
        }
        if style != IconButtonStyle::Surface {
            control = control
                .border_color(gpui::transparent_black())
                .bg(rgb(theme.panel).alpha(0.0))
                .text_color(rgb(theme.secondary));
        }
        if let IconButtonSemantics::Toggled(on) | IconButtonSemantics::Switch(on) = self.semantics {
            control = control.aria_toggled(if on { Toggled::True } else { Toggled::False });
            if matches!(self.semantics, IconButtonSemantics::Switch(_)) {
                control = control.role(Role::Switch);
            }
        }
        control = active_control(theme, control, self.selected && self.selected_fill);
        if self.selected && !self.selected_fill {
            control = control.text_color(rgb(theme.accent));
        }
        if self.enabled {
            if style == IconButtonStyle::Bare && !(self.selected && self.selected_fill) {
                control = control.group_hover("control", |style| {
                    style
                        .bg(gpui::transparent_black())
                        .text_color(rgb(theme.accent))
                });
            }
            control = activate_control(control, cx, action);
        }
        let control = customize(control);
        if self.enabled {
            control
        } else {
            let background = if self.selected && self.selected_fill {
                rgb(theme.selected).into()
            } else if style == IconButtonStyle::Surface {
                rgb(theme.control).into()
            } else {
                gpui::transparent_black()
            };
            let border = if style == IconButtonStyle::Surface {
                rgb(theme.structure_border).into()
            } else {
                gpui::transparent_black()
            };
            disable_control_interaction(
                control
                    .text_color(rgb(theme.disabled))
                    .border_color(border)
                    .focus_visible(move |style| style.border_color(border))
                    .bg(background)
                    .group_hover("control", move |style| {
                        style.bg(background).text_color(rgb(theme.disabled))
                    }),
            )
        }
    }
}

/// A layout of centrally constructed icon controls, without business state or
/// positioning/visibility policy. Convert to a Div before applying a hover shell.
pub struct IconButtonGroup {
    surface: Div,
    style: IconButtonStyle,
    density: IconButtonDensity,
}

impl IconButtonGroup {
    pub fn new(style: IconButtonStyle, density: IconButtonDensity) -> Self {
        Self {
            surface: div()
                .flex()
                .items_center()
                .min_w_0()
                .gap_0()
                .flex_shrink_0()
                .when(density == IconButtonDensity::Regular, |group| group.gap_1()),
            style,
            density,
        }
    }

    /// Wrap within the parent's width without shrinking individual hit areas.
    pub fn wrap(mut self) -> Self {
        self.surface = self.surface.flex_wrap().max_w_full().flex_shrink(1.);
        self
    }

    pub fn button<T: 'static>(
        self,
        item: IconButton,
        cx: &mut Context<T>,
        action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
    ) -> Self {
        self.button_with(item, cx, action, |control| control)
    }

    /// Preserve item-specific event handlers or primary/secondary emphasis while
    /// sharing construction. Disabled interaction barriers are applied afterward.
    pub fn button_with<T: 'static>(
        mut self,
        item: IconButton,
        cx: &mut Context<T>,
        action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
        customize: impl FnOnce(Stateful<Div>) -> Stateful<Div>,
    ) -> Self {
        self.surface =
            self.surface
                .child(item.build_with(self.style, self.density, cx, action, customize));
        self
    }

    pub fn into_div(self) -> Div {
        self.surface
    }
}

/// Unavailable actions retain owner guards and suppress all native activation paths.
pub fn disabled_control(
    theme: &'static crate::theme::Palette,
    control: Stateful<Div>,
    disabled: bool,
) -> Stateful<Div> {
    control.when(disabled, |view| {
        disable_control_interaction(
            view.text_color(rgb(theme.disabled))
                .border_color(rgb(theme.structure_border))
                .group_hover("control", |style| style),
        )
    })
}

// GPUI dispatches native mouse clicks in Bubble without checking prevent_default.
// Keep the capture barrier even after a press began while enabled. Window-owned
// gestures must observe release in ancestor Capture and finalize after child handlers.
fn disable_control_interaction(control: Stateful<Div>) -> Stateful<Div> {
    control
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

/// Independent square switch; Regular padding and minimum size follow UI scale.
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
        activate_control(
            icon_button_surface(theme, id, icon, hint, cx),
            cx,
            move |this, _, cx| {
                action(this, cx);
                cx.notify();
            },
        )
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
        grouped: bool,
        clicks: usize,
        releases: Vec<usize>,
    }

    impl Render for Controls {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = crate::theme::palette(cx);
            let control = if self.grouped {
                IconButtonGroup::new(IconButtonStyle::Quiet, IconButtonDensity::Compact)
                    .button_with(
                        IconButton::new("action", "♥", "Favorite")
                            .toggled(true)
                            .enabled(!self.disabled),
                        cx,
                        |this, _, cx| {
                            this.clicks += 1;
                            cx.notify();
                        },
                        |control| control.track_focus(&self.focus),
                    )
                    .into_div()
                    .into_any_element()
            } else {
                disabled_control(
                    theme,
                    button("action", "Apply", cx, |this, _, cx| {
                        this.clicks += 1;
                        cx.notify();
                    })
                    .track_focus(&self.focus)
                    .w(gpui::px(160.)),
                    self.disabled,
                )
                .into_any_element()
            };
            div()
                .id("window-owner")
                .size_full()
                .capture_any_mouse_up(cx.listener(|_, event: &gpui::MouseUpEvent, window, cx| {
                    if event.button == gpui::MouseButton::Left {
                        let entity = cx.entity().downgrade();
                        window.defer(cx, move |_, cx| {
                            let _ = entity.update(cx, |this, _| this.releases.push(this.clicks));
                        });
                    }
                }))
                .child(control)
        }
    }

    #[gpui::test]
    fn disabled_controls_block_pointer_and_native_keyboard_clicks(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        assert_disabled_controls(cx, false);
        assert_disabled_controls(cx, true);
    }

    fn assert_disabled_controls(cx: &mut TestAppContext, grouped: bool) {
        let focus = cx.update(|cx| cx.focus_handle());
        let window = cx.add_window(|_, _| Controls {
            focus: focus.clone(),
            disabled: true,
            grouped,
            clicks: 0,
            releases: Vec::new(),
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

    fn activation_edge(input: &str, pressed: bool) -> gpui::PlatformInput {
        if input == "pointer" {
            let position = gpui::point(gpui::px(12.), gpui::px(12.));
            if pressed {
                gpui::MouseDownEvent {
                    button: gpui::MouseButton::Left,
                    position,
                    click_count: 1,
                    ..Default::default()
                }
                .to_platform_input()
            } else {
                gpui::MouseUpEvent {
                    button: gpui::MouseButton::Left,
                    position,
                    click_count: 1,
                    ..Default::default()
                }
                .to_platform_input()
            }
        } else {
            let keystroke = gpui::Keystroke::parse(input).unwrap();
            if pressed {
                gpui::KeyDownEvent {
                    keystroke,
                    is_held: false,
                    prefer_character_input: false,
                }
                .to_platform_input()
            } else {
                gpui::KeyUpEvent { keystroke }.to_platform_input()
            }
        }
    }

    #[gpui::test]
    fn disabling_pressed_controls_cancels_pointer_and_keyboard_activation(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        for grouped in [false, true] {
            for input in ["pointer", "enter", "space"] {
                let focus = cx.update(|cx| cx.focus_handle());
                let window = cx.add_window(|_, _| Controls {
                    focus: focus.clone(),
                    disabled: false,
                    grouped,
                    clicks: 0,
                    releases: Vec::new(),
                });
                let controls = window.root(cx).unwrap();
                cx.update_window(window.into(), |_, window, cx| {
                    window.focus(&focus, cx);
                    window.draw(cx).clear(cx);
                    window.dispatch_event(activation_edge(input, true), cx);
                })
                .unwrap();
                controls.update(cx, |this, cx| {
                    this.disabled = true;
                    cx.notify();
                });
                cx.run_until_parked();
                cx.update_window(window.into(), |_, window, cx| {
                    window.draw(cx).clear(cx);
                    window.dispatch_event(activation_edge(input, false), cx);
                })
                .unwrap();
                cx.run_until_parked();
                controls.read_with(cx, |this, _| {
                    assert_eq!(this.clicks, 0, "grouped={grouped}, input={input}");
                    if input == "pointer" {
                        assert_eq!(
                            this.releases,
                            [0],
                            "window release survives disabled capture"
                        );
                    }
                });
            }
        }
    }

    #[gpui::test]
    fn disabled_controls_preserve_deferred_window_release(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        for grouped in [false, true] {
            let window = cx.add_window(|_, cx| Controls {
                focus: cx.focus_handle(),
                disabled: true,
                grouped,
                clicks: 0,
                releases: Vec::new(),
            });
            let controls = window.root(cx).unwrap();
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                // An existing window gesture began outside the unavailable button.
                window.dispatch_event(
                    gpui::MouseDownEvent {
                        button: gpui::MouseButton::Left,
                        position: gpui::point(gpui::px(200.), gpui::px(12.)),
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
                window.dispatch_event(activation_edge("pointer", false), cx);
                // The accessibility Click fallback synthesizes the same down/up pair.
                window.dispatch_event(activation_edge("pointer", true), cx);
                window.dispatch_event(activation_edge("pointer", false), cx);
            })
            .unwrap();
            cx.run_until_parked();
            controls.read_with(cx, |this, _| {
                assert_eq!(this.clicks, 0);
                assert_eq!(this.releases, [0, 0]);
            });
            controls.update(cx, |this, cx| {
                this.disabled = false;
                cx.notify();
            });
            cx.run_until_parked();
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                // Enabled child activation still precedes deferred owner finalization.
                window.dispatch_event(activation_edge("pointer", true), cx);
                window.dispatch_event(activation_edge("pointer", false), cx);
            })
            .unwrap();
            cx.run_until_parked();
            controls.read_with(cx, |this, _| {
                assert_eq!(this.clicks, 1);
                assert_eq!(
                    this.releases,
                    [0, 0, 1],
                    "finalization runs once, after the child action"
                );
            });
        }
    }

    #[gpui::test]
    fn grouped_icons_preserve_boolean_semantics_when_disabled(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        let entity = cx.new(|cx| Controls {
            focus: cx.focus_handle(),
            disabled: false,
            grouped: true,
            clicks: 0,
            releases: Vec::new(),
        });
        entity.update(cx, |_, cx| {
            for enabled in [true, false] {
                for on in [true, false] {
                    for switch in [true, false] {
                        let item =
                            IconButton::new("boolean", "♥", "Host-owned state").enabled(enabled);
                        let item = if switch {
                            item.switch(on)
                        } else {
                            item.toggled(on)
                        };
                        let control = item.build(
                            IconButtonStyle::Quiet,
                            IconButtonDensity::Compact,
                            cx,
                            |_, _, _| {},
                        );
                        let mut node = gpui::accesskit::Node::new(control.a11y_role().unwrap());
                        control.write_a11y_info(&mut node);
                        assert_eq!(
                            node.role(),
                            if switch { Role::Switch } else { Role::Button }
                        );
                        assert_eq!(node.label(), Some("Host-owned state"));
                        assert_eq!(
                            node.toggled(),
                            Some(if on { Toggled::True } else { Toggled::False })
                        );
                        assert_eq!(node.is_selected(), None);
                        assert_eq!(node.supports_action(gpui::AccessibleAction::Click), enabled);
                    }
                }
            }
            let control = IconButton::new("selected-action", "+", "Selected appearance")
                .selected(true)
                .build(
                    IconButtonStyle::Bare,
                    IconButtonDensity::Regular,
                    cx,
                    |_, _, _| {},
                );
            let mut node = gpui::accesskit::Node::new(control.a11y_role().unwrap());
            control.write_a11y_info(&mut node);
            assert_eq!(node.role(), Role::Button);
            assert_eq!(node.toggled(), None);
            assert_eq!(node.is_selected(), None);
        });
    }

    struct DisabledSelected {
        observed: std::rc::Rc<std::cell::RefCell<Option<gpui::Style>>>,
        clicks: usize,
    }

    impl Render for DisabledSelected {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = crate::theme::palette(cx);
            let decorate =
                |control| disabled_control(theme, active_control(theme, control, true), true);
            let mut probe = decorate(button("selected-probe", "Selected", cx, |_, _, _| {}));
            let observed = self.observed.clone();
            let control = decorate(button("selected", "Selected", cx, |this, _, _| {
                this.clicks += 1;
            }))
            .child(gpui::canvas(
                |_, _, _| {},
                move |_, _, window, cx| {
                    *observed.borrow_mut() =
                        Some(probe.interactivity().compute_style(None, None, window, cx));
                },
            ));
            div().flex().items_start().size_full().child(control)
        }
    }

    #[gpui::test]
    fn disabled_selected_buttons_preserve_fill_across_hover(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        let observed = std::rc::Rc::new(std::cell::RefCell::new(None));
        let window = cx.add_window({
            let observed = observed.clone();
            move |_, _| DisabledSelected {
                observed,
                clicks: 0,
            }
        });
        let root = window.root(cx).unwrap();
        for hovered in [false, true, false] {
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.dispatch_event(
                    gpui::MouseMoveEvent {
                        position: gpui::point(
                            gpui::px(if hovered { 12. } else { 300. }),
                            gpui::px(12.),
                        ),
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
                window.draw(cx).clear(cx);
                let theme = crate::theme::palette(cx);
                let observed = observed.borrow();
                let actual = observed.as_ref().unwrap();
                assert_eq!(actual.background, Some(rgb(theme.selected).into()));
                assert_eq!(actual.text.color, Some(rgb(theme.disabled).into()));
                assert_eq!(
                    actual.border_color,
                    Some(rgb(theme.structure_border).into())
                );
                window.dispatch_event(activation_edge("pointer", true), cx);
                window.dispatch_event(activation_edge("pointer", false), cx);
            })
            .unwrap();
            cx.run_until_parked();
            assert_eq!(root.read_with(cx, |this, _| this.clicks), 0);
        }
    }

    struct GlyphSelection {
        style: IconButtonStyle,
        enabled: bool,
        focus: FocusHandle,
        clicks: usize,
        observed: std::rc::Rc<std::cell::RefCell<Option<gpui::Style>>>,
    }

    impl Render for GlyphSelection {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let item = || {
                IconButton::new("glyph-selection", "♥", "Favorite")
                    .toggled(true)
                    .selected_fill(false)
                    .enabled(self.enabled)
            };
            let mut probe = item().build(self.style, IconButtonDensity::Compact, cx, |_, _, _| {});
            let mut node = gpui::accesskit::Node::new(probe.a11y_role().unwrap());
            probe.write_a11y_info(&mut node);
            assert_eq!(node.role(), Role::Button);
            assert_eq!(node.toggled(), Some(Toggled::True));
            assert_eq!(
                node.supports_action(gpui::AccessibleAction::Click),
                self.enabled
            );
            let observed = self.observed.clone();
            let control = item()
                .build(self.style, IconButtonDensity::Compact, cx, |this, _, _| {
                    this.clicks += 1;
                })
                .track_focus(&self.focus)
                .child(gpui::canvas(
                    |_, _, _| {},
                    move |_, _, window, cx| {
                        *observed.borrow_mut() =
                            Some(probe.interactivity().compute_style(None, None, window, cx));
                    },
                ));
            div().flex().items_start().size_full().child(control)
        }
    }

    #[gpui::test]
    fn glyph_only_selection_preserves_hover_semantics_and_disabled_barriers(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::theme::init_test);
        for style in [
            IconButtonStyle::Surface,
            IconButtonStyle::Quiet,
            IconButtonStyle::Bare,
        ] {
            let observed = std::rc::Rc::new(std::cell::RefCell::new(None));
            let window = cx.add_window({
                let observed = observed.clone();
                move |_, cx| GlyphSelection {
                    style,
                    enabled: true,
                    focus: cx.focus_handle(),
                    clicks: 0,
                    observed,
                }
            });
            let root = window.root(cx).unwrap();
            for enabled in [true, false] {
                root.update(cx, |this, cx| {
                    this.enabled = enabled;
                    cx.notify();
                });
                cx.run_until_parked();
                for hovered in [false, true, false] {
                    cx.update_window(window.into(), |_, window, cx| {
                        window.draw(cx).clear(cx);
                        window.dispatch_event(
                            gpui::MouseMoveEvent {
                                position: gpui::point(
                                    gpui::px(if hovered { 12. } else { 300. }),
                                    gpui::px(12.),
                                ),
                                ..Default::default()
                            }
                            .to_platform_input(),
                            cx,
                        );
                        window.draw(cx).clear(cx);
                        let theme = crate::theme::palette(cx);
                        let observed = observed.borrow();
                        let actual = observed.as_ref().unwrap();
                        let background: gpui::Fill =
                            if enabled && hovered && style != IconButtonStyle::Bare {
                                rgb(theme.highlight).into()
                            } else if style == IconButtonStyle::Surface {
                                rgb(theme.control).into()
                            } else if enabled && !(hovered && style == IconButtonStyle::Bare) {
                                rgb(theme.panel).alpha(0.0).into()
                            } else {
                                gpui::transparent_black().into()
                            };
                        assert_eq!(actual.background, Some(background));
                        assert_eq!(
                            actual.text.color,
                            Some(
                                rgb(if enabled {
                                    theme.accent
                                } else {
                                    theme.disabled
                                })
                                .into()
                            )
                        );
                    })
                    .unwrap();
                }
                for input in ["pointer", "enter", "space"] {
                    cx.update_window(window.into(), |_, window, cx| {
                        let focus = root.read(cx).focus.clone();
                        window.focus(&focus, cx);
                        window.draw(cx).clear(cx);
                        window.dispatch_event(activation_edge(input, true), cx);
                        window.dispatch_event(activation_edge(input, false), cx);
                    })
                    .unwrap();
                    cx.run_until_parked();
                }
                assert_eq!(root.read_with(cx, |this, _| this.clicks), 3);
            }
        }
    }

    struct GroupFocus {
        before: FocusHandle,
        external: Option<FocusHandle>,
        enabled: bool,
        activated: usize,
    }

    impl Render for GroupFocus {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .child(button("before", "Before", cx, |_, _, _| {}).track_focus(&self.before))
                .child(
                    IconButtonGroup::new(IconButtonStyle::Bare, IconButtonDensity::Compact)
                        .button_with(
                            IconButton::new("favorite", "♥", "Favorite")
                                .toggled(true)
                                .enabled(self.enabled),
                            cx,
                            |this, _, _| this.activated = 1,
                            |control| {
                                if let Some(focus) = &self.external {
                                    control.track_focus(focus)
                                } else {
                                    control
                                }
                            },
                        )
                        .into_div(),
                )
                .child(button("after", "After", cx, |this, _, _| {
                    this.activated = 2
                }))
        }
    }

    #[gpui::test]
    fn disabled_group_items_are_skipped_by_tab_navigation(cx: &mut TestAppContext) {
        assert_group_tab_navigation(cx, None);
    }

    #[gpui::test]
    fn disabled_group_external_focus_restores_original_tab_policy(cx: &mut TestAppContext) {
        for tab_stop in [true, false] {
            assert_group_tab_navigation(cx, Some(tab_stop));
        }
    }

    fn assert_group_tab_navigation(cx: &mut TestAppContext, external_tab_stop: Option<bool>) {
        cx.update(crate::theme::init_test);
        let window = cx.add_window(|_, cx| GroupFocus {
            before: cx.focus_handle().tab_stop(true),
            external: external_tab_stop.map(|tab_stop| cx.focus_handle().tab_stop(tab_stop)),
            enabled: false,
            activated: 0,
        });
        let root = window.root(cx).unwrap();
        let before = root.read_with(cx, |this, _| this.before.clone());
        let external = root.read_with(cx, |this, _| this.external.clone());
        for enabled in [false, true, false] {
            window
                .update(cx, |this, _, cx| {
                    this.enabled = enabled;
                    this.activated = 0;
                    cx.notify();
                })
                .unwrap();
            cx.run_until_parked();
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.focus(&before, cx);
                if let Some(focus) = &external {
                    assert_eq!(focus.tab_stop, external_tab_stop.unwrap());
                    assert_eq!(
                        focus.downgrade().upgrade().unwrap().tab_stop,
                        external_tab_stop.unwrap(),
                        "rendering must not change the external handle's shared policy"
                    );
                }
                window.focus_next(cx);
                let keystroke = gpui::Keystroke::parse("enter").unwrap();
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
            assert_eq!(
                root.read_with(cx, |this, _| this.activated),
                if enabled && external_tab_stop.unwrap_or(true) {
                    1
                } else {
                    2
                }
            );
        }
    }

    #[gpui::test]
    fn icon_names_and_boolean_roles_are_not_visual_glyphs(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        let entity = cx.new(|cx| Controls {
            focus: cx.focus_handle(),
            disabled: false,
            grouped: false,
            clicks: 0,
            releases: Vec::new(),
        });
        entity.update(cx, |_, cx| {
            let icon = IconButton::new("save", "󰆓", "Save changes").build(
                IconButtonStyle::Surface,
                IconButtonDensity::Regular,
                cx,
                |_, _, _| {},
            );
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

    type MeasuredBounds = std::rc::Rc<std::cell::Cell<[gpui::Bounds<gpui::Pixels>; 4]>>;

    fn measure_width(
        control: Stateful<Div>,
        index: usize,
        bounds: MeasuredBounds,
    ) -> Stateful<Div> {
        control.relative().child(
            gpui::canvas(
                move |value, _, _| {
                    let mut measured = bounds.get();
                    // Absolute children cover the padding box, excluding the 1px border.
                    measured[index] = gpui::Bounds::new(
                        value.origin - gpui::point(gpui::px(1.), gpui::px(1.)),
                        gpui::size(
                            value.size.width + gpui::px(2.),
                            value.size.height + gpui::px(2.),
                        ),
                    );
                    bounds.set(measured);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
    }

    struct IconWidths {
        bounds: MeasuredBounds,
        density: IconButtonDensity,
        style: IconButtonStyle,
        alternate: bool,
        wrap: bool,
        width: f32,
    }

    impl Render for IconWidths {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let mut group = IconButtonGroup::new(self.style, self.density);
            if self.wrap {
                group = group.wrap();
            }
            for (index, glyph) in if self.alternate {
                ["󰏤", "󰑘", "−"]
            } else {
                ["", "󰑖", "♥"]
            }
            .into_iter()
            .enumerate()
            {
                let bounds = self.bounds.clone();
                group = group.button_with(
                    IconButton::new(("width-action", index), glyph, "Action")
                        .toggled(self.alternate)
                        .enabled(index != 2 || self.alternate),
                    cx,
                    |_, _, _| {},
                    move |control| measure_width(control, index, bounds),
                );
            }
            let bounds = self.bounds.clone();
            crate::column().w(gpui::px(self.width)).child(
                group.into_div().self_start().relative().child(
                    gpui::canvas(
                        move |value, _, _| {
                            let mut measured = bounds.get();
                            measured[3] = value;
                            bounds.set(measured);
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                ),
            )
        }
    }

    #[gpui::test]
    fn icon_group_geometry_survives_glyph_and_state_changes(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        let bounds = MeasuredBounds::default();
        let window = cx.add_window({
            let bounds = bounds.clone();
            move |_, _| IconWidths {
                bounds,
                density: IconButtonDensity::Regular,
                style: IconButtonStyle::Surface,
                alternate: false,
                wrap: false,
                width: 400.,
            }
        });
        for scale in [0.75, 1.5] {
            for font_size in [8, 18] {
                cx.update(|cx| {
                    crate::theme::set_typography(
                        gpui::font("sans-serif"),
                        "sans-serif",
                        font_size,
                        cx,
                    );
                });
                for density in [IconButtonDensity::Regular, IconButtonDensity::Compact] {
                    for style in [
                        IconButtonStyle::Surface,
                        IconButtonStyle::Quiet,
                        IconButtonStyle::Bare,
                    ] {
                        let mut original = None;
                        for alternate in [false, true] {
                            window
                                .update(cx, |this, window, cx| {
                                    this.density = density;
                                    this.style = style;
                                    this.alternate = alternate;
                                    window.set_rem_size(gpui::px(16. * scale));
                                    cx.notify();
                                })
                                .unwrap();
                            cx.update_window(window.into(), |_, window, cx| {
                                window.draw(cx).clear(cx);
                            })
                            .unwrap();
                            let measured = bounds.get();
                            for item in &measured[..3] {
                                assert_eq!(item.size, measured[0].size);
                            }
                            assert!(measured[0].size.width >= gpui::px(24. * scale));
                            assert!(measured[0].size.height >= gpui::px(32. * scale));
                            assert_eq!(measured[3].right(), measured[2].right());
                            if let Some(original) = original {
                                assert_eq!(measured, original);
                            }
                            original = Some(measured);
                        }
                    }
                }
            }
        }
    }

    #[gpui::test]
    fn wrapping_group_reflows_without_compressing_buttons(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        let bounds = MeasuredBounds::default();
        let window = cx.add_window({
            let bounds = bounds.clone();
            move |_, _| IconWidths {
                bounds,
                density: IconButtonDensity::Regular,
                style: IconButtonStyle::Surface,
                alternate: false,
                wrap: true,
                width: 400.,
            }
        });
        let mut original = None;
        for width in [400., 80., 400.] {
            window
                .update(cx, |this, _, cx| {
                    this.width = width;
                    cx.notify();
                })
                .unwrap();
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
            })
            .unwrap();
            let measured = bounds.get();
            assert!(measured[3].size.width <= gpui::px(width));
            assert_eq!(measured[0].origin.y == measured[2].origin.y, width == 400.);
            if let Some(original) = original {
                for item in &measured[..3] {
                    assert_eq!(item.size, original);
                }
            }
            original = Some(measured[0].size);
        }
    }

    #[gpui::test]
    fn text_buttons_keep_content_width_and_bound_long_labels(cx: &mut TestAppContext) {
        struct TextWidths(MeasuredBounds);
        impl Render for TextWidths {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                crate::column().w(gpui::px(120.)).children(
                    [
                        "Go",
                        "保存更改 / Save changes with a deliberately long action label",
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(index, label)| {
                        measure_width(
                            button(("text-width", index), label, cx, |_, _, _| {}).self_start(),
                            index,
                            self.0.clone(),
                        )
                    }),
                )
            }
        }
        cx.update(crate::theme::init_test);
        let bounds = MeasuredBounds::default();
        let window = cx.add_window({
            let bounds = bounds.clone();
            move |_, _| TextWidths(bounds)
        });
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let [short, long, ..] = bounds.get();
        assert!(short.size.width < long.size.width);
        assert_eq!(long.size.width, gpui::px(120.));
        assert_eq!(short.size.height, long.size.height);
    }

    #[derive(Clone, Copy, Debug)]
    enum ControlFamily {
        Text,
        Icon,
        Toggle,
        Checkbox,
        Dropdown,
        Group,
    }

    struct CenteredControls {
        family: ControlFamily,
        density: IconButtonDensity,
        alternate: bool,
        focus: FocusHandle,
        bounds: MeasuredBounds,
    }

    impl Render for CenteredControls {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = crate::theme::palette(cx);
            let glyph = if self.alternate { "󰏤" } else { "" };
            let control = match self.family {
                ControlFamily::Text => disabled_control(
                    theme,
                    active_control(
                        theme,
                        button("centered-text", "Apply", cx, |_, _, _| {})
                            .w(gpui::px(112.))
                            .h(gpui::px(40.))
                            .track_focus(&self.focus),
                        self.alternate,
                    ),
                    self.alternate,
                ),
                ControlFamily::Icon => IconButton::new("centered-icon", glyph, "Play")
                    .toggled(self.alternate)
                    .selected_fill(false)
                    .enabled(!self.alternate)
                    .build_with(
                        IconButtonStyle::Quiet,
                        self.density,
                        cx,
                        |_, _, _| {},
                        |control| control.track_focus(&self.focus),
                    ),
                ControlFamily::Toggle => toggle_button(
                    "centered-toggle",
                    glyph,
                    "Playback",
                    self.alternate,
                    cx,
                    |_, _| {},
                ),
                ControlFamily::Checkbox => checkbox(
                    "centered-checkbox",
                    "Include",
                    self.alternate,
                    !self.alternate,
                    cx,
                    |_, _, _| {},
                ),
                ControlFamily::Dropdown => dropdown_trigger(
                    theme,
                    "centered-dropdown",
                    if self.alternate {
                        "A deliberately long selected option that must remain bounded"
                    } else {
                        "Short"
                    },
                    self.alternate,
                    cx,
                ),
                ControlFamily::Group => IconButtonGroup::new(IconButtonStyle::Bare, self.density)
                    .button_with(
                        IconButton::new("centered-primary", glyph, "Primary")
                            .toggled(self.alternate)
                            .enabled(!self.alternate),
                        cx,
                        |_, _, _| {},
                        |control| {
                            measure_width(
                                control
                                    .w(gpui::px(72.))
                                    .h(gpui::px(64.))
                                    .track_focus(&self.focus),
                                1,
                                self.bounds.clone(),
                            )
                        },
                    )
                    .button_with(
                        IconButton::new("centered-secondary", "♥", "Secondary")
                            .toggled(self.alternate)
                            .selected_fill(false),
                        cx,
                        |_, _, _| {},
                        |control| measure_width(control, 2, self.bounds.clone()),
                    )
                    .into_div()
                    .id("centered-group"),
            };
            let control = if matches!(self.family, ControlFamily::Toggle | ControlFamily::Dropdown)
                || (matches!(self.family, ControlFamily::Checkbox) && !self.alternate)
            {
                control.track_focus(&self.focus)
            } else {
                control
            };
            crate::row()
                .w(gpui::px(400.))
                .h(gpui::px(128.))
                .child(measure_width(control, 0, self.bounds.clone()))
        }
    }

    #[gpui::test]
    fn controls_inherit_centered_rows_and_preserve_state_geometry(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        let bounds = MeasuredBounds::default();
        let window = cx.add_window({
            let bounds = bounds.clone();
            move |_, cx| CenteredControls {
                family: ControlFamily::Text,
                density: IconButtonDensity::Regular,
                alternate: false,
                focus: cx.focus_handle(),
                bounds,
            }
        });
        let pixel_tolerance = cx
            .update_window(window.into(), |_, window, _| 0.5 / window.scale_factor())
            .unwrap();
        let assert_centered = |actual: gpui::Pixels, expected: gpui::Pixels| {
            // Rounded positions can differ by half a device pixel.
            assert!(
                (f32::from(actual) - f32::from(expected)).abs() <= pixel_tolerance,
                "{actual:?} is not centered on {expected:?}"
            );
        };
        for family in [
            ControlFamily::Text,
            ControlFamily::Icon,
            ControlFamily::Toggle,
            ControlFamily::Checkbox,
            ControlFamily::Dropdown,
            ControlFamily::Group,
        ] {
            for density in [IconButtonDensity::Regular, IconButtonDensity::Compact] {
                let mut original = None;
                for alternate in [false, true, false] {
                    window
                        .update(cx, |this, window, cx| {
                            this.family = family;
                            this.density = density;
                            this.alternate = alternate;
                            if alternate {
                                window.focus(&this.focus, cx);
                            } else {
                                window.blur(cx);
                            }
                            cx.notify();
                        })
                        .unwrap();
                    cx.update_window(window.into(), |_, window, cx| {
                        window.draw(cx).clear(cx);
                        let position = if alternate {
                            bounds.get()[0].center()
                        } else {
                            gpui::point(gpui::px(500.), gpui::px(200.))
                        };
                        window.dispatch_event(
                            gpui::MouseMoveEvent {
                                position,
                                ..Default::default()
                            }
                            .to_platform_input(),
                            cx,
                        );
                        window.draw(cx).clear(cx);
                    })
                    .unwrap();
                    let measured = bounds.get();
                    assert_centered(measured[0].center().y, gpui::px(64.));
                    match family {
                        ControlFamily::Text => {
                            assert_eq!(measured[0].size, gpui::size(gpui::px(112.), gpui::px(40.)));
                        }
                        ControlFamily::Dropdown => {
                            assert_eq!(measured[0].size.width, gpui::px(400.));
                        }
                        ControlFamily::Group => {
                            assert_eq!(measured[1].size, gpui::size(gpui::px(72.), gpui::px(64.)));
                            assert_centered(measured[1].center().y, measured[0].center().y);
                            assert_centered(measured[2].center().y, measured[0].center().y);
                            assert!(measured[1].size.height > measured[2].size.height);
                        }
                        _ => assert!(measured[0].size.height >= gpui::px(32.)),
                    }
                    if let Some(original) = original {
                        assert_eq!(measured, original, "{family:?} / {density:?}");
                    }
                    original = Some(measured);
                }
            }
        }
    }
}
