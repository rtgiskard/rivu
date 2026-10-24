//! Reusable single-line text input for GPUI.
//!
//! The text/IME implementation is adapted from Zed's GPUI input example at
//! <https://github.com/zed-industries/zed/blob/a84689073d296dfd39987bc7dd478e43ef76d83a/crates/gpui/examples/input.rs>.
//! That example and this adaptation are distributed under the Apache License,
//! Version 2.0.
//!
//! Mouse selection remains owned by its originating input outside the textbox
//! until release, window exit, focus loss, or disabling the input.

use std::{cell::RefCell, ops::Range, rc::Rc};

use gpui::AccessibleAction;
use gpui::accesskit::{
    ActionData, Node, NodeId, Rect, Role, TextDirection, TextPosition, TextSelection,
};
use gpui::prelude::*;
use gpui::{
    App, ClipboardItem, Context, CursorStyle, DispatchPhase, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    GlobalElementId, IntoElement, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseExitEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine,
    SharedString, Style, Subscription, TextRun, UTF16Selection, UnderlineStyle, Window, actions,
    div, fill, point, px, relative, rgb, size,
};
use unicode_segmentation::UnicodeSegmentation;

const INPUT_LINE_HEIGHT: f32 = 20.;
const INPUT_VERTICAL_PADDING: f32 = 7.;

/// Align a bordered control with a standalone [`Input`] at every UI scale.
/// The existing border stays unscaled; embedded inputs retain their parent geometry.
pub fn input_aligned_control(
    control: gpui::Stateful<gpui::Div>,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let scale = crate::theme::font_size_scale(cx);
    control
        .h_auto()
        .min_h_0()
        .font_size_scale(1.)
        .text_size(gpui::rems(14. * scale / 16.))
        .line_height(gpui::rems(INPUT_LINE_HEIGHT * scale / 16.))
        .py(gpui::rems(INPUT_VERTICAL_PADDING / 16.))
}

actions!(
    input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        SelectHome,
        SelectEnd,
        Paste,
        Cut,
        Copy,
        SubmitInput,
    ]
);

/// Notifications emitted by [`Input`] after editing or programmatic updates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// `user` distinguishes actual editing from [`Input::set_text`] updates.
    Changed { user: bool },
    /// The user pressed Enter while this input was focused.
    Submit,
}

/// A focused, editable, single-line text field.
///
/// Text offsets used internally are UTF-8 byte offsets, while GPUI's IME
/// protocol is UTF-16 based. Conversion is kept at the handler boundary so
/// editing and grapheme navigation remain safe for all Unicode text.
/// Accessibility exposes the explicit name independently of the placeholder,
/// the current value, and one synthetic text run. Its character positions use
/// Unicode scalar indices (matching the smallest selectable IME text unit),
/// converted to UTF-8 only at the action boundary. Selection preserves marked
/// text; replacement follows normal user editing and emits `Changed { user: true }`.
/// Disabled inputs expose their state but reject editing and focus actions.
pub struct Input {
    focus_handle: FocusHandle,
    enabled: bool,
    name: SharedString,
    accessibility: Rc<RefCell<InputAccessibility>>,
    content: SharedString,
    placeholder: SharedString,
    trailing_space: Pixels,
    trailing_text: Option<SharedString>,
    embedded: bool,
    control_surface: bool,
    scroll_offset: Pixels,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<gpui::Bounds<Pixels>>,
    selection_subscriptions: Option<[Subscription; 2]>,
}

impl EventEmitter<InputEvent> for Input {}

impl Input {
    /// Creates an input with an explicit accessible name, initial value and placeholder.
    pub fn new(
        name: impl Into<SharedString>,
        value: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let content = value.into();
        let end = content.len();
        Self {
            focus_handle: cx.focus_handle().tab_stop(true),
            enabled: true,
            name: name.into(),
            accessibility: Rc::default(),
            content,
            placeholder: placeholder.into(),
            trailing_space: px(7.),
            trailing_text: None,
            embedded: false,
            control_surface: false,
            scroll_offset: px(0.),
            selected_range: end..end,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            selection_subscriptions: None,
        }
    }

    /// Reserve room for an overlaid trailing control without overlapping text.
    pub fn with_trailing_space(mut self, space: Pixels) -> Self {
        self.trailing_space = space;
        self
    }

    /// Match select-trigger colors for an editable combo without changing input geometry.
    pub fn with_control_surface(mut self) -> Self {
        self.control_surface = true;
        self
    }

    /// Fill a containing compound control's allocation without adding a border.
    ///
    /// The parent owns the background, border, and height; the text line is
    /// vertically centered without the standalone input's vertical padding.
    pub fn embedded(mut self) -> Self {
        self.embedded = true;
        self
    }

    pub fn set_trailing_text(&mut self, text: SharedString, cx: &mut Context<Self>) {
        if self.trailing_text.as_ref() != Some(&text) {
            self.trailing_text = Some(text);
            cx.notify();
        }
    }

