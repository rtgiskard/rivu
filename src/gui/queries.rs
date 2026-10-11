//! View queries and paged buffers, including stale-response rejection and cancellation.
//! GuiApp retains the entity and the single frame scheduler.

use super::paged::PagedRows;
use super::*;
use crate::{
    model::{LibraryRow, LibrarySort, PlaylistEntryRow, PlaylistSummary, Query},
    response::ViewResponse,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ViewRequestKind {
    Library,
    Ranking,
    PlaylistSummaries,
    PlaylistEntries,
    Track,
    Stats,
}

impl ViewRequestKind {
    fn for_query(query: &Query) -> Option<Self> {
        Some(match query {
            Query::LibraryPage {
                sort: LibrarySort::MostPlayed,
                ..
            } => Self::Ranking,
            Query::LibraryPage { .. } => Self::Library,
            Query::PlaylistSummaries { .. } => Self::PlaylistSummaries,
            Query::PlaylistEntries { .. } => Self::PlaylistEntries,
            Query::Track { .. } => Self::Track,
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

    fn contains(&self, kind: ViewRequestKind) -> bool {
        self.generations.contains_key(&kind)
    }

    fn cancel(&mut self, kind: ViewRequestKind) {
        self.generations.remove(&kind);
    }

    fn invalidate_pages(&mut self) {
        self.generations
            .retain(|kind, _| *kind == ViewRequestKind::Track);
    }
}

#[derive(Default)]
pub(super) struct LibraryViewBuffer {
    pub(super) query: Option<String>,
    pub(super) favorite: Option<bool>,
    pub(super) missing: Option<bool>,
    pub(super) sort: LibrarySort,
    pub(super) offset: usize,
    pub(super) total: usize,
    pub(super) rows: PagedRows<LibraryRow>,
    pub(super) scroll: gpui::UniformListScrollHandle,
}

#[derive(Default)]
pub(super) struct PlaylistSummaryBuffer {
    pub(super) offset: usize,
    pub(super) total: usize,
    pub(super) rows: PagedRows<PlaylistSummary>,
    pub(super) scroll: gpui::UniformListScrollHandle,
}

#[derive(Default)]
pub(super) struct PlaylistEntriesBuffer {
    pub(super) playlist_id: Option<i64>,
    pub(super) offset: usize,
    pub(super) total: usize,
    pub(super) rows: PagedRows<PlaylistEntryRow>,
    pub(super) scroll: gpui::UniformListScrollHandle,
}

impl GuiApp {
    pub(super) fn view_page_size(&self) -> usize {
        self.state.system.config.page_size as usize
    }

    fn request_view(&mut self, query: Query, generation: u64, cx: &mut Context<Self>) {
        let Some(kind) = ViewRequestKind::for_query(&query) else {
            return;
        };
        self.view_requests.begin(kind, generation);
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
                    this.view_requests.is_current(kind, generation)
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
                if !this.view_requests.is_current(kind, generation) {
                    return;
                }
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        let message = error.to_string();
                        match kind {
                            ViewRequestKind::Library => {
                                this.library_buffer.rows.fail(message.clone())
                            }
                            ViewRequestKind::PlaylistSummaries => {
                                this.playlist_buffer.rows.fail(message.clone())
                            }
                            ViewRequestKind::PlaylistEntries => {
                                this.playlist_entries.rows.fail(message.clone())
                            }
                            _ => {}
                        }
                        this.error = Some(message);
                        cx.notify();
                        return;
                    }
                };
                if matches!(
                    kind,
                    ViewRequestKind::Library
                        | ViewRequestKind::Ranking
                        | ViewRequestKind::Track
                        | ViewRequestKind::Stats
                ) && revisions.library != this.state.library.revision
                {
                    match kind {
                        ViewRequestKind::Library => {
                            let navigation = this.library_navigation;
                            this.request_library_page(this.library_buffer.offset, cx);
                            this.library_navigation = navigation;
                        }
                        ViewRequestKind::Track => {
                            if let Some(id) = this.metadata_track {
                                this.request_track(id, cx);
                            }
                        }
                        ViewRequestKind::Ranking | ViewRequestKind::Stats => {
                            this.request_ranking(this.ranking_offset, cx)
                        }
                        _ => {}
                    }
                    return;
                }
                if matches!(
                    kind,
                    ViewRequestKind::PlaylistSummaries | ViewRequestKind::PlaylistEntries
                ) && revisions.playlist != this.state.library.playlist_revision
                {
                    match kind {
                        ViewRequestKind::PlaylistSummaries => {
                            this.request_playlist_summaries(this.playlist_buffer.offset, cx)
                        }
                        ViewRequestKind::PlaylistEntries => {
                            if let Some(id) = this.playlist_entries.playlist_id {
                                this.request_playlist_entries(id, this.playlist_entries.offset, cx);
                            }
                        }
                        _ => {}
                    }
                    return;
                }
                match result {
                    ViewResponse::LibraryPage(page) if kind == ViewRequestKind::Ranking => {
                        if this.ranking_offset > 0 && this.ranking_offset >= page.total {
                            let last = page.total.saturating_sub(1) / this.view_page_size()
                                * this.view_page_size();
                            this.request_ranking(last, cx);
                            return;
                        }
                        this.ranking_total = page.total;
                        this.ranking_rows = page.rows;
                    }
                    ViewResponse::LibraryPage(page) if kind == ViewRequestKind::Library => {
                        this.library_buffer.total = page.total;
                        this.library_buffer.rows.accept(page);
                        if let Some(index) = this
                            .library_buffer
                            .rows
                            .position(|row| this.selected.contains(&row.id))
                        {
                            this.library_buffer.rows.pin(Some(index));
                        }
                        if let Some(index) = this.library_navigation {
                            if let Some(id) = this.library_buffer.rows.get(index).map(|row| row.id)
                            {
                                this.library_navigation = None;
                                this.select_track(id, false, cx);
                            } else if index >= this.library_buffer.total {
                                this.library_navigation = None;
                            } else {
                                this.ensure_library_range(index..index + 1, cx);
                            }
                        }
                    }
                    ViewResponse::PlaylistSummaries(page)
                        if kind == ViewRequestKind::PlaylistSummaries =>
                    {
                        this.playlist_buffer.total = page.total;
                        this.playlist_buffer.rows.accept(page);
                        if let Some(index) = this
                            .playlist_buffer
                            .rows
                            .position(|row| Some(row.id) == this.selected_playlist)
                        {
                            this.playlist_buffer.rows.pin(Some(index));
                        }
                        if let Some(summary) = this
                            .playlist_buffer
                            .rows
                            .iter()
                            .find(|row| Some(row.id) == this.selected_playlist)
                        {
                            this.selected_playlist_name = summary.name.clone();
                        }
                    }
                    ViewResponse::PlaylistEntries(page)
                        if kind == ViewRequestKind::PlaylistEntries =>
                    {
                        this.playlist_entries.total = page.total;
                        this.playlist_entries.rows.accept(page);
                        if let Some(index) = this
                            .playlist_entries
                            .rows
                            .position(|row| Some(row.id) == this.selected_entry)
                        {
                            this.playlist_entries.rows.pin(Some(index));
                        }
                    }
                    ViewResponse::Track(track) if kind == ViewRequestKind::Track => {
                        let draft_dirty = this.full_track.as_ref().is_some_and(|current| {
                            this.value(Field::Title, cx) != current.title
                                || this.value(Field::Artist, cx) != current.artist
                                || this.value(Field::Album, cx) != current.album
                        });
                        if !draft_dirty {
                            if let Some(track) = track.as_ref() {
                                this.set_value(Field::Title, track.title.clone(), cx);
                                this.set_value(Field::Artist, track.artist.clone(), cx);
                                this.set_value(Field::Album, track.album.clone(), cx);
                            }
                        }
                        this.full_track = track;
                        this.sync_waveform(cx);
                    }
                    ViewResponse::LibraryStats(stats) if kind == ViewRequestKind::Stats => {
                        this.library_stats = Some(stats)
                    }
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn request_library_page(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.invalidate_library_selection();
        let query = self.value(Field::Search, cx).trim().to_lowercase();
        self.library_buffer.query = (!query.is_empty()).then_some(query);
        self.library_buffer.favorite = self.favorites_only.then_some(true);
        self.library_buffer.missing = self.missing_only.then_some(true);
        self.library_buffer.rows.invalidate();
        self.load_library_page(offset, cx);
    }

    fn load_library_page(&mut self, offset: usize, cx: &mut Context<Self>) {
        let page_size = self.view_page_size();
        let offset = offset / page_size * page_size;
        self.library_buffer.offset = offset;
        self.library_buffer.rows.begin(offset, page_size);
        let generation = self.view_requests.next_generation();
        self.request_view(
            Query::LibraryPage {
                query: self.library_buffer.query.clone(),
                favorite: self.library_buffer.favorite,
                missing: self.library_buffer.missing,
                sort: self.library_buffer.sort,
                offset,
                limit: page_size,
            },
            generation,
            cx,
        );
    }

    pub(super) fn request_directory_page(
        &mut self,
        branch: library::TreeBranch,
        offset: usize,
        cx: &mut Context<Self>,
    ) {
        let page_size = self.view_page_size();
        self.directory_tree.configure_page_size(page_size);
        let offset = offset / page_size * page_size;
        let generation = self.view_requests.next_generation();
        self.directory_tree
            .begin(branch.clone(), offset, generation);
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
            self.directory_tree.accept(&branch, generation, page);
            cx.notify();
            return;
        }
        let handle = self.handle.clone();
        let favorite = self.favorites_only.then_some(true);
        cx.spawn(async move |this, cx| {
            let request_path = branch.path().to_path_buf();
            let response = cx
                .background_executor()
                .spawn(async move {
                    handle.query(Query::DirectoryPage {
                        path: request_path,
                        favorite,
                        offset,
                        limit: page_size,
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.directory_tree.request_pending(&branch, generation) {
                    return;
                }
                let result = match response.result {
                    Ok(result) => result,
                    Err(error) => {
                        this.directory_tree.fail(&branch, generation);
                        this.error = Some(error.to_string());
                        cx.notify();
                        return;
                    }
                };
                if response.revisions.structure != this.state.library.structure_revision
                    || (favorite.is_some()
                        && response.revisions.library != this.state.library.revision)
                {
                    this.request_directory_page(branch, offset, cx);
                    return;
                }
                if let ViewResponse::DirectoryPage(page) = result {
                    let projection = this.directory_tree.projection_revision();
                    this.directory_tree.accept(&branch, generation, page);
                    if this.directory_tree.projection_revision() != projection {
                        this.invalidate_library_selection();
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn request_playlist_summaries(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.playlist_buffer.rows.invalidate();
        self.load_playlist_summaries(offset, cx);
    }

    fn load_playlist_summaries(&mut self, offset: usize, cx: &mut Context<Self>) {
        let page_size = self.view_page_size();
        let offset = offset / page_size * page_size;
        self.playlist_buffer.offset = offset;
        self.playlist_buffer.rows.begin(offset, page_size);
        let generation = self.view_requests.next_generation();
        self.request_view(
            Query::PlaylistSummaries {
                offset,
                limit: page_size,
            },
            generation,
            cx,
        );
    }

    pub(super) fn request_playlist_entries(
        &mut self,
        playlist_id: i64,
        offset: usize,
        cx: &mut Context<Self>,
    ) {
        if self.playlist_entries.playlist_id != Some(playlist_id) {
            self.selected_entry = None;
            self.clear_metadata_selection(cx);
            self.playlist_entries.rows.clear();
            self.playlist_entries.total = 0;
            self.playlist_entries
                .scroll
                .scroll_to_item(0, ScrollStrategy::Top);
        } else {
            self.playlist_entries.rows.invalidate();
        }
        self.playlist_entries.playlist_id = Some(playlist_id);
        self.load_playlist_entries(playlist_id, offset, cx);
    }

    fn load_playlist_entries(&mut self, playlist_id: i64, offset: usize, cx: &mut Context<Self>) {
        let page_size = self.view_page_size();
        let offset = offset / page_size * page_size;
        self.playlist_entries.offset = offset;
        self.playlist_entries.rows.begin(offset, page_size);
        let generation = self.view_requests.next_generation();
        self.request_view(
            Query::PlaylistEntries {
                playlist_id,
                offset,
                limit: page_size,
            },
            generation,
            cx,
        );
    }

    pub(super) fn ensure_library_range(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) {
        if let Some(offset) = self
            .library_buffer
            .rows
            .demand(range, self.view_page_size())
        {
            self.load_library_page(offset, cx);
        }
    }
    pub(super) fn ensure_playlist_range(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) {
        if let Some(offset) = self
            .playlist_buffer
            .rows
            .demand(range, self.view_page_size())
        {
            self.load_playlist_summaries(offset, cx);
        }
    }
    pub(super) fn ensure_entry_range(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.playlist_entries.playlist_id {
            if let Some(offset) = self
                .playlist_entries
                .rows
                .demand(range, self.view_page_size())
            {
                self.load_playlist_entries(id, offset, cx);
            }
        }
    }

    pub(super) fn request_track(&mut self, id: i64, cx: &mut Context<Self>) {
        let generation = self.view_requests.next_generation();
        if self.metadata_track != Some(id) {
            self.full_track = None;
        }
        self.metadata_track = Some(id);
        self.request_view(Query::Track { track_id: id }, generation, cx);
    }
    pub(super) fn clear_metadata_selection(&mut self, cx: &mut Context<Self>) {
        self.metadata_track = None;
        self.full_track = None;
        self.view_requests.cancel(ViewRequestKind::Track);
        self.sync_metadata_default(cx);
        self.sync_waveform(cx);
    }

    pub(super) fn toggle_library_view(&mut self, cx: &mut Context<Self>) {
        self.invalidate_library_selection();
        self.library_tree_active = !self.library_tree_active;
        if self.library_tree_active {
            if self
                .directory_tree
                .generation(&library::TreeBranch::default())
                .is_none()
            {
                self.request_directory_page(library::TreeBranch::default(), 0, cx);
            }
        } else if !self.view_requests.contains(ViewRequestKind::Library) {
            self.request_library_page(0, cx);
        }
        cx.notify();
    }

    pub(super) fn refresh_favorite_filter(&mut self, cx: &mut Context<Self>) {
        self.invalidate_library_selection();
        self.library_navigation = None;
        self.directory_tree.invalidate();
        self.request_directory_page(library::TreeBranch::default(), 0, cx);
        if self.library_tree_active {
            // Drop stale flat payloads and tokens; query only when that view is opened.
            self.view_requests.cancel(ViewRequestKind::Library);
            self.library_buffer.rows.clear();
            self.library_buffer.total = 0;
        } else {
            self.refresh_filter(cx);
        }
    }

    pub(super) fn refresh_filter(&mut self, cx: &mut Context<Self>) {
        // Search and missing filters use the flat track view.
        self.library_tree_active = false;
        self.library_navigation = None;
        self.library_buffer.rows.clear();
        self.library_buffer.total = 0;
        self.library_buffer
            .scroll
            .scroll_to_item(0, ScrollStrategy::Top);
        self.request_library_page(0, cx);
    }
    pub(super) fn request_ranking(&mut self, offset: usize, cx: &mut Context<Self>) {
        let page_size = self.view_page_size();
        let offset = offset / page_size * page_size;
        self.ranking_offset = offset;
        self.ranking_rows.clear();
        let generation = self.view_requests.next_generation();
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
        let generation = self.view_requests.next_generation();
        self.request_view(Query::LibraryStats, generation, cx);
    }

    pub(super) fn update_metadata_track(&mut self, id: i64, cx: &mut Context<Self>) {
        self.request_track(id, cx);
    }

    pub(super) fn sync_metadata_default(&mut self, cx: &mut Context<Self>) {
        if self.metadata_track.is_some() {
            return;
        }
        self.view_requests.cancel(ViewRequestKind::Track);
        let current = self.state.current_track();
        if self.full_track.as_ref() == current {
            return;
        }
        self.full_track = current.cloned();
        let (title, artist, album) = self
            .full_track
            .as_ref()
            .map(|track| {
                (
                    track.title.clone(),
                    track.artist.clone(),
                    track.album.clone(),
                )
            })
            .unwrap_or_default();
        self.set_value(Field::Title, title, cx);
        self.set_value(Field::Artist, artist, cx);
        self.set_value(Field::Album, album, cx);
    }

    pub(super) fn refresh_page_size(&mut self, cx: &mut Context<Self>) {
        // Invalidate query tokens and payload, not identity, metadata drafts, or scroll position.
        self.view_requests.invalidate_pages();
        self.request_library_page(self.library_buffer.rows.visible().start, cx);
        self.directory_tree
            .configure_page_size(self.view_page_size());
        self.directory_tree.invalidate();
        self.request_directory_page(library::TreeBranch::default(), 0, cx);
        self.request_playlist_summaries(self.playlist_buffer.rows.visible().start, cx);
        if let Some(id) = self.playlist_entries.playlist_id {
            self.request_playlist_entries(id, self.playlist_entries.rows.visible().start, cx);
        }
        self.request_ranking(0, cx);
    }
}
