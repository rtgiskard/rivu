use crate::{
    layout::{UI_INSET, caption, row},
    surface::floating_surface,
};
use gpui::{
    ClipboardItem, Context, Div, ElementId, Render, SharedString, Stateful, Window, div,
    prelude::*, px, rgb,
};

pub fn copyable_message<T: 'static>(
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

// GPUI renders tooltips and drag views as independent roots after the window root.
// Read its resolved font now, rather than capturing settings when an overlay is created.
fn overlay_font(cx: &gpui::App) -> gpui::Font {
    crate::theme::font(cx).clone()
}

/// Compact floating surface shared by action and row tooltips.
pub fn tooltip_surface(window: &Window, cx: &gpui::App, max_width_rems: f32) -> Div {
    let theme = crate::theme::palette(cx);
    let available = (window.viewport_size().width - px(UI_INSET * 2.)).max(px(0.));
    floating_surface(theme)
        .min_w_0()
        .max_w((window.rem_size() * max_width_rems).min(available))
        .px(px(UI_INSET))
        .py_1()
        .rounded_md()
        .font(overlay_font(cx))
        .text_sm()
        .text_color(rgb(theme.text))
        .whitespace_normal()
}

/// Shared tooltip used by icon buttons.
pub struct ButtonTooltip {
    pub text: SharedString,
}

impl Render for ButtonTooltip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        tooltip_surface(window, cx, 22.5).child(self.text.clone())
    }
}

/// Shared compact surface for independent drag previews.
pub fn drag_preview_surface(window: &Window, cx: &gpui::App) -> Div {
    let theme = crate::theme::palette(cx);
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
        .font(overlay_font(cx))
        .text_sm()
        .text_color(rgb(theme.text))
}

pub fn drag_preview(label: impl Into<SharedString>, window: &Window, cx: &gpui::App) -> Div {
    drag_preview_surface(window, cx).child(div().min_w_0().truncate().child(label.into()))
}

/// The constructor runs before GPUI installs its active drag. Defer within the
/// same event cycle, rather than waiting for another pointer move or timer.
pub fn begin_list_drag(window: &Window, cx: &mut gpui::App) {
    window.defer(cx, |window, cx| {
        cx.set_active_drag_cursor_style(gpui::CursorStyle::ClosedHand, window);
    });
}

/// Anchor list previews to the pointer, independently of the source row's grab offset.
pub fn anchor_list_drag_preview(surface: impl IntoElement, window: &Window) -> gpui::Anchored {
    gpui::anchored()
        .position(window.mouse_position() + gpui::point(px(10.), px(10.)))
        .child(surface)
}

pub fn list_drag_preview(
    label: impl Into<SharedString>,
    window: &Window,
    cx: &gpui::App,
) -> gpui::Anchored {
    anchor_list_drag_preview(drag_preview(label, window, cx), window)
}

pub struct ListDragPreview {
    pub label: SharedString,
}

impl Render for ListDragPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        list_drag_preview(self.label.clone(), window, cx)
    }
}

struct RowTooltip {
    title: SharedString,
    detail: SharedString,
}

impl Render for RowTooltip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::palette(cx);
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
pub fn row_tooltip(
    view: Stateful<Div>,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> Stateful<Div> {
    let title = title.into();
    let detail = detail.into();
    view.tooltip(move |_, cx| {
        cx.new(|_| RowTooltip {
            title: title.clone(),
            detail: detail.clone(),
        })
        .into()
    })
}

/// Build row detail only when the tooltip opens.
pub fn lazy_row_tooltip(
    view: Stateful<Div>,
    title: impl Into<SharedString>,
    detail: impl Fn() -> SharedString + 'static,
) -> Stateful<Div> {
    let title = title.into();
    view.tooltip(move |_, cx| {
        cx.new(|_| RowTooltip {
            title: title.clone(),
            detail: detail(),
        })
        .into()
    })
}
