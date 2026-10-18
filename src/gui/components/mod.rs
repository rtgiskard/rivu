pub(crate) mod artwork;
pub(crate) mod loading;
pub(crate) mod metadata;
pub(crate) mod search;
pub(crate) mod track;

use super::UI_INSET;
use gpui::{Div, SharedString, div, prelude::*, rgb};
pub(super) fn visualization_status(message: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .p(gpui::px(UI_INSET))
        .text_sm()
        .text_color(rgb(0xb1bbc5))
        .child(message.into())
}
