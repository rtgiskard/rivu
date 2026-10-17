//! View queries and paged buffers, including stale-response rejection and cancellation.
//! GuiApp retains the entity and the single frame scheduler.

use super::browser::requests::{DIRECTORY_WORKERS, ViewRequest};
use super::*;
use crate::{model::Query, response::ViewResponse};

fn library_search_counts(
    handle: &crate::core::CoreHandle,
    request: &browser::SearchCountRequest,
) -> Option<browser::SearchCountResult> {
    let browser::SearchCountRequest {
        query: worker_query,
        roots,
        revisions,
        ticket,
        ..
    } = request;
    let Query::LibraryRootPage {
        query,
        favorite,
        missing,
        sort,
        ..
    } = worker_query
    else {
        unreachable!()
    };
    let mut matched = 0;
    let mut total = 0;
    let mut root_counts = std::collections::HashMap::new();
    for (index, root) in roots.iter().enumerate() {
        // Nested configured roots describe the same tracks, not extra occurrences.
        if roots.iter().enumerate().any(|(other_index, parent)| {
            index != other_index
                && root.starts_with(parent)
                && (root != parent || other_index < index)
        }) {
            continue;
        }
        let response = handle.query_if_current(
            Query::LibraryRootPage {
                path: root.clone(),
                query: query.clone(),
                favorite: *favorite,
                missing: *missing,
                sort: *sort,
                offset: 0,
                limit: 0,
            },
            ticket.clone(),
        );
        if worker_query.stamp(response.revisions) != worker_query.stamp(*revisions) {
            return None;
        }
        let Ok(ViewResponse::LibraryPage(page)) = response.result else {
            return None;
        };
        let root_matched = page.total;
        let root_total = if query.is_none() && favorite.is_none() && missing.is_none() {
            root_matched
        } else {
            let response = handle.query_if_current(
                Query::LibraryRootPage {
                    path: root.clone(),
                    query: None,
                    favorite: None,
                    missing: None,
                    sort: *sort,
                    offset: 0,
                    limit: 0,
                },
                ticket.clone(),
            );
            if worker_query.stamp(response.revisions) != worker_query.stamp(*revisions) {
                return None;
            }
            let Ok(ViewResponse::LibraryPage(page)) = response.result else {
                return None;
            };
            page.total
        };
        matched += root_matched;
        total += root_total;
        root_counts.insert(root.clone(), (root_matched, root_total));
    }
    Some(browser::SearchCountResult {
        totals: (matched, total),
        roots: root_counts,
    })
}

impl GuiApp {
    pub(super) fn apply_playlist_search(&mut self, query: Option<String>, cx: &mut Context<Self>) {
        if self.browser.playlists().summaries().query == query {
            return;
        }
        self.begin_manual_intent();
        if !self.metadata_protected(cx) {
            self.clear_metadata_selection(cx);
        }
        self.content_menu = None;
        if matches!(
            self.playlist_action,
            Some(super::playlists::PlaylistAction::Menu(..))
        ) {
            self.playlist_action = None;
        }
        self.browser.search_playlists(query);
        self.playlist_entries_search
            .update(cx, |search, cx| search.reset(cx));
        if self.playlist_view_demand(ViewRequestKind::Summaries) {
            self.request_playlist_summaries(0, cx);
        }
        cx.notify();
    }

