pub(crate) mod search;

use super::fonts;

use super::UI_INSET;
use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, rgb};
use rivu_ui::{caption, column, list_row, row, row_tooltip};
pub(super) fn visualization_status(message: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .p(gpui::px(UI_INSET))
        .text_sm()
        .text_color(rgb(0xb1bbc5))
        .child(message.into())
}

/// Label/value row with a label width shared by its metadata column.
pub(crate) fn metadata_item(
    theme: &'static rivu_ui::theme::Palette,
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    label_width: gpui::Pixels,
    copy: Option<Stateful<Div>>,
) -> Div {
    row()
        .items_baseline()
        .group("metadata-field")
        .gap(gpui::rems(1.))
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
                .child(div().w(gpui::rems(1.4)).flex_shrink_0().children(copy)),
        )
}

/// Full-row artwork with a centered preview occupying 70% of the card content.
pub(crate) fn artwork_preview(
    theme: &'static rivu_ui::theme::Palette,
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

/// Numbered track row shared by album, playlist, and queue views.
pub(crate) fn track_list_row(
    theme: &'static rivu_ui::theme::Palette,
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

/// Standard two-line row text with an overflow tooltip.
pub(crate) fn row_text(
    theme: &'static rivu_ui::theme::Palette,
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> Stateful<Div> {
    track_row_text(theme, id, title, detail, false)
}

fn track_row_text(
    theme: &'static rivu_ui::theme::Palette,
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
