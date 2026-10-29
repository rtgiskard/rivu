use crate::{
    focus::dropdown_key,
    list::VirtualList,
    menu::POPOVER_MAX_REMS,
    overlay::text_tooltip,
    row::{DROPDOWN_ROW_REMS, dropdown_row, single_line_row_height},
    surface::Popover,
    theme::palette,
};
use gpui::{prelude::*, *};
use std::cell::Cell;

/// A candidate shown by a dropdown or autocomplete popup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChoiceItem<T> {
    pub value: T,
    pub label: SharedString,
}

impl<T> ChoiceItem<T> {
    pub fn new(value: T, label: impl Into<SharedString>) -> Self {
        Self {
            value,
            label: label.into(),
        }
    }
}

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
            // Handle in capture, before GPUI arms its native keyboard click.
            // Preventing this event's default does not clear an already-armed click.
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
    items: Vec<ChoiceItem<T>>,
    selected: usize,
    open: bool,
    scroll: UniformListScrollHandle,
    revision: u64,
    // The popup sets its explicit row height while rendering. It is independent
    // of candidate count, unlike the scroll handle's measured total contents.
    row_height: Cell<Option<Pixels>>,
}

impl<T> Default for ChoiceState<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            selected: 0,
            open: false,
            scroll: UniformListScrollHandle::new(),
            revision: 0,
            row_height: Cell::new(None),
        }
    }
}

impl<T> ChoiceState<T> {
    pub fn open(&mut self, items: impl IntoIterator<Item = ChoiceItem<T>>, selected: usize) {
        self.revision = self.revision.wrapping_add(1);
        self.items = items.into_iter().collect();
        self.open = !self.items.is_empty();
        self.select_index(selected);
    }

    pub fn close(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.open = false;
        self.selected = 0;
        self.items.clear();
    }
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn selected_index(&self) -> Option<usize> {
        (!self.items.is_empty()).then_some(self.selected)
    }
    pub fn item(&self, index: usize) -> Option<&ChoiceItem<T>> {
        self.items.get(index)
    }

    fn is_current(&self, revision: u64) -> bool {
        self.revision == revision && self.is_open()
    }

    fn accepts(&self, index: usize, revision: u64) -> bool {
        self.is_current(revision) && self.item(index).is_some()
    }

    pub fn select_index(&mut self, index: usize) {
        self.selected = index.min(self.items.len().saturating_sub(1));
        self.scroll_selected();
    }

    fn move_next(&mut self) {
        let count = self.items.len();
        if count > 0 {
            self.selected = (self.selected + 1) % count;
        }
    }

