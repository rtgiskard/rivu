//! View queries and paged buffers, including stale-response rejection and cancellation.
//! GuiApp retains the entity and the single frame scheduler.

use super::*;
use crate::{
    model::Query,
    projection::GuiLibrarySnapshot,
    response::{PlaylistEntryRevision, QueryRevisions, ViewResponse},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ViewRequestKind {
    Summaries,
    Entries,
    Selection,
    EntrySelection,
    Queue,
}

impl ViewRequestKind {
    fn for_query(query: &Query) -> Option<Self> {
        Some(match query {
            Query::PlaylistSummaries { .. } => Self::Summaries,
            Query::PlaylistEntries { .. } => Self::Entries,
            Query::PlaylistSummaryBatch { .. } => Self::Selection,
            _ => return None,
        })
    }

    fn revisions_match(
        self,
        revisions: QueryRevisions,
        library: &GuiLibrarySnapshot,
        playlist_id: Option<i64>,
    ) -> bool {
        if revisions.library != library.revision || revisions.playlist != library.playlist_revision
        {
            return false;
        }
        if !matches!(self, Self::Entries | Self::EntrySelection) {
            return true;
        }
        let entries = playlist_id.map(|playlist_id| PlaylistEntryRevision {
            playlist_id,
            revision: library
                .playlist_entry_revisions
                .get(&playlist_id)
                .copied()
                .unwrap_or(0),
        });
        match self {
            Self::Entries => entries.is_some() && revisions.playlist_entries == entries,
            // Missing entries have no identity to attach a token to; the captured
            // request baseline and catalog epoch still guard deletion results.
            Self::EntrySelection => revisions
                .playlist_entries
                .is_none_or(|revision| Some(revision) == entries),
            _ => true,
        }
    }
}

#[derive(Default)]
pub(super) struct ViewRequests {
    generation: u64,
    generations: HashMap<ViewRequestKind, u64>,
}

impl ViewRequests {
    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    fn begin(&mut self, kind: ViewRequestKind, generation: u64) {
        self.generations.insert(kind, generation);
    }

    fn is_current(&self, kind: ViewRequestKind, generation: u64) -> bool {
        self.generations.get(&kind) == Some(&generation)
    }

    fn finish(&mut self, kind: ViewRequestKind, generation: u64) -> bool {
        if !self.is_current(kind, generation) {
            return false;
        }
        self.cancel(kind);
        true
    }

    pub(super) fn pending(&self, kind: ViewRequestKind) -> bool {
        self.generations.contains_key(&kind)
    }

    pub(super) fn cancel(&mut self, kind: ViewRequestKind) {
        self.generations.remove(&kind);
    }

    fn invalidate_pages(&mut self) {
        // Existence lookups are not pages; changing page size cannot supersede them.
        self.generations.retain(|kind, _| {
            matches!(
                kind,
                ViewRequestKind::Selection | ViewRequestKind::EntrySelection
            )
        });
    }
}

impl GuiApp {
    pub(super) fn request_root_counts(&mut self, cx: &mut Context<Self>) {
        let roots = self.state.system.config.library_roots.clone();
        let revision = self.state.library.revision;
        let (generation, batches) = self.browser.library.begin_root_counts(&roots, revision);
        for offset in batches {
            let batch =
                roots[offset..(offset + crate::model::MAX_QUERY_ROWS).min(roots.len())].to_vec();
            let handle = self.handle.clone();
            cx.spawn(async move |this, cx| {
                let response = cx
                    .background_executor()
                    .spawn(async move { handle.query(Query::LibraryRootCounts { roots: batch }) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.refresh(cx);
                    if !this.browser.library.counts_current(
                        generation,
                        &this.state.system.config.library_roots,
                        this.state.library.revision,
                    ) {
                        return;
                    }
                    if response.revisions.library != revision {
                        this.browser.library.fail_root_counts(generation, offset);
                        this.request_root_counts(cx);
                        return;
                    }
                    match response.result {
                        Ok(ViewResponse::LibraryRootCounts(counts)) => this
                            .browser
                            .library
                            .accept_root_counts(generation, offset, counts),
                        Err(error) => {
                            this.browser.library.fail_root_counts(generation, offset);
                            this.error = Some(error.to_string());
                        }
                        _ => this.browser.library.fail_root_counts(generation, offset),
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    pub(super) fn view_page_size(&self) -> usize {
        self.state.system.config.page_size as usize
    }

    fn request_view(&mut self, query: Query, generation: u64, cx: &mut Context<Self>) {
        let Some(kind) = ViewRequestKind::for_query(&query) else {
            return;
        };
        self.browser.requests.begin(kind, generation);
        let selected_playlist = self.browser.playlists.selected();
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            // Coalesce quick changes; generations reject superseded queries.
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            if !this
                .update(cx, |this, _| {
                    this.browser.requests.is_current(kind, generation)
                })
                .unwrap_or(false)
            {
                return;
            }
            let response = cx
                .background_executor()
                .spawn(async move { handle.query(query) })
                .await;
            let revisions = response.revisions;
            let result = response.result;
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                if !this.browser.requests.finish(kind, generation) {
                    return;
                }
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        let message = error.to_string();
                        match kind {
                            ViewRequestKind::Summaries => {
                                this.browser.playlists.fail_page(false, message.clone())
                            }
                            ViewRequestKind::Entries => {
                                this.browser.playlists.fail_page(true, message.clone())
                            }
                            _ => {}
                        }
                        this.error = Some(message);
                        cx.notify();
                        return;
                    }
                };
                if !kind.revisions_match(
                    revisions,
                    &this.state.library,
                    this.browser.playlists.entries().playlist_id,
                ) {
                    match kind {
                        ViewRequestKind::Summaries => this.request_playlist_summaries(
                            this.browser.playlists.summaries().offset,
                            cx,
                        ),
                        ViewRequestKind::Entries => {
                            if let Some(id) = this.browser.playlists.entries().playlist_id {
                                this.request_playlist_entries(
                                    id,
                                    this.browser.playlists.entries().offset,
                                    cx,
                                );
                            }
                        }
                        ViewRequestKind::Selection => this.request_playlist_selection(cx),
                        ViewRequestKind::EntrySelection => {
                            unreachable!("entry selection uses its own lookup")
                        }
                        ViewRequestKind::Queue => unreachable!("queue pages use their own worker"),
                    }
                    return;
                }
                match result {
                    ViewResponse::PlaylistSummaries(page) if kind == ViewRequestKind::Summaries => {
                        this.browser.accept_playlist_summaries(page);
                    }
                    ViewResponse::PlaylistEntries(page) if kind == ViewRequestKind::Entries => {
                        this.browser.playlists.accept_entries(page);
                    }
                    ViewResponse::PlaylistSummaryBatch(batch)
                        if kind == ViewRequestKind::Selection =>
                    {
                        if let Some(id) = selected_playlist {
                            this.browser
                                .accept_playlist_lookup(id, batch.rows.into_iter().next());
                        }
                    }
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn request_directory_page(
        &mut self,
        branch: library::TreeBranch,
        offset: usize,
        cx: &mut Context<Self>,
    ) {
        let page_size = self.view_page_size();
        let generation = self.browser.requests.next_generation();
        let offset =
            self.browser
                .library
                .prepare_directory(branch.clone(), offset, page_size, generation);
        if branch.is_forest() {
            let roots = &self.state.system.config.library_roots;
            let page = crate::model::Page::<crate::model::DirectoryRow> {
                total: roots.len(),
                rows: roots
                    .iter()
                    .skip(offset)
                    .take(page_size)
                    .map(|path| crate::model::DirectoryRow::Directory { path: path.clone() })
                    .collect(),
            };
            self.browser
                .library
                .accept_directory(&branch, generation, page);
            cx.notify();
            return;
        }
        let handle = self.handle.clone();
        let favorite = self.browser.library.favorites_only().then_some(true);
        let query = self.browser.library.buffer().query.clone();
        let tree_active = self.browser.library.tree_active();
        let missing = self.browser.library.buffer().missing;
        let sort = self.browser.library.buffer().sort;
        let delay = if query.is_some() {
            crate::model::SEARCH_DEBOUNCE
        } else {
            Duration::ZERO
        };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            if !this
                .update(cx, |this, _| {
                    this.browser.library.directory_request_pending(
                        &branch,
                        generation,
                        query.as_deref(),
                        favorite,
                        tree_active,
                        missing,
                    )
                })
                .unwrap_or(false)
            {
                return;
            }
            let request_query = query.clone();
            let request_path = branch.path().to_path_buf();
            let response = cx
                .background_executor()
                .spawn(async move {
                    handle.query(if tree_active {
                        Query::DirectoryPage {
                            path: request_path,
                            query: request_query,
                            favorite,
                            missing,
                            offset,
                            limit: page_size,
                        }
                    } else {
                        Query::LibraryRootPage {
                            path: request_path,
                            query: request_query,
                            favorite,
                            missing,
                            sort,
                            offset,
                            limit: page_size,
                        }
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                if !this.browser.library.directory_request_pending(
                    &branch,
                    generation,
                    query.as_deref(),
                    favorite,
                    tree_active,
                    missing,
                ) {
                    return;
                }
                let result = match response.result {
                    Ok(result) => result,
                    Err(error) => {
                        this.browser.library.fail_directory(&branch, generation);
                        this.error = Some(error.to_string());
                        cx.notify();
                        return;
                    }
                };
                if response.revisions.structure != this.state.library.structure_revision
                    || response.revisions.library != this.state.library.revision
                {
                    this.request_directory_page(branch, offset, cx);
                    return;
                }
                let page = match result {
                    ViewResponse::DirectoryPage(page) if tree_active => Some(page),
                    ViewResponse::LibraryPage(page) if !tree_active => Some(crate::model::Page {
                        total: page.total,
                        rows: page
                            .rows
                            .into_iter()
                            .map(crate::model::DirectoryRow::Track)
                            .collect(),
                    }),
                    _ => None,
                };
                if let Some(page) = page {
                    this.browser
                        .library
                        .accept_directory(&branch, generation, page);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn request_playlist_selection(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.browser.playlists.selected() else {
            return;
        };
        let generation = self.browser.requests.next_generation();
        self.request_view(
            Query::PlaylistSummaryBatch {
                after: id.checked_sub(1),
                limit: 1,
                expected: None,
            },
            generation,
            cx,
        );
    }

    pub(super) fn request_playlist_summaries(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.browser.playlists.invalidate_summaries();
        self.load_playlist_summaries(offset, cx);
    }

    fn load_playlist_summaries(&mut self, offset: usize, cx: &mut Context<Self>) {
        let page_size = self.view_page_size();
        let query = self
            .browser
            .playlists
            .prepare_page(false, offset, page_size);
        let generation = self.browser.requests.next_generation();
        self.request_view(query, generation, cx);
    }

    pub(super) fn request_playlist_entries(
        &mut self,
        playlist_id: i64,
        offset: usize,
        cx: &mut Context<Self>,
    ) {
        self.browser.playlists.prepare_entries(playlist_id);
        self.load_playlist_entries(playlist_id, offset, cx);
        self.request_entry_selection(cx);
    }

    fn load_playlist_entries(&mut self, playlist_id: i64, offset: usize, cx: &mut Context<Self>) {
        let page_size = self.view_page_size();
        debug_assert_eq!(
            self.browser.playlists.entries().playlist_id,
            Some(playlist_id)
        );
        let query = self.browser.playlists.prepare_page(true, offset, page_size);
        let generation = self.browser.requests.next_generation();
        self.request_view(query, generation, cx);
    }

    pub(super) fn request_entry_selection(&mut self, cx: &mut Context<Self>) {
        let Some(playlist_id) = self.browser.playlists.entries().playlist_id else {
            return;
        };
        let Some(entry_id) = self.browser.playlists.selected_entry() else {
            return;
        };
        let generation = self.browser.requests.next_generation();
        let kind = ViewRequestKind::EntrySelection;
        self.browser.requests.begin(kind, generation);
        let library_revision = self.state.library.revision;
        let catalog_revision = self.state.library.playlist_revision;
        let entry_revision = self
            .state
            .library
            .playlist_entry_revisions
            .get(&playlist_id)
            .copied()
            .unwrap_or(0);
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            if !this
                .update(cx, |this, _| {
                    this.browser.requests.is_current(kind, generation)
                        && this.browser.playlists.entries().playlist_id == Some(playlist_id)
                        && this.browser.playlists.selected_entry() == Some(entry_id)
                })
                .unwrap_or(false)
            {
                return;
            }
            let response = cx
                .background_executor()
                .spawn(async move { handle.query(Query::PlaylistEntry { entry_id }) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Hidden windows still synchronize business state before accepting results.
                this.refresh(cx);
                if !this.browser.requests.is_current(kind, generation)
                    || this.browser.playlists.entries().playlist_id != Some(playlist_id)
                    || this.browser.playlists.selected_entry() != Some(entry_id)
                {
                    return;
                }
                if this.state.library.revision != library_revision
                    || this.state.library.playlist_revision != catalog_revision
                    || this
                        .state
                        .library
                        .playlist_entry_revisions
                        .get(&playlist_id)
                        .copied()
                        .unwrap_or(0)
                        != entry_revision
                    || !kind.revisions_match(
                        response.revisions,
                        &this.state.library,
                        Some(playlist_id),
                    )
                {
                    this.request_entry_selection(cx);
                    return;
                }
                this.browser.requests.cancel(kind);
                match response.result {
                    Ok(ViewResponse::PlaylistEntry(entry)) => {
                        this.browser.playlists.accept_entry_lookup(entry_id, entry);
                    }
                    Err(error) => this.error = Some(error.to_string()),
                    _ => this.error = Some("Unexpected playlist selection response".into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn ensure_playlist_range(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) {
        if let Some(offset) =
            self.browser
                .playlists
                .demand_page(false, range, self.view_page_size())
        {
            self.load_playlist_summaries(offset, cx);
        }
    }
    pub(super) fn ensure_entry_range(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.browser.playlists.entries().playlist_id
            && let Some(offset) =
                self.browser
                    .playlists
                    .demand_page(true, range, self.view_page_size())
        {
            self.load_playlist_entries(id, offset, cx);
        }
    }

    pub(super) fn retry_view_page(&mut self, kind: ViewRequestKind, cx: &mut Context<Self>) {
        let page_size = self.view_page_size();
        match kind {
            ViewRequestKind::Summaries => {
                if let Some(offset) = self.browser.playlists.retry_page(false, page_size) {
                    self.load_playlist_summaries(offset, cx);
                }
            }
            ViewRequestKind::Entries => {
                if let Some(id) = self.browser.playlists.entries().playlist_id
                    && let Some(offset) = self.browser.playlists.retry_page(true, page_size)
                {
                    self.load_playlist_entries(id, offset, cx);
                }
            }
            ViewRequestKind::Queue => {
                self.browser.queue.rows.retry();
                if self.browser.queue.busy {
                    self.refresh_queue(cx);
                }
                let range = self.browser.queue.rows.visible();
                self.ensure_queue_range(range, cx);
            }
            _ => {}
        }
    }

    pub(super) fn toggle_library_view(&mut self, cx: &mut Context<Self>) {
        self.browser.library.toggle_view();
        self.request_directory_page(library::TreeBranch::default(), 0, cx);
        cx.notify();
    }

    pub(super) fn refresh_favorite_filter(&mut self, cx: &mut Context<Self>) {
        self.browser.library.toggle_favorites();
        self.request_directory_page(library::TreeBranch::default(), 0, cx);
    }

    pub(super) fn refresh_search(&mut self, cx: &mut Context<Self>) {
        let query = self.value(Field::Search, cx).trim().to_lowercase();
        self.browser.library.refresh_search(query);
        self.request_directory_page(library::TreeBranch::default(), 0, cx);
    }

    pub(super) fn refresh_filter(&mut self, cx: &mut Context<Self>) {
        self.browser.library.refresh_filter();
        let query = self.value(Field::Search, cx).trim().to_lowercase();
        self.browser.library.prepare_query(query);
        self.request_directory_page(library::TreeBranch::default(), 0, cx);
    }

    pub(super) fn refresh_page_size(&mut self, cx: &mut Context<Self>) {
        // Invalidate query tokens and payload, not identity, metadata drafts, or scroll position.
        self.browser.requests.invalidate_pages();
        self.browser.library.refresh_tree(self.view_page_size());
        self.request_directory_page(library::TreeBranch::default(), 0, cx);
        self.request_playlist_summaries(
            self.browser.playlists.summaries().rows.visible().start,
            cx,
        );
        if let Some(id) = self.browser.playlists.entries().playlist_id {
            self.request_playlist_entries(
                id,
                self.browser.playlists.entries().rows.visible().start,
                cx,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ViewRequestKind, ViewRequests};
    use crate::{
        projection::GuiLibrarySnapshot,
        response::{PlaylistEntryRevision, QueryRevisions},
    };
    use std::sync::Arc;

    #[test]
    fn playlist_queries_reject_relevant_reorders_not_other_playlists() {
        let mut library = GuiLibrarySnapshot::default();
        let catalog = QueryRevisions::default();
        let old = QueryRevisions {
            playlist_entries: Some(PlaylistEntryRevision {
                playlist_id: 1,
                revision: 0,
            }),
            ..catalog
        };
        Arc::make_mut(&mut library.playlist_entry_revisions).insert(2, 1);
        assert!(ViewRequestKind::Summaries.revisions_match(catalog, &library, Some(1)));
        assert!(ViewRequestKind::Selection.revisions_match(catalog, &library, Some(1)));
        assert!(ViewRequestKind::Entries.revisions_match(old, &library, Some(1)));
        assert!(ViewRequestKind::EntrySelection.revisions_match(old, &library, Some(1)));
        Arc::make_mut(&mut library.playlist_entry_revisions).insert(1, 1);
        assert!(ViewRequestKind::Summaries.revisions_match(catalog, &library, Some(1)));
        assert!(!ViewRequestKind::Entries.revisions_match(old, &library, Some(1)));
        assert!(!ViewRequestKind::EntrySelection.revisions_match(old, &library, Some(1)));
        let current = QueryRevisions {
            playlist_entries: Some(PlaylistEntryRevision {
                playlist_id: 1,
                revision: 1,
            }),
            ..catalog
        };
        assert!(ViewRequestKind::Entries.revisions_match(current, &library, Some(1)));
        assert!(!ViewRequestKind::Entries.revisions_match(current, &library, Some(2)));
        let before_metadata = library.clone();
        library.revision += 1;
        assert!(!ViewRequestKind::Entries.revisions_match(current, &library, Some(1)));
        assert!(!ViewRequestKind::Summaries.revisions_match(catalog, &library, Some(1)));
        library = before_metadata;
        library.playlist_revision += 1;
        library.playlist_entry_revisions = Arc::default();
        // Epoch change rejects the old zero token even after the scoped map resets.
        assert!(!ViewRequestKind::Entries.revisions_match(old, &library, Some(1)));
        assert!(!ViewRequestKind::EntrySelection.revisions_match(catalog, &library, Some(1)));
    }

    #[test]
    fn entry_refresh_supersedes_entry_results_without_cancelling_catalog_requests() {
        let mut requests = ViewRequests::default();
        for kind in [
            ViewRequestKind::Summaries,
            ViewRequestKind::Selection,
            ViewRequestKind::Entries,
            ViewRequestKind::EntrySelection,
        ] {
            requests.begin(kind, 1);
        }
        requests.begin(ViewRequestKind::Entries, 2);
        requests.begin(ViewRequestKind::EntrySelection, 3);
        assert!(requests.is_current(ViewRequestKind::Summaries, 1));
        assert!(requests.is_current(ViewRequestKind::Selection, 1));
        assert!(!requests.is_current(ViewRequestKind::Entries, 1));
        assert!(!requests.is_current(ViewRequestKind::EntrySelection, 1));
        assert!(requests.is_current(ViewRequestKind::Entries, 2));
        assert!(requests.is_current(ViewRequestKind::EntrySelection, 3));
    }

    #[test]
    fn empty_playlist_completion_ends_loading_and_failed_refresh_stays_retryable() {
        let mut browser = super::BrowserState::default();
        let kind = ViewRequestKind::Entries;
        browser.playlists.select(41, "empty".into(), 0);
        browser.playlists.prepare_entries(41);
        browser.playlists.prepare_page(true, 0, 20);
        browser.requests.begin(kind, 1);
        assert_eq!(browser.playlists.entries().rows.len(), 1);
        assert!(browser.requests.pending(kind));

        assert!(browser.requests.finish(kind, 1));
        browser.playlists.accept_entries(crate::model::Page {
            total: 0,
            rows: Vec::new(),
        });
        assert_eq!(browser.playlists.selected(), Some(41));
        assert!(browser.playlists.entries().rows.is_empty());
        assert!(!browser.requests.pending(kind));
        assert_eq!(browser.playlists.demand_page(true, 0..1, 20), None);

        browser.playlists.prepare_entries(41);
        browser.playlists.prepare_page(true, 0, 20);
        browser.requests.begin(kind, 2);
        assert!(browser.requests.pending(kind));
        assert!(browser.requests.finish(kind, 2));
        browser.playlists.fail_page(true, "unavailable".into());
        assert!(browser.playlists.entries().rows.is_empty());
        assert!(!browser.requests.pending(kind));
        assert_eq!(
            browser.playlists.entries().rows.error(0),
            Some("unavailable")
        );

        browser.playlists.prepare_entries(41);
        browser.playlists.prepare_page(true, 0, 20);
        browser.requests.begin(kind, 3);
        assert!(browser.requests.pending(kind));
        assert_eq!(browser.playlists.entries().rows.error(0), None);
        assert!(browser.requests.finish(kind, 3));
        browser.playlists.accept_entries(crate::model::Page {
            total: 0,
            rows: Vec::new(),
        });
        assert!(browser.playlists.entries().rows.is_empty());
        assert!(!browser.requests.pending(kind));
    }

    #[test]
    fn finishing_a_superseded_page_cannot_end_its_replacement() {
        let mut requests = ViewRequests::default();
        requests.begin(ViewRequestKind::Entries, 1);
        requests.begin(ViewRequestKind::Entries, 2);
        assert!(!requests.finish(ViewRequestKind::Entries, 1));
        assert!(requests.pending(ViewRequestKind::Entries));
        assert!(requests.is_current(ViewRequestKind::Entries, 2));
        assert!(requests.finish(ViewRequestKind::Entries, 2));
        assert!(!requests.pending(ViewRequestKind::Entries));
        assert!(!requests.finish(ViewRequestKind::Entries, 2));
    }

    #[test]
    fn empty_catalog_releases_selected_playlist_and_pending_entry_requests() {
        let mut browser = super::BrowserState::default();
        assert!(browser.playlists.entries().rows.is_empty());
        browser.playlists.select(41, "deleted".into(), 0);
        browser.playlists.prepare_entries(41);
        browser.playlists.prepare_page(true, 0, 20);
        browser.playlists.prepare_page(false, 0, 20);
        for kind in [
            ViewRequestKind::Summaries,
            ViewRequestKind::Selection,
            ViewRequestKind::Entries,
            ViewRequestKind::EntrySelection,
        ] {
            browser.requests.begin(kind, 1);
        }

        assert!(browser.requests.finish(ViewRequestKind::Summaries, 1));
        browser.playlists.fail_page(false, "unavailable".into());
        assert!(!browser.requests.pending(ViewRequestKind::Summaries));
        assert_eq!(
            browser.playlists.summaries().rows.error(0),
            Some("unavailable")
        );
        assert_eq!(browser.playlists.selected(), Some(41));
        assert_eq!(browser.playlists.retry_page(false, 20), Some(0));
        browser.playlists.prepare_page(false, 0, 20);
        browser.requests.begin(ViewRequestKind::Summaries, 2);
        assert!(browser.requests.finish(ViewRequestKind::Summaries, 2));
        browser.accept_playlist_summaries(crate::model::Page {
            total: 0,
            rows: Vec::new(),
        });
        assert_eq!(browser.playlists.selected(), None);
        assert_eq!(browser.playlists.entries().playlist_id, None);
        assert!(browser.playlists.entries().rows.is_empty());
        assert!(browser.playlists.summaries().rows.is_empty());
        for kind in [
            ViewRequestKind::Summaries,
            ViewRequestKind::Selection,
            ViewRequestKind::Entries,
            ViewRequestKind::EntrySelection,
        ] {
            assert!(!browser.requests.pending(kind));
            assert!(!browser.requests.finish(kind, 1));
        }
        assert_eq!(browser.playlists.demand_page(true, 0..1, 20), None);
    }

    #[test]
    fn clearing_selection_releases_entry_owner_and_outstanding_requests() {
        let mut browser = super::BrowserState::default();
        browser.playlists.select(41, "selected".into(), 0);
        browser.playlists.prepare_entries(41);
        browser.playlists.prepare_page(true, 0, 20);
        for kind in [
            ViewRequestKind::Selection,
            ViewRequestKind::Entries,
            ViewRequestKind::EntrySelection,
        ] {
            browser.requests.begin(kind, 1);
        }

        browser.clear_selection();
        assert_eq!(browser.playlists.selected(), None);
        assert_eq!(browser.playlists.entries().playlist_id, None);
        assert!(browser.playlists.entries().rows.is_empty());
        for kind in [
            ViewRequestKind::Selection,
            ViewRequestKind::Entries,
            ViewRequestKind::EntrySelection,
        ] {
            assert!(!browser.requests.pending(kind));
            assert!(!browser.requests.finish(kind, 1));
        }
    }
}
