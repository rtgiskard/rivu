mod dropdown;
pub(crate) mod input;
#[cfg(test)]
mod keyboard_tests;
mod scrollbar;
pub(crate) mod search;

use super::fonts;
use super::{GuiApp, UI_INSET};

use gpui::{
    ClipboardItem, Context, Div, ElementId, Render, SharedString, Stateful, Window, div,
    prelude::*, px, rgb,
};
pub(crate) fn row() -> Div {
    div().flex().items_center().gap_2().min_w_0()
}
pub(crate) fn column() -> Div {
    div().flex().flex_col().min_w_0().min_h_0().gap_2()
}
pub(crate) fn copyable_message<T: 'static>(
    id: impl Into<ElementId>,
    message: String,
    cx: &mut Context<T>,
) -> Stateful<Div> {
    row()
        .id(id)
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .cursor_pointer()
        .child(
            div()
                .flex_initial()
                .min_w_0()
                .truncate()
                .child(message.clone()),
        )
        .child(div().flex_shrink_0().child("⧉"))
        .tooltip({
            let message: SharedString = message.clone().into();
            move |_, cx| {
                cx.new(|_| ButtonTooltip {
                    text: message.clone(),
                })
                .into()
            }
        })
        .on_click(cx.listener(move |_, _, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(message.clone()));
            cx.stop_propagation();
        }))
}
pub(crate) const LIST_ROW_REMS: f32 = 2.625;
pub(crate) const MENU_WIDTH: f32 = 260.0;
pub(crate) const POPOVER_MAX_REMS: f32 = 15.0;
pub(crate) const DROPDOWN_ROW_REMS: f32 = 2.0;

pub(crate) use dropdown::{DropdownItem, DropdownState};

/// Native tab order bounded to a transient menu or dialog.
#[derive(Clone)]
pub(crate) struct FocusScope {
    start: gpui::FocusHandle,
    end: gpui::FocusHandle,
}

impl FocusScope {
    pub(crate) fn new(cx: &mut gpui::App) -> Self {
        Self {
            start: cx.focus_handle(),
            end: cx.focus_handle().tab_index(isize::MAX),
        }
    }

    pub(crate) fn handle(&self) -> &gpui::FocusHandle {
        &self.start
    }

