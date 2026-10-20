use gpui::{
    AccessibleAction, App, Bounds, Context, Div, ElementId, Modifiers, MouseButton, Pixels, Point,
    Role, SharedString, Stateful, Window, div, prelude::*, px, relative, rems, rgb,
};
use std::rc::Rc;

fn normalized(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0., 1.)
    } else {
        0.
    }
}

fn track(theme: &'static crate::theme::Palette, value: f32) -> Div {
    div()
        .w_full()
        .h(rems(4. / 16.))
        .flex_shrink_0()
        .rounded_full()
        .bg(rgb(theme.border))
        .child(
            div()
                .h_full()
                .w(relative(value))
                .rounded_full()
                .bg(rgb(theme.accent)),
        )
}

/// A determinate, read-only fraction in the range 0..=1.
/// Nonfinite values display as zero; finite values are clamped.
pub struct ProgressBar {
    id: ElementId,
    value: f32,
    label: SharedString,
}

impl ProgressBar {
    pub fn new(id: impl Into<ElementId>, value: f32, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            value: normalized(value),
            label: label.into(),
        }
    }

    pub fn render(self, cx: &App) -> Stateful<Div> {
        div()
            .id(self.id)
            .role(Role::ProgressIndicator)
            .aria_label(self.label)
            .aria_numeric_value(f64::from(self.value))
            .aria_min_numeric_value(0.)
            .aria_max_numeric_value(1.)
            .w_full()
            .min_w_0()
            .child(track(crate::theme::palette(cx), self.value))
    }
}

/// A direction, not a step size: the host owns units and command semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliderKey {
    Decrease,
    Increase,
    Start,
    End,
}

impl SliderKey {
    pub fn resolve(key: &str, modifiers: Modifiers) -> Option<Self> {
        if modifiers != Modifiers::default() {
            return None;
        }
        match key {
            "left" | "down" => Some(Self::Decrease),
            "right" | "up" => Some(Self::Increase),
            "home" => Some(Self::Start),
            "end" => Some(Self::End),
            _ => None,
        }
    }
}

/// Local input intents. The host owns pointer arbitration and commit/cancellation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SliderInput {
    PointerDown { position: Point<Pixels>, alt: bool },
    Key(SliderKey),
}

/// Controlled horizontal slider with no global pointer capture or business state.
///
/// The owner supplies the current fraction and drag appearance, measures the returned
/// surface, and routes pointer movement/release/cancellation through its window owner.
/// Cancel that session before handling a key intent, including accessible increments,
/// so a later pointer release cannot commit a stale preview. Disabling or removing an
/// active slider must also cancel its host session.
pub struct Slider {
    id: ElementId,
    value: f32,
    label: SharedString,
    disabled: bool,
    dragging: bool,
}

impl Slider {
    pub fn new(id: impl Into<ElementId>, value: f32, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            value: normalized(value),
            label: label.into(),
            disabled: false,
            dragging: false,
        }
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn dragging(mut self, dragging: bool) -> Self {
        self.dragging = dragging;
        self
    }

