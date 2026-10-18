use crate::{
    control::{button, disabled_control, toggle_button},
    focus::FocusScope,
    layout::form_field,
    overlay::{ButtonTooltip, ListDragPreview},
};
use gpui::{
    AnyWindowHandle, Context, InputEvent, KeyDownEvent, KeyUpEvent, Keystroke, Render,
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
                .child(form_field(theme, "Direction", middle))
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
