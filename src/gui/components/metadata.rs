use gpui::{Div, SharedString, Stateful, div, prelude::*};
use rivu_ui::{caption, row};
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
                .child(div().w_6().flex_shrink_0().children(copy)),
        )
}