    pub(crate) fn surface(&self, surface: Stateful<Div>) -> Stateful<Div> {
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

    pub(crate) fn menu_surface(&self, surface: Stateful<Div>) -> Stateful<Div> {
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

    pub(crate) fn focus(&self, window: &mut Window, cx: &mut gpui::App) {
        self.start.focus(window, cx);
        let scope = self.clone();
        window.on_next_frame(move |window, cx| {
            // Do not steal focus if the user already chose an editor or closed the surface.
            if scope.start.is_focused(window) {
                scope.advance(false, window, cx);
            }
        });
    }

    pub(crate) fn advance(&self, backwards: bool, window: &mut Window, cx: &mut gpui::App) {
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

pub(crate) fn tab_key(event: &gpui::KeyDownEvent) -> bool {
    let modifiers = event.keystroke.modifiers;
    event.keystroke.key == "tab"
        && !modifiers.control
        && !modifiers.platform
        && !modifiers.alt
        && !modifiers.function
}

/// Dropdown navigation admits only bare keys, with Shift reserved for reverse Tab.
pub(crate) fn dropdown_key(event: &gpui::KeyDownEvent) -> Option<&str> {
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

pub(crate) fn context_menu_key(event: &gpui::KeyDownEvent) -> bool {
    let modifiers = event.keystroke.modifiers;
    !modifiers.control
        && !modifiers.platform
        && !modifiers.alt
        && !modifiers.function
        && ((event.keystroke.key == "f10" && modifiers.shift)
            || (event.keystroke.key == "menu" && !modifiers.shift))
}

/// Keep native keyboard clicks and pointer clicks on the exact same action path.
pub(crate) fn activate_control<T: 'static>(
    control: Stateful<Div>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    control.on_click(cx.listener(move |this, _, window, cx| {
        action(this, window, cx);
        cx.stop_propagation();
    }))
}

pub(crate) fn row_background(
    theme: &'static crate::theme::Palette,
    selected: bool,
    hovered: bool,
) -> gpui::Rgba {
    rgb(match (selected, hovered) {
        (true, true) => theme.selected_hover,
        (true, false) => theme.selected,
        (false, true) => theme.highlight,
        (false, false) => theme.panel,
    })
}

pub(super) fn visualization_status(message: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .p(gpui::px(UI_INSET))
        .text_sm()
        .text_color(rgb(0xb1bbc5))
        .child(message.into())
}

#[derive(Clone, Copy)]
pub(crate) enum StatusKind {
    Info,
    Success,
    Error,
}

/// Shared inline status text for asynchronous panel and settings actions.
pub(crate) fn status_message(
    theme: &'static crate::theme::Palette,
    message: impl Into<SharedString>,
    kind: StatusKind,
) -> Div {
    let color = match kind {
        StatusKind::Info => theme.secondary,
        StatusKind::Success => theme.accent,
        StatusKind::Error => theme.error,
    };
    caption(theme, message).text_color(rgb(color))
}

/// Shared wrapping toolbar shell for panel actions.
pub(crate) fn panel_toolbar() -> Div {
    row().flex_wrap().flex_shrink_0()
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
    theme: &'static crate::theme::Palette,
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
) -> Div {
    column()
        .gap_1()
        .flex_shrink_0()
        .child(
            div()
                .text_lg()
                .text_color(rgb(theme.text))
                .child(title.into()),
        )
        .child(caption(theme, subtitle))
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
        .p(px(1.))
        .child(child)
}

/// Adds a reserved scrollbar gutter to a full-height virtualized list.
/// The stable id scopes the scrollbar's retained pointer interaction state.
pub(crate) fn scrollable_list(
    id: impl Into<gpui::ElementId>,
    child: impl gpui::IntoElement,
    scroll: &gpui::UniformListScrollHandle,
) -> impl gpui::IntoElement {
    scrollbar::scrollable_list(id, child, scroll)
}

/// Applies the same scrollbar and edge treatment to a standard scroll viewport.
pub(crate) fn scrollable_area(
    id: impl Into<gpui::ElementId>,
    child: impl gpui::IntoElement,
    scroll: &std::rc::Rc<gpui::ScrollHandle>,
) -> impl gpui::IntoElement {
    scrollbar::scrollable_area(id, child, scroll)
}

// GPUI renders tooltips and drag views as independent roots after the window root.
// Read its resolved font now, rather than capturing settings when an overlay is created.
fn overlay_font(window: &Window, cx: &gpui::App) -> gpui::Font {
    window
        .root::<GuiApp>()
        .flatten()
        .expect("GUI overlays belong to a GuiApp window")
        .read(cx)
        .ui_font
        .clone()
}

/// Opaque elevated material for menus, dropdowns, and tooltips only.
pub(crate) fn floating_surface(theme: &'static crate::theme::Palette) -> Div {
    div()
        .bg(rgb(theme.control))
        .border_1()
        .border_color(rgb(theme.border))
        .shadow_sm()
}

/// Compact floating surface shared by action and row tooltips.
fn tooltip_surface(window: &Window, cx: &gpui::App, max_width_rems: f32) -> Div {
    let theme = crate::gui::theme::palette(cx);
    let available = (window.viewport_size().width - px(UI_INSET * 2.)).max(px(0.));
    floating_surface(theme)
        .min_w_0()
        .max_w((window.rem_size() * max_width_rems).min(available))
        .px(px(UI_INSET))
        .py_1()
        .rounded_md()
        .font(overlay_font(window, cx))
        .text_sm()
        .text_color(rgb(theme.text))
        .whitespace_normal()
}

/// Shared tooltip used by icon buttons.
pub(crate) struct ButtonTooltip {
    pub(super) text: SharedString,
}

impl Render for ButtonTooltip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        tooltip_surface(window, cx, 22.5).child(self.text.clone())
    }
}

/// Applies the shared selected state to any control with a selectable meaning.
pub(crate) fn active_control(
    theme: &'static crate::theme::Palette,
    control: Stateful<Div>,
    active: bool,
) -> Stateful<Div> {
    control.when(active, |view| {
        view.bg(rgb(theme.selected))
            .text_color(rgb(theme.accent))
            .group_hover("control", |style| style.bg(rgb(theme.selected_hover)))
    })
}

/// Shared geometry and interaction surface for buttons and select triggers.
/// Variants replace the shared group's hover material; focus retains its own border.
fn control_surface(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
) -> Stateful<Div> {
    div()
        .id(id)
        .group("control")
        .tab_index(0)
        .focus(|style| style.border_color(rgb(theme.accent)))
        .flex()
        .items_center()
        .min_w_0()
        .px(gpui::px(UI_INSET))
        .py_1()
        .rounded_sm()
        .min_h(gpui::rems(2.0))
        .text_sm()
        .text_color(rgb(theme.text))
        .cursor_pointer()
        .bg(rgb(theme.control))
        .border_1()
        .border_color(rgb(theme.border))
        .group_hover("control", |style| style.bg(rgb(theme.highlight)))
}

/// The transport's primary action keeps its accent fill on hover and focus.
pub(crate) fn primary_control(
    theme: &'static crate::theme::Palette,
    control: Stateful<Div>,
) -> Stateful<Div> {
    control
        .border_2()
        .border_color(rgb(theme.accent))
        .bg(rgb(theme.accent))
        .text_color(rgb(theme.on_accent))
        .group_hover("control", |style| style.bg(rgb(theme.accent_hover)))
        .focus(|style| style.border_color(rgb(theme.text)))
}

/// Reusable visual button primitive. It has no application-specific behavior.
pub(crate) fn button_style(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    control_surface(theme, id)
        .justify_center()
        .child(label.into())
}

/// Text button with shared application event handling.
pub(crate) fn button<T: 'static>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = crate::gui::theme::palette(cx);
    activate_control(button_style(theme, id, label), cx, action)
}

/// Sortable list header control with shared active-direction styling.
pub(crate) fn sortable_header<T: 'static>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    descending: bool,
    width: Option<gpui::Pixels>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = crate::gui::theme::palette(cx);
    let mut control = div()
        .id(id)
        .tab_index(0)
        .focus(|style| style.border_color(rgb(theme.accent)))
        .flex()
        .items_center()
        .gap_1()
        .h_full()
        .min_w_0()
        .overflow_hidden()
        .border_1()
        .border_color(gpui::transparent_black())
        .text_xs()
        .text_color(rgb(if active { theme.accent } else { theme.muted }))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme.highlight)).text_color(rgb(theme.text)))
        .on_click(cx.listener(move |this, _, window, cx| {
            action(this, window, cx);
            cx.stop_propagation();
        }))
        .child(div().min_w_0().truncate().child(label.into()))
        .when(active, |control| {
            control.child(
                div()
                    .flex_shrink_0()
                    .font_family(fonts::SYMBOLS_FAMILY)
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
        .font_family(fonts::SYMBOLS_FAMILY)
        .text_size(gpui::rems(1.))
        .size(gpui::rems(2.))
        .p_0()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .tooltip(move |_, cx| cx.new(|_| ButtonTooltip { text: hint.clone() }).into())
}

/// Low-emphasis inline action; preserve the shared icon hit area and tooltip.
pub(crate) fn quiet_icon_button(
    theme: &'static crate::theme::Palette,
    control: Stateful<Div>,
) -> Stateful<Div> {
    control
        .border_color(gpui::transparent_black())
        .bg(rgb(theme.panel).alpha(0.0))
        .text_color(rgb(theme.secondary))
}

/// Shared unavailable-action appearance; the action owner retains its guard.
pub(crate) fn disabled_control(
    theme: &'static crate::theme::Palette,
    control: Stateful<Div>,
    disabled: bool,
) -> Stateful<Div> {
    control.when(disabled, |view| {
        view.text_color(rgb(theme.disabled))
            .border_color(rgb(theme.structure_border))
            .group_hover("control", |style| style.bg(rgb(theme.control)))
            .cursor_default()
            .tab_stop(false)
            .capture_key_down(|event, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    window.prevent_default();
                    cx.stop_propagation();
                }
            })
            .capture_key_up(|event, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    window.prevent_default();
                    cx.stop_propagation();
                }
            })
            .capture_any_mouse_down(|_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .capture_any_mouse_up(|_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
    })
}

