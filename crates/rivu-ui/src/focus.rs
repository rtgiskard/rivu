use gpui::{Div, Stateful, Window, div, prelude::*};

/// Native tab order bounded to a transient menu or dialog.
#[derive(Clone)]
pub struct FocusScope {
    start: gpui::FocusHandle,
    end: gpui::FocusHandle,
}

impl FocusScope {
    pub fn new(cx: &mut gpui::App) -> Self {
        Self {
            start: cx.focus_handle(),
            end: cx.focus_handle().tab_index(isize::MAX),
        }
    }

    pub fn handle(&self) -> &gpui::FocusHandle {
        &self.start
    }

    pub fn surface(&self, surface: Stateful<Div>) -> Stateful<Div> {
        let scope = self.clone();
        surface
            .tab_group()
            .tab_index(0)
            .tab_stop(false)
            .track_focus(&self.start)
            .child(
                div()
                    .id("focus-scope-end")
                    .absolute()
                    .w_0()
                    .h_0()
                    .tab_index(isize::MAX)
                    .tab_stop(false)
                    .track_focus(&self.end),
            )
            .on_key_down(move |event, window, cx| {
                if tab_key(event) {
                    scope.advance(event.keystroke.modifiers.shift, window, cx);
                    cx.stop_propagation();
                }
            })
    }

    pub fn menu_surface(&self, surface: Stateful<Div>) -> Stateful<Div> {
        let scope = self.clone();
        self.surface(surface).on_key_down(move |event, window, cx| {
            if matches!(event.keystroke.key.as_str(), "up" | "down")
                && !event.keystroke.modifiers.modified()
            {
                scope.advance(event.keystroke.key == "up", window, cx);
                cx.stop_propagation();
            }
        })
    }

    pub fn focus(&self, window: &mut Window, cx: &mut gpui::App) {
        self.start.focus(window, cx);
        let scope = self.clone();
        window.on_next_frame(move |window, cx| {
            // Do not steal focus if the user already chose an editor or closed the surface.
            if scope.start.is_focused(window) {
                scope.advance(false, window, cx);
            }
        });
    }

    pub fn advance(&self, backwards: bool, window: &mut Window, cx: &mut gpui::App) {
        if backwards {
            window.focus_prev(cx);
        } else {
            window.focus_next(cx);
        }
        if !self.start.contains_focused(window, cx) {
            if backwards {
                self.end.focus(window, cx);
                window.focus_prev(cx);
            } else {
                self.start.focus(window, cx);
                window.focus_next(cx);
            }
            if !self.start.contains_focused(window, cx) {
                self.start.focus(window, cx);
            }
        }
    }
}

pub fn tab_key(event: &gpui::KeyDownEvent) -> bool {
    let modifiers = event.keystroke.modifiers;
    event.keystroke.key == "tab"
        && !modifiers.control
        && !modifiers.platform
        && !modifiers.alt
        && !modifiers.function
}

/// Dropdown navigation admits only bare keys, with Shift reserved for reverse Tab.
pub fn dropdown_key(event: &gpui::KeyDownEvent) -> Option<&str> {
    if tab_key(event) {
        return Some("tab");
    }
    if event.keystroke.modifiers.modified() {
        return None;
    }
    let key = event.keystroke.key.as_str();
    matches!(
        key,
        "up" | "down" | "pageup" | "pagedown" | "enter" | "space" | "escape"
    )
    .then_some(key)
}

pub fn context_menu_key(event: &gpui::KeyDownEvent) -> bool {
    let modifiers = event.keystroke.modifiers;
    !modifiers.control
        && !modifiers.platform
        && !modifiers.alt
        && !modifiers.function
        && ((event.keystroke.key == "f10" && modifiers.shift)
            || (event.keystroke.key == "menu" && !modifiers.shift))
}