    /// Installs the key bindings used by every [`Input`] instance.
    pub fn init(cx: &mut App) {
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, Some("Input")),
            KeyBinding::new("delete", Delete, Some("Input")),
            KeyBinding::new("left", Left, Some("Input")),
            KeyBinding::new("right", Right, Some("Input")),
            KeyBinding::new("shift-left", SelectLeft, Some("Input")),
            KeyBinding::new("shift-right", SelectRight, Some("Input")),
            KeyBinding::new("home", Home, Some("Input")),
            KeyBinding::new("end", End, Some("Input")),
            KeyBinding::new("shift-home", SelectHome, Some("Input")),
            KeyBinding::new("shift-end", SelectEnd, Some("Input")),
            KeyBinding::new("enter", SubmitInput, Some("Input")),
            KeyBinding::new("ctrl-a", SelectAll, Some("Input")),
            KeyBinding::new("ctrl-c", Copy, Some("Input")),
            KeyBinding::new("ctrl-v", Paste, Some("Input")),
            KeyBinding::new("ctrl-x", Cut, Some("Input")),
        ]);
    }

    /// Disabled inputs retain their entity and value but reject user focus and editing.
    pub fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        self.focus_handle = self.focus_handle.clone().tab_stop(enabled);
        self.selection_subscriptions = None;
        self.marked_range = None;
        cx.notify();
    }

    /// Returns whether this input currently owns window focus.
    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    pub fn is_composing(&self) -> bool {
        self.marked_range.is_some()
    }

    pub fn focus_and_select_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    /// Cancel the current preedit without clearing committed input text.
    pub fn dismiss_composition(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(marked) = self.marked_range.take() else {
            return false;
        };
        self.content = format!(
            "{}{}",
            &self.content[..marked.start],
            &self.content[marked.end..]
        )
        .into();
        self.selected_range = marked.start..marked.start;
        self.selection_reversed = false;
        self.last_layout = None;
        self.last_bounds = None;
        cx.emit(InputEvent::Changed { user: true });
        cx.notify();
        true
    }

    /// Returns the current value.
    pub fn text(&self) -> &str {
        &self.content
    }

    /// Replaces the value and places the caret at its end.
    pub fn set_text(&mut self, value: impl Into<SharedString>, cx: &mut Context<Self>) {
        let value = value.into();
        if self.content == value {
            return;
        }
        let end = value.len();
        self.content = value;
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.scroll_offset = px(0.);
        self.last_layout = None;
        self.last_bounds = None;
        cx.emit(InputEvent::Changed { user: false });
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        if self.selected_range.is_empty() {
            let previous = self.previous_boundary(self.cursor_offset());
            if previous == self.cursor_offset() {
                window.play_system_bell();
                return;
            }
            self.select_to(previous, cx);
        }
        self.replace_selection(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if next == self.cursor_offset() {
                window.play_system_bell();
                return;
            }
            self.select_to(next, cx);
        }
        self.replace_selection(None, "", window, cx);
    }

    fn submit(&mut self, _: &SubmitInput, _: &mut Window, cx: &mut Context<Self>) {
        if self.enabled {
            cx.emit(InputEvent::Submit);
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            return;
        }
        self.focus_handle.focus(window, cx);
        self.selection_subscriptions = Some([
            cx.on_blur(&self.focus_handle, window, |this, _, _| {
                this.selection_subscriptions = None;
            }),
            cx.observe_window_activation(window, |this, window, _| {
                if !window.is_window_active() {
                    this.selection_subscriptions = None;
                }
            }),
        ]);
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left || self.selection_subscriptions.is_none() {
            return;
        }
        self.selection_subscriptions = None;
        if self.enabled && self.is_focused(window) {
            self.select_to(self.index_for_mouse_position(event.position), cx);
            cx.stop_propagation();
        }
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selection_subscriptions.is_none() {
            return;
        }
        if !self.enabled || !self.is_focused(window) || !event.dragging() {
            self.selection_subscriptions = None;
            return;
        }
        self.select_to(self.index_for_mouse_position(event.position), cx);
        cx.stop_propagation();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_selection(None, &text.replace('\n', " "), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_selection(None, "", window, cx);
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left() + self.scroll_offset)
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.content.len())
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        utf16_offset_from_utf8(&self.content, offset)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        utf16_range_from_utf8(&self.content, range)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        utf8_range_from_utf16(&self.content, range)
    }

    fn replace_range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.range_from_utf16(range)
    }

    fn replace_selection(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|range| self.replace_range_from_utf16(range))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.content =
            (self.content[..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.selection_reversed = false;
        self.marked_range = None;
        self.last_layout = None;
        self.last_bounds = None;
        cx.emit(InputEvent::Changed { user: true });
        cx.notify();
    }

    fn accessible_edit(
        &mut self,
        data: Option<&ActionData>,
        replace_all: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            return;
        }
        let Some(ActionData::Value(value)) = data else {
            return;
        };
        let range = replace_all.then(|| 0..self.offset_to_utf16(self.content.len()));
        // Use the same user-edit path as typing/paste, including marked-text replacement.
        if value.contains(['\r', '\n']) {
            self.replace_selection(range, &value.replace(['\r', '\n'], " "), window, cx);
        } else {
            self.replace_selection(range, value, window, cx);
        }
    }

    fn accessible_selection(&mut self, data: Option<&ActionData>, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        let Some(ActionData::SetTextSelection(selection)) = data else {
            return;
        };
        let run_id = self.accessibility.borrow().run_id;
        if Some(selection.anchor.node) != run_id || Some(selection.focus.node) != run_id {
            return;
        }
        let Some(anchor) =
            utf8_offset_from_character(&self.content, selection.anchor.character_index)
        else {
            return;
        };
        let Some(focus) =
            utf8_offset_from_character(&self.content, selection.focus.character_index)
        else {
            return;
        };
        // Selection changes must not commit or cancel an in-progress IME composition.
        self.move_to(anchor, cx);
        self.select_to(focus, cx);
    }
}

const CARET_WIDTH: f32 = 1.5;

fn scroll_to_caret(previous: Pixels, caret: Pixels, width: Pixels, line_width: Pixels) -> Pixels {
    let visible_width = (width - px(CARET_WIDTH)).max(px(0.));
    let max_scroll = (line_width - visible_width).max(px(0.));
    previous
        .max(caret - visible_width)
        .min(caret)
        .clamp(px(0.), max_scroll)
}

#[derive(Default)]
struct InputAccessibility {
    run_id: Option<NodeId>,
    run: Option<Node>,
}

fn utf8_offset_from_character(text: &str, index: usize) -> Option<usize> {
    text.char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(text.len()))
        .nth(index)
}

fn character_index_from_utf8(text: &str, offset: usize) -> usize {
    text.char_indices()
        .take_while(|(index, _)| *index < offset)
        .count()
}

fn utf8_offset_from_utf16(text: &str, offset: usize) -> usize {
    let mut utf8 = 0;
    let mut utf16 = 0;
    for ch in text.chars() {
        let next = utf16 + ch.len_utf16();
        if next > offset {
            break;
        }
        utf16 = next;
        utf8 += ch.len_utf8();
    }
    utf8
}

