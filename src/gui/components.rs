mod dropdown;

use super::{ACCENT, BORDER, GuiApp, HIGHLIGHT, PANEL, TEXT, UI_INSET};

use gpui::{
    Context, Div, ElementId, Render, SharedString, Stateful, Window, div, prelude::*, px, rgb,
};
pub(crate) const ROW_HEIGHT: f32 = 42.0;
pub(crate) const MENU_WIDTH: f32 = 260.0;
pub(crate) const POPOVER_MAX_HEIGHT: f32 = 240.0;
pub(crate) const DROPDOWN_ROW_REMS: f32 = 2.0;
pub(crate) use dropdown::{DropdownItem, DropdownState};

pub(super) fn visualization_status(message: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .p(gpui::px(UI_INSET))
        .text_sm()
        .text_color(rgb(super::MUTED))
        .child(message.into())
}

#[derive(Clone, Copy)]
pub(crate) enum StatusKind {
    Info,
    Success,
    Error,
}

/// Shared inline status text for asynchronous panel and settings actions.
pub(crate) fn status_message(message: impl Into<SharedString>, kind: StatusKind) -> Div {
    let color = match kind {
        StatusKind::Info => super::MUTED,
        StatusKind::Success => ACCENT,
        StatusKind::Error => super::ERROR,
    };
    caption(message).text_color(rgb(color))
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
        .flex_1()
        .flex()
        .flex_col()
        .gap_2()
        .min_w_0()
        .min_h_0()
        .overflow_hidden()
}

/// Shared heading block for content panels.
pub(crate) fn panel_header(
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
) -> Div {
    super::column()
        .gap_1()
        .flex_shrink_0()
        .child(div().text_lg().text_color(rgb(TEXT)).child(title.into()))
        .child(caption(subtitle))
}

/// Shared viewport shell for virtualized lists.
pub(crate) fn list_viewport(child: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .w_full()
        .overflow_hidden()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
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

/// Applies the shared selected state to any control with a selectable meaning.
pub(crate) fn active_control(control: Stateful<Div>, active: bool) -> Stateful<Div> {
    control.when(active, |view| {
        view.bg(rgb(HIGHLIGHT))
            .border_color(rgb(ACCENT))
            .text_color(rgb(ACCENT))
    })
}

/// Shared geometry and interaction surface for buttons and select triggers.
/// Owns the hover style; callers must not register a second GPUI hover style.
fn control_surface(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .min_w_0()
        .px(gpui::px(UI_INSET))
        .py_1()
        .rounded_md()
        .min_h(gpui::rems(2.0))
        .text_sm()
        .text_color(rgb(TEXT))
        .cursor_pointer()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .hover(|style| style.bg(rgb(HIGHLIGHT)).border_color(rgb(ACCENT)))
}

/// Reusable visual button primitive. It has no application-specific behavior.
pub(crate) fn button_style(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    control_surface(id).justify_center().child(label.into())
}

/// Text button with shared application event handling.
pub(crate) fn button<T: 'static>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    button_style(id, label).on_click(cx.listener(move |this, _, window, cx| {
        action(this, window, cx);
        cx.stop_propagation();
    }))
}

/// Sortable list header control with shared active-direction styling.
pub(crate) fn sortable_header(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    descending: bool,
    width: Option<gpui::Pixels>,
    cx: &mut Context<GuiApp>,
    action: impl Fn(&mut GuiApp, &mut Window, &mut Context<GuiApp>) + 'static,
) -> Stateful<Div> {
    let mut control = div()
        .id(id)
        .flex()
        .items_center()
        .gap_1()
        .h_full()
        .min_w_0()
        .overflow_hidden()
        .text_xs()
        .text_color(rgb(if active { ACCENT } else { super::MUTED }))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(HIGHLIGHT)).text_color(rgb(TEXT)))
        .on_click(cx.listener(move |this, _, window, cx| {
            action(this, window, cx);
            cx.stop_propagation();
        }))
        .child(div().min_w_0().truncate().child(label.into()))
        .when(active, |control| {
            control.child(
                div()
                    .flex_shrink_0()
                    .font_family(super::fonts::SYMBOLS_FAMILY)
                    .child(if descending { "󰁅" } else { "󰁝" }),
            )
        });
    if let Some(width) = width {
        control = control.w(width).flex_shrink_0();
    } else {
        control = control.flex_1().min_w_0();
    }
    control
}

/// Icon button with the Nerd symbol font and shared tooltip behavior.
pub(crate) fn icon_button<T: 'static>(
    id: impl Into<ElementId>,
    icon: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let hint = hint.into();
    button(id, icon, cx, action)
        .font_family(super::fonts::SYMBOLS_FAMILY)
        .size(gpui::rems(2.))
        .p_0()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .tooltip(move |_, cx| cx.new(|_| ButtonTooltip { text: hint.clone() }).into())
}

/// Low-emphasis inline action; preserve the shared icon hit area and tooltip.
pub(crate) fn quiet_icon_button(control: Stateful<Div>) -> Stateful<Div> {
    control
        .border_0()
        .bg(rgb(PANEL).alpha(0.0))
        .text_color(rgb(super::MUTED))
}

/// Compact toggle button used by visual and system settings.
pub(crate) fn toggle_button<T: 'static>(
    id: impl Into<ElementId>,
    icon: impl Into<SharedString>,
    label: impl Into<SharedString>,
    enabled: bool,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let label = label.into();
    active_control(
        icon_button(
            id,
            icon,
            format!("{}: {}", label, if enabled { "On" } else { "Off" }),
            cx,
            move |this, _, cx| {
                action(this, cx);
                cx.notify();
            },
        )
        .text_size(gpui::rems(1.)),
        enabled,
    )
}