    fn move_previous(&mut self) {
        let count = self.items.len();
        if count > 0 {
            self.selected = self.selected.checked_sub(1).unwrap_or(count - 1);
        }
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
                    "up" => self.move_previous(),
                    "down" => self.move_next(),
                    "pageup" | "pagedown" => {
                        if let Some(index) = self.selected_index() {
                            let height = self.scroll.0.borrow().base_handle.bounds().size.height;
                            let row_height =
                                self.row_height.get().map(f32::from).unwrap_or_else(|| {
                                    DROPDOWN_ROW_REMS * f32::from(window.rem_size())
                                });
                            let visible =
                                ((f32::from(height) / row_height).floor() as usize).max(1);
                            self.selected = (if key == "pagedown" {
                                index.saturating_add(visible)
                            } else {
                                index.saturating_sub(visible)
                            })
                            .min(self.items.len().saturating_sub(1));
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
        let count = choices.items.len();
        let selected = choices.selected_index();
        let revision = choices.revision;
        let row_height = single_line_row_height(window, cx);
        choices.row_height.set(Some(row_height));
        let height = (count as f32 * f32::from(row_height))
            .min(f32::from(window.rem_size()) * POPOVER_MAX_REMS)
            .min(f32::from(window.viewport_size().height) * 0.45);
        let list = VirtualList::new(
            self.id.clone(),
            count,
            &choices.scroll,
            cx.processor(move |owner, range: std::ops::Range<usize>, _window, cx| {
                let choices = state(owner);
                // The list calls this later for measurement and prepaint. Never combine
                // a popup's captured revision/geometry with replacement candidates.
                if !choices.is_current(revision) {
                    return Vec::new();
                }
                range
                    .filter_map(|index| {
                        let item = choices.item(index)?;
                        let label = item.label.clone();
                        let hint = label.clone();
                        let choose = choice_callback(state, choose, index, revision);
                        Some(
                            dropdown_row(
                                palette(cx),
                                ("choice", index),
                                selected == Some(index),
                                label,
                            )
                            .h(row_height)
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
                        if state(owner).is_current(revision) && dismiss(owner, event, window, cx) {
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            // Retained row press state belongs to this candidate set,
                            // not to a replacement row at the same list index.
                            .id(("choice-revision", revision))
                            .h(px(height))
                            .w_full()
                            .child(list),
                    ),
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

    fn with_choice_window(test: impl FnOnce(&Window)) {
        let mut cx = TestAppContext::single();
        let window = cx.add_window(|_, _| PopupChoice {
            choices: ChoiceState::default(),
            committed: Vec::new(),
        });
        cx.update_window(window.into(), |_, window, _| test(window))
            .unwrap();
    }

    fn navigate(state: &mut ChoiceState<u8>, key: &str, window: &Window) {
        state.key(
            &KeyDownEvent {
                keystroke: Keystroke::parse(key).unwrap(),
                is_held: false,
                prefer_character_input: false,
            },
            ChoiceMode::Select,
            false,
            window,
        );
    }

    #[test]
    fn closing_releases_candidates_and_removes_selection() {
        let value = std::rc::Rc::new(());
        let mut state = ChoiceState::default();
        state.open([ChoiceItem::new(value.clone(), "candidate")], 0);
        assert_eq!(std::rc::Rc::strong_count(&value), 2);
        state.close();
        assert_eq!(std::rc::Rc::strong_count(&value), 1);
        assert!(!state.is_open());
        assert_eq!(state.selected_index(), None);
        assert!(state.item(0).is_none());
    }

    #[test]
    fn empty_open_does_not_leave_a_zero_height_popup() {
        with_choice_window(|window| {
            let mut state = ChoiceState::<u8>::default();
            state.open([], 0);
            assert!(!state.is_open());
            assert_eq!(state.selected_index(), None);
            navigate(&mut state, "down", window);
            navigate(&mut state, "up", window);
            state.select_index(usize::MAX);
            assert_eq!(state.selected_index(), None);
            assert!(state.item(0).is_none());
        });
    }

    #[test]
    fn keyboard_selection_wraps_and_clamps() {
        with_choice_window(|window| {
            let mut state = ChoiceState::default();
            state.open(
                [
                    ChoiceItem::new(1, "one"),
                    ChoiceItem::new(2, "two"),
                    ChoiceItem::new(3, "three"),
                ],
                0,
            );
            navigate(&mut state, "up", window);
            assert_eq!(state.selected_index(), Some(2));
            navigate(&mut state, "down", window);
            assert_eq!(state.selected_index(), Some(0));
            state.select_index(usize::MAX);
            assert_eq!(state.selected_index(), Some(2));
        });
    }

    #[test]
    fn replacing_choices_resets_selection_and_empty_choices_close_popup() {
        with_choice_window(|window| {
            let mut state = ChoiceState::default();
            state.open(
                [ChoiceItem::new(10, "ten"), ChoiceItem::new(20, "twenty")],
                1,
            );
            assert_eq!(state.selected_index(), Some(1));
            state.close();
            assert!(!state.is_open());

            state.open([ChoiceItem::new(30, "thirty")], usize::MAX);
            assert!(state.is_open());
            assert_eq!(state.selected_index(), Some(0));
            assert_eq!(state.item(0).map(|item| item.value), Some(30));
            assert!(state.item(1).is_none());
            navigate(&mut state, "down", window);
            navigate(&mut state, "up", window);
            assert_eq!(state.selected_index(), Some(0));

            state.open([], 0);
            assert!(!state.is_open());
            assert_eq!(state.selected_index(), None);
            assert!(state.item(0).is_none());
        });
    }

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
            self.committed.push(self.choices.item(index).unwrap().value);
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
                        |this, _, _, _| {
                            this.choices.close();
                            true
                        },
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
                this.choices.open([ChoiceItem::new(1, "Old")], 0);
                let old = this.callback(0);
                this.choices.close();
                old(this, window, cx);
                assert!(this.committed.is_empty());

                this.choices.open([ChoiceItem::new(2, "Reopened")], 0);
                old(this, window, cx);
                let replaced = this.callback(0);
                this.choices
                    .open([ChoiceItem::new(3, "Three"), ChoiceItem::new(4, "Four")], 0);
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

                this.choices.open([ChoiceItem::new(5, "Five")], 0);
                let emptied = this.callback(0);
                this.choices.open([], 0);
                emptied(this, window, cx);
                assert_eq!(this.committed, [4]);
                this.choices.open([ChoiceItem::new(6, "Six")], 0);
                this.callback(0)(this, window, cx);
                assert_eq!(this.committed, [4, 6]);
            });
        })
        .unwrap();
    }

    #[test]
    fn page_navigation_before_repaint_keeps_the_rendered_row_height() {
        let mut cx = TestAppContext::single();
        cx.update(crate::theme::init_test);
        let window = cx.add_window(|_, _| PopupChoice {
            choices: ChoiceState::default(),
            committed: Vec::new(),
        });
        let choice = window.root(&mut cx).unwrap();
        for font_size in [11, 18] {
            cx.update(|cx| {
                crate::theme::set_typography(font("sans-serif"), "sans-serif", font_size, cx)
            });
            cx.update_window(window.into(), |_, window, cx| {
                choice.update(cx, |this, cx| {
                    this.choices
                        .open((0..100).map(|i| ChoiceItem::new(i, "Old")), 0);
                    cx.notify();
                });
                window.draw(cx).clear(cx);
                let viewport = choice
                    .read(cx)
                    .choices
                    .scroll
                    .0
                    .borrow()
                    .base_handle
                    .bounds()
                    .size
                    .height;
                let page = (f32::from(viewport) / f32::from(single_line_row_height(window, cx)))
                    .floor() as usize;
                assert!(page > 1 && page < 50);
                for count in [50, 200] {
                    choice.update(cx, |this, _| {
                        this.choices
                            .open((0..count).map(|i| ChoiceItem::new(i, "New")), 0);
                        // No frame has measured the replacement count yet.
                        navigate(&mut this.choices, "pagedown", window);
                        assert_eq!(this.choices.selected_index(), Some(page));
                        navigate(&mut this.choices, "pageup", window);
                        assert_eq!(this.choices.selected_index(), Some(0));
                    });
                }
            })
            .unwrap();
        }
    }

    #[test]
    fn painted_outside_dismissal_cannot_close_a_replacement_popup() {
        use gpui::InputEvent as _;
        let mut cx = TestAppContext::single();
        cx.update(crate::theme::init_test);
        let window = cx.add_window(|_, _| PopupChoice {
            choices: ChoiceState::default(),
            committed: Vec::new(),
        });
        let choice = window.root(&mut cx).unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            for close_first in [false, true] {
                choice.update(cx, |this, cx| {
                    this.choices.open([ChoiceItem::new(1, "Old")], 0);
                    cx.notify();
                });
                window.draw(cx).clear(cx);
                choice.update(cx, |this, _| {
                    if close_first {
                        this.choices.close();
                    }
                    this.choices.open([ChoiceItem::new(2, "New")], 0);
                });
                let outside = MouseDownEvent {
                    position: point(px(400.), px(400.)),
                    button: MouseButton::Left,
                    ..Default::default()
                };
                window.dispatch_event(outside.clone().to_platform_input(), cx);
                assert!(choice.read(cx).choices.is_open());
                choice.update(cx, |_, cx| cx.notify());
                window.draw(cx).clear(cx);
                window.dispatch_event(outside.to_platform_input(), cx);
                assert!(!choice.read(cx).choices.is_open());
            }
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
                    this.choices.open([ChoiceItem::new(1, "Old")], 0);
                    cx.notify();
                });
                window.draw(cx).clear(cx);
                let before = choice.read(cx).committed.len();
                choice.update(cx, |this, _| {
                    if close_first {
                        this.choices.close();
                    }
                    if let Some(value) = replacement {
                        this.choices.open([ChoiceItem::new(value, "New")], 0);
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

    #[test]
    fn repainted_popup_press_belongs_to_candidate_revision_not_highlight() {
        use gpui::InputEvent as _;
        let mut cx = TestAppContext::single();
        cx.update(crate::theme::init_test);
        let window = cx.add_window(|_, _| PopupChoice {
            choices: ChoiceState::default(),
            committed: Vec::new(),
        });
        let choice = window.root(&mut cx).unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            let position = point(px(16.), px(40.));
            // Replacement, close/reopen, and highlight-only repaint retain
            // the same pointer position and row index.
            for (close_first, replace) in [(false, true), (true, true), (false, false)] {
                choice.update(cx, |this, cx| {
                    this.choices
                        .open([ChoiceItem::new(1, "Old"), ChoiceItem::new(3, "Other")], 0);
                    cx.notify();
                });
                window.draw(cx).clear(cx);
                let before = choice.read(cx).committed.len();
                window.dispatch_event(
                    MouseDownEvent {
                        button: MouseButton::Left,
                        position,
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
                choice.update(cx, |this, cx| {
                    if close_first {
                        this.choices.close();
                    }
                    if replace {
                        this.choices.open([ChoiceItem::new(2, "New")], 0);
                    } else {
                        this.choices.select_index(1);
                    }
                    cx.notify();
                });
                window.draw(cx).clear(cx);
                window.dispatch_event(
                    MouseMoveEvent {
                        position,
                        pressed_button: Some(MouseButton::Left),
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
                if replace {
                    assert_eq!(choice.read(cx).committed.len(), before);
                    click_first_choice(window, cx);
                    assert_eq!(choice.read(cx).committed.len(), before + 1);
                    assert_eq!(choice.read(cx).committed.last(), Some(&2));
                } else {
                    assert_eq!(choice.read(cx).committed.len(), before + 1);
                    assert_eq!(choice.read(cx).committed.last(), Some(&1));
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
                    this.choices
                        .open([ChoiceItem::new(1, "One"), ChoiceItem::new(2, "Two")], 0);
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