/// Destructive text remains visible on hover; keep the shared focus treatment.
pub(crate) fn danger_control(
    theme: &'static crate::theme::Palette,
    control: Stateful<Div>,
) -> Stateful<Div> {
    control
        .text_color(rgb(theme.error))
        .border_color(rgb(theme.error))
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
    let theme = crate::gui::theme::palette(cx);
    let label = label.into();
    active_control(
        theme,
        icon_button(
            id,
            icon,
            format!("{}: {}", label, if enabled { "On" } else { "Off" }),
            cx,
            move |this, _, cx| {
                action(this, cx);
                cx.notify();
            },
        ),
        enabled,
    )
}

/// Label/value row with a label width shared by its metadata column.
pub(crate) fn metadata_item(
    theme: &'static crate::theme::Palette,
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    label_width: gpui::Pixels,
    copy: Option<Stateful<Div>>,
) -> Div {
    // Captions use 0.75rem, so 1.5rem is a 2em label/value gap.
    row()
        .items_start()
        .group("metadata-field")
        .gap(gpui::rems(1.5))
        .flex_1()
        .min_w_0()
        .child(caption(theme, label).w(label_width).flex_shrink_0())
        .child(
            row()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .truncate()
                        .child(value.into()),
                )
                .child(div().w(gpui::rems(1.4)).flex_shrink_0().children(copy)),
        )
}

