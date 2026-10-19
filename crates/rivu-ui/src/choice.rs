use crate::{
    dropdown::{DropdownItem, DropdownState},
    focus::dropdown_key,
    list::VirtualList,
    menu::POPOVER_MAX_REMS,
    overlay::text_tooltip,
    row::{DROPDOWN_ROW_REMS, dropdown_row, single_line_row_height},
    surface::Popover,
    theme::palette,
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
    revision: u64,
}

impl<T> Default for ChoiceState<T> {
    fn default() -> Self {
        Self {
            candidates: DropdownState::default(),
            scroll: UniformListScrollHandle::new(),
            revision: 0,
        }
    }
}

impl<T> ChoiceState<T> {
    pub fn open(&mut self, items: impl IntoIterator<Item = DropdownItem<T>>, selected: usize) {
        self.revision = self.revision.wrapping_add(1);
        self.candidates.open(items);
        self.select_index(selected);
    }

    pub fn close(&mut self) {
        self.revision = self.revision.wrapping_add(1);
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

    fn is_current(&self, revision: u64) -> bool {
        self.revision == revision && self.is_open()
    }

    fn accepts(&self, index: usize, revision: u64) -> bool {
        self.is_current(revision) && self.filtered_item(index).is_some()
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
                            let count = self.candidates.filtered().count().max(1);
                            let row_height = self
                                .scroll
                                .0
                                .borrow()
                                .last_item_size
                                .map(|size| f32::from(size.contents.height) / count as f32)
                                .filter(|height| *height > 0.)
                                .unwrap_or(DROPDOWN_ROW_REMS * f32::from(window.rem_size()));
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

fn choice_callback<Owner: 'static, Value: 'static>(
    state: fn(&Owner) -> &ChoiceState<Value>,
    choose: fn(&mut Owner, usize, &mut Window, &mut Context<Owner>),
    index: usize,
    revision: u64,
) -> impl Fn(&mut Owner, &mut Window, &mut Context<Owner>) + Copy {
    move |owner, window, cx| {
        if state(owner).accepts(index, revision) {
            choose(owner, index, window, cx);
        }
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
        let revision = choices.revision;
        let height = (count as f32 * f32::from(single_line_row_height(window, cx)))
            .min(f32::from(window.rem_size()) * POPOVER_MAX_REMS)
            .min(f32::from(window.viewport_size().height) * 0.45);
        let list = VirtualList::new(
            self.id.clone(),
            count,
            &choices.scroll,
            cx.processor(move |owner, range: std::ops::Range<usize>, window, cx| {
                let choices = state(owner);
                // The list calls this later for measurement and prepaint. Never combine
                // a popup's captured revision/geometry with replacement candidates.
                if !choices.is_current(revision) {
                    return Vec::new();
                }
                range
                    .filter_map(|index| {
                        let (_, item) = choices.filtered_item(index)?;
                        let label: SharedString = item.label.clone().into();
                        let hint = label.clone();
                        let choose = choice_callback(state, choose, index, revision);
                        Some(
                            dropdown_row(
                                palette(cx),
                                ("choice", index),
                                selected == Some(index),
                                label,
                            )
                            .h(single_line_row_height(window, cx))
                            .role(Role::ListBoxOption)
                            .aria_label(hint.clone())
                            .aria_selected(selected == Some(index))
                            .aria_position_in_set(index + 1)
                            .aria_size_of_set(count)
                            .on_a11y_action(AccessibleAction::Click, {
                                let owner = cx.entity().downgrade();
                                move |_, window, cx| {
                                    let _ = owner.update(cx, |owner, cx| {
                                        choose(owner, window, cx);
                                    });
                                }
                            })
                            .tooltip(text_tooltip(hint))
                            .on_click(
                                cx.listener(move |owner, _, window, cx| choose(owner, window, cx)),
                            ),
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

    struct PopupChoice {
        choices: ChoiceState<u8>,
        committed: Vec<u8>,
    }

    impl PopupChoice {
        fn choose(&mut self, index: usize, _: &mut Window, cx: &mut Context<Self>) {
            self.committed
                .push(self.choices.filtered_item(index).unwrap().1.value);
            self.choices.close();
            cx.notify();
        }

        fn callback(
            &self,
            index: usize,
        ) -> impl Fn(&mut Self, &mut Window, &mut Context<Self>) + Copy + use<> {
            choice_callback(
                |this| &this.choices,
                Self::choose,
                index,
                self.choices.revision,
            )
        }
    }

    impl Render for PopupChoice {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().when(self.choices.is_open(), |view| {
                view.child(
                    ChoicePopup::new(
                        "popup",
                        "popup-scrollbar",
                        Bounds::new(point(px(0.), px(0.)), size(px(200.), px(20.))),
                    )
                    .render(
                        self,
                        |this| &this.choices,
                        Self::choose,
                        |_, _, _, _| false,
                        window,
                        cx,
                    ),
                )
            })
        }
    }

    #[test]
    fn choice_callbacks_commit_only_current_candidate_values() {
        let mut cx = TestAppContext::single();
        cx.update(crate::theme::init_test);
        let window = cx.add_window(|_, _| PopupChoice {
            choices: ChoiceState::default(),
            committed: Vec::new(),
        });
        let choice = window.root(&mut cx).unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            choice.update(cx, |this, cx| {
                this.choices.open([DropdownItem::new(1, "Old")], 0);
                let old = this.callback(0);
                this.choices.close();
                old(this, window, cx);
                assert!(this.committed.is_empty());

                this.choices.open([DropdownItem::new(2, "Reopened")], 0);
                old(this, window, cx);
                let replaced = this.callback(0);
                this.choices.open(
                    [DropdownItem::new(3, "Three"), DropdownItem::new(4, "Four")],
                    0,
                );
                replaced(this, window, cx);
                this.callback(2)(this, window, cx);
                assert!(this.committed.is_empty());
                assert_eq!(this.choices.selected_index(), Some(0));

                let current = this.callback(1);
                let navigation = this.choices.key(
                    &KeyDownEvent {
                        keystroke: Keystroke::parse("down").unwrap(),
                        is_held: false,
                        prefer_character_input: false,
                    },
                    ChoiceMode::Select,
                    false,
                    window,
                );
                assert_eq!(navigation.action, ChoiceAction::Navigated);
                assert_eq!(this.choices.selected_index(), Some(1));
                this.choices.select_index(0);
                current(this, window, cx);
                current(this, window, cx);
                assert_eq!(this.committed, [4]);
                assert!(!this.choices.is_open());

                this.choices.open([DropdownItem::new(5, "Five")], 0);
                let emptied = this.callback(0);
                this.choices.open([], 0);
                emptied(this, window, cx);
                assert_eq!(this.committed, [4]);
                this.choices.open([DropdownItem::new(6, "Six")], 0);
                this.callback(0)(this, window, cx);
                assert_eq!(this.committed, [4, 6]);
            });
        })
        .unwrap();
    }

    fn click_first_choice(window: &mut Window, cx: &mut App) {
        use gpui::InputEvent as _;
        let position = point(px(16.), px(40.));
        window.dispatch_event(
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
    }

    #[test]
    fn painted_pointer_callbacks_do_not_commit_after_choice_lifecycle_changes() {
        let mut cx = TestAppContext::single();
        cx.update(crate::theme::init_test);
        let window = cx.add_window(|_, _| PopupChoice {
            choices: ChoiceState::default(),
            committed: Vec::new(),
        });
        let choice = window.root(&mut cx).unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            for (close_first, replacement) in [
                (true, None),
                (true, Some(2)),
                (false, Some(3)),
                (false, None),
            ] {
                choice.update(cx, |this, cx| {
                    this.choices.open([DropdownItem::new(1, "Old")], 0);
                    cx.notify();
                });
                window.draw(cx).clear(cx);
                let before = choice.read(cx).committed.len();
                choice.update(cx, |this, _| {
                    if close_first {
                        this.choices.close();
                    }
                    if let Some(value) = replacement {
                        this.choices.open([DropdownItem::new(value, "New")], 0);
                    } else if !close_first {
                        this.choices.open([], 0);
                    }
                });
                // Keep the painted listeners: the owner changed before its next frame.
                click_first_choice(window, cx);
                assert_eq!(choice.read(cx).committed.len(), before);

                choice.update(cx, |_, cx| cx.notify());
                window.draw(cx).clear(cx);
                if let Some(value) = replacement {
                    click_first_choice(window, cx);
                    assert_eq!(choice.read(cx).committed.len(), before + 1);
                    assert_eq!(choice.read(cx).committed.last(), Some(&value));
                }
            }
        })
        .unwrap();
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
                crate::dropdown_trigger(
                    palette(cx),
                    "trigger",
                    "Choose",
                    self.choices.is_open(),
                    cx,
                ),
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