/// Label/value row with a label width shared by its metadata column.
pub(crate) fn metadata_item(
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    label_width: gpui::Pixels,
) -> Div {
    // Captions use 0.75rem, so 1.5rem is a 2em label/value gap.
    super::row()
        .items_start()
        .gap(gpui::rems(1.5))
        .flex_1()
        .min_w_0()
        .child(caption(label).w(label_width).flex_shrink_0())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .truncate()
                .child(value.into()),
        )
}

/// Full-row artwork with a centered preview occupying 70% of the card content.
pub(crate) fn artwork_preview(image: gpui::Img, aspect_ratio: Option<f32>) -> Div {
    super::column()
        .w_full()
        .gap_2()
        .child(caption("Album artwork"))
        .child(
            div().flex().justify_center().child(
                div()
                    .relative()
                    .w(gpui::relative(0.7))
                    .flex_shrink_0()
                    .when_some(aspect_ratio, |view, ratio| view.aspect_ratio(ratio))
                    .child(image.absolute().size_full()),
            ),
        )
}

/// Menu item with shared application event handling.
pub(crate) fn menu_item(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &mut Context<GuiApp>,
    action: impl Fn(&mut GuiApp, &mut Window, &mut Context<GuiApp>) + 'static,
) -> Stateful<Div> {
    menu_item_style(id, label).on_click(cx.listener(move |this, _, window, cx| {
        action(this, window, cx);
        cx.stop_propagation();
    }))
}

/// Shared bordered section used by settings and metadata panels.
pub(crate) fn section_group(title: impl Into<SharedString>, content: impl IntoElement) -> Div {
    super::column()
        .gap_2()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .child(caption(title))
        .child(content)
}

/// Shared responsive two-column layout.
pub(crate) fn two_column(first: Div, second: Div) -> Div {
    super::row()
        .items_start()
        .flex_wrap()
        .gap_4()
        .child(first.flex_1())
        .child(second.flex_1())
}

/// Shared label-and-control form field layout.
pub(crate) fn form_field(label: impl Into<SharedString>, control: impl IntoElement) -> Div {
    super::column()
        .w_full()
        .flex_shrink_0()
        .gap_1()
        .child(caption(label))
        .child(control)
}
/// Shared trigger for settings and other anchored dropdowns.
pub(crate) fn dropdown_trigger(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    control_surface(id)
        .w_full()
        .gap_2()
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
        .child(
            div()
                .flex_shrink_0()
                .text_color(rgb(super::MUTED))
                .font_family(super::fonts::SYMBOLS_FAMILY)
                .child("󰅀"),
        )
}

/// Shared shell for application context menus. The caller supplies menu rows
/// and owns dismissal and action dispatch.
pub(crate) fn context_menu_container(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .w(gpui::rems(MENU_WIDTH / 16.))
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
        .gap_2()
        .items_center()
        .text_sm()
        .cursor_pointer()
        .border_0()
        .bg(rgb(PANEL).alpha(0.0))
        .rounded_sm()
        .hover(|style| style.bg(rgb(HIGHLIGHT)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .whitespace_normal()
                .child(label.into()),
        )
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

/// Shared drag preview used by panel and queue drag sources.
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

pub(crate) fn tree_row(id: impl Into<ElementId>, depth: usize, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .h(gpui::rems(1.75))
        .px_2()
        .pr_2()
        .pl(gpui::rems(0.5 + depth as f32 * 1.25))
        .min_w_0()
        .flex()
        .items_center()
        .gap_1()
        .rounded_sm()
        .cursor_pointer()
        .bg(rgb(if selected { HIGHLIGHT } else { PANEL }))
        .hover(|style| style.bg(rgb(HIGHLIGHT)))
}

/// Standard compact option row used by anchored dropdowns.
pub(crate) fn dropdown_row(
    id: impl Into<ElementId>,
    selected: bool,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    list_row(id, selected)
        .h(gpui::rems(DROPDOWN_ROW_REMS))
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
        .flex()
        .items_center()
        .gap_2()
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
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let width = (window.viewport_size().width - px(UI_INSET * 2.)).max(px(0.));
        super::column()
            .w(px(560.).min(width))
            .px(gpui::px(UI_INSET))
            .py_2()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .text_color(rgb(TEXT))
            .whitespace_normal()
            .child(
                div()
                    .w_full()
                    .flex_shrink_0()
                    .whitespace_normal()
                    .child(self.title.clone()),
            )
            .when(!self.detail.is_empty(), |view| {
                view.child(
                    caption(self.detail.clone())
                        .w_full()
                        .flex_shrink_0()
                        .whitespace_normal(),
                )
            })
    }
}

/// Full, wrapping row information, independent of the visible row layout.
pub(crate) fn row_tooltip(
    view: Stateful<Div>,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> Stateful<Div> {
    let title = title.into();
    let detail = detail.into();
    view.tooltip(move |_, cx| {
        cx.new(|_| TrackTooltip {
            title: title.clone(),
            detail: detail.clone(),
        })
        .into()
    })
}

/// Standard two-line row text with an overflow tooltip.
pub(crate) fn row_text(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> Stateful<Div> {
    let title = title.into();
    let detail = detail.into();
    let view = super::column()
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
        .child(caption(detail.clone()).truncate());
    row_tooltip(view, title, detail)
}