/// Full-row artwork with a centered preview occupying 70% of the card content.
pub(crate) fn artwork_preview(
    theme: &'static crate::theme::Palette,
    label: impl Into<SharedString>,
    image: impl IntoElement,
    aspect_ratio: Option<f32>,
) -> Div {
    column()
        .w_full()
        .gap_2()
        .child(caption(theme, label))
        .child(
            div().flex().justify_center().child(
                div()
                    .relative()
                    .w(gpui::relative(0.7))
                    .flex_shrink_0()
                    .when_some(aspect_ratio, |view, ratio| view.aspect_ratio(ratio))
                    .child(div().absolute().size_full().child(image)),
            ),
        )
}

/// Menu item with shared application event handling.
pub(crate) fn menu_item<T: 'static>(
    id: impl Into<ElementId>,
    label: impl IntoElement,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = crate::gui::theme::palette(cx);
    activate_control(menu_item_style(theme, id, label), cx, action)
}

pub(crate) fn menu_separator(theme: &'static crate::theme::Palette) -> Div {
    div().h(gpui::px(1.)).mx_2().my_1().bg(rgb(theme.border))
}

pub(crate) fn menu_title(
    theme: &'static crate::theme::Palette,
    title: impl Into<SharedString>,
) -> Div {
    div()
        .px(gpui::px(UI_INSET))
        .py_1()
        .text_xs()
        .text_color(rgb(theme.muted))
        .child(title.into())
}

pub(crate) fn menu_back<T: 'static>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let label = label.into();
    menu_item(id, "", cx, action)
        .font_family(fonts::SYMBOLS_FAMILY)
        .tooltip(move |_, cx| {
            cx.new(|_| ButtonTooltip {
                text: label.clone(),
            })
            .into()
        })
}

/// Shared heading and spacing for settings and metadata sections.
pub(crate) fn section_group(
    theme: &'static crate::theme::Palette,
    title: impl Into<SharedString>,
    content: impl IntoElement,
) -> Div {
    column()
        .gap_2()
        .p_2()
        // Retain the established content inset without painting a nested frame.
        .border_1()
        .border_color(gpui::transparent_black())
        .child(caption(theme, title))
        .child(content)
}

/// Shared responsive two-column layout.
pub(crate) fn two_column(first: Div, second: Div) -> Div {
    row()
        .items_start()
        .flex_wrap()
        .gap_4()
        .child(first.flex_1())
        .child(second.flex_1())
}

/// Shared label-and-control form field layout.
pub(crate) fn form_field(
    theme: &'static crate::theme::Palette,
    label: impl Into<SharedString>,
    control: impl IntoElement,
) -> Div {
    column()
        .w_full()
        .flex_shrink_0()
        .gap_1()
        .child(caption(theme, label))
        .child(control)
}
/// Shared trigger for settings and other anchored dropdowns.
pub(crate) fn dropdown_trigger(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    control_surface(theme, id)
        .w_full()
        .gap_2()
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
        .child(
            div()
                .flex_shrink_0()
                .text_color(rgb(theme.muted))
                .font_family(fonts::SYMBOLS_FAMILY)
                .child("󰅀"),
        )
}

