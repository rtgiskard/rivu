use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px, rgb};

pub(crate) const UI_INSET: f32 = 8.0;

pub fn row() -> Div {
    div().flex().items_center().gap_2().min_w_0()
}
pub fn column() -> Div {
    div().flex().flex_col().min_w_0().min_h_0().gap_2()
}

#[derive(Clone, Copy)]
pub enum StatusKind {
    Info,
    Success,
    Error,
}

/// Shared inline status text for asynchronous panel and settings actions.
pub fn status_message(
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
pub fn panel_toolbar() -> Div {
    row().flex_wrap().flex_shrink_0()
}

/// Shared root surface for docked panels.
pub fn panel_surface(id: impl Into<ElementId>) -> Stateful<Div> {
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
pub fn panel_header(
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

/// Two-line list heading whose title and count each truncate without wrapping.
pub fn list_panel_header(
    theme: &'static crate::theme::Palette,
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
) -> Div {
    column()
        .gap_1()
        .flex_shrink_0()
        .child(
            div()
                .min_w_0()
                .text_lg()
                .text_color(rgb(theme.text))
                .truncate()
                .child(title.into()),
        )
        .child(caption(theme, subtitle).min_w_0().truncate())
}

/// Shared viewport shell for virtualized lists.
pub fn list_viewport(child: impl IntoElement) -> Div {
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

/// Shared heading and spacing for settings and metadata sections.
pub fn section_group(
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
pub fn two_column(first: Div, second: Div) -> Div {
    row()
        .items_start()
        .flex_wrap()
        .gap_4()
        .child(first.flex_1())
        .child(second.flex_1())
}

/// Shared label-and-control form field layout.
pub fn form_field(
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

/// Secondary text used throughout panels and controls.
pub fn caption(theme: &'static crate::theme::Palette, text: impl Into<SharedString>) -> Div {
    div()
        .text_xs()
        .text_color(rgb(theme.secondary))
        .child(text.into())
}
