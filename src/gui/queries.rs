//! View queries and paged buffers, including stale-response rejection and cancellation.
//! GuiApp retains the entity and the single frame scheduler.

use super::*;
use crate::{model::Query, projection::QueryProjection, response::ViewResponse};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ViewRequestKind {
    Summaries,
    Entries,
    Selection,
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
}
const DIRECTORY_WORKERS: usize = 4;

#[derive(Default)]
pub(super) struct ViewRequests {
    generation: u64,
    generations: HashMap<ViewRequestKind, u64>,
    projections: HashMap<ViewRequestKind, QueryProjection>,
    workers: HashSet<ViewRequestKind>,
    queued: HashMap<ViewRequestKind, (Query, u64)>,
    directory_workers: HashSet<library::TreeBranch>,
    directory_queued: std::collections::VecDeque<(library::TreeBranch, usize)>,
    root_count_workers: HashSet<usize>,
    selection_baseline: Option<(u64, crate::model::QueryStamp)>,
}

impl ViewRequests {
    pub(super) fn cancel_directory_demand(&mut self) {
        self.directory_queued.clear();
    }

    fn queue_directory(&mut self, branch: library::TreeBranch, offset: usize) {
        if let Some(index) = self
            .directory_queued
            .iter()
            .position(|(key, _)| key == &branch)
        {
            self.directory_queued.remove(index);
        } else if self.directory_queued.len() == DIRECTORY_WORKERS {
            self.directory_queued.pop_front();
        }
        // Unsubmitted demand owns no pending token; a still-visible dropped row
        // can demand again after any completion, without retaining a visited-tree log.
        self.directory_queued.push_back((branch, offset));
    }

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
        self.generations.remove(&kind);
        true
    }

    pub(super) fn pending(&self, kind: ViewRequestKind) -> bool {
        self.generations.contains_key(&kind)
    }

    pub(super) fn cancel(&mut self, kind: ViewRequestKind) {
        self.generations.remove(&kind);
        self.queued.remove(&kind);
        if let Some(projection) = self.projections.get_mut(&kind) {
            projection.cancel();
        }
    }

    fn invalidate_pages(&mut self) {
        // Existence lookups are not pages; changing page size cannot supersede them.
        for kind in [
            ViewRequestKind::Summaries,
            ViewRequestKind::Entries,
            ViewRequestKind::Queue,
        ] {
            self.cancel(kind);
        }
    }
}

