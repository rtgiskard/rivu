use super::*;
use crate::gui::components::{caption, column, icon_button, row};
use crate::keyboard::{self, Action, Context as KeyContext, Modifiers as KeyModifiers};

fn visible_panel(node: &Node, id: u64) -> Option<&layout::Panel> {
    match node {
        Node::Split { children, .. } => children.iter().find_map(|child| visible_panel(child, id)),
        Node::Tabs { panels, active, .. } => {
            panels.iter().find(|panel| panel.id == id && *active == id)
        }
    }
}

fn modifiers(event: &KeyDownEvent) -> KeyModifiers {
    let modifiers = event.keystroke.modifiers;
    KeyModifiers {
        control: modifiers.control,
        alt: modifiers.alt,
        shift: modifiers.shift,
        platform: modifiers.platform,
        function: modifiers.function,
    }
}

impl GuiApp {
    pub(super) fn open_keyboard_help(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_interaction(window, cx);
        self.keyboard_help = Some(self.keyboard_context());
        self.help_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    pub(super) fn keyboard_panel(&self) -> Option<&layout::Panel> {
        visible_panel(self.layouts.current().root.as_ref()?, self.search_panel?)
    }

    fn keyboard_context(&self) -> KeyContext {
        match self.keyboard_panel().map(|panel| panel.kind.as_str()) {
            Some("library") => KeyContext::GuiLibrary,
            Some("queue") => KeyContext::GuiQueue,
            Some("playlists") if self.active_list_focus() == Some(ListFocus::PlaylistTracks) => {
                KeyContext::GuiPlaylistTracks
            }
            Some("playlists") => KeyContext::GuiPlaylists,
            Some("history") => KeyContext::GuiHistory,
            Some("albums") => KeyContext::GuiAlbums,
            _ => KeyContext::Gui,
        }
    }

    pub(super) fn keyboard_input(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let modifiers = modifiers(event);
        let plain = modifiers == KeyModifiers::default();
        let cancel = key == "escape" && plain;
        if cancel && self.dismiss_input_composition(window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.keyboard_help.is_some() {
            if let Some(action) = keyboard::resolve(key, modifiers, KeyContext::GuiHelp) {
                self.help_key(action, cx);
            }
            cx.stop_propagation();
            return;
        }
        if matches!(
            self.keyboard_surface,
            Some(shell::KeyboardSurface::Menu(..))
        ) {
            if plain {
                self.menu_key(key, window, cx);
            }
            cx.stop_propagation();
            return;
        }
        if self.manual_confirmation_open() {
            if cancel {
                self.cancel_manual_action(cx);
            }
            cx.stop_propagation();
            return;
        }
        if components::tab_key(event) {
            if modifiers.shift {
                window.focus_prev(cx);
            } else {
                window.focus_next(cx);
            }
            cx.stop_propagation();
            return;
        }
        // Leaving an editor is one operation; dismissing its containing surface is another.
        if cancel && self.input_focused(window, cx) {
            self.focus_layout(window, cx);
            cx.stop_propagation();
            return;
        }
        if self.playlist_action.is_some() {
            if cancel {
                self.dismiss_playlist_action(window, cx);
            }
            cx.stop_propagation();
            return;
        }
        if self.playlist_editor.is_some() {
            if cancel {
                self.cancel_playlist_name(window, cx);
                cx.stop_propagation();
            } else if !self.input_focused(window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        if self.duplicate_review.open {
            if cancel {
                self.close_duplicate_review(window, cx);
            }
            cx.stop_propagation();
            return;
        }
        if self.about_open {
            if cancel {
                self.about_open = false;
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }
        if self.settings_open {
            return;
        }
        if self.layouts_open {
            if cancel {
                if self.layout_action.is_some() {
                    self.layout_action = None;
                } else {
                    self.layouts_open = false;
                }
                self.focus_layout(window, cx);
                cx.notify();
                cx.stop_propagation();
            }
            return;
        }
        let context = self.keyboard_context();
        let action = keyboard::resolve(key, modifiers, context);
        if action == Some(Action::Search) && self.route_search_key(true, window, cx) {
            cx.stop_propagation();
            return;
        }
        if cancel && self.route_search_key(false, window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.input_focused(window, cx) {
            return;
        }
        // Native controls own Enter/Space; playback never also activates behind them.
        if !self.layout_focus.is_focused(window) {
            return;
        }
        let Some(action) = action else { return };
        match action {
            Action::Cancel => self.request_clear_selection(cx),
            Action::Up | Action::Down => {
                if let Some(focus) = self.active_list_focus() {
                    self.navigate_list(focus, action == Action::Down, cx);
                }
            }
            Action::First | Action::Last => {
                if let Some(focus) = self.active_list_focus() {
                    let last = action == Action::Last;
                    match focus {
                        ListFocus::Library => self.jump_library_edge(last, cx),
                        ListFocus::Queue => self.jump_queue_edge(last, cx),
                        ListFocus::PlaylistTracks => self.jump_playlist_edge(last, cx),
                    }
                }
            }
            Action::PageUp | Action::PageDown => {
                if self
                    .scroll_views
                    .page(action == Action::PageDown, window.mouse_position())
                {
                    cx.notify();
                }
            }
            Action::Parent | Action::Expand => {
                if self.active_list_focus() == Some(ListFocus::Library) {
                    self.library_tree_key(
                        if action == Action::Parent {
                            "left"
                        } else {
                            "right"
                        },
                        cx,
                    );
                }
            }
            Action::Activate => {
                if let Some(focus) = self.active_list_focus() {
                    self.play_focused_selection(focus, cx);
                }
            }
            Action::Remove => {
                if let Some(focus) = self.active_list_focus() {
                    self.remove_focused_queue(focus, cx);
                }
            }
            Action::SelectAll => match context {
                KeyContext::GuiQueue => self.select_all_queue_entries(cx),
                KeyContext::GuiPlaylistTracks => self.select_all_playlist_entries(cx),
                KeyContext::GuiHistory => {
                    self.browser.history.select_all();
                    cx.notify();
                }
                _ => {}
            },
            Action::ExtendUp | Action::ExtendDown | Action::AddExtendUp | Action::AddExtendDown => {
                let down = matches!(action, Action::ExtendDown | Action::AddExtendDown);
                let multi = matches!(action, Action::AddExtendUp | Action::AddExtendDown);
                self.navigate_playlist(down, true, multi, cx);
            }
            _ => {
                if let Some(command) = keyboard::playback_command(action, &self.state.playback) {
                    self.send(command, cx);
                }
            }
        }
        cx.stop_propagation();
    }

    fn help_key(&mut self, action: Action, cx: &mut Context<Self>) {
        let handle = &self.help_scroll;
        let mut offset = handle.offset();
        let maximum = handle.max_offset().y.max(px(0.));
        let page = handle.bounds().size.height;
        match action {
            Action::Cancel => self.keyboard_help = None,
            Action::Up => offset.y += px(32. * self.state.system.config.ui_scale),
            Action::Down => offset.y -= px(32. * self.state.system.config.ui_scale),
            Action::PageUp => offset.y += page,
            Action::PageDown => offset.y -= page,
            Action::First => offset.y = px(0.),
            Action::Last => offset.y = -maximum,
            _ => return,
        }
        handle.set_offset(point(offset.x, offset.y.max(-maximum).min(px(0.))));
        cx.notify();
    }

    pub(super) fn keyboard_help_overlay(
        &self,
        context: KeyContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = crate::gui::theme::palette(cx);
        let mut rows = column().flex_shrink_0().gap_2();
        let mut row_count = 0;
        for binding in keyboard::help_rows(&[context]) {
            rows = rows.child(
                row()
                    .gap_4()
                    .line_height(rems(1.25))
                    .child(
                        div()
                            .w(rems(12.))
                            .flex_shrink_0()
                            .text_color(rgb(theme.accent))
                            .child(binding.label()),
                    )
                    .child(div().flex_1().child(self.tr(binding.description))),
            );
            row_count += 1;
        }
        div()
            .id("keyboard-help-modal")
            .map(|surface| self.dialog_surface(surface))
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
            .child(
                column()
                    .id("keyboard-help-card")
                    .w(rems(42.))
                    .max_w(relative(0.9))
                    .max_h(relative(0.85))
                    .min_h_0()
                    .p_4()
                    .gap_3()
                    .bg(rgb(theme.panel))
                    .border_1()
                    .border_color(rgb(theme.border))
                    .rounded_md()
                    .child(
                        row().child(self.tr("shell-keyboard-help")).child(
                            components::quiet_icon_button(
                                theme,
                                icon_button(
                                    "keyboard-help-close",
                                    "",
                                    self.tr("shell-close-keyboard-help"),
                                    cx,
                                    |this, _, cx| {
                                        this.keyboard_help = None;
                                        cx.notify();
                                    },
                                ),
                            )
                            .focus(|style| {
                                style
                                    .border_color(gpui::transparent_black())
                                    .text_color(rgb(theme.accent))
                            })
                            .ml_auto(),
                        ),
                    )
                    .child(caption(theme, self.tr("shell-keyboard-scope")))
                    .child(
                        div()
                            .h(rems(row_count as f32 * 1.75 - 0.5))
                            .flex_shrink_1()
                            .min_h_0()
                            .px_4()
                            .child(components::scrollable_area(
                                "keyboard-help-scrollbar",
                                column()
                                    .id("keyboard-help-scroll")
                                    .size_full()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.help_scroll)
                                    .child(rows),
                                &self.help_scroll,
                            )),
                    )
                    .child(caption(theme, self.tr("shell-keyboard-help-navigation")))
                    .child(
                        row().px_4().flex_wrap().gap_2().children(
                            keyboard::help_rows(&[KeyContext::GuiHelp])
                                .map(|binding| caption(theme, binding.label())),
                        ),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn hidden_tabs_cannot_own_row_commands() {
        let panel = |id, kind: &str| layout::Panel {
            id,
            kind: kind.into(),
            show_tab_bar: true,
            audio_visualizer: None,
            album_artwork: None,
        };
        let node = Node::Tabs {
            id: 1,
            panels: vec![panel(2, "queue"), panel(3, "albums")],
            active: 3,
        };
        assert!(visible_panel(&node, 2).is_none());
        assert_eq!(
            visible_panel(&node, 3).map(|panel| panel.kind.as_str()),
            Some("albums")
        );
    }
}
