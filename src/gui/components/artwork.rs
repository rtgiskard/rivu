use gpui::{Div, SharedString, div, prelude::*};
use rivu_ui::{caption, column};
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