impl GuiApp {
    pub(super) fn request_root_counts(&mut self, cx: &mut Context<Self>) {
        if !self.panel_demand("library") {
            return;
        }
        if !self.browser.requests.root_count_workers.is_empty() {
            return;
        }
        let roots = self.state.system.config.library_roots.clone();
        let structure_revision = self.state.library.structure_revision;
        let (generation, batches) = self
            .browser
            .library
            .begin_root_counts(&roots, structure_revision);
        self.browser
            .requests
            .root_count_workers
            .extend(batches.iter().copied());
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
                    this.browser.requests.root_count_workers.remove(&offset);
                    this.refresh(cx);
                    if !this.browser.library.counts_current(
                        generation,
                        &this.state.system.config.library_roots,
                        this.state.library.structure_revision,
                    ) {
                        this.request_root_counts(cx);
                        return;
                    }
                    let query = Query::LibraryRootCounts { roots: Vec::new() };
                    if query.stamp(response.revisions)
                        != query.stamp(this.state.query_revisions(None))
                    {
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
                    this.request_root_counts(cx);
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
        let revisions = self
            .state
            .query_revisions(self.browser.playlists.entries().playlist_id);
        let owner = self.browser.requests.projections.entry(kind).or_default();
        owner.sync(&query, revisions);
        let owner_generation = owner.generation();
        if !self.browser.requests.workers.insert(kind) {
            self.browser
                .requests
                .queued
                .insert(kind, (query, generation));
            return;
        }
        let selected_playlist = self.browser.playlists.selected();
        let selection_generation = self.browser.playlists.selection_generation();
        let request_query = query.clone();
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            // Coalesce quick changes; generations reject superseded queries.
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            // The physical gate remains held through cancellation and core completion.
            let response = cx
                .background_executor()
                .spawn(async move { handle.query(query) })
                .await;
            let revisions = response.revisions;
            let result = response.result;
            let _ = this.update(cx, |this, cx| {
                this.browser.requests.workers.remove(&kind);
                this.refresh(cx);
                if !this.browser.requests.is_current(kind, generation) {
                    this.dispatch_queued_view(kind, cx);
                    return;
                }
                let current = this
                    .state
                    .query_revisions(this.browser.playlists.entries().playlist_id);
                let owner = this.browser.requests.projections.entry(kind).or_default();
                owner.sync(&request_query, current);
                let accepted = owner.accepts(&request_query, owner_generation, revisions);
                this.browser.requests.finish(kind, generation);
                if kind == ViewRequestKind::Selection
                    && this.browser.playlists.selection_generation() != selection_generation
                {
                    this.dispatch_queued_view(kind, cx);
                    return;
                }
                if !accepted {
                    this.dispatch_queued_view(kind, cx);
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
                this.dispatch_queued_view(kind, cx);
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

    fn dispatch_queued_view(&mut self, kind: ViewRequestKind, cx: &mut Context<Self>) {
        if let Some((query, generation)) = self.browser.requests.queued.remove(&kind) {
            if !self.panel_demand("playlists") {
                self.browser.requests.cancel(kind);
                match kind {
                    ViewRequestKind::Summaries => self.browser.playlists.cancel_page(false),
                    ViewRequestKind::Entries => self.browser.playlists.cancel_page(true),
                    _ => {}
                }
                return;
            }
            self.request_view(query, generation, cx);
        }
    }

    pub(super) fn request_directory_page(
        &mut self,
        branch: library::TreeBranch,
        offset: usize,
        cx: &mut Context<Self>,
    ) {
        if !self.panel_demand("library") {
            return;
        }
        let page_size = self.view_page_size();
        if self
            .browser
            .library
            .sync_projection(self.state.query_revisions(None))
        {
            self.browser.requests.cancel_directory_demand();
        }
        let owner_query = self.browser.library.projection_query();
        let owner_generation = self.browser.library.projection.generation();
        if !branch.is_forest()
            && (self.browser.requests.directory_workers.contains(&branch)
                || self.browser.requests.directory_workers.len() >= DIRECTORY_WORKERS)
        {
            self.browser.requests.queue_directory(branch, offset);
            return;
        }
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
        self.browser
            .requests
            .directory_workers
            .insert(branch.clone());
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
                .update(cx, |this, cx| {
                    this.refresh(cx);
                    if this.panel_demand("library")
                        && this.browser.library.projection.accepts(
                            &owner_query,
                            owner_generation,
                            this.state.query_revisions(None),
                        )
                        && this.browser.library.directory_request_pending(
                            &branch,
                            generation,
                            query.as_deref(),
                            favorite,
                            tree_active,
                            missing,
                        )
                    {
                        return true;
                    }
                    this.browser.library.cancel_directory(&branch, generation);
                    this.browser.requests.directory_workers.remove(&branch);
                    this.dispatch_directory_demand(cx);
                    cx.notify();
                    false
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
                this.browser.requests.directory_workers.remove(&branch);
                this.refresh(cx);
                (|| {
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
                    if !this.browser.library.projection.accepts(
                        &owner_query,
                        owner_generation,
                        response.revisions,
                    ) {
                        return;
                    }
                    let page = match result {
                        ViewResponse::DirectoryPage(page) if tree_active => Some(page),
                        ViewResponse::LibraryPage(page) if !tree_active => {
                            Some(crate::model::Page {
                                total: page.total,
                                rows: page
                                    .rows
                                    .into_iter()
                                    .map(crate::model::DirectoryRow::Track)
                                    .collect(),
                            })
                        }
                        _ => None,
                    };
                    if let Some(page) = page {
                        this.browser
                            .library
                            .accept_directory(&branch, generation, page);
                    }
                    cx.notify();
                })();
                this.dispatch_directory_demand(cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn dispatch_directory_demand(&mut self, cx: &mut Context<Self>) {
        if !self.panel_demand("library") {
            self.browser.requests.directory_queued.clear();
            return;
        }
        while self.browser.requests.directory_workers.len() < DIRECTORY_WORKERS {
            let next = self
                .browser
                .requests
                .directory_queued
                .iter()
                .position(|(branch, _)| !self.browser.requests.directory_workers.contains(branch));
            let Some(index) = next else {
                break;
            };
            let (branch, offset) = self
                .browser
                .requests
                .directory_queued
                .remove(index)
                .expect("queued directory demand");
            self.request_directory_page(branch, offset, cx);
        }
    }

    pub(super) fn request_playlist_selection(&mut self, cx: &mut Context<Self>) {
        if !self.panel_demand("playlists") {
            return;
        }
        let Some(id) = self.browser.playlists.selected() else {
            return;
        };
        let query = Query::PlaylistSummaryBatch {
            after: id.checked_sub(1),
            limit: 1,
            expected: None,
        };
        let baseline = (
            self.browser.playlists.selection_generation(),
            query.stamp(self.state.query_revisions(None)),
        );
        if self.browser.requests.selection_baseline == Some(baseline) {
            return;
        }
        self.browser.requests.selection_baseline = Some(baseline);
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
        if !self.panel_demand("playlists") {
            return;
        }
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
        if !self.panel_demand("playlists") {
            return;
        }
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

    pub(super) fn refresh_page_size(&mut self) {
        // Invalidate query tokens and payload, not identity, metadata drafts, or scroll position.
        self.browser.requests.invalidate_pages();
        self.browser.library.refresh_tree(self.view_page_size());
        self.browser.playlists.invalidate_summaries();
        if let Some(id) = self.browser.playlists.entries().playlist_id {
            self.browser.playlists.prepare_entries(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ViewRequestKind, ViewRequests};
    use crate::projection::GuiLibrarySnapshot;
    use std::sync::Arc;

    #[test]
    fn playlist_query_owners_separate_metadata_catalog_and_occurrences() {
        use crate::model::Query;
        let mut library = GuiLibrarySnapshot::default();
        Arc::make_mut(&mut library.playlist_entry_revisions).insert(1, 0);
        let summaries = Query::PlaylistSummaries {
            offset: 0,
            limit: 20,
        };
        let entries = Query::PlaylistEntries {
            playlist_id: 1,
            offset: 0,
            limit: 20,
        };
        let compact = Query::PlaylistEntryLocations {
            playlist_id: 1,
            entry_ids: vec![3],
        };
        let before = library.query_revisions(Some(1));
        library.revision += 1;
        let metadata = library.query_revisions(Some(1));
        assert_eq!(summaries.stamp(before), summaries.stamp(metadata));
        assert_ne!(entries.stamp(before), entries.stamp(metadata));
        assert_eq!(compact.stamp(before), compact.stamp(metadata));
        library.playlist_revision += 1;
        let renamed = library.query_revisions(Some(1));
        assert_ne!(summaries.stamp(metadata), summaries.stamp(renamed));
        assert_eq!(entries.stamp(metadata), entries.stamp(renamed));
        assert_eq!(compact.stamp(metadata), compact.stamp(renamed));
        Arc::make_mut(&mut library.playlist_entry_revisions).remove(&1);
        let deleted = library.query_revisions(Some(1));
        assert_ne!(compact.stamp(renamed), compact.stamp(deleted));
    }

    #[test]
    fn cancelling_a_view_retains_its_physical_worker_gate() {
        let mut requests = ViewRequests::default();
        requests.begin(ViewRequestKind::Entries, 1);
        requests.workers.insert(ViewRequestKind::Entries);
        requests.cancel(ViewRequestKind::Entries);
        assert!(!requests.pending(ViewRequestKind::Entries));
        assert!(requests.workers.contains(&ViewRequestKind::Entries));
        assert!(!requests.workers.insert(ViewRequestKind::Entries));
        assert!(requests.workers.remove(&ViewRequestKind::Entries));
        assert!(requests.workers.insert(ViewRequestKind::Entries));
    }

    #[test]
    fn queued_directory_demand_coalesces_without_releasing_a_worker() {
        let mut requests = ViewRequests::default();
        let branch = super::library::TreeBranch::default();
        requests.directory_workers.insert(branch.clone());
        requests.queue_directory(branch.clone(), 0);
        requests.queue_directory(branch.clone(), 40);
        assert_eq!(requests.directory_queued.len(), 1);
        assert_eq!(
            requests.directory_queued.front(),
            Some(&(branch.clone(), 40))
        );
        requests.cancel_directory_demand();
        assert!(requests.directory_queued.is_empty());
        assert!(requests.directory_workers.contains(&branch));
    }

    #[test]
    fn entry_refresh_supersedes_entry_results_without_cancelling_catalog_requests() {
        let mut requests = ViewRequests::default();
        for kind in [
            ViewRequestKind::Summaries,
            ViewRequestKind::Selection,
            ViewRequestKind::Entries,
        ] {
            requests.begin(kind, 1);
        }
        requests.begin(ViewRequestKind::Entries, 2);
        assert!(requests.is_current(ViewRequestKind::Summaries, 1));
        assert!(requests.is_current(ViewRequestKind::Selection, 1));
        assert!(!requests.is_current(ViewRequestKind::Entries, 1));
        assert!(requests.is_current(ViewRequestKind::Entries, 2));
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
        for kind in [ViewRequestKind::Selection, ViewRequestKind::Entries] {
            browser.requests.begin(kind, 1);
        }

        browser.clear_selection();
        assert_eq!(browser.playlists.selected(), None);
        assert_eq!(browser.playlists.entries().playlist_id, None);
        assert!(browser.playlists.entries().rows.is_empty());
        for kind in [ViewRequestKind::Selection, ViewRequestKind::Entries] {
            assert!(!browser.requests.pending(kind));
            assert!(!browser.requests.finish(kind, 1));
        }
    }
}
