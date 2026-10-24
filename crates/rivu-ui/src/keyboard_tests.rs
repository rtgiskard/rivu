use crate::{
    control::{button, disabled_control, toggle_button},
    focus::FocusScope,
    layout::FormField,
    overlay::{ButtonTooltip, ListDragPreview},
};
use gpui::{
    AnyWindowHandle, Context, Focusable, InputEvent, KeyDownEvent, KeyUpEvent, Keystroke, Render,
    TestAppContext, Window, div, prelude::*,
};

struct KeyboardControls {
    scope: FocusScope,
    counts: [usize; 4],
    disabled: bool,
}

impl Render for KeyboardControls {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::palette(cx);
        let first = button("first", "First", cx, |this, _, _| this.counts[0] += 1);
        let middle = disabled_control(
            theme,
            button("middle", "Unavailable", cx, |this, _, _| {
                this.counts[1] += 1
            }),
            self.disabled,
        );
        let toggle = toggle_button("toggle", "T", "Toggle: off", false, cx, |this, _| {
            this.counts[2] += 1;
        });
        let outside = button("outside", "Outside", cx, |this, _, _| this.counts[3] += 1);
        div().size_full().child(outside).child(
            self.scope
                .menu_surface(div().id("menu").flex().flex_col())
                .child(first)
                .child(FormField::new(theme, "Direction", middle).into_div())
                .child(toggle),
        )
    }
}

fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.draw(cx).clear(cx);
    })
    .unwrap();
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &str) {
    let keystroke = Keystroke::parse(key).unwrap();
    cx.update_window(window, |_, window, cx| {
        window.dispatch_event(
            KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: false,
                prefer_character_input: false,
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(KeyUpEvent { keystroke }.to_platform_input(), cx);
    })
    .unwrap();
    draw(cx, window);
}

#[test]
fn native_menu_focus_wraps_skips_disabled_and_activates_shared_controls() {
    let mut cx = TestAppContext::single();
    cx.update(crate::theme::init_test);
    let scope = cx.update(FocusScope::new);
    let window = cx.add_window({
        let scope = scope.clone();
        move |_, _| KeyboardControls {
            scope,
            counts: [0; 4],
            disabled: true,
        }
    });
    let handle = window.into();
    draw(&mut cx, handle);
    cx.update_window(handle, |_, window, cx| {
        scope.handle().focus(window, cx);
        scope.advance(false, window, cx);
    })
    .unwrap();
    draw(&mut cx, handle);
    press(&mut cx, handle, "enter");
    press(&mut cx, handle, "down");
    press(&mut cx, handle, "space");
    press(&mut cx, handle, "down");
    press(&mut cx, handle, "enter");
    press(&mut cx, handle, "up");
    press(&mut cx, handle, "space");
    // Reverse Tab wraps at the first item; forward Tab wraps at the last item.
    press(&mut cx, handle, "shift-tab");
    press(&mut cx, handle, "enter");
    press(&mut cx, handle, "shift-tab");
    press(&mut cx, handle, "space");
    press(&mut cx, handle, "tab");
    press(&mut cx, handle, "enter");
    let counts = cx
        .update_window(handle, |root, _, cx| {
            root.downcast::<KeyboardControls>().unwrap().read(cx).counts
        })
        .unwrap();
    assert_eq!(counts, [4, 0, 3, 0]);
}

#[test]
fn modified_arrows_do_not_move_menu_focus_or_turn_into_activation() {
    let mut cx = TestAppContext::single();
    cx.update(crate::theme::init_test);
    let scope = cx.update(FocusScope::new);
    let window = cx.add_window({
        let scope = scope.clone();
        move |_, _| KeyboardControls {
            scope,
            counts: [0; 4],
            disabled: true,
        }
    });
    let handle = window.into();
    draw(&mut cx, handle);
    cx.update_window(handle, |_, window, cx| {
        scope.handle().focus(window, cx);
        scope.advance(false, window, cx);
    })
    .unwrap();
    draw(&mut cx, handle);
    for key in ["shift-down", "ctrl-down", "alt-down", "shift-up"] {
        press(&mut cx, handle, key);
    }
    press(&mut cx, handle, "enter");
    press(&mut cx, handle, "tab");
    press(&mut cx, handle, "space");
    press(&mut cx, handle, "shift-tab");
    press(&mut cx, handle, "enter");
    let counts = cx
        .update_window(handle, |root, _, cx| {
            root.downcast::<KeyboardControls>().unwrap().read(cx).counts
        })
        .unwrap();
    assert_eq!(counts, [2, 0, 1, 0]);
}

#[test]
fn disabling_a_focused_control_cancels_keyboard_activation() {
    let mut cx = TestAppContext::single();
    cx.update(crate::theme::init_test);
    let scope = cx.update(FocusScope::new);
    let window = cx.add_window({
        let scope = scope.clone();
        move |_, _| KeyboardControls {
            scope,
            counts: [0; 4],
            disabled: false,
        }
    });
    let handle = window.into();
    draw(&mut cx, handle);
    cx.update_window(handle, |_, window, cx| {
        scope.handle().focus(window, cx);
        scope.advance(false, window, cx);
        scope.advance(false, window, cx);
    })
    .unwrap();
    draw(&mut cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.dispatch_event(
            KeyDownEvent {
                keystroke: Keystroke::parse("enter").unwrap(),
                is_held: false,
                prefer_character_input: false,
            }
            .to_platform_input(),
            cx,
        );
    })
    .unwrap();
    cx.update_window(handle, |root, _, cx| {
        root.downcast::<KeyboardControls>()
            .unwrap()
            .update(cx, |this, cx| {
                this.disabled = true;
                cx.notify();
            });
    })
    .unwrap();
    draw(&mut cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.dispatch_event(
            KeyUpEvent {
                keystroke: Keystroke::parse("enter").unwrap(),
            }
            .to_platform_input(),
            cx,
        );
    })
    .unwrap();
    press(&mut cx, handle, "enter");
    press(&mut cx, handle, "space");
    let counts = cx
        .update_window(handle, |root, _, cx| {
            root.downcast::<KeyboardControls>().unwrap().read(cx).counts
        })
        .unwrap();
    assert_eq!(counts, [0; 4]);
    press(&mut cx, handle, "tab");
    press(&mut cx, handle, "space");
    let counts = cx
        .update_window(handle, |root, _, cx| {
            root.downcast::<KeyboardControls>().unwrap().read(cx).counts
        })
        .unwrap();
    assert_eq!(counts, [0, 0, 1, 0]);
}

