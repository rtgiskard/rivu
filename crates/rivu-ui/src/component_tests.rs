use crate::{
    control::{IconButton, IconButtonDensity, IconButtonStyle, button, disabled_control},
    focus::FocusScope,
    layout::FormField,
    overlay::ListDragPreview,
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
        let toggle = IconButton::new("toggle", "T", "Toggle: off")
            .switch(false)
            .build(
                IconButtonStyle::Surface,
                IconButtonDensity::Regular,
                cx,
                |this, _, cx| {
                    this.counts[2] += 1;
                    cx.notify();
                },
            );
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

        cx.update_window(handle, |_, window, cx| {
            let position = initial[2].center();
            window.simulate_mouse_move(position, cx);
        })
        .unwrap();
        draw(&mut cx, handle);
        cx.update_window(handle, |_, window, cx| {
            let position = initial[2].center();
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
        })
        .unwrap();
        draw(&mut cx, handle);
        cx.update_window(handle, |_, window, cx| {
            assert!(actions[0].is_focused(window));
            assert_reveal_paint(window, [Some(REVEAL_HOVER[0]), None]);
            window.simulate_mouse_move(gpui::point(gpui::px(500.), gpui::px(500.)), cx);
        })
        .unwrap();
        draw(&mut cx, handle);
        cx.update_window(handle, |_, window, _| {
            assert!(actions[0].is_focused(window));
            assert_reveal_paint(window, [None, None]);
        })
        .unwrap();
        press(&mut cx, handle, "enter");
        cx.update_window(handle, |_, window, cx| {
            assert_reveal_paint(window, [Some(REVEAL_IDLE[0]), None]);
            scope.handle().focus(window, cx);
        })
        .unwrap();

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
                    assert_reveal_paint(window, [None, Some(REVEAL_IDLE[1])]);
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

type RowSurfaceBounds = std::rc::Rc<std::cell::Cell<[gpui::Bounds<gpui::Pixels>; 5]>>;

struct RowSurfaces {
    bounds: RowSurfaceBounds,
    selected: bool,
    alternate: bool,
    width: f32,
}

impl Render for RowSurfaces {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::palette(cx);
        let bounds = self.bounds.clone();
        let text = crate::row::two_line_text(
            theme,
            if self.alternate {
                "短い題名"
            } else {
                "A long title that must truncate in a narrow row"
            },
            if self.alternate {
                "Different detail"
            } else {
                "A long detail that must truncate independently"
            },
        )
        .on_children_prepainted(move |children, _, _| {
            let mut measured = bounds.get();
            measured[3] = children[0];
            measured[4] = children[1];
            bounds.set(measured);
        });
        let label = if self.alternate {
            "＋"
        } else {
            "A long single-line row label"
        };
        let bounds = self.bounds.clone();
        crate::column()
            .w(gpui::px(self.width))
            .font(crate::theme::font(cx).clone())
            .text_size(gpui::px(14.))
            .font_size_scale(crate::theme::font_size_scale(cx))
            .text_sm()
            .child(
                crate::row::list_row(
                    theme,
                    "geometry-list",
                    self.selected,
                    crate::row::list_row_height(window, cx),
                )
                .child(text),
            )
            .child(
                crate::row::tree_row(
                    theme,
                    "geometry-tree",
                    1,
                    self.selected,
                    crate::row::tree_row_height(window, cx),
                )
                .child(div().flex_1().min_w_0().truncate().child(label)),
            )
            .child(crate::row::dropdown_row(
                theme,
                "geometry-dropdown",
                self.selected,
                label,
                crate::row::single_line_row_height(window, cx),
            ))
            .on_children_prepainted(move |children, _, _| {
                let mut measured = bounds.get();
                measured[..3].copy_from_slice(&children);
                bounds.set(measured);
            })
    }
}

