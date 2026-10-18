use crate::{
    ButtonTooltip, DROPDOWN_ROW_REMS, DropdownItem, DropdownState, POPOVER_MAX_REMS, VirtualList,
    dropdown_key, dropdown_row, surface::Popover, theme::palette,
};
use gpui::{prelude::*, *};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceMode {
    Select,
    Editable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceAction {
    None,
    Open,
    Navigated,
    Commit(usize),
    Dismiss,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChoiceDisposition {
    pub action: ChoiceAction,
    pub stop_propagation: bool,
    pub prevent_default: bool,
}

impl ChoiceDisposition {
    pub fn apply(self, window: &mut Window, cx: &mut App) -> ChoiceAction {
        if self.prevent_default {
            // Clear GPUI's pending native keyboard click before key-up can reopen a trigger.
            window.prevent_default();
        }
        if self.stop_propagation {
            cx.stop_propagation();
        }
        self.action
    }
}

fn key_action(open: bool, mode: ChoiceMode, composing: bool, key: &str) -> ChoiceDisposition {
    let action = if open && key == "tab" {
        ChoiceAction::Dismiss
    } else if composing {
        ChoiceAction::None
    } else if !open {
        if mode == ChoiceMode::Editable && matches!(key, "up" | "down") {
            ChoiceAction::Open
        } else {
            ChoiceAction::None
        }
    } else {
        match key {
            "tab" | "escape" => ChoiceAction::Dismiss,
            "up" | "down" | "pageup" | "pagedown" => ChoiceAction::Navigated,
            "enter" | "space" if mode == ChoiceMode::Select => ChoiceAction::Commit(0),
            _ => ChoiceAction::None,
        }
    };
    let handled = action != ChoiceAction::None && key != "tab";
    ChoiceDisposition {
        action,
        stop_propagation: handled,
        prevent_default: handled && mode == ChoiceMode::Select,
    }
}

/// Candidate and scroll state only. Owners retain filtering, values and commit policy.
pub struct ChoiceState<T> {
    candidates: DropdownState<T>,
    scroll: UniformListScrollHandle,
}

impl<T> Default for ChoiceState<T> {
    fn default() -> Self {
        Self {
            candidates: DropdownState::default(),
            scroll: UniformListScrollHandle::new(),
        }
    }
}

impl<T> ChoiceState<T> {
    pub fn open(&mut self, items: impl IntoIterator<Item = DropdownItem<T>>, selected: usize) {
        self.candidates.open(items);
        self.select_index(selected);
    }

    pub fn close(&mut self) {
        self.candidates.close();
    }
    pub fn is_open(&self) -> bool {
        self.candidates.is_open()
    }
    pub fn selected_index(&self) -> Option<usize> {
        self.candidates.selected_index()
    }
    pub fn filtered_item(&self, index: usize) -> Option<(usize, &DropdownItem<T>)> {
        self.candidates.filtered_item(index)
    }

    pub fn select_index(&mut self, index: usize) {
        self.candidates.select_index(index);
        self.scroll_selected();
    }

    fn scroll_selected(&self) {
        if let Some(index) = self.selected_index() {
            self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        }
    }

    pub fn key(
        &mut self,
        event: &KeyDownEvent,
        mode: ChoiceMode,
        composing: bool,
        window: &Window,
    ) -> ChoiceDisposition {
        let Some(key) = dropdown_key(event) else {
            return key_action(false, ChoiceMode::Select, false, "");
        };
        let mut disposition = key_action(self.is_open(), mode, composing, key);
        match disposition.action {
            ChoiceAction::Navigated => {
                match key {
                    "up" => self.candidates.move_previous(),
                    "down" => self.candidates.move_next(),
                    "pageup" | "pagedown" => {
                        if let Some(index) = self.selected_index() {
                            let height = self.scroll.0.borrow().base_handle.bounds().size.height;
                            let row_height = DROPDOWN_ROW_REMS * f32::from(window.rem_size());
                            let visible =
                                ((f32::from(height) / row_height).floor() as usize).max(1);
                            self.candidates.select_index(if key == "pagedown" {
                                index.saturating_add(visible)
                            } else {
                                index.saturating_sub(visible)
                            });
                        }
                    }
                    _ => unreachable!(),
                }
                self.scroll_selected();
            }
            ChoiceAction::Commit(_) => {
                disposition.action = self
                    .selected_index()
                    .map_or(ChoiceAction::None, ChoiceAction::Commit);
            }
            _ => {}
        }
        disposition
    }
}

/// Anchored, viewport-bounded candidate popup. Outside pointer events request dismissal;
/// the owner can reject it (for example over an editable anchor) without changing focus.
pub struct ChoicePopup {
    id: ElementId,
    scrollbar_id: ElementId,
    anchor: Bounds<Pixels>,
}

impl ChoicePopup {
    pub fn new(
        id: impl Into<ElementId>,
        scrollbar_id: impl Into<ElementId>,
        anchor: Bounds<Pixels>,
    ) -> Self {
        Self {
            id: id.into(),
            scrollbar_id: scrollbar_id.into(),
            anchor,
        }
    }

    pub fn render<Owner: 'static, Value: 'static>(
        self,
        owner: &Owner,
        state: fn(&Owner) -> &ChoiceState<Value>,
        choose: fn(&mut Owner, usize, &mut Window, &mut Context<Owner>),
        dismiss: fn(&mut Owner, &MouseDownEvent, &mut Window, &mut Context<Owner>) -> bool,
        window: &Window,
        cx: &mut Context<Owner>,
    ) -> AnyElement {
        let choices = state(owner);
        let count = choices.candidates.filtered().count();
        let selected = choices.selected_index();
        let height = (count as f32 * DROPDOWN_ROW_REMS * f32::from(window.rem_size()))
            .min(f32::from(window.rem_size()) * POPOVER_MAX_REMS)
            .min(f32::from(window.viewport_size().height) * 0.45);
        let list = VirtualList::new(
            self.id.clone(),
            count,
            &choices.scroll,
            cx.processor(move |owner, range: std::ops::Range<usize>, _, cx| {
                range
                    .filter_map(|index| {
                        let (_, item) = state(owner).filtered_item(index)?;
                        let label: SharedString = item.label.clone().into();
                        let hint = label.clone();
                        Some(
                            dropdown_row(
                                palette(cx),
                                ("choice", index),
                                selected == Some(index),
                                label,
                            )
                            .tooltip(move |_, cx| {
                                cx.new(|_| ButtonTooltip { text: hint.clone() }).into()
                            })
                            .on_click(cx.listener(
                                move |owner, _, window, cx| choose(owner, index, window, cx),
                            )),
                        )
                    })
                    .collect()
            }),
        )
        .h_full()
        .w_full()
        .scrollbar(self.scrollbar_id);
        deferred(
            anchored()
                .position(self.anchor.bottom_left())
                .snap_to_window()
                .child(
                    Popover::choices(
                        palette(cx),
                        self.id,
                        self.anchor.size.width.min(window.viewport_size().width),
                    )
                    .max_h(window.viewport_size().height)
                    .overflow_hidden()
                    .on_mouse_down_out(cx.listener(move |owner, event, window, cx| {
                        if dismiss(owner, event, window, cx) {
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .child(div().h(px(height)).w_full().child(list)),
                ),
        )
        .with_priority(2)
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn selection_consumes_native_click_but_tab_keeps_focus_traversal() {
        for key in [
            "enter", "space", "escape", "up", "down", "pageup", "pagedown",
        ] {
            let disposition = key_action(true, ChoiceMode::Select, false, key);
            assert!(disposition.prevent_default);
            assert!(disposition.stop_propagation);
        }
        let tab = key_action(true, ChoiceMode::Select, false, "tab");
        assert_eq!(tab.action, ChoiceAction::Dismiss);
        assert!(!tab.prevent_default && !tab.stop_propagation);
    }

    #[test]
    fn editable_submission_and_composition_belong_to_input() {
        for key in ["enter", "space"] {
            assert_eq!(
                key_action(true, ChoiceMode::Editable, false, key).action,
                ChoiceAction::None
            );
        }
        for key in ["enter", "escape", "up", "down"] {
            assert_eq!(
                key_action(true, ChoiceMode::Editable, true, key).action,
                ChoiceAction::None
            );
        }
        assert_eq!(
            key_action(false, ChoiceMode::Editable, false, "down").action,
            ChoiceAction::Open
        );
        assert_eq!(
            key_action(true, ChoiceMode::Editable, false, "escape").action,
            ChoiceAction::Dismiss
        );
        let tab = key_action(true, ChoiceMode::Editable, true, "tab");
        assert_eq!(tab.action, ChoiceAction::Dismiss);
        assert!(!tab.stop_propagation && !tab.prevent_default);
    }

    struct NativeChoice {
        scope: crate::FocusScope,
        choices: ChoiceState<u8>,
        opens: usize,
        committed: Option<usize>,
    }

    impl Render for NativeChoice {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let trigger = crate::activate_control(
                crate::dropdown_trigger(palette(cx), "trigger", "Choose", cx),
                cx,
                |this, _, cx| {
                    this.opens += 1;
                    this.choices.open(
                        [DropdownItem::new(1, "One"), DropdownItem::new(2, "Two")],
                        0,
                    );
                    cx.notify();
                },
            );
            self.scope
                .surface(div().id("choice-scope"))
                .capture_key_down(cx.listener(|this, event, window, cx| {
                    let disposition = this.choices.key(event, ChoiceMode::Select, false, window);
                    match disposition.apply(window, cx) {
                        ChoiceAction::Commit(index) => {
                            this.committed = Some(index);
                            this.choices.close();
                        }
                        ChoiceAction::Dismiss => this.choices.close(),
                        _ => {}
                    }
                    cx.notify();
                }))
                .child(trigger)
        }
    }

    #[test]
    fn selecting_on_native_trigger_does_not_reopen_on_key_up() {
        use gpui::InputEvent as _;
        let mut cx = TestAppContext::single();
        cx.update(crate::theme::init_test);
        let scope = cx.update(crate::FocusScope::new);
        let window = cx.add_window({
            let scope = scope.clone();
            move |_, _| NativeChoice {
                scope,
                choices: ChoiceState::default(),
                opens: 0,
                committed: None,
            }
        });
        let handle: AnyWindowHandle = window.into();
        cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
            scope.handle().focus(window, cx);
            scope.advance(false, window, cx);
            window.draw(cx).clear(cx);
        })
        .unwrap();
        for key in ["enter", "down", "enter"] {
            cx.update_window(handle, |_, window, cx| {
                let keystroke = Keystroke::parse(key).unwrap();
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
                window.draw(cx).clear(cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
        cx.update_window(handle, |root, _, cx| {
            let choice = root.downcast::<NativeChoice>().unwrap();
            let choice = choice.read(cx);
            assert_eq!(choice.opens, 1);
            assert_eq!(choice.committed, Some(1));
            assert!(!choice.choices.is_open());
        })
        .unwrap();
    }
}