fn utf16_offset_from_utf8(text: &str, offset: usize) -> usize {
    let mut utf8 = 0;
    let mut utf16 = 0;
    for ch in text.chars() {
        let next = utf8 + ch.len_utf8();
        if next > offset {
            break;
        }
        utf8 = next;
        utf16 += ch.len_utf16();
    }
    utf16
}

fn utf8_range_from_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    let start = utf8_offset_from_utf16(text, range.start);
    let end = utf8_offset_from_utf16(text, range.end);
    start.min(end)..start.max(end)
}

fn utf16_range_from_utf8(text: &str, range: &Range<usize>) -> Range<usize> {
    let start = utf16_offset_from_utf8(text, range.start);
    let end = utf16_offset_from_utf8(text, range.end);
    start.min(end)..start.max(end)
}
fn selected_range_after_replacement(
    new_text: &str,
    selected_range_utf16: Option<&Range<usize>>,
    replacement_start: usize,
) -> Range<usize> {
    selected_range_utf16
        .map(|selected| utf8_range_from_utf16(new_text, selected))
        .map(|selected| replacement_start + selected.start..replacement_start + selected.end)
        .unwrap_or_else(|| replacement_start + new_text.len()..replacement_start + new_text.len())
}

impl EntityInputHandler for Input {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_selection(range_utf16, new_text, window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.content =
            (self.content[..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.marked_range =
            (!new_text.is_empty()).then_some(range.start..range.start + new_text.len());
        self.selected_range = selected_range_after_replacement(
            new_text,
            new_selected_range_utf16.as_ref(),
            range.start,
        );
        self.selection_reversed = false;
        self.last_layout = None;
        self.last_bounds = None;
        cx.emit(InputEvent::Changed { user: true });
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: gpui::Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<gpui::Bounds<Pixels>> {
        let line = self.last_layout.as_ref()?;
        let bounds = self.last_bounds?;
        let origin = bounds.left() - self.scroll_offset;
        let range = self.range_from_utf16(&range_utf16);
        Some(gpui::Bounds::from_corners(
            point(origin + line.x_for_index(range.start), bounds.top()),
            point(origin + line.x_for_index(range.end), bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let line = self.last_layout.as_ref()?;
        let utf8_index = line.index_for_x(line_point.x + self.scroll_offset)?;
        Some(self.offset_to_utf16(utf8_index))
    }
}

struct InputElement {
    input: Entity<Input>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    scroll_offset: Pixels,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for InputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for InputElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let theme = crate::theme::palette(cx);
        let input = self.input.read(cx);
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor_offset = input.cursor_offset();
        let style = window.text_style();
        let (display_text, text_color) = if content.is_empty() {
            (input.placeholder.clone(), rgb(theme.muted).into())
        } else {
            (content, style.color)
        };
        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked) = input.marked_range.as_ref() {
            vec![
                TextRun {
                    len: marked.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - marked.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };
        let font_size = style.font_size_in_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &runs, None);
        let cursor_pos = line.x_for_index(cursor_offset);
        let scroll_offset = scroll_to_caret(
            input.scroll_offset,
            cursor_pos,
            bounds.size.width,
            if input.content.is_empty() {
                px(0.)
            } else {
                line.width()
            },
        );
        if window.is_a11y_active() {
            let scale = window.scale_factor();
            let mut run = Node::new(Role::TextRun);
            run.set_value(input.content.to_string());
            run.set_text_direction(TextDirection::LeftToRight);
            run.set_character_lengths(
                input
                    .content
                    .chars()
                    .map(|ch| ch.len_utf8() as u8)
                    .collect::<Vec<_>>(),
            );
            let mut positions = Vec::with_capacity(input.content.chars().count());
            let mut widths = Vec::with_capacity(positions.capacity());
            for (offset, ch) in input.content.char_indices() {
                let start = line.x_for_index(offset);
                let end = line.x_for_index(offset + ch.len_utf8());
                positions.push(f32::from(start.min(end)) * scale);
                widths.push(f32::from((end - start).abs()) * scale);
            }
            run.set_character_positions(positions);
            run.set_character_widths(widths);
            let origin = bounds.left() - scroll_offset;
            run.set_bounds(Rect {
                x0: f64::from(f32::from(origin) * scale),
                y0: f64::from(f32::from(bounds.top()) * scale),
                x1: f64::from(
                    f32::from(
                        origin
                            + if input.content.is_empty() {
                                px(0.)
                            } else {
                                line.width()
                            },
                    ) * scale,
                ),
                y1: f64::from(f32::from(bounds.bottom()) * scale),
            });
            input.accessibility.borrow_mut().run = Some(run);
        }
        let origin = bounds.left() - scroll_offset;
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    gpui::Bounds::new(
                        point(origin + cursor_pos, bounds.top()),
                        size(px(CARET_WIDTH), bounds.bottom() - bounds.top()),
                    ),
                    rgb(theme.accent),
                )),
            )
        } else {
            (
                Some(fill(
                    gpui::Bounds::from_corners(
                        point(
                            origin + line.x_for_index(selected_range.start),
                            bounds.top(),
                        ),
                        point(
                            origin + line.x_for_index(selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    rgb(theme.accent).alpha(0.28),
                )),
                None,
            )
        };
        PrepaintState {
            line: Some(line),
            scroll_offset,
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        let enabled = self.input.read(cx).enabled;
        if enabled {
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.input.clone()),
                cx,
            );
        }
        let line = prepaint.line.take().expect("input text line is prepared");
        let origin = point(bounds.left() - prepaint.scroll_offset, bounds.top());
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            if enabled && let Some(selection) = prepaint.selection.take() {
                window.paint_quad(selection);
            }
            line.paint(
                origin,
                window.line_height(),
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            )
            .ok();
            if enabled
                && focus_handle.is_focused(window)
                && let Some(cursor) = prepaint.cursor.take()
            {
                window.paint_quad(cursor);
            }
        });
        self.input.update(cx, |input, _| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
            input.scroll_offset = prepaint.scroll_offset;
        });
        if !enabled {
            return;
        }

        // Selection belongs to the input where the press began, not the hovered field.
        let input = self.input.downgrade();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture {
                let _ = input.update(cx, |input, cx| input.on_mouse_move(event, window, cx));
            }
        });
        let input = self.input.downgrade();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture {
                let _ = input.update(cx, |input, cx| input.on_mouse_up(event, window, cx));
            }
        });
        let input = self.input.downgrade();
        window.on_mouse_event(move |_: &MouseExitEvent, phase, _, cx| {
            if phase == DispatchPhase::Capture {
                let _ = input.update(cx, |input, _| input.selection_subscriptions = None);
            }
        });
    }
}

