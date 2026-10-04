mod dropdown;
mod list;
mod menu;
mod tree;

use super::{ACCENT, BORDER, PANEL, TEXT};
pub(crate) use dropdown::{DropdownItem, DropdownState};
use gpui::{Context, Div, ElementId, Render, SharedString, Stateful, Window, div, prelude::*, rgb};
#[allow(unused_imports)]
pub(crate) use list::{SelectableListState, SelectionMode};
#[allow(unused_imports)]
pub(crate) use menu::ContextMenuState;
#[allow(unused_imports)]
pub(crate) use tree::{TreeKey, TreeRow, TreeState};

pub(super) fn visualization_status(message: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .p_3()
        .text_sm()
        .text_color(rgb(0x8794a4))
        .child(message.into())
}

/// Shared tooltip used by icon buttons.
pub(crate) struct ButtonTooltip {
    pub(super) text: SharedString,
}

impl Render for ButtonTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(gpui::px(360.))
            .px_3()
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
        .px_3()
        .py_1()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .hover(|style| style.bg(rgb(0x292e42)).border_color(rgb(ACCENT)))
        .child(label.into())
}

pub(super) struct NerdSymbols(pub bool);
impl gpui::Global for NerdSymbols {}

/// Shared shell for application context menus. The caller supplies menu rows
/// and owns dismissal and action dispatch.
pub(crate) fn context_menu_container(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .w(gpui::px(260.))
        .occlude()
        .p_1()
        .gap_0()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .rounded_md()
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

/// Visual tree row matching the compact Neovim/Snacks-style indentation.
#[allow(dead_code)]
pub(crate) fn tree_row(
    id: impl Into<ElementId>,
    depth: usize,
    selected: bool,
    expanded: bool,
    has_children: bool,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    let disclosure = if has_children {
        if expanded { "▼" } else { "▶" }
    } else {
        " "
    };
    div()
        .id(id)
        .w_full()
        .h(gpui::px(28.))
        .px_2()
        .pl(gpui::px(depth as f32 * 16.))
        .flex()
        .items_center()
        .gap_1()
        .rounded_sm()
        .cursor_pointer()
        .bg(rgb(if selected { 0x293d40 } else { PANEL }))
        .hover(|style| style.bg(rgb(0x293039)))
        .child(div().w(gpui::px(14.)).text_xs().child(disclosure))
        .child(label.into())
}

pub(crate) const TRACK_HEIGHT: f32 = 42.0;
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
        .h(gpui::px(TRACK_HEIGHT))
        .flex_shrink_0()
        .min_w_0()
        .px_2()
        .rounded_sm()
        .overflow_hidden()
        .cursor_pointer()
        .bg(rgb(if selected { 0x293d40 } else { PANEL }))
        .hover(|style| style.bg(rgb(0x293039)))
}

struct TrackTooltip {
    title: SharedString,
    detail: SharedString,
}

impl Render for TrackTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        super::column()
            .max_w(gpui::px(560.))
            .px_3()
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
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> Stateful<Div> {
    let title = title.into();
    let detail = detail.into();
    super::column()
        .id("track-text")
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
