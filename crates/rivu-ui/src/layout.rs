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
    id: impl Into<ElementId>,
    theme: &'static rivu_format::palette::Palette,
    message: impl Into<SharedString>,
    kind: StatusKind,
) -> Stateful<Div> {
    let color = match kind {
        StatusKind::Info => theme.secondary,
        StatusKind::Success => theme.accent,
        StatusKind::Error => theme.error,
    };
    div()
        .id(id)
        .text_xs()
        .text_color(rgb(color))
        .child(crate::overlay::truncated_label("status", message))
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
    theme: &'static rivu_format::palette::Palette,
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
    theme: &'static rivu_format::palette::Palette,
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
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme.secondary))
                .min_w_0()
                .truncate()
                .child(crate::overlay::truncated_label("subtitle", subtitle)),
        )
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
    theme: &'static rivu_format::palette::Palette,
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
    theme: &'static rivu_format::palette::Palette,
}

impl FormField {
    pub fn new(
        theme: &'static rivu_format::palette::Palette,
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
    id: impl Into<ElementId>,
    theme: &'static rivu_format::palette::Palette,
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    label_width: gpui::Pixels,
    action: Option<Stateful<Div>>,
) -> Stateful<Div> {
    row()
        .id(id)
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
                        .child(crate::overlay::truncated_label("value", value)),
                )
                .child(div().min_w_8().flex_shrink_0().children(action)),
        )
}

/// Secondary text used throughout panels and controls.
pub fn caption(
    theme: &'static rivu_format::palette::Palette,
    text: impl Into<SharedString>,
) -> Div {
    div()
        .text_xs()
        .text_color(rgb(theme.secondary))
        .child(text.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, Context, InputEvent, Pixels, Render, TestAppContext, Window};
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        time::Duration,
    };

    const VALUES: [&str; 2] = [
        "First complete field value that exceeds its row",
        "Second complete field value that exceeds its row",
    ];
    const STATUSES: [&str; 2] = [
        "First complete status that exceeds its row",
        "Second complete status that exceeds its row",
    ];

    struct RepeatedLabels {
        bounds: Rc<Cell<[Bounds<Pixels>; 6]>>,
    }

    impl Render for RepeatedLabels {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = crate::theme::palette(cx);
            let bounds = self.bounds.clone();
            column()
                .size_full()
                .items_start()
                .font(crate::theme::font(cx).clone())
                .children(VALUES.into_iter().enumerate().map(|(index, value)| {
                    label_value_row(("field", index), theme, "L", value, px(16.), None)
                        .w(px(120.))
                        .flex_none()
                }))
                .children(STATUSES.into_iter().enumerate().map(|(index, message)| {
                    status_message(("status-row", index), theme, message, StatusKind::Info)
                        .w(px(80.))
                        .truncate()
                }))
                .children((0_usize..2).map(|index| {
                    status_message(
                        ("multiline-row", index),
                        theme,
                        "First line\nSecond line",
                        StatusKind::Info,
                    )
                    .w(px(400.))
                }))
                .on_children_prepainted(move |children, _, _| {
                    bounds.set(children.try_into().unwrap());
                })
        }
    }

    #[gpui::test]
    fn repeated_fields_and_statuses_keep_independent_hover_state(cx: &mut TestAppContext) {
        cx.update(crate::theme::init_test);
        let bounds = Rc::new(Cell::new([Bounds::default(); 6]));
        let window = cx.add_window(|_, _| RepeatedLabels {
            bounds: bounds.clone(),
        });
        let seen = Rc::new(RefCell::new(Vec::new()));
        let observed = seen.clone();
        let _subscription = cx.update(|cx| {
            cx.observe_new::<crate::ButtonTooltip>(move |tooltip, _, _| {
                observed.borrow_mut().push(tooltip.text.clone());
            })
        });
        for index in [0, 1, 2, 3, 4, 5, 1, 0] {
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.dispatch_event(
                    gpui::MouseMoveEvent {
                        position: gpui::point(px(800.), px(600.)),
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
            })
            .unwrap();
            cx.run_until_parked();
            seen.borrow_mut().clear();
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                let row = bounds.get()[index];
                let position = gpui::point(
                    row.left() + px(if index < 2 { 40. } else { 4. }),
                    if index < 4 {
                        row.center().y
                    } else {
                        row.top() + px(6.)
                    },
                );
                window.dispatch_event(
                    gpui::MouseMoveEvent {
                        position,
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
            })
            .unwrap();
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_secs(1));
            cx.run_until_parked();
            let seen = seen.borrow();
            if index < 4 {
                let expected = if index < 2 {
                    VALUES[index]
                } else {
                    STATUSES[index - 2]
                };
                assert!(!seen.is_empty(), "row {index} must show its full text");
                assert!(
                    seen.iter().all(|text| text.as_ref() == expected),
                    "row {index}: {seen:?}"
                );
            } else {
                assert!(seen.is_empty(), "visible multiline row {index}: {seen:?}");
            }
        }
    }
}