#[test]
fn independent_overlay_roots_render_after_live_typography_changes() {
    let mut cx = TestAppContext::single();
    cx.update(crate::theme::init_test);
    let tooltip = cx.add_window(|_, _| ButtonTooltip {
        text: "Tooltip".into(),
    });
    let drag = cx.add_window(|_, _| ListDragPreview {
        label: "Drag".into(),
    });
    draw(&mut cx, tooltip.into());
    draw(&mut cx, drag.into());
    cx.update(|cx| {
        let mut font = gpui::font("sans-serif");
        font.weight = gpui::FontWeight::BOLD;
        crate::theme::set_typography(font.clone(), "updated-icons", 24, cx);
        assert_eq!(crate::theme::font(cx), &font);
        assert_eq!(crate::theme::icon_family(cx).as_ref(), "updated-icons");
    });
    draw(&mut cx, tooltip.into());
    draw(&mut cx, drag.into());
}

struct TypographySamples {
    base: gpui::TextLayout,
    small: gpui::TextLayout,
    large: gpui::TextLayout,
    icon: gpui::TextLayout,
    tooltip: gpui::TextLayout,
    row_title: gpui::TextLayout,
    row_detail: gpui::TextLayout,
    row_bounds: std::rc::Rc<std::cell::Cell<gpui::Bounds<gpui::Pixels>>>,
}

