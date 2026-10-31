use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px, rgb};

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

/// Bottom-align a shrinking heading and fixed actions on one compact line.
pub fn panel_heading_row(heading: impl IntoElement + Styled, actions: impl IntoElement) -> Div {
    row()
        .gap_1()
        .flex_shrink_0()
        .items_end()
        .child(heading.flex_1().min_w_0())
        .child(actions)
}

/// Keep upper-right panel controls together with compact spacing.
pub fn panel_header_actions() -> Div {
    row().gap_1().flex_shrink_0()
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

/// Shared viewport shell; vertical edges coincide with its scrollable content.
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
        .px(px(1.))
        .child(child)
}

/// Shared heading and spacing for settings and metadata sections.
pub fn section_group(
    theme: &'static crate::theme::Palette,
    title: impl IntoElement,
    content: impl IntoElement,
) -> Div {
    column()
        .gap_2()
        .p_2()
        // Retain the established content inset without painting a nested frame.
        .border_1()
        .border_color(gpui::transparent_black())
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme.secondary))
                .child(title),
        )
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

/// Horizontal attachments for an existing numeric editor; values and edits remain owner-owned.
/// This shell does not parse text, constrain values, or replace a vertical [`FormField`].
pub struct NumericInput {
    surface: Div,
}

impl NumericInput {
    pub fn new(input: impl IntoElement) -> Self {
        Self {
            surface: row().gap_1().child(div().flex_1().min_w_0().child(input)),
        }
    }

    pub fn unit(mut self, unit: impl IntoElement) -> Self {
        self.surface = self.surface.child(unit);
        self
    }

    /// Append an existing step button with its owner-provided action and disabled state.
    pub fn step(mut self, action: impl IntoElement) -> Self {
        self.surface = self.surface.child(action);
        self
    }

    pub fn into_div(self) -> Div {
        self.surface
    }
}

/// Shared label-and-control field with optional, content-sized feedback.
pub struct FormField {
    surface: Div,
    theme: &'static crate::theme::Palette,
}

impl FormField {
    pub fn new(
        theme: &'static crate::theme::Palette,
        label: impl Into<SharedString>,
        control: impl IntoElement,
    ) -> Self {
        Self {
            surface: column()
                .w_full()
                .flex_shrink_0()
                .gap_1()
                .child(caption(theme, label))
                .child(control),
            theme,
        }
    }

    /// Append a field action without creating an additional layout wrapper.
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.surface = self.surface.child(action);
        self
    }

    pub fn hint(mut self, text: impl Into<SharedString>) -> Self {
        self.surface = self
            .surface
            .child(caption(self.theme, text).text_color(rgb(self.theme.muted)));
        self
    }

    pub fn error(mut self, text: impl Into<SharedString>) -> Self {
        self.surface = self
            .surface
            .child(caption(self.theme, text).text_color(rgb(self.theme.error)));
        self
    }

    pub fn into_div(self) -> Div {
        self.surface
    }
}

/// Baseline-aligned label/value row with a stable trailing action hit area.
pub fn label_value_row(
    theme: &'static crate::theme::Palette,
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    label_width: gpui::Pixels,
    action: Option<Stateful<Div>>,
) -> Div {
    row()
        .items_baseline()
        .group("label-value-row")
        .gap_4()
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
                        .flex_initial()
                        .min_w_0()
                        .text_sm()
                        .truncate()
                        .child(value.into()),
                )
                .child(div().min_w_8().flex_shrink_0().children(action)),
        )
}

/// Secondary text used throughout panels and controls.
pub fn caption(theme: &'static crate::theme::Palette, text: impl Into<SharedString>) -> Div {
    div()
        .text_xs()
        .text_color(rgb(theme.secondary))
        .child(text.into())
}
