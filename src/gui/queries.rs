//! View queries and paged buffers, including stale-response rejection and cancellation.
//! GuiApp retains the entity and the single frame scheduler.

use super::*;
use crate::{
    model::{LibrarySort, Query},
    response::ViewResponse,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ViewRequestKind {
    Ranking,
    PlaylistSummaries,
    PlaylistEntries,
    PlaylistSelection,
    Stats,
}

impl ViewRequestKind {
    fn for_query(query: &Query) -> Option<Self> {
        Some(match query {
            Query::LibraryPage {
                sort: LibrarySort::MostPlayed,
                ..
            } => Self::Ranking,
            Query::PlaylistSummaries { .. } => Self::PlaylistSummaries,
            Query::PlaylistEntries { .. } => Self::PlaylistEntries,
            Query::PlaylistSummaryBatch { .. } => Self::PlaylistSelection,
            Query::LibraryStats => Self::Stats,
            _ => return None,
        })
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

    pub(super) fn cancel(&mut self, kind: ViewRequestKind) {
        self.generations.remove(&kind);
    }

    fn invalidate_pages(&mut self) {
        // Existence lookups are not pages; changing page size cannot supersede them.
        self.generations
            .retain(|kind, _| *kind == ViewRequestKind::PlaylistSelection);
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
        let delay = if matches!(&query, Query::LibraryPage { query: Some(_), .. }) {
            crate::model::SEARCH_DEBOUNCE
        } else {
            Duration::from_millis(80)
        };
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            // Search waits for a quiet interval; generations reject superseded queries.
            cx.background_executor().timer(delay).await;
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
                if !this.browser.requests.is_current(kind, generation) {
                    return;
                }
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        let message = error.to_string();
                        match kind {
                            ViewRequestKind::PlaylistSummaries => {
                                this.browser.playlists.fail_page(false, message.clone())
                            }
                            ViewRequestKind::PlaylistEntries => {
                                this.browser.playlists.fail_page(true, message.clone())
                            }
                            _ => {}
                        }
                        this.error = Some(message);
                        cx.notify();
                        return;
                    }
                };
                if matches!(kind, ViewRequestKind::Ranking | ViewRequestKind::Stats)
                    && revisions.library != this.state.library.revision
                {
                    match kind {
                        ViewRequestKind::Ranking => {
                            this.request_ranking_page(this.browser.history.offset(), cx)
                        }
                        ViewRequestKind::Stats => this.request_stats(cx),
                        _ => {}
                    }
                    return;
                }
                if matches!(
                    kind,
                    ViewRequestKind::PlaylistSummaries
                        | ViewRequestKind::PlaylistEntries
                        | ViewRequestKind::PlaylistSelection
                ) && revisions.playlist != this.state.library.playlist_revision
                {
                    match kind {
                        ViewRequestKind::PlaylistSummaries => this.request_playlist_summaries(
                            this.browser.playlists.summaries().offset,
                            cx,
                        ),
                        ViewRequestKind::PlaylistEntries => {
                            if let Some(id) = this.browser.playlists.entries().playlist_id {
                                this.request_playlist_entries(
                                    id,
                                    this.browser.playlists.entries().offset,
                                    cx,
                                );
                            }
                        }
                        ViewRequestKind::PlaylistSelection => this.request_playlist_selection(cx),
                        _ => {}
                    }
                    return;
                }
                match result {
                    ViewResponse::LibraryPage(page) if kind == ViewRequestKind::Ranking => {
                        if this.browser.history.offset() > 0
                            && this.browser.history.offset() >= page.total
                        {
                            let last = page.total.saturating_sub(1) / this.view_page_size()
                                * this.view_page_size();
                            this.request_ranking_page(last, cx);
                            return;
                        }
                        this.browser.history.accept(page);
                    }
                    ViewResponse::PlaylistSummaries(page)
                        if kind == ViewRequestKind::PlaylistSummaries =>
                    {
                        this.browser.playlists.accept_summaries(page);
                    }
                    ViewResponse::PlaylistEntries(page)
                        if kind == ViewRequestKind::PlaylistEntries =>
                    {
                        this.browser.playlists.accept_entries(page);
                    }
                    ViewResponse::PlaylistSummaryBatch(batch)
                        if kind == ViewRequestKind::PlaylistSelection =>
                    {
                        if let Some(id) = selected_playlist {
                            this.browser
                                .accept_playlist_lookup(id, batch.rows.into_iter().next());
                        }
                    }
                    ViewResponse::LibraryStats(stats) if kind == ViewRequestKind::Stats => {
                        this.browser.history.accept_stats(stats)
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
        if let Some(id) = self.browser.playlists.entries().playlist_id {
            if let Some(offset) =
                self.browser
                    .playlists
                    .demand_page(true, range, self.view_page_size())
            {
                self.load_playlist_entries(id, offset, cx);
            }
        }
    }

    pub(super) fn retry_view_page(&mut self, kind: ViewRequestKind, cx: &mut Context<Self>) {
        let page_size = self.view_page_size();
        match kind {
            ViewRequestKind::PlaylistSummaries => {
                if let Some(offset) = self.browser.playlists.retry_page(false, page_size) {
                    self.load_playlist_summaries(offset, cx);
                }
            }
            ViewRequestKind::PlaylistEntries => {
                if let Some(id) = self.browser.playlists.entries().playlist_id
                    && let Some(offset) = self.browser.playlists.retry_page(true, page_size)
                {
                    self.load_playlist_entries(id, offset, cx);
                }
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
        // The missing filter retains its flat-view behavior.
        self.browser.library.refresh_filter();
        let query = self.value(Field::Search, cx).trim().to_lowercase();
        self.browser.library.prepare_query(query);
        self.request_directory_page(library::TreeBranch::default(), 0, cx);
    }
    pub(super) fn request_ranking(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.request_ranking_page(offset, cx);
        self.request_stats(cx);
    }

    fn request_ranking_page(&mut self, offset: usize, cx: &mut Context<Self>) {
        let page_size = self.view_page_size();
        let offset = offset / page_size * page_size;
        self.browser.history.begin(offset);
        let generation = self.browser.requests.next_generation();
        self.request_view(
            Query::LibraryPage {
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::MostPlayed,
                offset,
                limit: page_size,
            },
            generation,
            cx,
        );
    }

    fn request_stats(&mut self, cx: &mut Context<Self>) {
        let generation = self.browser.requests.next_generation();
        self.request_view(Query::LibraryStats, generation, cx);
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
        self.request_ranking(0, cx);
    }
}
