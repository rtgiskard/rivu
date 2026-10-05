mod dropdown;
mod list;
mod tree;

use super::{ACCENT, BORDER, HIGHLIGHT, PANEL, TEXT, UI_INSET};

use gpui::{Context, Div, ElementId, Render, SharedString, Stateful, Window, div, prelude::*, rgb};
pub(crate) const ROW_HEIGHT: f32 = 42.0;
pub(crate) const MENU_WIDTH: f32 = 260.0;
pub(crate) const POPOVER_MAX_HEIGHT: f32 = 240.0;
pub(crate) use dropdown::{DropdownItem, DropdownState};
pub(crate) use list::{SelectableListState, SelectionMode, SelectionModel};
pub(crate) use tree::{TreeKey, TreeRow, TreeState};

pub(super) fn visualization_status(message: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .p(gpui::px(UI_INSET))
        .text_sm()
        .text_color(rgb(super::MUTED))
        .child(message.into())
}

/// Shared empty-state surface used by panels and virtualized lists.
pub(crate) fn empty_state(message: impl Into<SharedString>) -> Div {
    visualization_status(message)
}

/// Shared wrapping toolbar shell for panel actions.
pub(crate) fn panel_toolbar() -> Div {
    super::row().flex_wrap().flex_shrink_0()
}

/// Shared root surface for docked panels.
pub(crate) fn panel_surface(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .size_full()
        .flex()
        .flex_col()
        .min_w_0()
        .min_h_0()
        .overflow_hidden()
}

/// Shared viewport shell for virtualized lists.
pub(crate) fn list_viewport(child: impl IntoElement) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .w_full()
        .overflow_hidden()
        .child(child)
}

/// Shared tooltip used by icon buttons.
pub(crate) struct ButtonTooltip {
    pub(super) text: SharedString,
}

impl Render for ButtonTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(gpui::px(360.))
            .px(gpui::px(UI_INSET))
            .py_2()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .text_sm()
            .text_color(rgb(TEXT))
            .child(self.text.clone())
    }
}

/// Reusable visual button primitive. It has no application-specific behavior.
pub(crate) fn button_style(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    div()
        .id(id)
        .px(gpui::px(UI_INSET))
        .py_1()
        .rounded_md()
        .min_h(gpui::rems(2.0))
        .text_sm()
        .cursor_pointer()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .hover(|style| style.bg(rgb(HIGHLIGHT)).border_color(rgb(ACCENT)))
        .child(label.into())
}

/// Shared trigger for settings and other anchored dropdowns.
pub(crate) fn dropdown_trigger(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .w_full()
        .px(gpui::px(UI_INSET))
        .py_1()
        .rounded_md()
        .min_h(gpui::rems(2.0))
        .text_sm()
        .cursor_pointer()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .hover(|style| style.bg(rgb(HIGHLIGHT)).border_color(rgb(ACCENT)))
        .child(div().flex_1().truncate().child(label.into()))
        .child("⌄")
}

pub(super) struct NerdSymbols(pub bool);
impl gpui::Global for NerdSymbols {}

/// Shared shell for application context menus. The caller supplies menu rows
/// and owns dismissal and action dispatch.
pub(crate) fn context_menu_container(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .w(gpui::px(MENU_WIDTH))
        .occlude()
        .p_1()
        .gap_0()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .rounded_md()
}

/// Standard menu item shell; pages only provide the action callback.
pub(crate) fn menu_item_style(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .min_h(gpui::rems(2.0))
        .px(gpui::px(UI_INSET))
        .py_0()
        .flex()
        .items_center()
        .text_sm()
        .cursor_pointer()
        .border_0()
        .bg(rgb(PANEL).alpha(0.0))
        .rounded_sm()
        .hover(|style| style.bg(rgb(HIGHLIGHT)))
        .child(label.into())
}

/// Shared shell for anchored dropdowns.
pub(crate) fn dropdown_container(id: impl Into<ElementId>, width: gpui::Pixels) -> Stateful<Div> {
    div()
        .id(id)
        .w(width)
        .gap_0()
        .p_1()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .rounded_sm()
        .occlude()
}

/// Shared drag preview used by panel and library drag sources.
pub(crate) fn drag_preview(label: impl Into<SharedString>) -> Div {
    div()
        .px_4()
        .py_2()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(ACCENT))
        .rounded_md()
        .text_color(rgb(TEXT))
        .child(label.into())
}

pub(crate) fn tree_row(
    id: impl Into<ElementId>,
    depth: usize,
    selected: bool,
    expanded: bool,
    has_children: bool,
) -> Stateful<Div> {
    let disclosure = if has_children {
        if expanded { "▼" } else { "▶" }
    } else {
        " "
    };
    div()
        .id(id)
        .w_full()
        .h(gpui::rems(1.75))
        .px_2()
        .pl(gpui::px(depth as f32 * 16.))
        .flex()
        .items_center()
        .gap_1()
        .rounded_sm()
        .cursor_pointer()
        .bg(rgb(if selected { HIGHLIGHT } else { PANEL }))
        .hover(|style| style.bg(rgb(HIGHLIGHT)))
        .child(div().w(gpui::px(14.)).text_xs().child(disclosure))
}

/// Standard compact option row used by anchored dropdowns.
pub(crate) fn dropdown_row(
    id: impl Into<ElementId>,
    selected: bool,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    list_row(id, selected)
        .h(gpui::rems(2.0))
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
}

pub(crate) const TRACK_HEIGHT: f32 = ROW_HEIGHT;
/// Secondary text used throughout panels and controls.
pub(crate) fn caption(text: impl Into<SharedString>) -> Div {
    div()
        .text_xs()
        .text_color(rgb(super::MUTED))
        .child(text.into())
}

/// Standard selectable row shared by library, queue, playlist, and history.
pub(crate) fn list_row(id: impl Into<ElementId>, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .h(gpui::rems(2.625))
        .flex_shrink_0()
        .min_w_0()
        .px_2()
        .rounded_sm()
        .overflow_hidden()
        .cursor_pointer()
        .bg(rgb(if selected { HIGHLIGHT } else { PANEL }))
        .hover(|style| style.bg(rgb(HIGHLIGHT)))
}

struct TrackTooltip {
    title: SharedString,
    detail: SharedString,
}

impl Render for TrackTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        super::column()
            .max_w(gpui::px(560.))
            .px(gpui::px(UI_INSET))
            .py_2()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .text_color(rgb(TEXT))
            .child(self.title.clone())
            .child(caption(self.detail.clone()))
    }
}

/// Standard two-line row text with an overflow tooltip.
pub(crate) fn row_text(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> Stateful<Div> {
    let title = title.into();
    let detail = detail.into();
    super::column()
        .id(id)
        .flex_1()
        .gap_0()
        .overflow_hidden()
        .child(
            div()
                .text_sm()
                .text_color(rgb(TEXT))
                .truncate()
                .child(title.clone()),
        )
        .child(caption(detail.clone()).truncate())
        .tooltip(move |_, cx| {
            cx.new(|_| TrackTooltip {
                title: title.clone(),
                detail: detail.clone(),
            })
            .into()
        })
}

/// Shared track row for panels whose content is a two-line track label.
pub(crate) fn track_row(
    id: impl Into<ElementId>,
    text_id: impl Into<ElementId>,
    selected: bool,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> Stateful<Div> {
    list_row(id, selected).child(row_text(text_id, title, detail))
}