impl Render for TypographySamples {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let base = gpui::StyledText::new("Base");
        let small = gpui::StyledText::new("Small");
        let large = gpui::StyledText::new("Large");
        let icon = gpui::StyledText::new("I");
        let tooltip = gpui::StyledText::new("Tooltip");
        let row_title = gpui::StyledText::new("Title");
        let row_detail = gpui::StyledText::new("Detail");
        self.row_title = row_title.layout().clone();
        self.row_detail = row_detail.layout().clone();
        let row_bounds = self.row_bounds.clone();
        self.base = base.layout().clone();
        self.small = small.layout().clone();
        self.large = large.layout().clone();
        self.icon = icon.layout().clone();
        self.tooltip = tooltip.layout().clone();
        div()
            .flex()
            .flex_col()
            .text_size(gpui::px(14.))
            .font(crate::theme::font(cx).clone())
            .font_size_scale(crate::theme::font_size_scale(cx))
            .child(base)
            .child(div().text_xs().child(small))
            .child(div().text_lg().child(large))
            .child(crate::icon("", cx).text_sm().child(icon))
            .child(crate::tooltip_surface(window, cx, 22.5).child(tooltip))
            .child(
                crate::list_row(crate::theme::palette(cx), "sized-row", false)
                    .relative()
                    .h(crate::list_row_height(window, cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0()
                            .child(div().text_sm().child(row_title))
                            .child(div().text_xs().child(row_detail)),
                    )
                    .child(
                        gpui::canvas(
                            move |bounds, _, _| {
                                row_bounds.set(bounds);
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    ),
            )
    }
}

#[test]
fn live_base_size_preserves_hierarchy_icons_and_rem_geometry() {
    let mut cx = TestAppContext::single();
    cx.update(crate::theme::init_test);
    let window = cx.add_window(|_, _| TypographySamples {
        base: Default::default(),
        small: Default::default(),
        large: Default::default(),
        icon: Default::default(),
        tooltip: Default::default(),
        row_title: Default::default(),
        row_detail: Default::default(),
        row_bounds: Default::default(),
    });
    for size in [11, 18, 8] {
        cx.update(|cx| {
            crate::theme::set_typography(gpui::font("sans-serif"), "sans-serif", size, cx)
        });
        draw(&mut cx, window.into());
        cx.update_window(window.into(), |view, window, cx| {
            let entity = view.downcast::<TypographySamples>().unwrap();
            let samples = entity.read(cx);
            let font_size =
                |layout: &gpui::TextLayout| f32::from(layout.line_layouts()[0].font_size());
            let approx = |actual: f32, expected: f32| {
                assert!((actual - expected).abs() < 0.001, "{actual} != {expected}")
            };
            let logical_px = size as f32 * (4. / 3.);
            approx(font_size(&samples.base), logical_px);
            approx(font_size(&samples.small), 12. * logical_px / 14.);
            approx(font_size(&samples.large), 18. * logical_px / 14.);
            approx(font_size(&samples.tooltip), logical_px);
            approx(font_size(&samples.icon), 14.);
            assert_eq!(window.rem_size(), gpui::px(16.));
            assert!(samples.base.line_height() >= gpui::px(logical_px));
            let bounds = samples.row_bounds.get();
            assert!(samples.row_title.bounds().top() >= bounds.top());
            assert!(samples.row_detail.bounds().bottom() <= bounds.bottom());
            assert!(
                bounds.size.height
                    >= samples.row_title.line_height() + samples.row_detail.line_height()
            );
        })
        .unwrap();
    }
}

type RevealBounds = std::rc::Rc<std::cell::Cell<[gpui::Bounds<gpui::Pixels>; 8]>>;

fn reveal_bounds(index: usize, bounds: RevealBounds) -> impl IntoElement {
    gpui::canvas(
        move |value, _, _| {
            let mut measured = bounds.get();
            measured[index] = value;
            bounds.set(measured);
        },
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0()
}

const REVEAL_IDLE: [u32; 2] = [0xe319a1, 0xd17913];
const REVEAL_HOVER: [u32; 2] = [0x199ae3, 0x31ba79];

struct RevealRows {
    scope: FocusScope,
    before: gpui::FocusHandle,
    actions: [gpui::FocusHandle; 2],
    after: gpui::FocusHandle,
    bounds: RevealBounds,
    overlay: bool,
}

impl Render for RevealRows {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::palette(cx);
        let mut surface = self.scope.surface(
            crate::column()
                .id("reveal-scope")
                .size_full()
                .font(crate::theme::font(cx).clone())
                .text_size(gpui::px(14.))
                .font_size_scale(crate::theme::font_size_scale(cx)),
        );
        surface = surface
            .child(button("before-reveal", "Before", cx, |_, _, _| {}).track_focus(&self.before));
        for index in 0..2 {
            let action = button(("revealed-action", index), "", cx, |_, _, _| {})
                .track_focus(&self.actions[index])
                .size_8()
                .p_0()
                .border_0()
                .bg(gpui::rgb(REVEAL_IDLE[index]))
                .group_hover("control", move |style| {
                    style.bg(gpui::rgb(REVEAL_HOVER[index]))
                })
                .relative()
                .child(reveal_bounds(index * 4 + 2, self.bounds.clone()));
            let reveal = if self.overlay {
                crate::row::hover_actions(theme, ("action-reveal", index), "reveal-test-row", false)
                    .child(action)
            } else {
                crate::row::hover_reveal(("action-reveal", index), "reveal-test-row", action)
                    .relative()
            }
            .child(reveal_bounds(index * 4 + 1, self.bounds.clone()));
            surface = surface.child(
                crate::row()
                    .id(("reveal-row", index))
                    .group("reveal-test-row")
                    .relative()
                    .w_64()
                    .h_12()
                    .flex_shrink_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .relative()
                            .child("Row text")
                            .child(reveal_bounds(index * 4 + 3, self.bounds.clone())),
                    )
                    .child(reveal)
                    .child(reveal_bounds(index * 4, self.bounds.clone())),
            );
        }
        surface.child(button("after-reveal", "After", cx, |_, _, _| {}).track_focus(&self.after))
    }
}

fn assert_reveal_paint(window: &Window, expected: [Option<u32>; 2]) {
    let quads = window.painted_quads();
    for index in 0..2 {
        for color in [REVEAL_IDLE[index], REVEAL_HOVER[index]] {
            let color_value: gpui::Hsla = gpui::rgb(color).into();
            let painted = quads
                .iter()
                .any(|quad| quad.background.as_solid() == Some(color_value));
            assert_eq!(
                painted,
                expected[index] == Some(color),
                "row {index}, {color:x}, expected={expected:?}"
            );
        }
    }
}

#[test]
fn row_action_reveal_tracks_own_focus_and_hover_without_layout_or_tab_stops() {
    for overlay in [false, true] {
        let mut cx = TestAppContext::single();
        cx.update(crate::theme::init_test);
        let scope = cx.update(FocusScope::new);
        let before = cx.update(|cx| cx.focus_handle().tab_stop(true));
        let actions = cx.update(|cx| {
            [
                cx.focus_handle().tab_stop(true),
                cx.focus_handle().tab_stop(true),
            ]
        });
        let after = cx.update(|cx| cx.focus_handle().tab_stop(true));
        let bounds = RevealBounds::default();
        let window = cx.add_window({
            let scope = scope.clone();
            let before = before.clone();
            let actions = actions.clone();
            let after = after.clone();
            let bounds = bounds.clone();
            move |_, _| RevealRows {
                scope,
                before,
                actions,
                after,
                bounds,
                overlay,
            }
        });
        let handle = window.into();
        draw(&mut cx, handle);
        cx.update_window(handle, |_, window, cx| {
            window.simulate_mouse_move(gpui::point(gpui::px(500.), gpui::px(500.)), cx);
            scope.handle().focus(window, cx);
        })
        .unwrap();
        draw(&mut cx, handle);
        let initial = bounds.get();
        for index in 0..2 {
            let [row, reveal, action, text] = &initial[index * 4..index * 4 + 4] else {
                unreachable!()
            };
            assert_eq!(reveal.size.width, action.size.width);
            assert_eq!(reveal.left(), action.left());
            assert_eq!(reveal.right(), row.right());
            assert_eq!(action.center().y, row.center().y);
            assert!(text.right() <= row.right());
            assert!(text.top() >= row.top());
            assert!(text.bottom() <= row.bottom());
            if overlay {
                assert_eq!(reveal.size.height, row.size.height);
                assert_eq!(text.right(), row.right());
            } else {
                assert_eq!(reveal.size, action.size);
                assert!(text.right() < reveal.left());
            }
        }
        cx.update_window(handle, |_, window, _| {
            assert_reveal_paint(window, [None, None])
        })
        .unwrap();

        // Focusing the enclosing scope must not reveal every descendant row.
        // Hovering row text reveals only that row; child hover retains its own material.
        for (position, expected) in [
            (initial[3].center(), [Some(REVEAL_IDLE[0]), None]),
            (initial[2].center(), [Some(REVEAL_HOVER[0]), None]),
            (initial[7].center(), [None, Some(REVEAL_IDLE[1])]),
            (gpui::point(gpui::px(500.), gpui::px(500.)), [None, None]),
        ] {
            cx.update_window(handle, |_, window, cx| {
                window.simulate_mouse_move(position, cx)
            })
            .unwrap();
            draw(&mut cx, handle);
            cx.update_window(handle, |_, window, _| assert_reveal_paint(window, expected))
                .unwrap();
            assert_eq!(bounds.get(), initial);
        }

        cx.update_window(handle, |_, window, cx| scope.advance(false, window, cx))
            .unwrap();
        draw(&mut cx, handle);
        cx.update_window(handle, |_, window, _| {
            assert!(
                before.is_focused(window),
                "overlay={overlay}, root={}, actions={:?}, after={}",
                scope.handle().is_focused(window),
                actions.each_ref().map(|action| action.is_focused(window)),
                after.is_focused(window)
            );
        })
        .unwrap();
        for (key, focused, expected) in [
            ("tab", &actions[0], [Some(REVEAL_IDLE[0]), None]),
            ("tab", &actions[1], [None, Some(REVEAL_IDLE[1])]),
            ("tab", &after, [None, None]),
            ("shift-tab", &actions[1], [None, Some(REVEAL_IDLE[1])]),
            ("shift-tab", &actions[0], [Some(REVEAL_IDLE[0]), None]),
            ("shift-tab", &before, [None, None]),
        ] {
            press(&mut cx, handle, key);
            cx.update_window(handle, |_, window, _| {
                assert!(focused.is_focused(window), "{key}, overlay={overlay}");
                assert_reveal_paint(window, expected);
            })
            .unwrap();
            assert_eq!(bounds.get(), initial);
            if focused == &actions[0] {
                cx.update_window(handle, |_, window, cx| {
                    window.simulate_mouse_move(initial[7].center(), cx);
                })
                .unwrap();
                draw(&mut cx, handle);
                cx.update_window(handle, |_, window, _| {
                    assert_reveal_paint(window, [Some(REVEAL_IDLE[0]), Some(REVEAL_IDLE[1])]);
                })
                .unwrap();
                assert_eq!(bounds.get(), initial);
                cx.update_window(handle, |_, window, cx| {
                    window.simulate_mouse_move(gpui::point(gpui::px(500.), gpui::px(500.)), cx);
                })
                .unwrap();
                draw(&mut cx, handle);
            }
        }
    }
}

struct RowActionFocus {
    search: gpui::Entity<crate::input::Input>,
    action_focus: gpui::FocusHandle,
    action_bounds: std::rc::Rc<std::cell::Cell<gpui::Bounds<gpui::Pixels>>>,
    drag_starts: std::rc::Rc<std::cell::Cell<usize>>,
    activations: usize,
    selections: usize,
    region: usize,
}

impl Render for RowActionFocus {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = self.action_bounds.clone();
        let action = crate::IconButton::new("favorite", "♥", "Favorite")
            .toggled(!self.activations.is_multiple_of(2))
            .build_with(
                crate::IconButtonStyle::Quiet,
                crate::IconButtonDensity::Compact,
                cx,
                |this, _, cx| {
                    this.activations += 1;
                    cx.notify();
                },
                |control| {
                    control.track_focus(&self.action_focus).relative().child(
                        gpui::canvas(move |measured, _, _| bounds.set(measured), |_, _, _, _| {})
                            .absolute()
                            .size_full(),
                    )
                },
            );
        let theme = crate::theme::palette(cx);
        let actions = match self.region {
            0 => crate::row_action_region("action-region").child(action),
            1 => crate::hover_reveal("action-region", "action-row", action),
            _ => crate::hover_actions(theme, "action-region", "action-row", false).child(action),
        };
        let drag_starts = self.drag_starts.clone();
        crate::column()
            .size_full()
            .child(self.search.clone())
            .child(
                crate::list_row(theme, "track-row", false)
                    .group("action-row")
                    .relative()
                    .w_64()
                    .h_12()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _, _| this.selections += 1),
                    )
                    .on_drag((), move |_, _, _, cx| {
                        drag_starts.set(drag_starts.get() + 1);
                        cx.new(|_| ListDragPreview {
                            label: "Track".into(),
                        })
                    })
                    .child(div().flex_1().child("Track"))
                    .child(actions),
            )
    }
}

