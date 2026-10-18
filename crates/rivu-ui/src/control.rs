use crate::{layout::UI_INSET, overlay::text_tooltip};
use gpui::{Context, Div, ElementId, SharedString, Stateful, Window, div, prelude::*, rgb};

/// Keep native keyboard clicks and pointer clicks on the exact same action path.
pub fn activate_control<T: 'static>(
    control: Stateful<Div>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    control.on_click(cx.listener(move |this, _, window, cx| {
        action(this, window, cx);
        cx.stop_propagation();
    }))
}

/// Applies the shared selected state to any control with a selectable meaning.
pub fn active_control(
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

/// Reusable visual button primitive. It has no application-specific behavior.
pub fn button_style(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    control_surface(theme, id)
        .justify_center()
        .child(label.into())
}

/// Text button with shared application event handling.
pub fn button<T: 'static>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = crate::theme::palette(cx);
    activate_control(button_style(theme, id, label), cx, action)
}

/// Sortable list header control with shared active-direction styling.
pub fn sortable_header<T: 'static>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    descending: bool,
    width: Option<gpui::Pixels>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = crate::theme::palette(cx);
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
                    .font_family(crate::theme::icon_family(cx))
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
pub fn icon_button<T: 'static>(
    id: impl Into<ElementId>,
    icon: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    button(id, icon, cx, action)
        .font_family(crate::theme::icon_family(cx))
        .text_size(gpui::rems(1.))
        .size(gpui::rems(2.))
        .p_0()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .tooltip(text_tooltip(hint))
}

/// Low-emphasis inline action; preserve the shared icon hit area and tooltip.
pub fn quiet_icon_button(
    theme: &'static crate::theme::Palette,
    control: Stateful<Div>,
) -> Stateful<Div> {
    control
        .border_color(gpui::transparent_black())
        .bg(rgb(theme.panel).alpha(0.0))
        .text_color(rgb(theme.secondary))
}

/// Shared unavailable-action appearance; the action owner retains its guard.
pub fn disabled_control(
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
pub fn danger_control(
    theme: &'static crate::theme::Palette,
    control: Stateful<Div>,
) -> Stateful<Div> {
    control
        .text_color(rgb(theme.error))
        .border_color(rgb(theme.error))
}

/// Compact toggle button used by visual and system settings.
pub fn toggle_button<T: 'static>(
    id: impl Into<ElementId>,
    icon: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    enabled: bool,
    cx: &mut Context<T>,
    action: impl Fn(&mut T, &mut Context<T>) + 'static,
) -> Stateful<Div> {
    let theme = crate::theme::palette(cx);
    active_control(
        theme,
        icon_button(id, icon, hint, cx, move |this, _, cx| {
            action(this, cx);
            cx.notify();
        }),
        enabled,
    )
}

/// Shared trigger for settings and other anchored dropdowns.
pub fn dropdown_trigger(
    theme: &'static crate::theme::Palette,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &gpui::App,
) -> Stateful<Div> {
    control_surface(theme, id)
        .w_full()
        .gap_2()
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
        .child(
            div()
                .flex_shrink_0()
                .text_color(rgb(theme.muted))
                .font_family(crate::theme::icon_family(cx))
                .child("󰅀"),
        )
}

pub fn compact_icon_button(
    theme: &'static crate::theme::Palette,
    control: gpui::Stateful<Div>,
) -> gpui::Stateful<Div> {
    quiet_icon_button(theme, control).w(gpui::rems(1.4))
}
