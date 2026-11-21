use gpui::{
    App, InputEvent, KeyDownEvent, KeyUpEvent, Keystroke, MouseButton, MouseDownEvent,
    MouseUpEvent, Pixels, Point, Window,
};

// Dispatch only: callers decide when the painted frame or focus should change.
pub(crate) fn key_down(key: &str) -> KeyDownEvent {
    KeyDownEvent {
        keystroke: Keystroke::parse(key).unwrap(),
        is_held: false,
        prefer_character_input: false,
    }
}

pub(crate) fn key_up(key: &str) -> KeyUpEvent {
    KeyUpEvent {
        keystroke: Keystroke::parse(key).unwrap(),
    }
}

pub(crate) fn press(window: &mut Window, cx: &mut App, key: &str) {
    window.dispatch_event(key_down(key).to_platform_input(), cx);
    window.dispatch_event(key_up(key).to_platform_input(), cx);
}

pub(crate) fn down(button: MouseButton, position: Point<Pixels>) -> MouseDownEvent {
    MouseDownEvent {
        button,
        position,
        click_count: 1,
        ..Default::default()
    }
}

pub(crate) fn up(button: MouseButton, position: Point<Pixels>) -> MouseUpEvent {
    MouseUpEvent {
        button,
        position,
        click_count: 1,
        ..Default::default()
    }
}

pub(crate) fn click(
    window: &mut Window,
    cx: &mut App,
    button: MouseButton,
    position: Point<Pixels>,
) {
    window.dispatch_event(down(button, position).to_platform_input(), cx);
    window.dispatch_event(up(button, position).to_platform_input(), cx);
}