#[test]
fn row_action_regions_preserve_native_focus_click_and_keyboard_without_selecting_or_dragging() {
    for region in 0..3 {
        let mut cx = TestAppContext::single();
        cx.update(crate::theme::init_test);
        let window = cx.add_window(|_, cx| RowActionFocus {
            search: cx.new(|cx| crate::input::Input::new("Search", "", "", cx)),
            action_focus: cx.focus_handle(),
            action_bounds: Default::default(),
            drag_starts: Default::default(),
            activations: 0,
            selections: 0,
            region,
        });
        let root = window.root(&mut cx).unwrap();
        let handle = window.into();
        cx.update_window(handle, |_, window, cx| {
            root.read(cx)
                .search
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        })
        .unwrap();
        draw(&mut cx, handle);
        cx.dispatch_keystroke(handle, Keystroke::parse("s").unwrap());
        draw(&mut cx, handle);
        let position = root.read_with(&cx, |this, _| this.action_bounds.get().center());
        cx.update_window(handle, |_, window, cx| {
            window.simulate_mouse_move(position, cx);
            window.draw(cx).clear(cx);
            window.dispatch_event(
                gpui::MouseDownEvent {
                    button: gpui::MouseButton::Left,
                    position,
                    click_count: 1,
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                gpui::MouseUpEvent {
                    button: gpui::MouseButton::Left,
                    position,
                    click_count: 1,
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            assert!(root.read(cx).action_focus.is_focused(window));
        })
        .unwrap();
        draw(&mut cx, handle);
        cx.dispatch_keystroke(handle, Keystroke::parse("i").unwrap());
        press(&mut cx, handle, "enter");
        press(&mut cx, handle, "space");
        root.read_with(&cx, |this, cx| {
            assert_eq!(this.search.read(cx).text(), "s");
            assert_eq!(this.activations, 3);
            assert_eq!(this.selections, 0);
        });
        // Moving a pressed action must not arm the ancestor's native row drag.
        cx.update_window(handle, |_, window, cx| {
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
                gpui::MouseMoveEvent {
                    position: position - gpui::point(gpui::px(64.), gpui::px(0.)),
                    pressed_button: Some(gpui::MouseButton::Left),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            assert!(!cx.has_active_drag());
        })
        .unwrap();
        root.read_with(&cx, |this, _| {
            assert_eq!(this.drag_starts.get(), 0);
            assert_eq!(this.selections, 0);
        });
        // The neighboring row body still selects and starts its ordinary drag.
        cx.update_window(handle, |_, window, cx| {
            let row_position = position - gpui::point(gpui::px(64.), gpui::px(0.));
            window.dispatch_event(
                gpui::MouseUpEvent {
                    button: gpui::MouseButton::Left,
                    position: row_position,
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                gpui::MouseDownEvent {
                    button: gpui::MouseButton::Left,
                    position: row_position,
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                gpui::MouseMoveEvent {
                    position: row_position - gpui::point(gpui::px(64.), gpui::px(0.)),
                    pressed_button: Some(gpui::MouseButton::Left),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            assert!(cx.has_active_drag());
        })
        .unwrap();
        root.read_with(&cx, |this, _| {
            assert_eq!(this.drag_starts.get(), 1);
            assert_eq!(this.selections, 1);
        });
    }
}

struct OverlayMaterials {
    actions: [gpui::FocusHandle; 2],
    bounds: std::rc::Rc<std::cell::Cell<[gpui::Bounds<gpui::Pixels>; 2]>>,
}

impl Render for OverlayMaterials {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::palette(cx);
        crate::column().children((0..2).map(|index| {
            let selected = index == 1;
            let bounds = self.bounds.clone();
            crate::row()
                .id(("material-row", index))
                .group("material-row")
                .relative()
                .w_64()
                .h_12()
                .bg(crate::row_background(theme, selected, false))
                .hover(move |style| style.bg(crate::row_background(theme, selected, true)))
                .child("Row text")
                .child(
                    crate::hover_actions(
                        theme,
                        ("material-actions", index),
                        "material-row",
                        selected,
                    )
                    .child(
                        crate::IconButton::new(("material-action", index), "*", "Favorite")
                            .toggled(true)
                            .selected_fill(false)
                            .build_with(
                                crate::IconButtonStyle::Quiet,
                                crate::IconButtonDensity::Compact,
                                cx,
                                |_, _, _| {},
                                |control| {
                                    control
                                        .track_focus(&self.actions[index])
                                        .size_8()
                                        .p_0()
                                        .border_0()
                                },
                            ),
                    )
                    .child(
                        gpui::canvas(
                            move |value, _, _| {
                                let mut measured = bounds.get();
                                measured[index] = value;
                                bounds.set(measured);
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    ),
                )
        }))
    }
}

#[test]
fn overlay_material_follows_row_hover_without_treating_keyboard_focus_as_hover() {
    let mut cx = TestAppContext::single();
    cx.update(crate::theme::init_test);
    let theme = cx.read(crate::theme::palette);
    let actions = cx.update(|cx| {
        [
            cx.focus_handle().tab_stop(true),
            cx.focus_handle().tab_stop(true),
        ]
    });
    let bounds = std::rc::Rc::new(std::cell::Cell::new([gpui::Bounds::default(); 2]));
    let window = cx.add_window({
        let actions = actions.clone();
        let bounds = bounds.clone();
        move |_, _| OverlayMaterials { actions, bounds }
    });
    let handle = window.into();
    draw(&mut cx, handle);
    let original = bounds.get();
    for (focused, action) in actions.iter().enumerate() {
        for hovered in [None, Some(0), Some(1), None] {
            cx.update_window(handle, |_, window, cx| {
                action.focus(window, cx);
                let position = hovered
                    .map_or(gpui::point(gpui::px(500.), gpui::px(500.)), |index| {
                        gpui::point(gpui::px(8.), original[index].center().y)
                    });
                window.simulate_mouse_move(position, cx);
            })
            .unwrap();
            draw(&mut cx, handle);
            cx.update_window(handle, |_, window, _| {
                let scale = window.scale_factor();
                for (index, bounds) in original.iter().enumerate() {
                    if index != focused && hovered != Some(index) {
                        continue;
                    }
                    let expected: gpui::Hsla =
                        crate::row_background(theme, index == 1, hovered == Some(index)).into();
                    let area = bounds.scale(scale);
                    assert!(
                        window.painted_quads().iter().any(|quad| {
                            quad.bounds == area && quad.background.as_solid() == Some(expected)
                        }),
                        "row={index}, focused={focused}, hovered={hovered:?}"
                    );
                }
            })
            .unwrap();
            assert_eq!(bounds.get(), original);
        }
    }
}
