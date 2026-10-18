use crate::i18n::Localizer;
use gpui::{Context, SharedString};
use rivu_ui::search::{SearchField, SearchLabels};

/// Rivu supplies text and query pacing; the reusable field owns only local interaction.
pub(in crate::gui) fn new_search(i18n: &Localizer, cx: &mut Context<SearchField>) -> SearchField {
    SearchField::new(
        SearchLabels {
            placeholder: i18n.text("search-placeholder").to_owned().into(),
            toggle: i18n.text("search-toggle").to_owned().into(),
            clear: i18n.text("search-clear").to_owned().into(),
        },
        crate::model::SEARCH_DEBOUNCE,
        cx,
    )
}

pub(in crate::gui) fn set_search_counts(
    search: &mut SearchField,
    matched: usize,
    total: usize,
    i18n: &Localizer,
    cx: &mut Context<SearchField>,
) {
    search.set_counts(
        matched,
        total,
        |matched, total| -> SharedString {
            i18n.format(
                "search-count",
                &[("matched", matched.into()), ("total", total.into())],
            )
            .into()
        },
        cx,
    );
}