impl Render for Input {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::palette(cx);
        let focused = self.enabled && self.focus_handle.is_focused(window);
        let scale = window.rem_size() / px(16.);
        let typography_scale = crate::theme::font_size_scale(cx);
        let accessibility = self.accessibility.clone();
        let placeholder = self.placeholder.clone();
        let content = self.content.clone();
        let enabled = self.enabled;
        let anchor = if self.selection_reversed {
            self.selected_range.end
        } else {
            self.selected_range.start
        };
        let focus = self.cursor_offset();
        div()
            .id("input")
            .role(Role::TextInput)
            .aria_label(self.name.clone())
            .aria_value(self.content.clone())
            .a11y_synthetic_children(move |builder| {
                let mut accessibility = accessibility.borrow_mut();
                let run_id = builder.synthetic_node_id("text");
                accessibility.run_id = Some(run_id);
                if let Some(run) = accessibility.run.take() {
                    builder.push_child(run_id, run);
                    builder.parent_node().set_text_selection(TextSelection {
                        anchor: TextPosition {
                            node: run_id,
                            character_index: character_index_from_utf8(&content, anchor),
                        },
                        focus: TextPosition {
                            node: run_id,
                            character_index: character_index_from_utf8(&content, focus),
                        },
                    });
                }
                let node = builder.parent_node();
                node.set_placeholder(placeholder.to_string());
                if !enabled {
                    node.set_disabled();
                    node.remove_action(AccessibleAction::Focus);
                }
            })
            .when(self.enabled, |view| {
                view.on_a11y_action(AccessibleAction::Focus, {
                    let input = cx.entity().downgrade();
                    move |_, window, cx| {
                        let _ = input.update(cx, |input, cx| {
                            if input.enabled {
                                input.focus_handle.focus(window, cx);
                            }
                        });
                    }
                })
                .on_a11y_action(AccessibleAction::SetValue, {
                    let input = cx.entity().downgrade();
                    move |data, window, cx| {
                        let _ = input.update(cx, |input, cx| {
                            input.accessible_edit(data, true, window, cx)
                        });
                    }
                })
                .on_a11y_action(AccessibleAction::ReplaceSelectedText, {
                    let input = cx.entity().downgrade();
                    move |data, window, cx| {
                        let _ = input.update(cx, |input, cx| {
                            input.accessible_edit(data, false, window, cx)
                        });
                    }
                })
                .on_a11y_action(AccessibleAction::SetTextSelection, {
                    let input = cx.entity().downgrade();
                    move |data, _, cx| {
                        let _ = input.update(cx, |input, cx| input.accessible_selection(data, cx));
                    }
                })
            })
            .flex()
            .key_context("Input")
            .when(self.enabled, |view| {
                view.track_focus(&self.focus_handle(cx)).tab_index(0)
            })
            .cursor(if self.enabled {
                CursorStyle::IBeam
            } else {
                CursorStyle::Arrow
            })
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::submit))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape"
                    && event.keystroke.modifiers == gpui::Modifiers::default()
                    && this.dismiss_composition(cx)
                {
                    cx.stop_propagation();
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .when(!self.embedded, |view| {
                view.bg(rgb(if self.control_surface {
                    theme.control
                } else {
                    theme.input
                }))
                .border_1()
                .border_color(if focused {
                    rgb(theme.accent)
                } else {
                    rgb(theme.border)
                })
                .rounded(px(if self.control_surface { 2. } else { 4. }) * scale)
                .when(self.control_surface && self.enabled, |view| {
                    view.hover(|style| style.bg(rgb(theme.highlight)))
                })
            })
            .when(self.embedded, |view| {
                view.w_full().h_full().min_w_0().min_h_0()
            })
            .text_color(rgb(theme.text))
            .when(!self.enabled, |view| view.text_color(rgb(theme.disabled)))
            .font_size_scale(typography_scale)
            .line_height(px(INPUT_LINE_HEIGHT) * scale)
            .text_size(px(14.) * scale)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .px(px(7.) * scale)
                    .when(self.embedded, |view| {
                        view.flex().items_center().h_full().min_w_6().px_0()
                    })
                    .when(!self.embedded, |view| {
                        view.h(px(
                            INPUT_LINE_HEIGHT * typography_scale + 2. * INPUT_VERTICAL_PADDING
                        ) * scale)
                            .py(px(INPUT_VERTICAL_PADDING) * scale)
                    })
                    .pr(self.trailing_space * scale)
                    .when(self.embedded, |view| {
                        view.pr_0()
                            .when(self.trailing_text.is_some(), |view| view.pr_1())
                    })
                    .child(InputElement { input: cx.entity() }),
            )
            .when_some(self.trailing_text.clone(), |view, text| {
                view.child(
                    div()
                        .flex()
                        .items_center()
                        .flex_shrink_0()
                        .pr(px(7.) * scale)
                        .text_size(px(12.) * scale)
                        .text_color(rgb(theme.muted))
                        .whitespace_nowrap()
                        .when(self.embedded, |view| {
                            view.flex_shrink(1.).min_w_0().truncate().pr_0()
                        })
                        .child(text),
                )
            })
    }
}

impl Focusable for Input {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
#[cfg(test)]
mod tests {
    use gpui::InputEvent;

    use super::*;