/// Shared shell for application context menus. The caller supplies menu rows
/// and owns dismissal and action dispatch.
pub(crate) fn context_menu_container(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
) -> Stateful<Div> {
    floating_surface(theme)
        .id(id)
        .w(gpui::rems(MENU_WIDTH / 16.))
        .occlude()
        .p_1()
        .gap_0()
        .rounded_md()
}

/// Destructive menu rows use text, not a button border, to identify the action.
pub(crate) fn danger_menu_item(
    theme: &'static crate::theme::Palette,
    control: Stateful<Div>,
) -> Stateful<Div> {
    control.text_color(rgb(theme.error))
}

/// Standard menu item shell; pages only provide the action callback.
pub(crate) fn menu_item_style(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    label: impl IntoElement,
) -> Stateful<Div> {
    div()
        .id(id)
        .tab_index(0)
        .focus(|style| style.bg(rgb(theme.selected)))
        .w_full()
        .min_h(gpui::rems(2.0))
        .px(gpui::px(UI_INSET))
        .py_0()
        .flex()
        .gap_2()
        .items_center()
        .text_sm()
        .cursor_pointer()
        .border_1()
        .border_color(gpui::transparent_black())
        .bg(rgb(theme.panel).alpha(0.0))
        .rounded_sm()
        .hover(|style| style.bg(rgb(theme.selected_hover)))
        .child(div().flex_1().min_w_0().whitespace_normal().child(label))
}

/// Shared shell for anchored dropdowns.
pub(crate) fn dropdown_container(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    width: gpui::Pixels,
) -> Stateful<Div> {
    floating_surface(theme)
        .id(id)
        .w(width)
        .gap_0()
        .p_1()
        .rounded_sm()
        .occlude()
}

/// Shared compact surface for independent drag previews.
pub(crate) fn drag_preview_surface(window: &Window, cx: &gpui::App) -> Div {
    let theme = crate::gui::theme::palette(cx);
    let available = (window.viewport_size().width - px(UI_INSET * 2.)).max(px(0.));
    div()
        .flex()
        .items_center()
        .min_w_0()
        .max_w((window.rem_size() * 22.5).min(available))
        .overflow_hidden()
        .px_2()
        .py_1()
        .bg(rgb(theme.panel))
        .rounded_sm()
        .font(overlay_font(window, cx))
        .text_sm()
        .text_color(rgb(theme.text))
}

pub(crate) fn drag_preview(label: impl Into<SharedString>, window: &Window, cx: &gpui::App) -> Div {
    drag_preview_surface(window, cx).child(div().min_w_0().truncate().child(label.into()))
}

/// The constructor runs before GPUI installs its active drag. Defer within the
/// same event cycle, rather than waiting for another pointer move or timer.
pub(crate) fn begin_list_drag(window: &Window, cx: &mut gpui::App) {
    window.defer(cx, |window, cx| {
        cx.set_active_drag_cursor_style(gpui::CursorStyle::ClosedHand, window);
    });
}

/// Anchor list previews to the pointer, independently of the source row's grab offset.
pub(crate) fn anchor_list_drag_preview(
    surface: impl IntoElement,
    window: &Window,
) -> gpui::Anchored {
    gpui::anchored()
        .position(window.mouse_position() + gpui::point(px(10.), px(10.)))
        .child(surface)
}

pub(crate) fn list_drag_preview(
    label: impl Into<SharedString>,
    window: &Window,
    cx: &gpui::App,
) -> gpui::Anchored {
    anchor_list_drag_preview(drag_preview(label, window, cx), window)
}

pub(crate) struct ListDragPreview {
    pub(crate) label: SharedString,
}

impl Render for ListDragPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        list_drag_preview(self.label.clone(), window, cx)
    }
}

pub(crate) fn tree_row(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    depth: usize,
    selected: bool,
) -> Stateful<Div> {
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
        .border_1()
        .border_color(gpui::transparent_black())
        .cursor_pointer()
        .bg(row_background(theme, selected, false))
        .hover(move |style| style.bg(row_background(theme, selected, true)))
}

/// Standard compact option row used by anchored dropdowns.
pub(crate) fn dropdown_row(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    selected: bool,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    list_row_surface(theme, id, selected)
        .when(!selected, |row| row.bg(rgb(theme.panel).alpha(0.0)))
        .hover(move |style| {
            style.bg(rgb(if selected {
                theme.selected_hover
            } else {
                theme.highlight
            }))
        })
        .h(gpui::rems(DROPDOWN_ROW_REMS))
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
}