    /// Maps a pointer to a clamped fraction. Invalid or empty geometry is inert.
    pub fn value_at(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<f32> {
        let origin = f32::from(bounds.origin.x);
        let width = f32::from(bounds.size.width);
        let pointer = f32::from(position.x);
        if !origin.is_finite() || !width.is_finite() || width <= 0. || !pointer.is_finite() {
            return None;
        }
        // f64 subtraction avoids overflowing for finite f32 coordinates.
        Some(((f64::from(pointer) - f64::from(origin)) / f64::from(width)).clamp(0., 1.) as f32)
    }

    pub fn render<T: 'static>(
        self,
        cx: &mut Context<T>,
        action: impl Fn(&mut T, SliderInput, &mut Window, &mut Context<T>) + 'static,
    ) -> Stateful<Div> {
        let theme = crate::theme::palette(cx);
        let disabled = self.disabled;
        let value = self.value;
        let control = div()
            .id(self.id)
            .tab_index(0)
            .tab_stop(!disabled)
            .role(Role::Slider)
            .aria_label(self.label)
            .aria_numeric_value(f64::from(value))
            .aria_min_numeric_value(0.)
            .aria_max_numeric_value(1.)
            .aria_orientation(gpui::accesskit::Orientation::Horizontal)
            .rounded_sm()
            .focus_visible(|style| {
                style.shadow(vec![
                    gpui::BoxShadow::new(px(0.), px(0.), rgb(theme.accent).into())
                        .spread_radius(px(1.))
                        .inset(),
                ])
            })
            .group("rivu-slider")
            .relative()
            .h(rems(20. / 16.))
            .w_full()
            .min_w(rems(60. / 16.))
            .flex()
            .items_center()
            .child(track(theme, value))
            .when(!disabled, |view| {
                view.cursor_pointer().child(
                    div()
                        .absolute()
                        .inset_0()
                        .mx(rems(5. / 16.))
                        .flex()
                        .items_center()
                        .opacity(0.)
                        .group_hover("rivu-slider", |style| style.opacity(1.))
                        .when(self.dragging, |view| view.opacity(1.))
                        .child(
                            div()
                                .relative()
                                .left(relative(value))
                                .ml(rems(-5. / 16.))
                                .size(rems(10. / 16.))
                                .flex_shrink_0()
                                .rounded_full()
                                .bg(rgb(theme.text)),
                        ),
                )
            });
        if disabled {
            // Block native pointer focus, but let releases reach the host so an
            // existing drag can finish even when released over a disabled slider.
            return control
                .cursor_default()
                .opacity(0.5)
                .a11y_synthetic_children(|builder| {
                    let node = builder.parent_node();
                    node.set_disabled();
                    node.remove_action(AccessibleAction::Focus);
                })
                .capture_any_mouse_down(|_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                });
        }
        let action = Rc::new(action);
        let control = control
            .on_key_down(cx.listener({
                let action = Rc::clone(&action);
                move |this, event: &gpui::KeyDownEvent, window, cx| {
                    if let Some(key) =
                        SliderKey::resolve(event.keystroke.key.as_str(), event.keystroke.modifiers)
                    {
                        action(this, SliderInput::Key(key), window, cx);
                        window.prevent_default();
                        cx.stop_propagation();
                    }
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener({
                    let action = Rc::clone(&action);
                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                        action(
                            this,
                            SliderInput::PointerDown {
                                position: event.position,
                                alt: event.modifiers.alt,
                            },
                            window,
                            cx,
                        );
                    }
                }),
            );
        let mut control = control;
        for (accessible, key) in [
            (AccessibleAction::Decrement, SliderKey::Decrease),
            (AccessibleAction::Increment, SliderKey::Increase),
        ] {
            let owner = cx.entity().downgrade();
            let action = Rc::clone(&action);
            control = control.on_a11y_action(accessible, move |_, window, cx| {
                let _ = owner.update(cx, |this, cx| {
                    action(this, SliderInput::Key(key), window, cx);
                });
            });
        }
        control
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Element, FocusHandle, InputEvent, Render, TestAppContext};

    struct SliderHost {
        focus: FocusHandle,
        other_focus: FocusHandle,
        disabled: bool,
        requests: usize,
        releases: usize,
    }

    impl Render for SliderHost {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| this.releases += 1),
                )
                .child(
                    Slider::new("slider", 0.5, "Value")
                        .disabled(self.disabled)
                        .render(cx, |this, _, _, _| this.requests += 1)
                        .track_focus(&self.focus),
                )
                .child(
                    div()
                        .id("other")
                        .tab_index(0)
                        .track_focus(&self.other_focus)
                        .h_8(),
                )
        }
    }

    #[gpui::test]
    fn disabling_slider_blocks_pointer_and_focused_keyboard_intents(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        let focus = cx.update(|cx| cx.focus_handle());
        let window = cx.add_window(|_, cx| SliderHost {
            focus: focus.clone(),
            other_focus: cx.focus_handle(),
            disabled: true,
            requests: 0,
            releases: 0,
        });
        let host = window.root(cx).unwrap();
        let handle = window.into();
        for (disabled, expected, releases) in [(true, 0, 1), (false, 2, 2), (true, 2, 3)] {
            host.update(cx, |this, cx| {
                this.disabled = disabled;
                cx.notify();
            });
            cx.update_window(handle, |_, window, cx| window.focus(&focus, cx))
                .unwrap();
            cx.run_until_parked();
            cx.update_window(handle, |_, window, cx| {
                window.draw(cx).clear(cx);
                window.dispatch_event(
                    gpui::MouseDownEvent {
                        button: MouseButton::Left,
                        position: gpui::point(px(12.), px(10.)),
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
                window.dispatch_event(
                    gpui::MouseUpEvent {
                        button: MouseButton::Left,
                        position: gpui::point(px(12.), px(10.)),
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
                let keystroke = gpui::Keystroke::parse("right").unwrap();
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
            assert_eq!(host.read_with(cx, |this, _| this.requests), expected);
            assert_eq!(host.read_with(cx, |this, _| this.releases), releases);
        }
    }

    #[gpui::test]
    fn disabled_slider_preserves_other_control_focus_on_pointer_input(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        let focus = cx.update(|cx| cx.focus_handle());
        let other_focus = cx.update(|cx| cx.focus_handle());
        let window = cx.add_window(|_, _| SliderHost {
            focus: focus.clone(),
            other_focus: other_focus.clone(),
            disabled: true,
            requests: 0,
            releases: 0,
        });
        let host = window.root(cx).unwrap();
        let handle = window.into();
        for disabled in [true, false, true] {
            host.update(cx, |this, cx| {
                this.disabled = disabled;
                cx.notify();
            });
            cx.run_until_parked();
            for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
                cx.update_window(handle, |_, window, cx| {
                    window.focus(&other_focus, cx);
                    window.draw(cx).clear(cx);
                    window.dispatch_event(
                        gpui::MouseDownEvent {
                            button,
                            position: gpui::point(px(12.), px(10.)),
                            ..Default::default()
                        }
                        .to_platform_input(),
                        cx,
                    );
                    window.dispatch_event(
                        gpui::MouseUpEvent {
                            button,
                            position: gpui::point(px(12.), px(10.)),
                            ..Default::default()
                        }
                        .to_platform_input(),
                        cx,
                    );
                    assert_eq!(other_focus.is_focused(window), disabled);
                    assert_eq!(focus.is_focused(window), !disabled);
                })
                .unwrap();
            }
        }
    }

    #[gpui::test]
    fn fraction_components_expose_finite_clamped_accessible_values(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        let entity = cx.new(|_| ());
        entity.update(cx, |_, cx| {
            for (value, expected) in [
                (f32::NAN, 0.),
                (f32::INFINITY, 0.),
                (f32::NEG_INFINITY, 0.),
                (-1., 0.),
                (2., 1.),
                (0.5, 0.5),
            ] {
                let progress = ProgressBar::new("progress", value, "Progress").render(cx);
                let slider = Slider::new("slider", value, "Value").render(cx, |_, _, _, _| {});
                for (control, role, label) in [
                    (progress, Role::ProgressIndicator, "Progress"),
                    (slider, Role::Slider, "Value"),
                ] {
                    let mut node = gpui::accesskit::Node::new(control.a11y_role().unwrap());
                    control.write_a11y_info(&mut node);
                    assert_eq!(node.role(), role);
                    assert_eq!(node.label(), Some(label));
                    assert_eq!(node.numeric_value(), Some(expected));
                    assert_eq!(node.min_numeric_value(), Some(0.));
                    assert_eq!(node.max_numeric_value(), Some(1.));
                }
            }
        });
    }

    #[test]
    fn pointer_fraction_rejects_invalid_geometry_and_clamps_outside() {
        let bounds = Bounds::new(Point::new(px(10.), px(0.)), gpui::size(px(20.), px(4.)));
        assert_eq!(
            Slider::value_at(bounds, Point::new(px(20.), px(0.))),
            Some(0.5)
        );
        assert_eq!(
            Slider::value_at(bounds, Point::new(px(-10.), px(0.))),
            Some(0.)
        );
        assert_eq!(
            Slider::value_at(bounds, Point::new(px(80.), px(0.))),
            Some(1.)
        );
        for width in [0., -1., f32::NAN, f32::INFINITY] {
            let invalid = Bounds::new(bounds.origin, gpui::size(px(width), px(4.)));
            assert_eq!(Slider::value_at(invalid, bounds.origin), None);
        }
        assert_eq!(
            Slider::value_at(bounds, Point::new(px(f32::NAN), px(0.))),
            None
        );
        let extreme = Bounds::new(
            Point::new(px(-f32::MAX), px(0.)),
            gpui::size(px(f32::MAX), px(4.)),
        );
        assert_eq!(
            Slider::value_at(extreme, Point::new(px(f32::MAX), px(0.))),
            Some(1.)
        );
    }

    #[test]
    fn slider_keys_are_local_and_unmodified() {
        for (key, expected) in [
            ("left", SliderKey::Decrease),
            ("down", SliderKey::Decrease),
            ("right", SliderKey::Increase),
            ("up", SliderKey::Increase),
            ("home", SliderKey::Start),
            ("end", SliderKey::End),
        ] {
            assert_eq!(
                SliderKey::resolve(key, Modifiers::default()),
                Some(expected)
            );
            for modifiers in [
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                Modifiers {
                    platform: true,
                    ..Modifiers::default()
                },
                Modifiers {
                    function: true,
                    ..Modifiers::default()
                },
            ] {
                assert_eq!(SliderKey::resolve(key, modifiers), None);
            }
        }
        for key in ["space", "enter", "pageup", "pagedown", "a"] {
            assert_eq!(SliderKey::resolve(key, Modifiers::default()), None);
        }
    }
}