    struct InputDialog {
        input: gpui::Entity<Input>,
        escapes: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl Render for InputDialog {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let escapes = self.escapes.clone();
            div()
                .size_full()
                .child(self.input.clone())
                .on_key_down(move |event, _, _| {
                    if event.keystroke.key == "escape" {
                        escapes.set(escapes.get() + 1);
                    }
                })
        }
    }

    struct SelectionInputs {
        input: Entity<Input>,
        other: Entity<Input>,
    }

    impl Render for SelectionInputs {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_start()
                .child(div().w(px(160.)).child(self.input.clone()))
                .child(div().w(px(160.)).child(self.other.clone()))
        }
    }

    fn selection_fixture(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<Input>, Entity<Input>, gpui::AnyWindowHandle) {
        let input = cx.update(|cx| cx.new(|cx| Input::new("First field", "a中😀z", "", cx)));
        let other = cx.update(|cx| cx.new(|cx| Input::new("Other field", "untouched", "", cx)));
        let window = cx.add_window({
            let input = input.clone();
            let other = other.clone();
            move |_, _| SelectionInputs { input, other }
        });
        let handle = window.into();
        // GPUI test windows start inactive; blur notifications need an active frame.
        cx.update_window(handle, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            assert!(window.is_window_active());
            window.draw(cx).clear(cx);
        })
        .unwrap();
        (input, other, handle)
    }

    #[test]
    fn live_font_size_updates_input_shaping_and_content_height() {
        let mut cx = gpui::TestAppContext::single();
        cx.update(crate::theme::init_test);
        let (input, _, handle) = selection_fixture(&mut cx);
        for font_size in [11, 18, 8] {
            cx.update(|cx| {
                crate::theme::set_typography(gpui::font("sans-serif"), "sans-serif", font_size, cx)
            });
            cx.run_until_parked();
            cx.update_window(handle, |_, window, cx| {
                window.draw(cx).clear(cx);
                let field = input.read(cx);
                let line = field.last_layout.as_ref().unwrap();
                let logical_px = font_size as f32 * (4. / 3.);
                assert!((f32::from(line.font_size) - logical_px).abs() < 0.001);
                assert!(
                    field.last_bounds.unwrap().size.height
                        >= px(INPUT_LINE_HEIGHT * logical_px / 14.).round()
                );
                assert!(field.last_bounds.unwrap().size.height >= line.ascent + line.descent);
                assert_eq!(window.rem_size(), px(16.));
            })
            .unwrap();
        }
    }

    fn dispatch_pointer(
        cx: &mut gpui::TestAppContext,
        handle: gpui::AnyWindowHandle,
        event: impl gpui::InputEvent,
    ) {
        cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
            window.dispatch_event(event.to_platform_input(), cx);
        })
        .unwrap();
    }

    #[test]
    fn disabled_inputs_reject_pointer_tab_and_accessible_focus() {
        let mut cx = gpui::TestAppContext::single();
        cx.update(crate::theme::init_test);
        let (input, other, handle) = selection_fixture(&mut cx);
        cx.update_window(handle, |_, window, cx| {
            input.update(cx, |input, cx| input.set_enabled(false, cx));
            let focus = other.read(cx).focus_handle.clone();
            focus.focus(window, cx);
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let position = input.read_with(&cx, |input, _| input.last_bounds.unwrap().center());
        dispatch_pointer(
            &mut cx,
            handle,
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                ..Default::default()
            },
        );
        dispatch_pointer(
            &mut cx,
            handle,
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                ..Default::default()
            },
        );
        cx.update_window(handle, |_, window, cx| {
            assert!(!input.read(cx).is_focused(window));
            let focus = other.read(cx).focus_handle.clone();
            focus.focus(window, cx);
            window.focus_next(cx);
            assert!(other.read(cx).is_focused(window));
            window.focus_prev(cx);
            assert!(other.read(cx).is_focused(window));
            window.draw(cx).clear(cx);
            window.dispatch_keystroke(gpui::Keystroke::parse("x").unwrap(), cx);
            input.update(cx, |input, cx| {
                let element = input.render(window, cx).into_element();
                let mut node = Node::new(Role::TextInput);
                element.write_a11y_info(&mut node);
                assert!(!node.supports_action(AccessibleAction::Focus));
                assert!(!node.supports_action(AccessibleAction::SetValue));
                drop(element);
                assert_eq!(input.text(), "a中😀z");
            });
        })
        .unwrap();
        assert_eq!(
            other.read_with(&cx, |input, _| input.text().to_owned()),
            "untouchedx"
        );
    }

    #[test]
    fn replacements_discard_stale_pointer_geometry_until_next_draw() {
        for marked in [false, true] {
            let mut cx = gpui::TestAppContext::single();
            cx.update(crate::theme::init_test);
            let (input, _, handle) = selection_fixture(&mut cx);
            cx.update_window(handle, |_, window, cx| {
                input.update(cx, |input, cx| input.set_text("abcdef", cx));
                window.draw(cx).clear(cx);
                let position = {
                    let field = input.read(cx);
                    let bounds = field.last_bounds.unwrap();
                    point(bounds.right() - px(1.), bounds.center().y)
                };
                input.update(cx, |input, cx| {
                    if marked {
                        input.replace_and_mark_text_in_range(
                            Some(0..6),
                            "中",
                            Some(1..1),
                            window,
                            cx,
                        );
                    } else {
                        input.replace_selection(Some(0..6), "中", window, cx);
                    }
                    assert_eq!(input.content.as_ref(), "中");
                });
                // Keep the old painted hitboxes: dispatch_pointer would draw first.
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
                input.update(cx, |input, cx| {
                    assert_eq!(input.selected_range, 0..0);
                    input.replace_selection(None, "x", window, cx);
                    assert_eq!(input.content.as_ref(), if marked { "x" } else { "x中" });
                    assert_eq!(input.selected_range, 1..1);
                });
                window.draw(cx).clear(cx);
                let position = {
                    let field = input.read(cx);
                    let bounds = field.last_bounds.unwrap();
                    point(bounds.right() - px(1.), bounds.center().y)
                };
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
                input.update(cx, |input, cx| {
                    let end = input.content.len();
                    assert_eq!(input.selected_range, end..end);
                    input.replace_selection(None, "!", window, cx);
                    assert_eq!(input.content.as_ref(), if marked { "x!" } else { "x中!" });
                    let end = input.content.len();
                    assert_eq!(input.selected_range, end..end);
                });
            })
            .unwrap();
        }
    }

    #[test]
    fn mouse_selection_tracks_outside_motion_and_final_release_without_touching_other_inputs() {
        let mut cx = gpui::TestAppContext::single();
        cx.update(crate::theme::init_test);
        let (input, other, handle) = selection_fixture(&mut cx);
        let (start, outside, over_other) = cx.update(|cx| {
            let field = input.read(cx);
            let bounds = field.last_bounds.unwrap();
            let other_bounds = other.read(cx).last_bounds.unwrap();
            (
                point(bounds.left(), bounds.center().y),
                point(bounds.right() + px(80.), bounds.center().y),
                other_bounds.center(),
            )
        });
        dispatch_pointer(
            &mut cx,
            handle,
            MouseDownEvent {
                button: MouseButton::Left,
                position: start,
                ..Default::default()
            },
        );
        dispatch_pointer(
            &mut cx,
            handle,
            MouseMoveEvent {
                position: outside,
                pressed_button: Some(MouseButton::Left),
                ..Default::default()
            },
        );
        cx.update(|cx| {
            assert_eq!(input.read(cx).selected_range, 0..9);
            assert!(input.read(cx).selection_subscriptions.is_some());
            assert_eq!(other.read(cx).selected_range, 9..9);
        });
        dispatch_pointer(
            &mut cx,
            handle,
            MouseMoveEvent {
                position: over_other,
                pressed_button: Some(MouseButton::Left),
                ..Default::default()
            },
        );
        dispatch_pointer(
            &mut cx,
            handle,
            MouseUpEvent {
                button: MouseButton::Right,
                position: outside,
                ..Default::default()
            },
        );
        cx.update(|cx| assert!(input.read(cx).selection_subscriptions.is_some()));
        // No intervening move: the release position itself must update the endpoint.
        let release = point(start.x - px(80.), start.y);
        dispatch_pointer(
            &mut cx,
            handle,
            MouseUpEvent {
                button: MouseButton::Left,
                position: release,
                ..Default::default()
            },
        );
        dispatch_pointer(
            &mut cx,
            handle,
            MouseMoveEvent {
                position: outside,
                pressed_button: Some(MouseButton::Left),
                ..Default::default()
            },
        );
        cx.update(|cx| {
            assert_eq!(input.read(cx).selected_range, 0..0);
            assert!(input.read(cx).selection_subscriptions.is_none());
            assert_eq!(other.read(cx).selected_range, 9..9);
            assert!(other.read(cx).selection_subscriptions.is_none());
        });
    }

    #[test]
    fn mouse_selection_cancels_on_missing_button_window_exit_blur_and_disable() {
        let mut cx = gpui::TestAppContext::single();
        cx.update(crate::theme::init_test);
        let (input, other, handle) = selection_fixture(&mut cx);
        let (start, outside) = cx.update(|cx| {
            let bounds = input.read(cx).last_bounds.unwrap();
            (
                point(bounds.left(), bounds.center().y),
                point(bounds.right() + px(80.), bounds.center().y),
            )
        });
        for cancellation in [
            "missing-button",
            "right-only",
            "window-exit",
            "blur",
            "disable",
        ] {
            dispatch_pointer(
                &mut cx,
                handle,
                MouseDownEvent {
                    button: MouseButton::Left,
                    position: start,
                    ..Default::default()
                },
            );
            cx.update(|cx| assert!(input.read(cx).selection_subscriptions.is_some()));
            match cancellation {
                "missing-button" | "right-only" => dispatch_pointer(
                    &mut cx,
                    handle,
                    MouseMoveEvent {
                        position: outside,
                        pressed_button: (cancellation == "right-only")
                            .then_some(MouseButton::Right),
                        ..Default::default()
                    },
                ),
                "window-exit" => dispatch_pointer(
                    &mut cx,
                    handle,
                    MouseExitEvent {
                        position: outside,
                        pressed_button: Some(MouseButton::Left),
                        ..Default::default()
                    },
                ),
                "blur" => {
                    cx.update_window(handle, |_, window, cx| {
                        let focus = other.read(cx).focus_handle.clone();
                        focus.focus(window, cx);
                        // GPUI dispatches focus observers when a frame commits its focus path.
                        window.draw(cx).clear(cx);
                    })
                    .unwrap();
                }
                "disable" => {
                    input.update(&mut cx, |this, cx| this.set_enabled(false, cx));
                }
                _ => unreachable!(),
            }
            cx.update(|cx| {
                assert!(
                    input.read(cx).selection_subscriptions.is_none(),
                    "{cancellation}"
                );
            });
            cx.update_window(handle, |_, window, cx| {
                input.update(cx, |this, cx| {
                    this.set_enabled(true, cx);
                    this.focus_handle.focus(window, cx);
                });
            })
            .unwrap();
            // Regaining focus/enabled state cannot revive the cancelled press.
            dispatch_pointer(
                &mut cx,
                handle,
                MouseMoveEvent {
                    position: outside,
                    pressed_button: Some(MouseButton::Left),
                    ..Default::default()
                },
            );
            dispatch_pointer(
                &mut cx,
                handle,
                MouseUpEvent {
                    button: MouseButton::Left,
                    position: outside,
                    ..Default::default()
                },
            );
            cx.update(|cx| {
                assert_eq!(input.read(cx).selected_range, 0..0, "{cancellation}");
                assert!(input.read(cx).selection_subscriptions.is_none());
                assert_eq!(other.read(cx).selected_range, 9..9);
            });
        }
    }

    #[test]
    fn endpoint_selection_preserves_anchor_across_direction_changes() {
        let mut cx = gpui::TestAppContext::single();
        cx.update(crate::theme::init_test);
        cx.update(Input::init);
        let input = cx.update(|cx| cx.new(|cx| Input::new("Dialog field", "a中😀z", "", cx)));
        let window = cx.add_window({
            let input = input.clone();
            move |_, _| InputDialog {
                input,
                escapes: Default::default(),
            }
        });
        let handle = window.into();
        cx.update_window(handle, |_, window, cx| {
            input.update(cx, |this, cx| {
                this.focus_handle.focus(window, cx);
                this.move_to(4, cx);
            });
        })
        .unwrap();

        for (key, range, reversed) in [
            ("shift-home", 0..4, true),
            ("shift-home", 0..4, true),
            ("shift-end", 4..9, false),
            ("shift-home", 0..4, true),
            ("end", 9..9, false),
            ("shift-home", 0..9, true),
            ("home", 0..0, false),
            ("shift-end", 0..9, false),
            ("shift-home", 0..0, false),
        ] {
            cx.update_window(handle, |_, window, cx| {
                window.draw(cx).clear(cx);
                window.dispatch_event(
                    gpui::KeyDownEvent {
                        keystroke: gpui::Keystroke::parse(key).unwrap(),
                        is_held: false,
                        prefer_character_input: false,
                    }
                    .to_platform_input(),
                    cx,
                );
                let field = input.read(cx);
                assert_eq!(field.selected_range, range, "{key}");
                assert_eq!(field.selection_reversed, reversed, "{key}");
                assert_eq!(field.content.as_ref(), "a中😀z");
            })
            .unwrap();
        }
        cx.update_window(handle, |_, window, cx| {
            input.update(cx, |this, cx| {
                this.set_text("", cx);
                this.select_end(&SelectEnd, window, cx);
                this.select_home(&SelectHome, window, cx);
                assert_eq!(this.selected_range, 0..0);
                this.set_text("locked", cx);
                this.set_enabled(false, cx);
                this.select_home(&SelectHome, window, cx);
                assert_eq!(this.selected_range, 6..6);
            });
        })
        .unwrap();
    }

    #[test]
    fn escape_cancels_ime_preedit_without_dismissing_parent_dialog() {
        let mut cx = gpui::TestAppContext::single();
        cx.update(crate::theme::init_test);
        let input = cx.update(|cx| cx.new(|cx| Input::new("Dialog field", "kept", "", cx)));
        let escapes = std::rc::Rc::new(std::cell::Cell::new(0));
        let window = cx.add_window({
            let input = input.clone();
            let escapes = escapes.clone();
            move |_, _| InputDialog { input, escapes }
        });
        let handle = window.into();
        cx.update_window(handle, |_, window, cx| {
            input.update(cx, |this, cx| {
                this.focus_handle.focus(window, cx);
                this.replace_and_mark_text_in_range(None, "中", Some(1..1), window, cx);
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.dispatch_event(
                gpui::KeyDownEvent {
                    keystroke: gpui::Keystroke::parse("escape").unwrap(),
                    is_held: false,
                    prefer_character_input: false,
                }
                .to_platform_input(),
                cx,
            );
        })
        .unwrap();
        assert_eq!(escapes.get(), 0);
        cx.update(|cx| {
            assert_eq!(input.read(cx).content.as_ref(), "kept");
            assert!(!input.read(cx).is_composing());
        });
        cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
            window.dispatch_event(
                gpui::KeyDownEvent {
                    keystroke: gpui::Keystroke::parse("escape").unwrap(),
                    is_held: false,
                    prefer_character_input: false,
                }
                .to_platform_input(),
                cx,
            );
        })
        .unwrap();
        assert_eq!(escapes.get(), 1);
    }

    #[test]
    fn caret_scrolls_to_both_edges_without_moving_a_visible_caret() {
        let width = px(101.5);
        let line = px(300.);
        assert_eq!(scroll_to_caret(px(0.), px(300.), width, line), px(200.));
        assert_eq!(scroll_to_caret(px(200.), px(0.), width, line), px(0.));
        assert_eq!(scroll_to_caret(px(100.), px(150.), width, line), px(100.));
        assert_eq!(scroll_to_caret(px(100.), px(90.), width, line), px(90.));
        assert_eq!(scroll_to_caret(px(0.), px(101.), width, line), px(1.));
    }

    #[test]
    fn caret_scroll_clamps_after_resize_or_text_shrink() {
        assert_eq!(
            scroll_to_caret(px(200.), px(300.), px(401.5), px(300.)),
            px(0.)
        );
        assert_eq!(
            scroll_to_caret(px(200.), px(20.), px(101.5), px(20.)),
            px(0.)
        );
        assert_eq!(scroll_to_caret(px(200.), px(0.), px(101.5), px(0.)), px(0.));
        assert_eq!(scroll_to_caret(px(0.), px(20.), px(0.), px(20.)), px(20.));
    }

    #[test]
    fn ime_ranges_convert_at_unicode_boundaries() {
        assert_eq!(utf8_range_from_utf16("😀中", &(2..3)), 4..7);
        assert_eq!(utf8_range_from_utf16("😀中", &(1..3)), 0..7);
        assert_eq!(utf16_range_from_utf8("😀中", &(4..7)), 2..3);
    }

    #[test]
    fn ime_selected_range_is_relative_to_replacement_start() {
        assert_eq!(
            selected_range_after_replacement("😀a", Some(&(2..3)), 5),
            9..10
        );
    }

    #[test]
    fn accessible_character_positions_round_trip_utf8_and_reject_invalid_indices() {
        let text = "a中😀e\u{301}";
        for (index, byte) in [0, 1, 4, 8, 9, 11].into_iter().enumerate() {
            assert_eq!(utf8_offset_from_character(text, index), Some(byte));
            assert_eq!(character_index_from_utf8(text, byte), index);
        }
        assert_eq!(utf8_offset_from_character(text, 6), None);
        assert_eq!(utf8_offset_from_character("", 0), Some(0));
        assert_eq!(utf8_offset_from_character("", 1), None);
    }

    #[test]
    fn accessible_selection_and_replacement_use_the_user_edit_path() {
        let mut cx = gpui::TestAppContext::single();
        cx.update(crate::theme::init_test);
        let (input, _, window) = selection_fixture(&mut cx);
        let edits = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            cx.subscribe(&input, {
                let edits = edits.clone();
                move |_, event, _| edits.borrow_mut().push(event.clone())
            })
        });
        cx.update_window(window, |_, window, cx| {
            input.update(cx, |input, cx| {
                let run = NodeId(42);
                input.accessibility.borrow_mut().run_id = Some(run);
                let selection = ActionData::SetTextSelection(TextSelection {
                    anchor: TextPosition {
                        node: run,
                        character_index: 3,
                    },
                    focus: TextPosition {
                        node: run,
                        character_index: 1,
                    },
                });
                input.accessible_selection(Some(&selection), cx);
                assert_eq!(input.selected_range, 1..8);
                assert!(input.selection_reversed);
                let foreign = ActionData::SetTextSelection(TextSelection {
                    anchor: TextPosition {
                        node: NodeId(43),
                        character_index: 0,
                    },
                    focus: TextPosition {
                        node: run,
                        character_index: 0,
                    },
                });
                input.accessible_selection(Some(&foreign), cx);
                assert_eq!(input.selected_range, 1..8);
                let invalid = ActionData::SetTextSelection(TextSelection {
                    anchor: TextPosition {
                        node: run,
                        character_index: 0,
                    },
                    focus: TextPosition {
                        node: run,
                        character_index: 99,
                    },
                });
                input.accessible_selection(Some(&invalid), cx);
                assert_eq!(input.selected_range, 1..8);
                input.accessible_edit(Some(&ActionData::Value("Ω".into())), false, window, cx);
                assert_eq!(input.text(), "aΩz");
                assert_eq!(input.selected_range, 3..3);
            });
        })
        .unwrap();
        assert_eq!(
            *edits.borrow(),
            vec![super::InputEvent::Changed { user: true }]
        );
    }

    #[test]
    fn accessible_actions_preserve_composition_and_reject_disabled_edits() {
        let mut cx = gpui::TestAppContext::single();
        cx.update(crate::theme::init_test);
        let (input, _, window) = selection_fixture(&mut cx);
        cx.update_window(window, |_, window, cx| {
            input.update(cx, |input, cx| {
                let run = NodeId(42);
                input.accessibility.borrow_mut().run_id = Some(run);
                input.replace_and_mark_text_in_range(None, "候", Some(1..1), window, cx);
                let marked = input.marked_range.clone();
                let selection = ActionData::SetTextSelection(TextSelection {
                    anchor: TextPosition {
                        node: run,
                        character_index: 0,
                    },
                    focus: TextPosition {
                        node: run,
                        character_index: 1,
                    },
                });
                input.accessible_selection(Some(&selection), cx);
                assert_eq!(input.marked_range, marked);
                input.accessible_edit(Some(&ActionData::Value("補".into())), false, window, cx);
                assert_eq!(input.text(), "a中😀z補");
                assert!(!input.is_composing());
                input.accessible_edit(Some(&ActionData::Value("😀\n中".into())), true, window, cx);
                assert_eq!(input.text(), "😀 中");
                input.set_enabled(false, cx);
                let selection_before = input.selected_range.clone();
                input.accessible_selection(Some(&selection), cx);
                input.accessible_edit(Some(&ActionData::Value("ignored".into())), true, window, cx);
                input.accessible_edit(
                    Some(&ActionData::Value("ignored".into())),
                    false,
                    window,
                    cx,
                );
                assert_eq!(input.text(), "😀 中");
                assert_eq!(input.selected_range, selection_before);
            });
        })
        .unwrap();
    }

    #[test]
    fn aligned_controls_follow_input_height_across_font_and_interface_scales() {
        struct Controls {
            input: Entity<Input>,
            heights: std::rc::Rc<std::cell::Cell<[Pixels; 3]>>,
        }
        impl Render for Controls {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let measure = |index, child: gpui::AnyElement| {
                    let heights = self.heights.clone();
                    div().relative().w_48().flex_shrink_0().child(child).child(
                        gpui::canvas(
                            move |bounds, _, _| {
                                let mut values = heights.get();
                                values[index] = bounds.size.height;
                                heights.set(values);
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                };
                let theme = crate::theme::palette(cx);
                let button = crate::button("aligned-text", "Apply", cx, |_, _, _| {});
                let dropdown =
                    crate::dropdown_trigger(theme, "aligned-dropdown", "Choice", false, cx);
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_start()
                    .font_size_scale(crate::theme::font_size_scale(cx))
                    .child(measure(0, self.input.clone().into_any_element()))
                    .child(measure(
                        1,
                        input_aligned_control(button, cx).into_any_element(),
                    ))
                    .child(measure(
                        2,
                        input_aligned_control(dropdown, cx).into_any_element(),
                    ))
            }
        }
        let mut cx = gpui::TestAppContext::single();
        cx.update(crate::theme::init_test);
        let heights = std::rc::Rc::new(std::cell::Cell::new([px(0.); 3]));
        let window = cx.add_window({
            let heights = heights.clone();
            move |_, cx| Controls {
                input: cx.new(|cx| Input::new("Size", "24", "", cx)),
                heights,
            }
        });
        for interface_scale in [0.75, 1., 1.5, 2.] {
            for font_size in [8, 11, 18] {
                cx.update(|cx| {
                    crate::theme::set_typography(
                        gpui::font("sans-serif"),
                        "sans-serif",
                        font_size,
                        cx,
                    )
                });
                cx.update_window(window.into(), |_, window, cx| {
                    window.set_rem_size(px(16. * interface_scale));
                    window.draw(cx).clear(cx);
                })
                .unwrap();
                let [input, button, dropdown] = heights.get();
                assert!(input > px(font_size as f32 * (4. / 3.) * interface_scale));
                for control in [button, dropdown] {
                    assert!(
                        (f32::from(input - control)).abs() <= 1.,
                        "font={font_size}, scale={interface_scale}: input={input:?}, control={control:?}"
                    );
                }
            }
        }
    }
}