pub(crate) fn list_row_height(window: &Window) -> gpui::Pixels {
    window.rem_size() * LIST_ROW_REMS
}

/// Numbered track row shared by album, playlist, and queue views.
pub(crate) fn track_list_row(
    theme: &'static crate::theme::Palette,
    id: impl Into<gpui::ElementId>,
    text_id: impl Into<gpui::ElementId>,
    (title, detail): (String, String),
    index: usize,
    playing: bool,
    selected: bool,
) -> gpui::Stateful<Div> {
    list_row(theme, id, selected)
        .child(
            div()
                .w(gpui::rems(2.))
                .flex_shrink_0()
                .text_xs()
                .whitespace_nowrap()
                .font_features(fonts::tabular_numbers())
                .text_color(rgb(if playing {
                    theme.accent
                } else {
                    theme.secondary
                }))
                .when(playing, |view| view.font_family(fonts::SYMBOLS_FAMILY))
                .child(if playing {
                    "".to_owned()
                } else {
                    (index + 1).to_string()
                }),
        )
        .child(track_row_text(theme, text_id, title, detail, playing))
}

/// Secondary text used throughout panels and controls.
pub(crate) fn caption(theme: &'static crate::theme::Palette, text: impl Into<SharedString>) -> Div {
    div()
        .text_xs()
        .text_color(rgb(theme.secondary))
        .child(text.into())
}

/// Shared row geometry and idle background; each caller owns its single hover style.
fn list_row_surface(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    selected: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .h(gpui::rems(LIST_ROW_REMS))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_2()
        .min_w_0()
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(gpui::transparent_black())
        .overflow_hidden()
        .cursor_pointer()
        .bg(row_background(theme, selected, false))
}

/// Standard selectable row shared by library, queue, playlist, and history.
pub(crate) fn list_row(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    selected: bool,
) -> Stateful<Div> {
    list_row_surface(theme, id, selected)
        .hover(move |style| style.bg(row_background(theme, selected, true)))
}

struct TrackTooltip {
    title: SharedString,
    detail: SharedString,
}

impl Render for TrackTooltip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::gui::theme::palette(cx);
        tooltip_surface(window, cx, 35.)
            .child(
                div()
                    .min_w_0()
                    .flex_shrink_0()
                    .whitespace_normal()
                    .child(self.title.clone()),
            )
            .when(!self.detail.is_empty(), |view| {
                view.child(
                    caption(theme, self.detail.clone())
                        .min_w_0()
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

/// Build row detail only when the tooltip opens.
pub(crate) fn lazy_row_tooltip(
    view: Stateful<Div>,
    title: impl Into<SharedString>,
    detail: impl Fn() -> SharedString + 'static,
) -> Stateful<Div> {
    let title = title.into();
    view.tooltip(move |_, cx| {
        cx.new(|_| TrackTooltip {
            title: title.clone(),
            detail: detail(),
        })
        .into()
    })
}

/// Standard two-line row text with an overflow tooltip.
pub(crate) fn row_text(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> Stateful<Div> {
    track_row_text(theme, id, title, detail, false)
}

fn track_row_text(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    playing: bool,
) -> Stateful<Div> {
    let title = title.into();
    let detail = detail.into();
    let view = column()
        .id(id)
        .flex_1()
        .gap_0()
        .overflow_hidden()
        .child(
            div()
                .text_sm()
                .text_color(rgb(if playing { theme.accent } else { theme.text }))
                .truncate()
                .child(title.clone()),
        )
        .child(caption(theme, detail.clone()).truncate());
    row_tooltip(view, title, detail)
}

pub(super) fn compact_icon_button(
    theme: &'static crate::theme::Palette,
    control: gpui::Stateful<Div>,
) -> gpui::Stateful<Div> {
    quiet_icon_button(theme, control).w(gpui::rems(1.4))
}

// Out-of-flow actions leave the full text width available until the row is hovered.
pub(super) fn hover_actions(
    theme: &'static crate::theme::Palette,
    group: &'static str,
    selected: bool,
) -> Div {
    row()
        .absolute()
        .right_0()
        .top_0()
        .h_full()
        .gap_0()
        .bg(row_background(theme, selected, true))
        .invisible()
        .group_hover(group, |style| style.visible())
}
