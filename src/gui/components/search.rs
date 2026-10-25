use crate::i18n::Localizer;
use gpui::{Context, SharedString};
use rivu_ui::search::{SearchField, SearchLabels};

pub(in crate::gui) enum SearchScope {
    Library,
    Albums,
    PlaylistNames,
    Tracks,
    Recent,
}

/// Rivu supplies text and query pacing; the reusable field owns only local interaction.
pub(in crate::gui) fn new_search(
    i18n: &Localizer,
    scope: SearchScope,
    cx: &mut Context<SearchField>,
) -> SearchField {
    let (placeholder, hint) = match scope {
        SearchScope::Library => ("search-library-placeholder", "search-library-hint"),
        SearchScope::Albums => ("search-albums-placeholder", "search-albums-hint"),
        SearchScope::PlaylistNames => ("search-playlists-placeholder", "search-playlists-hint"),
        SearchScope::Tracks => ("search-tracks-placeholder", "search-tracks-hint"),
        SearchScope::Recent => ("search-recent-placeholder", "search-recent-hint"),
    };
    SearchField::new(
        SearchLabels {
            name: i18n.text("search-toggle").to_owned().into(),
            placeholder: i18n.text(placeholder).to_owned().into(),
            toggle: i18n.text(hint).to_owned().into(),
            clear: i18n.text("search-clear").to_owned().into(),
        },
        rivu_protocol::model::SEARCH_DEBOUNCE,
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