    pub(super) fn apply_playlist_entries_search(
        &mut self,
        query: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.browser.playlists().entries().query == query {
            return;
        }
        self.begin_manual_intent();
        if !self.metadata_protected(cx) {
            self.clear_metadata_selection(cx);
        }
        self.content_menu = None;
        self.browser.search_playlist_entries(query);
        if let Some(id) = self.browser.playlists().selected() {
            self.sync_entry_projection(cx);
            if self.panel_demand("playlists") {
                self.request_playlist_entries(id, 0, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn cancel_window_reads(&mut self) {
        self.browser.cancel_window_reads();
        self.cancel_metadata_read();
    }

    pub(super) fn start_loading_text(
        &mut self,
        owner: loading::LoadingOwner,
        token: u64,
        cx: &mut Context<Self>,
    ) {
        self.loading_text.begin(owner, token, Instant::now());
        self.loading_text
            .schedule(cx, |this| &mut this.loading_text);
    }

    pub(super) fn finish_loading_text(
        &mut self,
        owner: &loading::LoadingOwner,
        token: u64,
        cx: &mut Context<Self>,
    ) {
        self.loading_text.finish(owner, token);
        self.loading_text
            .schedule(cx, |this| &mut this.loading_text);
    }

    pub(super) fn sync_loading_text(&mut self, cx: &mut Context<Self>) {
        let library_visible = self.panel_demand("library");
        let playlists_visible = self.panel_demand("playlists");
        let queue_visible = self.panel_demand("queue");
        let browser = &self.browser;
        self.loading_text.retain(|owner, token| match owner {
            loading::LoadingOwner::Branch(branch) => {
                library_visible && browser.library.tree().request_pending(branch, token)
            }
            loading::LoadingOwner::View(ViewRequestKind::Queue) => {
                queue_visible && browser.queue.page_loading(token)
            }
            loading::LoadingOwner::View(kind) => {
                playlists_visible && browser.requests.is_current(*kind, token)
            }
        });
        self.loading_text
            .schedule(cx, |this| &mut this.loading_text);
    }

    fn playlist_view_demand(&self, kind: ViewRequestKind) -> bool {
        self.panel_demand("playlists")
            || (kind == ViewRequestKind::Summaries
                && self.window_visible
                && !self.state.system.shutting_down
                && self
                    .content_menu
                    .as_ref()
                    .is_some_and(|menu| menu.playlists))
    }

    pub(super) fn request_root_counts(&mut self, cx: &mut Context<Self>) {
        if !self.panel_demand("library") {
            return;
        }
        self.request_library_search_counts(cx);
        if !self.browser.requests.root_count_workers.is_empty()
            || self.browser.library.root_counts_settled()
        {
            return;
        }
        let roots = &self.state.system.config.library_roots;
        let structure_revision = self.state.library.structure_revision;
        let (generation, batches) = self
            .browser
            .library
            .begin_root_counts(roots, structure_revision);
        self.browser
            .requests
            .root_count_workers
            .extend(batches.iter().copied());
        for offset in batches {
            let batch =
                roots[offset..(offset + crate::model::MAX_QUERY_ROWS).min(roots.len())].to_vec();
            let handle = self.handle.clone();
            let ticket = self.browser.library.root_count_ticket();
            cx.spawn(async move |this, cx| {
                let response = cx
                    .background_executor()
                    .spawn(async move {
                        handle.query_if_current(Query::LibraryRootCounts { roots: batch }, ticket)
                    })
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

    fn request_view(&mut self, prepared: ViewRequest, cx: &mut Context<Self>) {
        let kind = prepared.kind();
        if kind != ViewRequestKind::Selection
            && self.panel_demand("playlists")
            && let Some(generation) = prepared.pending_generation()
        {
            self.start_loading_text(loading::LoadingOwner::View(kind), generation, cx);
        }
        let Some((read, ticket)) = prepared.into_read() else {
            return;
        };
        let generation = read.generation;
        let query = read.query.clone();
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(read.delay).await;
            if !this
                .update(cx, |this, cx| {
                    this.refresh(cx);
                    let revisions = this
                        .state
                        .query_revisions(this.browser.playlists().entries().playlist_id);
                    let current = this.playlist_view_demand(kind)
                        && this.browser.playlist_read_current(&read, revisions);
                    if !current {
                        this.finish_loading_text(
                            &loading::LoadingOwner::View(kind),
                            generation,
                            cx,
                        );
                        this.browser.abandon_playlist_read(&read);
                        this.dispatch_queued_view(kind, cx);
                        cx.notify();
                    }
                    current
                })
                .unwrap_or(false)
            {
                return;
            }
            let response = cx
                .background_executor()
                .spawn(async move { handle.query_if_current(query, ticket) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.finish_loading_text(&loading::LoadingOwner::View(kind), generation, cx);
                this.refresh(cx);
                let revisions = this
                    .state
                    .query_revisions(this.browser.playlists().entries().playlist_id);
                if let Some(error) = this
                    .browser
                    .complete_playlist_read(&read, revisions, response)
                {
                    this.error = Some(error);
                }
                this.dispatch_queued_view(kind, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn dispatch_queued_view(&mut self, kind: ViewRequestKind, cx: &mut Context<Self>) {
        let revisions = self
            .state
            .query_revisions(self.browser.playlists().entries().playlist_id);
        let demanded = self.playlist_view_demand(kind);
        let prepared = self.browser.queued_playlist_read(kind, demanded, revisions);
        self.request_view(prepared, cx);
    }

    fn request_library_search_counts(&mut self, cx: &mut Context<Self>) {
        let (changed, request) = self.browser.library.prepare_search_counts(
            &self.state.system.config.library_roots,
            self.state.query_revisions(None),
        );
        if changed {
            self.library_search
                .update(cx, |search, cx| search.clear_counts(cx));
        }
        let Some(request) = request else {
            return;
        };
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let (request, counts) = cx
                .background_executor()
                .spawn(async move {
                    let counts = library_search_counts(&handle, &request);
                    (request, counts)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                if this.browser.library.accept_search_counts(
                    request,
                    &this.state.system.config.library_roots,
                    this.state.query_revisions(None),
                    counts,
                ) {
                    if let Some((matched, total)) = this.browser.library.search_counts() {
                        this.library_search
                            .update(cx, |search, cx| search.set_counts(matched, total, cx));
                    }
                } else if this.panel_demand("library") {
                    this.request_library_search_counts(cx);
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
        if !self.panel_demand("library") {
            return;
        }
        self.request_library_search_counts(cx);
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
        self.start_loading_text(
            loading::LoadingOwner::Branch(branch.clone()),
            generation,
            cx,
        );
        self.browser
            .requests
            .directory_workers
            .insert(branch.clone());
        let ticket = self.browser.library.projection.ticket();
        let handle = self.handle.clone();
        let favorite = self.browser.library.favorites_only().then_some(true);
        let query = self.browser.library.buffer().query.clone();
        let tree_active = self.browser.library.tree_active();
        let missing = self.browser.library.buffer().missing;
        let sort = self.browser.library.buffer().sort;
        cx.spawn(async move |this, cx| {
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
                    this.finish_loading_text(
                        &loading::LoadingOwner::Branch(branch.clone()),
                        generation,
                        cx,
                    );
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
                    handle.query_if_current(
                        if tree_active {
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
                        },
                        ticket,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.finish_loading_text(
                    &loading::LoadingOwner::Branch(branch.clone()),
                    generation,
                    cx,
                );
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
                    if this.browser.library.projection.generation() != owner_generation {
                        this.browser.library.cancel_directory(&branch, generation);
                        return;
                    }
                    let result = match response.result {
                        Ok(result) => result,
                        Err(error) => {
                            this.browser.library.fail_directory(&branch, generation);
                            this.error = Some(error.to_string());
                            return;
                        }
                    };
                    if !this.browser.library.projection.accepts(
                        &owner_query,
                        owner_generation,
                        response.revisions,
                    ) {
                        this.browser.library.cancel_directory(&branch, generation);
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
                })();
                this.dispatch_directory_demand(cx);
                cx.notify();
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
        let prepared = self
            .browser
            .prepare_playlist_lookup(self.state.query_revisions(None));
        self.request_view(prepared, cx);
    }

    pub(super) fn request_playlist_summaries(&mut self, offset: usize, cx: &mut Context<Self>) {
        let prepared = self.browser.request_playlist_summaries(
            offset,
            self.view_page_size(),
            self.state.query_revisions(None),
        );
        self.request_view(prepared, cx);
    }

    pub(super) fn request_playlist_entries(
        &mut self,
        playlist_id: i64,
        offset: usize,
        cx: &mut Context<Self>,
    ) {
        let prepared = self.browser.request_playlist_entries(
            playlist_id,
            offset,
            self.view_page_size(),
            self.state.query_revisions(Some(playlist_id)),
        );
        self.request_view(prepared, cx);
    }

    pub(super) fn ensure_playlist_range(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) {
        if !self.playlist_view_demand(ViewRequestKind::Summaries) {
            return;
        }
        let revisions = self
            .state
            .query_revisions(self.browser.playlists().entries().playlist_id);
        let prepared =
            self.browser
                .demand_playlist_summaries(range, self.view_page_size(), revisions);
        self.request_view(prepared, cx);
    }
    pub(super) fn ensure_entry_range(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) {
        if !self.panel_demand("playlists") {
            return;
        }
        let revisions = self
            .state
            .query_revisions(self.browser.playlists().entries().playlist_id);
        let prepared =
            self.browser
                .demand_playlist_entries(range, self.view_page_size(), revisions);
        self.request_view(prepared, cx);
    }

    pub(super) fn retry_view_page(&mut self, kind: ViewRequestKind, cx: &mut Context<Self>) {
        let page_size = self.view_page_size();
        match kind {
            ViewRequestKind::Summaries | ViewRequestKind::Entries => {
                let revisions = self
                    .state
                    .query_revisions(self.browser.playlists().entries().playlist_id);
                let prepared = if kind == ViewRequestKind::Entries {
                    self.browser.retry_playlist_entries(page_size, revisions)
                } else {
                    self.browser.retry_playlist_summaries(page_size, revisions)
                };
                self.request_view(prepared, cx);
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

    pub(super) fn apply_library_search(&mut self, query: Option<String>, cx: &mut Context<Self>) {
        let query = query.unwrap_or_default().trim().to_lowercase();
        if self
            .browser
            .library
            .buffer()
            .query
            .as_deref()
            .unwrap_or_default()
            == query
        {
            return;
        }
        self.browser.library.refresh_search(query);
        self.request_directory_page(library::TreeBranch::default(), 0, cx);
        cx.notify();
    }

    pub(super) fn refresh_filter(&mut self, cx: &mut Context<Self>) {
        self.browser.library.refresh_filter();
        self.request_directory_page(library::TreeBranch::default(), 0, cx);
    }

    pub(super) fn refresh_page_size(&mut self) {
        self.browser.configure_page_size(self.view_page_size());
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn library_search_counts_query_deduplicates_roots_and_resumes_cancelled_reads() {
        use crate::{
            config::Config,
            core::Runtime,
            gui::browser::LibraryBrowser,
            library::{ScanRecord, ScanResult},
            model::{MediaInfo, Query},
            store::Store,
        };

        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("music");
        let nested = parent.join("album");
        let sibling = directory.path().join("other");
        let roots = vec![
            nested.clone(),
            parent.clone(),
            parent.clone(),
            sibling.clone(),
        ];
        let store = Store::open(&directory.path().join("library.db")).unwrap();
        store
            .apply_scan(&ScanResult {
                roots: vec![directory.path().to_path_buf()],
                records: [
                    (parent.join("one.wav"), "needle"),
                    (nested.join("two.wav"), "other"),
                    (sibling.join("three.wav"), "needle"),
                    (sibling.join("four.wav"), "other"),
                    (directory.path().join("outside.wav"), "needle"),
                ]
                .into_iter()
                .map(|(path, title)| ScanRecord {
                    path,
                    size: 1,
                    modified_ns: 0,
                    fingerprint: None,
                    cue: None,
                    media: MediaInfo {
                        title: title.into(),
                        artist: String::new(),
                        album: String::new(),
                        duration: Some(1.0),
                        bitrate_bps: None,
                        bitrate_exact: false,
                        track_number: None,
                        disc_number: None,
                        bits_per_sample: None,
                        release_date: None,
                        codec: "PCM_S16LE".into(),
                        channels: 1,
                        sample_rate: 8000,
                    },
                })
                .collect(),
                ..Default::default()
            })
            .unwrap();
        drop(store);
        let config_path = directory.path().join("config.toml");
        Config {
            volume: 0.0,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            ..Config::default()
        }
        .save(&config_path)
        .unwrap();
        let runtime = Runtime::start(directory.path(), &config_path).unwrap();
        let response = runtime
            .handle
            .query(Query::LibraryRootCounts { roots: Vec::new() });
        assert!(response.result.is_ok());
        let revisions = response.revisions;
        let mut library = LibraryBrowser::default();
        let (_, request) = library.prepare_search_counts(&roots, revisions);
        let request = request.unwrap();
        let result = super::library_search_counts(&runtime.handle, &request).unwrap();
        assert_eq!(result.totals, (4, 4));
        assert_eq!(result.roots.len(), 2);
        assert_eq!(result.roots.get(&parent), Some(&(2, 2)));
        assert_eq!(result.roots.get(&sibling), Some(&(2, 2)));
        assert!(!result.roots.contains_key(&nested));
        assert!(library.accept_search_counts(request, &roots, revisions, Some(result)));
        library.refresh_search("needle".into());
        let (_, request) = library.prepare_search_counts(&roots, revisions);
        let request = request.unwrap();
        library.cancel_pending_reads();
        let cancelled = super::library_search_counts(&runtime.handle, &request);
        assert!(cancelled.is_none());
        assert!(!library.accept_search_counts(request, &roots, revisions, cancelled));
        let (_, request) = library.prepare_search_counts(&roots, revisions);
        let request = request.unwrap();
        let result = super::library_search_counts(&runtime.handle, &request).unwrap();
        assert_eq!(result.totals, (2, 4));
        assert!(library.accept_search_counts(request, &roots, revisions, Some(result)));
        assert_eq!(library.search_counts(), Some((2, 4)));
        assert_eq!(library.root_count_label(&parent), "1 / 2");
        assert_eq!(library.root_count_label(&sibling), "1 / 2");
        let (_, request) = library.prepare_search_counts(&[], revisions);
        let request = request.unwrap();
        let result = super::library_search_counts(&runtime.handle, &request).unwrap();
        assert_eq!(result.totals, (0, 0));
        assert!(result.roots.is_empty());
        assert!(library.accept_search_counts(request, &[], revisions, Some(result)));
        assert_eq!(library.search_counts(), Some((0, 0)));
    }
}