#[test]
fn list_tree_and_dropdown_geometry_survives_text_selection_hover_and_scaling() {
    let mut cx = TestAppContext::single();
    cx.update(crate::theme::init_test);
    let bounds = RowSurfaceBounds::default();
    let window = cx.add_window({
        let bounds = bounds.clone();
        move |_, _| RowSurfaces {
            bounds,
            selected: false,
            alternate: false,
            width: 240.,
        }
    });
    let root = window.root(&mut cx).unwrap();
    let handle = window.into();
    for rem in [12., 24.] {
        for font_size in [8, 18] {
            cx.update(|cx| {
                crate::theme::set_typography(gpui::font("sans-serif"), "sans-serif", font_size, cx);
            });
            cx.update_window(handle, |_, window, _| window.set_rem_size(gpui::px(rem)))
                .unwrap();
            for width in [96., 240.] {
                root.update(&mut cx, |this, cx| {
                    this.width = width;
                    cx.notify();
                });
                draw(&mut cx, handle);
                let baseline = bounds.get();
                for selected in [false, true] {
                    for alternate in [false, true] {
                        root.update(&mut cx, |this, cx| {
                            this.selected = selected;
                            this.alternate = alternate;
                            cx.notify();
                        });
                        for position in baseline[..3]
                            .iter()
                            .map(|bounds| bounds.center())
                            .chain([gpui::point(gpui::px(500.), gpui::px(500.))])
                        {
                            cx.update_window(handle, |_, window, cx| {
                                window.simulate_mouse_move(position, cx)
                            })
                            .unwrap();
                            draw(&mut cx, handle);
                            let measured = bounds.get();
                            assert_eq!(&measured[..3], &baseline[..3]);
                            for row in &measured[..3] {
                                assert_eq!(row.size.width, gpui::px(width));
                            }
                            for text in &measured[3..] {
                                assert!(text.left() >= measured[0].left());
                                assert!(text.right() <= measured[0].right());
                                assert!(text.top() >= measured[0].top());
                                assert!(text.bottom() <= measured[0].bottom());
                            }
                        }
                    }
                }
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                crate::list_row(
                    theme,
                    "track-row",
                    false,
                    crate::list_row_height(window, cx),
                )
                .group("action-row")
                .relative()
                .w_64()
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

struct ClippedDropTarget {
    drops: usize,
}

impl Render for ClippedDropTarget {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::palette(cx);
        crate::column()
            .w(gpui::px(200.))
            .child(
                div()
                    .id("source")
                    .w_full()
                    .h(gpui::px(40.))
                    .on_drag(0u8, |_, _, _, cx| {
                        cx.new(|_| ListDragPreview {
                            label: "drag".into(),
                        })
                    }),
            )
            .child(
                div().h(gpui::px(20.)).w_full().overflow_hidden().child(
                    crate::list_row(theme, "partial-row", false, gpui::px(40.))
                        .relative()
                        .map(|mut row| {
                            row.style().overflow.x = Some(gpui::Overflow::Visible);
                            row.style().overflow.y = Some(gpui::Overflow::Visible);
                            row
                        })
                        .children([false, true].map(|after| {
                            crate::list_drop_overlay(
                                crate::list_drop_target(("target", usize::from(after)), after)
                                    .can_drop(|value, _, _| value.is::<u8>())
                                    .on_drop(cx.listener(|this, _: &u8, _, _| this.drops += 1)),
                            )
                        })),
                ),
            )
    }
}

#[test]
fn deferred_row_drop_targets_reject_pointer_outside_list_viewport() {
    for (y, expected) in [(50., 1), (70., 0)] {
        let mut cx = TestAppContext::single();
        cx.update(crate::theme::init_test);
        let window = cx.add_window(|_, _| ClippedDropTarget { drops: 0 });
        let root = window.root(&mut cx).unwrap();
        let handle = window.into();
        draw(&mut cx, handle);
        cx.update_window(handle, |_, window, cx| {
            window.dispatch_event(
                gpui::MouseDownEvent {
                    button: gpui::MouseButton::Left,
                    position: gpui::point(gpui::px(20.), gpui::px(20.)),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                gpui::MouseMoveEvent {
                    pressed_button: Some(gpui::MouseButton::Left),
                    position: gpui::point(gpui::px(40.), gpui::px(y)),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            assert!(cx.has_active_drag());
        })
        .unwrap();
        draw(&mut cx, handle);
        cx.update_window(handle, |_, window, cx| {
            window.simulate_mouse_move(gpui::point(gpui::px(40.), gpui::px(y)), cx);
            window.draw(cx).clear(cx);
            window.dispatch_event(
                gpui::MouseUpEvent {
                    button: gpui::MouseButton::Left,
                    position: gpui::point(gpui::px(40.), gpui::px(y)),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
        })
        .unwrap();
        assert_eq!(
            root.read_with(&cx, |this, _| this.drops),
            expected,
            "drop y={y}"
        );
    }
}
