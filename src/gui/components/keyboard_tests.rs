use super::*;
use gpui::{AnyWindowHandle, InputEvent, KeyDownEvent, KeyUpEvent, Keystroke, TestAppContext};

struct KeyboardControls {
    scope: FocusScope,
    counts: [usize; 4],
    disabled: bool,
}

impl Render for KeyboardControls {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::gui::theme::palette(cx);
        let first = button("first", "First", cx, |this, _, _| this.counts[0] += 1);
        let middle = disabled_control(
            theme,
            button("middle", "Unavailable", cx, |this, _, _| {
                this.counts[1] += 1
            }),
            self.disabled,
        );
        let toggle = toggle_button("toggle", "T", "Toggle", false, cx, |this, _| {
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
