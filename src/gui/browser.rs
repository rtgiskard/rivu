//! Browser state is independent of the window entity and frame scheduler.
//!
//! Selection identities outlive page eviction. Projection changes cancel range
//! selection and deferred navigation before replacing payloads; only explicit
//! selection/cancellation changes identities. Rendering and scope capture borrow
//! read-only buffers and tree state. Only the owners prepare, accept, retry, and
//! pin payloads or start and complete range selection.

pub(super) mod selection;

use super::queue::QueueSelection;
use super::{
    BrowserFocus, HistorySort, ListFocus,
    library::{DirectoryTree, LibraryNode, TreeBranch},
    metadata::ManualIntentId,
    paged::PagedRows,
    queries::ViewRequests,
};
use crate::model::{
    DirectoryRow, HistoryEntry, LibrarySort, Page, PlaylistEntryLocation, PlaylistEntryRow,
    PlaylistSummary, Query,
};
use crate::{projection::QueryProjection, response::QueryRevisions};
use gpui::ScrollStrategy;
use selection::LibrarySelection;
use std::{collections::HashSet, ops::Range, sync::Arc};

pub(in crate::gui) fn merge_selection(
    tracks: &mut HashSet<i64>,
    nodes: &mut HashSet<LibraryNode>,
    range: HashSet<LibraryNode>,
    additive: bool,
) {
    if !additive {
        tracks.clear();
        nodes.clear();
    }
    for node in range {
        match node {
            LibraryNode::Track(id) => {
                tracks.insert(id);
            }
            LibraryNode::Directory(_) => {
                nodes.insert(node);
            }
        }
    }
}
#[derive(Default)]
pub(super) struct BrowserState {
    pub(super) library: LibraryBrowser,
    pub(super) playlists: PlaylistBrowser,
    pub(super) queue: QueueSelection,
    pub(super) history: HistoryBrowser,
    pub(super) requests: ViewRequests,
    focus: Option<BrowserFocus>,
}

impl BrowserState {
    pub(super) fn focus(&self) -> Option<BrowserFocus> {
        self.focus
    }

    pub(super) fn focus_list(&mut self, focus: ListFocus) {
        self.focus = Some(BrowserFocus::List(focus));
    }

    pub(super) fn focus_playlists(&mut self) {
        self.focus = Some(BrowserFocus::Playlists);
    }

    pub(super) fn focus_history(&mut self) {
        self.focus = Some(BrowserFocus::History);
    }

    pub(super) fn clear_focus(&mut self) {
        self.focus = None;
    }

    pub(super) fn remove_playlist_selection(&mut self) {
        self.requests
            .cancel(super::queries::ViewRequestKind::Selection);
        self.requests
            .cancel(super::queries::ViewRequestKind::Entries);
        self.playlists.clear_selection();
    }

    pub(super) fn accept_playlist_summaries(&mut self, page: Page<PlaylistSummary>) {
        if page.total == 0 && self.playlists.summaries.query.is_none() {
            self.remove_playlist_selection();
        }
        self.playlists.accept_summaries(page);
    }

    /// The first row after `id - 1` proves existence without scanning other pages.
    pub(super) fn accept_playlist_lookup(&mut self, id: i64, first: Option<PlaylistSummary>) {
        if self.playlists.selected() != Some(id) {
            return;
        }
        if let Some(summary) = first.filter(|summary| summary.id == id) {
            self.playlists.selected_name = summary.name;
            self.playlists.selected_total = Some(summary.entry_count);
        } else {
            self.remove_playlist_selection();
        }
    }

    pub(super) fn clear_selection(&mut self) {
        self.library.clear_selection();
        self.queue.clear();
        self.history.clear_selection();
        self.remove_playlist_selection();
    }
}

#[derive(Default)]
struct RootCounts {
    owner: QueryProjection,
    roots: Vec<std::path::PathBuf>,
    totals: std::collections::HashMap<std::path::PathBuf, usize>,
    pending: HashSet<usize>,
    failed: HashSet<usize>,
    settled: bool,
}

impl RootCounts {
    fn invalidate(&mut self) {
        self.owner.cancel();
        self.totals.clear();
        self.pending.clear();
        self.failed.clear();
        self.settled = false;
    }
}

#[derive(Default)]
struct SearchCounts {
    projection: QueryProjection,
    // Logical cancellation retires the epoch, not the physical worker slot.
    worker: Option<u64>,
    failed: bool,
    roots: Vec<std::path::PathBuf>,
    totals: Option<(usize, usize)>,
    root_counts: std::collections::HashMap<std::path::PathBuf, (usize, usize)>,
}

pub(super) struct SearchCountRequest {
    pub(super) query: Query,
    pub(super) roots: Vec<std::path::PathBuf>,
    pub(super) revisions: QueryRevisions,
    pub(super) ticket: crate::core::QueryTicket,
    generation: u64,
}

pub(super) struct SearchCountResult {
    pub(super) totals: (usize, usize),
    pub(super) roots: std::collections::HashMap<std::path::PathBuf, (usize, usize)>,
}

#[derive(Default)]
pub(super) struct LibraryViewBuffer {
    pub(super) query: Option<String>,
    pub(super) favorite: Option<bool>,
    pub(super) missing: Option<bool>,
    pub(super) sort: LibrarySort,
}

pub(super) struct LibraryBrowser {
    pub(super) projection: QueryProjection,
    search_counts: SearchCounts,
    buffer: LibraryViewBuffer,
    tree: DirectoryTree,
    range_selection: LibrarySelection,
    navigation_intent: Option<ManualIntentId>,
    selected: HashSet<i64>,
    selected_nodes: HashSet<LibraryNode>,
    tree_active: bool,
    favorites_only: bool,
    missing_only: bool,
    force_scan: bool,
    counts: RootCounts,
    pub(super) locate: super::library::LibraryLocateState,
}

impl Default for LibraryBrowser {
    fn default() -> Self {
        Self {
            projection: Default::default(),
            search_counts: SearchCounts::default(),
            buffer: LibraryViewBuffer {
                sort: LibrarySort::Album,
                ..Default::default()
            },
            tree: DirectoryTree::default(),
            range_selection: LibrarySelection::default(),
            navigation_intent: None,
            selected: HashSet::new(),
            selected_nodes: HashSet::new(),
            tree_active: true,
            favorites_only: false,
            missing_only: false,
            force_scan: false,
            counts: RootCounts::default(),
            locate: Default::default(),
        }
    }
}

impl LibraryBrowser {
    /// Reconcile the complete demand before reserving the sole physical worker.
    /// The boolean tells the presentation layer to forget its previous counts.
    pub(super) fn prepare_search_counts(
        &mut self,
        roots: &[std::path::PathBuf],
        revisions: QueryRevisions,
    ) -> (bool, Option<SearchCountRequest>) {
        let query = Query::LibraryRootPage {
            path: Default::default(),
            query: self.buffer.query.clone(),
            favorite: self.buffer.favorite,
            missing: self.buffer.missing,
            sort: self.buffer.sort,
            offset: 0,
            limit: 0,
        };
        let counts = &mut self.search_counts;
        let roots_changed = counts.roots != roots;
        let changed = counts.projection.sync(&query, revisions) || roots_changed;
        if changed {
            if roots_changed {
                counts.projection.cancel();
                counts.roots = roots.to_vec();
            }
            counts.totals = None;
            counts.root_counts.clear();
            counts.failed = false;
        }
        if counts.totals.is_some() || counts.failed || counts.worker.is_some() {
            return (changed, None);
        }
        let generation = counts.projection.generation();
        counts.worker = Some(generation);
        (
            changed,
            Some(SearchCountRequest {
                query,
                roots: roots.to_vec(),
                revisions,
                ticket: counts.projection.ticket(),
                generation,
            }),
        )
    }

    /// Completion releases its physical slot even when its payload is obsolete.
    /// A current failure is retained until explicit retry or changed demand.
    pub(super) fn accept_search_counts(
        &mut self,
        request: SearchCountRequest,
        roots: &[std::path::PathBuf],
        revisions: QueryRevisions,
        result: Option<SearchCountResult>,
    ) -> bool {
        let counts = &mut self.search_counts;
        if counts.worker != Some(request.generation) {
            return false;
        }
        counts.worker = None;
        let Query::LibraryRootPage {
            query,
            favorite,
            missing,
            sort,
            ..
        } = &request.query
        else {
            unreachable!()
        };
        if query != &self.buffer.query
            || *favorite != self.buffer.favorite
            || *missing != self.buffer.missing
            || *sort != self.buffer.sort
            || request.roots != roots
            || counts.roots != roots
            || !counts
                .projection
                .accepts(&request.query, request.generation, revisions)
        {
            return false;
        }
        counts.failed = result.is_none();
        if let Some(result) = result {
            counts.totals = Some(result.totals);
            counts.root_counts = result.roots;
        }
        true
    }

    pub(super) fn search_counts_pending(&self) -> bool {
        self.search_counts.worker.is_some()
    }

    pub(super) fn search_counts(&self) -> Option<(usize, usize)> {
        self.search_counts.totals
    }

    pub(super) fn projection_query(&self) -> Query {
        if self.tree_active {
            Query::DirectoryPage {
                path: Default::default(),
                query: self.buffer.query.clone(),
                favorite: self.buffer.favorite,
                missing: self.buffer.missing,
                offset: 0,
                limit: 1,
            }
        } else {
            Query::LibraryRootPage {
                path: Default::default(),
                query: self.buffer.query.clone(),
                favorite: self.buffer.favorite,
                missing: self.buffer.missing,
                sort: self.buffer.sort,
                offset: 0,
                limit: 1,
            }
        }
    }

    pub(super) fn sync_projection(&mut self, revisions: QueryRevisions) -> bool {
        if !self.projection.sync(&self.projection_query(), revisions) {
            return false;
        }
        self.invalidate_projection();
        self.tree.invalidate();
        true
    }
    pub(super) fn sync_root_counts(
        &mut self,
        roots: &[std::path::PathBuf],
        structure_revision: u64,
    ) {
        let revisions = QueryRevisions {
            structure: structure_revision,
            ..Default::default()
        };
        let stamp = Query::LibraryRootCounts { roots: Vec::new() }.stamp(revisions);
        if self.counts.roots == roots && self.counts.owner.stamp() == Some(stamp) {
            return;
        }
        let query = Query::LibraryRootCounts {
            roots: roots.to_vec(),
        };
        if self.counts.owner.sync(&query, revisions) {
            self.counts.totals.clear();
            self.counts.pending.clear();
            self.counts.failed.clear();
            self.counts.settled = false;
            self.counts.roots = roots.to_vec();
        }
    }

    pub(super) fn root_count_ticket(&mut self) -> crate::core::QueryTicket {
        self.counts.owner.ticket()
    }

    pub(super) fn cancel_pending_reads(&mut self) {
        self.projection.cancel();
        self.tree.cancel_pending();
        if self.search_counts_pending() {
            self.search_counts.projection.cancel();
        }
        if !self.counts.pending.is_empty() {
            self.counts.owner.cancel();
            self.counts.pending.clear();
            self.counts.settled = false;
        }
    }

    pub(super) fn retry_root_counts(&mut self) {
        self.search_counts.failed = false;
        self.counts.failed.clear();
        self.counts.settled = false;
    }

    pub(super) fn root_counts_settled(&self) -> bool {
        self.counts.settled
    }
    pub(super) fn begin_root_counts(
        &mut self,
        roots: &[std::path::PathBuf],
        structure_revision: u64,
    ) -> (u64, Vec<usize>) {
        self.sync_root_counts(roots, structure_revision);
        if !self.counts.pending.is_empty() {
            return (self.counts.owner.generation(), Vec::new());
        }
        let batches = (0..roots.len())
            .step_by(crate::model::MAX_QUERY_ROWS)
            .filter(|offset| {
                !self.counts.pending.contains(offset)
                    && !self.counts.failed.contains(offset)
                    && roots[*offset..(*offset + crate::model::MAX_QUERY_ROWS).min(roots.len())]
                        .iter()
                        .any(|root| !self.counts.totals.contains_key(root))
            })
            .take(1)
            .collect::<Vec<_>>();
        self.counts.pending.extend(batches.iter().copied());
        self.counts.settled = batches.is_empty();
        (self.counts.owner.generation(), batches)
    }

    pub(super) fn counts_current(
        &self,
        generation: u64,
        roots: &[std::path::PathBuf],
        structure_revision: u64,
    ) -> bool {
        let query = Query::LibraryRootCounts { roots: Vec::new() };
        self.counts.roots == roots
            && self.counts.owner.generation() == generation
            && self.counts.owner.stamp()
                == Some(query.stamp(QueryRevisions {
                    structure: structure_revision,
                    ..Default::default()
                }))
    }

    pub(super) fn accept_root_counts(
        &mut self,
        generation: u64,
        offset: usize,
        counts: Vec<crate::model::LibraryRootCount>,
    ) {
        if self.counts.owner.generation() == generation && self.counts.pending.remove(&offset) {
            self.counts
                .totals
                .extend(counts.into_iter().map(|count| (count.path, count.total)));
        }
    }

    pub(super) fn fail_root_counts(&mut self, generation: u64, offset: usize) {
        if self.counts.owner.generation() == generation && self.counts.pending.remove(&offset) {
            self.counts.failed.insert(offset);
        }
    }

    pub(super) fn root_count(&self, path: &std::path::Path) -> Option<usize> {
        self.counts.totals.get(path).copied()
    }

    pub(super) fn root_count_label(&self, path: &std::path::Path) -> String {
        if self.buffer.query.is_some()
            || self.buffer.favorite.is_some()
            || self.buffer.missing.is_some()
        {
            self.search_counts.root_counts.get(path).map_or_else(
                || "—".to_owned(),
                |(matched, total)| format!("{matched} / {total}"),
            )
        } else {
            self.root_count(path)
                .map_or_else(|| "—".to_owned(), |total| total.to_string())
        }
    }

    pub(super) fn root_count_failed(&self, path: &std::path::Path) -> bool {
        if self.buffer.query.is_some()
            || self.buffer.favorite.is_some()
            || self.buffer.missing.is_some()
        {
            return self.search_counts.failed;
        }
        self.counts.failed.iter().any(|&offset| {
            self.counts.roots
                [offset..(offset + crate::model::MAX_QUERY_ROWS).min(self.counts.roots.len())]
                .iter()
                .any(|root| root == path)
        })
    }

    pub(super) fn buffer(&self) -> &LibraryViewBuffer {
        &self.buffer
    }

    pub(super) fn tree(&self) -> &DirectoryTree {
        &self.tree
    }

    pub(in crate::gui) fn take_ready_manual_navigation(
        &mut self,
    ) -> Option<(ManualIntentId, usize)> {
        let id = self.navigation_intent?;
        let index = self.tree.ready_selection()?;
        self.navigation_intent = None;
        Some((id, index))
    }

    pub(super) fn refresh_tree(&mut self, page_size: usize) {
        if self.tree.configure_page_size(page_size) {
            self.invalidate_projection();
        }
    }

    pub(super) fn prepare_directory(
        &mut self,
        branch: TreeBranch,
        offset: usize,
        page_size: usize,
        generation: u64,
    ) -> usize {
        if self.tree.configure_page_size(page_size) {
            self.invalidate_projection();
        }
        let offset = offset / page_size * page_size;
        self.tree.begin(branch, offset, generation);
        offset
    }

    pub(super) fn directory_request_pending(
        &self,
        branch: &TreeBranch,
        generation: u64,
        query: Option<&str>,
        favorite: Option<bool>,
        tree_active: bool,
        missing: Option<bool>,
    ) -> bool {
        query == self.buffer.query.as_deref()
            && favorite == self.favorites_only.then_some(true)
            && tree_active == self.tree_active
            && missing == self.buffer.missing
            && self.tree.request_pending(branch, generation)
    }

    pub(super) fn accept_directory(
        &mut self,
        branch: &TreeBranch,
        generation: u64,
        page: Page<DirectoryRow>,
    ) {
        let projection = self.tree.projection_revision();
        self.tree.accept(branch, generation, page);
        if self.tree.projection_revision() != projection {
            self.invalidate_selection();
        }
        self.remember_selection();
    }

    pub(super) fn fail_directory(&mut self, branch: &TreeBranch, generation: u64) {
        self.tree.fail(branch, generation);
    }

    pub(super) fn cancel_directory(&mut self, branch: &TreeBranch, generation: u64) {
        self.tree.cancel_request(branch, generation);
    }

    pub(super) fn retry_directory(&mut self, branch: &TreeBranch) {
        self.tree.retry(branch);
    }

    pub(super) fn expand_directory(&mut self, branch: &TreeBranch, expanded: bool) -> bool {
        self.invalidate_selection();
        self.cancel_navigation();
        if expanded {
            self.tree.expand(branch);
            matches!(
                self.tree.status(branch),
                super::library::BranchStatus::Unloaded | super::library::BranchStatus::Error
            )
        } else {
            self.tree.collapse(branch);
            false
        }
    }

    pub(super) fn reveal_playing(
        &mut self,
        root_index: usize,
        roots: &[std::path::PathBuf],
        locations: &[crate::model::LibraryLocation],
    ) -> Option<(TreeBranch, usize)> {
        self.invalidate_selection();
        self.tree.reveal_playing(root_index, roots, locations)
    }

    pub(super) fn clear_locate_filters(&mut self) {
        self.favorites_only = false;
        self.missing_only = false;
        self.buffer.query = None;
        self.buffer.favorite = None;
        self.buffer.missing = None;
        self.invalidate_projection();
        self.tree.invalidate();
    }

    pub(super) fn demand_directories(&mut self, range: Range<usize>) -> Vec<(TreeBranch, usize)> {
        self.tree.requests_for_range(range)
    }

    pub(super) fn navigate_tree_row(&mut self, index: usize) -> Option<LibraryNode> {
        let Some(super::library::tree::TreeItem::Entry(visible)) = self.tree.item(index) else {
            self.tree.defer_selection(index);
            return None;
        };
        self.cancel_navigation();
        Some(match visible.row {
            DirectoryRow::Directory { path } => LibraryNode::Directory(std::sync::Arc::from(path)),
            DirectoryRow::Track(track) => LibraryNode::Track(track.id),
        })
    }

    pub(super) fn selected(&self) -> &HashSet<i64> {
        &self.selected
    }
    pub(super) fn selected_nodes(&self) -> &HashSet<LibraryNode> {
        &self.selected_nodes
    }
    pub(super) fn drag_nodes(&self, source: &LibraryNode) -> Vec<LibraryNode> {
        let selected = match source {
            LibraryNode::Track(id) => self.selected.contains(id),
            LibraryNode::Directory(_) => self.selected_nodes.contains(source),
        };
        if !selected {
            return vec![source.clone()];
        }
        self.selected_nodes
            .iter()
            .cloned()
            .chain(self.selected.iter().copied().map(LibraryNode::Track))
            .collect()
    }

    pub(super) fn tree_active(&self) -> bool {
        self.tree_active
    }
    pub(super) fn favorites_only(&self) -> bool {
        self.favorites_only
    }
    pub(super) fn missing_only(&self) -> bool {
        self.missing_only
    }
    pub(super) fn force_scan(&self) -> bool {
        self.force_scan
    }
    pub(super) fn cancel_navigation(&mut self) {
        self.locate.invalidate();
        self.tree.cancel_navigation();
        self.navigation_intent = None;
    }

    pub(super) fn invalidate_selection(&mut self) {
        self.range_selection.cancel(true);
    }

    pub(super) fn invalidate_projection(&mut self) {
        self.invalidate_selection();
        self.cancel_navigation();
    }

    pub(super) fn clear_selection(&mut self) {
        self.invalidate_projection();
        self.selected.clear();
        self.selected_nodes.clear();
        self.remember_selection();
    }

    pub(super) fn select_node(&mut self, node: &LibraryNode, multi: bool) {
        self.cancel_navigation();
        if !multi {
            self.selected.clear();
            self.selected_nodes.clear();
        }
        match node {
            LibraryNode::Track(id) => {
                if multi && !self.selected.insert(*id) {
                    self.selected.remove(id);
                } else if !multi {
                    self.selected.insert(*id);
                }
            }
            LibraryNode::Directory(_) => {
                if multi && !self.selected_nodes.insert(node.clone()) {
                    self.selected_nodes.remove(node);
                } else if !multi {
                    self.selected_nodes.insert(node.clone());
                }
            }
        }
        self.remember_selection();
    }

    pub(super) fn apply_range(&mut self, nodes: HashSet<LibraryNode>, additive: bool) {
        merge_selection(
            &mut self.selected,
            &mut self.selected_nodes,
            nodes,
            additive,
        );
        self.remember_selection();
    }

    pub(super) fn remember_selection(&mut self) {
        self.tree
            .remember_selection(&self.selected_nodes, &self.selected);
    }

    pub(super) fn roots_changed(&mut self) {
        self.invalidate_projection();
        self.tree.clear();
        self.counts.invalidate();
        self.selected_nodes.clear();
    }

    pub(super) fn toggle_view(&mut self) {
        self.invalidate_projection();
        self.tree_active = !self.tree_active;
        self.tree.set_grouped(!self.tree_active);
    }

    pub(super) fn toggle_favorites(&mut self) {
        self.favorites_only = !self.favorites_only;
        self.invalidate_projection();
        self.tree.invalidate();
        self.buffer.favorite = self.favorites_only.then_some(true);
    }

    pub(super) fn toggle_missing(&mut self) {
        self.missing_only = !self.missing_only;
    }

    pub(super) fn toggle_force_scan(&mut self) {
        self.force_scan = !self.force_scan;
    }

    pub(super) fn refresh_filter(&mut self) {
        self.invalidate_projection();
        self.buffer.missing = self.missing_only.then_some(true);
        self.tree.invalidate();
        self.scroll_filter_to_top();
    }

    pub(super) fn refresh_search(&mut self, query: String) {
        self.clear_selection();
        self.prepare_query(query);
        self.scroll_filter_to_top();
    }

    pub(super) fn scroll_filter_to_top(&self) {
        self.tree.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    pub(super) fn prepare_query(&mut self, query: String) {
        self.invalidate_projection();
        let query = (!query.is_empty()).then_some(query);
        if self.buffer.query != query {
            self.tree.invalidate();
        }
        self.buffer.query = query;
        self.buffer.favorite = self.favorites_only.then_some(true);
        self.buffer.missing = self.missing_only.then_some(true);
    }
}

#[derive(Default)]
pub(super) struct PlaylistSummaryBuffer {
    pub(super) query: Option<String>,
    pub(super) offset: usize,
    pub(super) rows: PagedRows<PlaylistSummary>,
    pub(super) scroll: gpui::UniformListScrollHandle,
}

pub(super) struct PlaylistEntriesBuffer {
    pub(super) query: Option<String>,
    pub(super) playlist_id: Option<i64>,
    pub(super) offset: usize,
    pub(super) rows: PagedRows<PlaylistEntryRow>,
    pub(super) scroll: gpui::UniformListScrollHandle,
}

impl Default for PlaylistEntriesBuffer {
    fn default() -> Self {
        // Without a selected playlist there is no unknown page to load.
        let mut rows = PagedRows::default();
        rows.set_total(0);
        Self {
            playlist_id: None,
            query: None,
            offset: 0,
            rows,
            scroll: Default::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PlaylistRefresh {
    pub(super) summaries: bool,
    pub(super) entries: bool,
}

#[derive(Default)]
pub(super) struct PlaylistBrowser {
    summaries: PlaylistSummaryBuffer,
    entries: PlaylistEntriesBuffer,
    selected: Option<i64>,
    pub(super) track_selection: super::playlist_selection::EntrySelection,
    selected_name: String,
    selected_total: Option<usize>,
    selection_generation: u64,
}

impl PlaylistBrowser {
    pub(super) fn selected_total(&self) -> Option<usize> {
        self.selected_total
    }
    pub(super) fn search_summaries(&mut self, query: Option<String>) {
        self.summaries.query = query;
        self.summaries.rows.clear();
        self.summaries.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }
    pub(super) fn search_entries(&mut self, query: Option<String>) {
        self.track_selection.query = query.clone();
        self.entries.query = query;
        self.track_selection.clear();
        self.entries.rows.clear();
        self.entries.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }
    pub(super) fn refresh_changes(
        &self,
        previous: &crate::projection::GuiLibrarySnapshot,
        next: &crate::projection::GuiLibrarySnapshot,
    ) -> PlaylistRefresh {
        let summaries_query = Query::PlaylistSummaries {
            query: self.summaries.query.clone(),
            offset: 0,
            limit: 1,
        };
        let summaries = summaries_query.stamp(previous.query_revisions(None))
            != summaries_query.stamp(next.query_revisions(None));
        let entries = self.entries.playlist_id.is_some_and(|playlist_id| {
            let query = Query::PlaylistEntries {
                query: self.entries.query.clone(),
                playlist_id,
                offset: 0,
                limit: 1,
            };
            query.stamp(previous.query_revisions(Some(playlist_id)))
                != query.stamp(next.query_revisions(Some(playlist_id)))
        });
        PlaylistRefresh { summaries, entries }
    }

    pub(super) fn summaries(&self) -> &PlaylistSummaryBuffer {
        &self.summaries
    }

    pub(super) fn entries(&self) -> &PlaylistEntriesBuffer {
        &self.entries
    }

    pub(super) fn invalidate_summaries(&mut self) {
        self.summaries.rows.invalidate();
    }

    pub(super) fn configure_page_size(&mut self, page_size: usize) {
        self.summaries.rows.configure_page_size(page_size);
        self.entries.rows.configure_page_size(page_size);
    }

    pub(super) fn cancel_page(&mut self, entries: bool) {
        if entries {
            self.entries.rows.cancel_pending();
        } else {
            self.summaries.rows.cancel_pending();
        }
    }

    pub(super) fn prepare_page(&mut self, entries: bool, offset: usize, page_size: usize) -> Query {
        let offset = offset / page_size * page_size;
        if entries {
            self.entries.offset = offset;
            self.entries.rows.begin(offset, page_size);
            Query::PlaylistEntries {
                query: self.entries.query.clone(),
                playlist_id: self
                    .entries
                    .playlist_id
                    .expect("entries prepared for a playlist"),
                offset,
                limit: page_size,
            }
        } else {
            self.summaries.offset = offset;
            self.summaries.rows.begin(offset, page_size);
            Query::PlaylistSummaries {
                query: self.summaries.query.clone(),
                offset,
                limit: page_size,
            }
        }
    }

    pub(super) fn demand_page(
        &mut self,
        entries: bool,
        range: Range<usize>,
        page_size: usize,
    ) -> Option<usize> {
        if entries {
            self.entries.rows.demand(range, page_size)
        } else {
            self.summaries.rows.demand(range, page_size)
        }
    }

    pub(super) fn retry_page(&mut self, entries: bool, page_size: usize) -> Option<usize> {
        let range = if entries {
            self.entries.rows.retry();
            self.entries.rows.visible()
        } else {
            self.summaries.rows.retry();
            self.summaries.rows.visible()
        };
        self.demand_page(entries, range, page_size)
    }

    pub(super) fn fail_page(&mut self, entries: bool, message: String) {
        if entries {
            self.entries.rows.fail(message);
        } else {
            self.summaries.rows.fail(message);
        }
    }

    pub(super) fn selected(&self) -> Option<i64> {
        self.selected
    }
    pub(super) fn selection_generation(&self) -> u64 {
        self.selection_generation
    }
    pub(super) fn selected_entry(&self) -> Option<i64> {
        self.track_selection.active.map(|row| row.id)
    }

    pub(super) fn select(&mut self, id: i64, name: String, index: usize) -> bool {
        let changed = self.selected != Some(id);
        if changed {
            self.selection_generation = self.selection_generation.wrapping_add(1);
            self.track_selection.clear();
        }
        self.selected = Some(id);
        self.selected_name = name;
        self.selected_total = self
            .summaries
            .rows
            .get(index)
            .filter(|row| row.id == id)
            .map(|row| row.entry_count);
        self.summaries.rows.pin(Some(index));
        changed
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.selected = None;
        self.selected_total = None;
        self.track_selection.clear();
        self.selected_name.clear();
        self.summaries.rows.pin(None);
        self.entries = PlaylistEntriesBuffer::default();
    }

    #[cfg(test)]
    pub(super) fn select_entry(&mut self, id: i64, index: usize) {
        let track_id = self
            .entries
            .rows
            .get(index)
            .filter(|row| row.id == id)
            .map(|row| row.track_id)
            .unwrap_or(0);
        self.track_selection.select(
            super::playlist_selection::EntryIdentity {
                id,
                track_id,
                position: index,
                offset: index,
            },
            false,
        );
        self.sync_active_entry();
    }

    pub(super) fn sync_active_entry(&mut self) {
        self.entries.rows.pin(
            self.track_selection
                .active
                .filter(|_| self.track_selection.ready())
                .map(|row| row.offset),
        );
    }

    pub(super) fn prepare_entries(&mut self, id: i64) {
        let changed = self.entries.playlist_id != Some(id);
        if changed {
            self.entries.query = None;
            self.track_selection.query = None;
            self.track_selection.clear();
            self.entries.rows.clear();
            self.entries.scroll.scroll_to_item(0, ScrollStrategy::Top);
        } else {
            self.entries.rows.invalidate();
        }
        self.entries.playlist_id = Some(id);
    }

    pub(super) fn reconcile_selection(&mut self) {
        // Eviction is not deletion; identities survive unloaded pages.
        if self.selected.is_none() || self.entries.playlist_id != self.selected {
            self.track_selection.clear();
        }
    }

    pub(super) fn accept_summaries(&mut self, page: Page<PlaylistSummary>) {
        self.summaries.rows.accept(page);
        if let Some(index) = self
            .summaries
            .rows
            .position(|row| Some(row.id) == self.selected)
        {
            self.summaries.rows.pin(Some(index));
        }
        if let Some(summary) = self
            .summaries
            .rows
            .iter()
            .find(|row| Some(row.id) == self.selected)
        {
            self.selected_name.clone_from(&summary.name);
            self.selected_total = Some(summary.entry_count);
        }
    }

    pub(super) fn accept_entries(&mut self, page: Page<PlaylistEntryRow>) {
        self.entries.rows.accept(page);
        if let Some(index) = self
            .entries
            .rows
            .position(|row| Some(row.id) == self.selected_entry())
        {
            self.entries.rows.pin(Some(index));
        }
    }

    pub(super) fn accept_entry_locations(&mut self, mut rows: Vec<PlaylistEntryLocation>) {
        rows.retain(|row| Some(row.playlist_id) == self.entries.playlist_id);
        if let Some(row) = rows.first() {
            self.entries.rows.set_total(row.total);
        }
        self.track_selection.reconcile(
            rows.iter()
                .map(super::playlist_selection::EntryIdentity::from)
                .collect(),
        );
        self.sync_active_entry();
    }

    #[cfg(test)]
    pub(super) fn entry_ready(&self) -> bool {
        self.track_selection.active.is_some() && self.track_selection.ready()
    }
}

pub(super) struct HistoryCommit {
    pub(super) track_id: i64,
    selected: HashSet<i64>,
    anchor: Option<i64>,
    generation: u64,
    selection_generation: u64,
}

pub(super) struct HistoryBrowser {
    source: Arc<Vec<HistoryEntry>>,
    indices: Vec<usize>,
    title_keys: Vec<String>,
    query: Option<String>,
    positions: std::collections::HashMap<i64, usize>,
    selected: HashSet<i64>,
    anchor: Option<i64>,
    generation: u64,
    selection_generation: u64,
    playback: Option<(i64, i64)>,
    sort: HistorySort,
    descending: bool,
}

impl Default for HistoryBrowser {
    fn default() -> Self {
        Self {
            source: Arc::default(),
            indices: Vec::new(),
            title_keys: Vec::new(),
            query: None,
            positions: Default::default(),
            selected: HashSet::new(),
            anchor: None,
            generation: 0,
            selection_generation: 0,
            playback: None,
            sort: HistorySort::Recent,
            descending: true,
        }
    }
}

impl HistoryBrowser {
    pub(super) fn sync(&mut self, source: &Arc<Vec<HistoryEntry>>, playback: Option<(i64, i64)>) {
        let source_changed = !Arc::ptr_eq(&self.source, source);
        let playback_changed = self.playback != playback;
        self.playback = playback;
        if !source_changed {
            if playback_changed && self.sort == HistorySort::Recent {
                self.rebuild();
            }
            return;
        }
        self.source = Arc::clone(source);
        self.title_keys = source
            .iter()
            .map(|entry| entry.title.to_lowercase())
            .collect();
        self.rebuild();
        self.selected.retain(|id| self.positions.contains_key(id));
        self.anchor = self.anchor.filter(|id| self.positions.contains_key(id));
    }

    pub(super) fn set_query(&mut self, query: Option<String>) {
        let query = query.map(|query| query.to_lowercase());
        self.clear_selection();
        if self.query == query {
            return;
        }
        self.query = query;
        self.rebuild();
    }

    pub(super) fn total(&self) -> usize {
        self.source.len()
    }

    fn rebuild(&mut self) {
        self.indices.clear();
        self.indices.extend((0..self.source.len()).filter(|&index| {
            self.query
                .as_ref()
                .is_none_or(|query| self.title_keys[index].contains(query))
        }));
        self.indices.sort_by(|&a, &b| {
            let ordering = match self.sort {
                HistorySort::Recent => {
                    let key = |entry: &HistoryEntry| {
                        let current = self.playback.filter(|&(id, _)| id == entry.track_id);
                        (
                            current.map_or(entry.played_at, |(_, time)| time),
                            current.is_some(),
                        )
                    };
                    key(&self.source[a]).cmp(&key(&self.source[b]))
                }
                HistorySort::Title => self.title_keys[a].cmp(&self.title_keys[b]),
                HistorySort::Plays => self.source[a].play_count.cmp(&self.source[b].play_count),
            };
            if self.descending {
                ordering.reverse()
            } else {
                ordering
            }
        });
        self.positions.clear();
        self.positions.extend(
            self.indices
                .iter()
                .enumerate()
                .map(|(position, &index)| (self.source[index].track_id, position)),
        );
        self.generation = self.generation.wrapping_add(1);
    }

    pub(super) fn contains(&self, id: i64) -> bool {
        self.selected.contains(&id)
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn prepare_selection(
        &self,
        id: i64,
        shift: bool,
        additive: bool,
    ) -> Option<HistoryCommit> {
        let &position = self.positions.get(&id)?;
        let mut selected = if additive {
            self.selected.clone()
        } else {
            HashSet::new()
        };
        let anchor = self.anchor.filter(|id| self.positions.contains_key(id));
        if shift {
            let start = anchor.map_or(position, |id| self.positions[&id]);
            selected.extend(
                self.indices[start.min(position)..=start.max(position)]
                    .iter()
                    .map(|&index| self.source[index].track_id),
            );
        } else if !additive || !selected.remove(&id) {
            selected.insert(id);
        }
        Some(HistoryCommit {
            track_id: id,
            selected,
            anchor: if shift { anchor.or(Some(id)) } else { Some(id) },
            generation: self.generation,
            selection_generation: self.selection_generation,
        })
    }

    pub(super) fn commit_valid(&self, commit: &HistoryCommit) -> bool {
        commit.generation == self.generation
            && commit.selection_generation == self.selection_generation
            && self.positions.contains_key(&commit.track_id)
    }

    pub(super) fn commit_selection(&mut self, commit: HistoryCommit) -> bool {
        if !self.commit_valid(&commit) {
            return false;
        }
        self.selected = commit.selected;
        self.anchor = commit.anchor;
        self.selection_generation = self.selection_generation.wrapping_add(1);
        true
    }

    pub(super) fn select_all(&mut self) {
        self.selected.clear();
        self.selected.extend(self.positions.keys().copied());
        self.anchor = None;
        self.selection_generation = self.selection_generation.wrapping_add(1);
    }

    pub(super) fn clear_selection(&mut self) {
        self.selected.clear();
        self.anchor = None;
        self.selection_generation = self.selection_generation.wrapping_add(1);
    }

    pub(super) fn ordered_selection(&self) -> Vec<i64> {
        self.indices
            .iter()
            .map(|&index| self.source[index].track_id)
            .filter(|id| self.selected.contains(id))
            .collect()
    }

    pub(super) fn drag_ids(&self, id: i64, generation: u64) -> Vec<i64> {
        if generation != self.generation || !self.positions.contains_key(&id) {
            return Vec::new();
        }
        if self.contains(id) {
            self.ordered_selection()
        } else {
            vec![id]
        }
    }

    pub(super) fn len(&self) -> usize {
        self.indices.len()
    }

    pub(super) fn get(&self, index: usize) -> Option<&HistoryEntry> {
        self.indices.get(index).map(|&index| &self.source[index])
    }

    pub(super) fn sort(&self) -> HistorySort {
        self.sort
    }
    pub(super) fn descending(&self) -> bool {
        self.descending
    }
    pub(super) fn set_sort(&mut self, sort: HistorySort) {
        if self.sort == sort {
            self.descending = !self.descending;
        } else {
            self.sort = sort;
            self.descending = matches!(sort, HistorySort::Recent | HistorySort::Plays);
        }
        self.rebuild();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search_count_result(root: &std::path::Path, totals: (usize, usize)) -> SearchCountResult {
        SearchCountResult {
            totals,
            roots: [(root.to_path_buf(), totals)].into_iter().collect(),
        }
    }

    #[test]
    fn search_counts_changed_demand_rejects_old_completion_and_releases_worker() {
        // Completion must also reject changes not yet reconciled by viewport demand.
        for reconcile in [false, true] {
            for change in 0..6 {
                let mut library = LibraryBrowser::default();
                let mut roots = vec![std::path::PathBuf::from("/music")];
                let mut revisions = QueryRevisions::default();
                let (changed, request) = library.prepare_search_counts(&roots, revisions);
                assert!(changed);
                let request = request.unwrap();
                assert!(library.search_counts_pending());
                match change {
                    0 => roots[0] = "/other".into(),
                    1 => library.refresh_search("needle".into()),
                    2 => library.toggle_favorites(),
                    3 => {
                        library.toggle_missing();
                        library.refresh_filter();
                    }
                    4 => revisions.library += 1,
                    5 => revisions.structure += 1,
                    _ => unreachable!(),
                }
                if reconcile {
                    let (changed, replacement) = library.prepare_search_counts(&roots, revisions);
                    assert!(changed);
                    assert!(replacement.is_none());
                    assert!(!request.ticket.is_current());
                    assert!(library.search_counts_pending());
                }
                let stale = search_count_result(&request.roots[0], (99, 99));
                assert!(!library.accept_search_counts(request, &roots, revisions, Some(stale)));
                assert!(!library.search_counts_pending());
                assert_eq!(library.search_counts(), None);
                assert!(!library.root_count_failed(&roots[0]));
                let (_, current) = library.prepare_search_counts(&roots, revisions);
                assert!(library.accept_search_counts(
                    current.unwrap(),
                    &roots,
                    revisions,
                    Some(search_count_result(&roots[0], (2, 3))),
                ));
                assert_eq!(library.search_counts(), Some((2, 3)));
                assert!(!library.search_counts_pending());
                assert!(library.prepare_search_counts(&roots, revisions).1.is_none());
            }
        }
    }

    #[test]
    fn search_counts_hidden_cancellation_keeps_worker_until_completion_then_resumes() {
        for failed_completion in [false, true] {
            let mut library = LibraryBrowser::default();
            let roots = vec![std::path::PathBuf::from("/music")];
            let revisions = QueryRevisions::default();
            library.refresh_search("needle".into());
            let (_, request) = library.prepare_search_counts(&roots, revisions);
            let request = request.unwrap();
            library.cancel_pending_reads();
            assert!(!request.ticket.is_current());
            assert!(library.search_counts_pending());
            // Revealing before physical completion cannot submit another worker.
            assert!(library.prepare_search_counts(&roots, revisions).1.is_none());
            let result = (!failed_completion).then(|| search_count_result(&roots[0], (99, 99)));
            assert!(!library.accept_search_counts(request, &roots, revisions, result));
            assert!(!library.search_counts_pending());
            assert_eq!(library.search_counts(), None);
            assert!(!library.root_count_failed(&roots[0]));
            let (_, request) = library.prepare_search_counts(&roots, revisions);
            assert!(library.accept_search_counts(
                request.unwrap(),
                &roots,
                revisions,
                Some(search_count_result(&roots[0], (1, 3))),
            ));
            assert_eq!(library.root_count_label(&roots[0]), "1 / 3");
            library.cancel_pending_reads();
            assert_eq!(library.search_counts(), Some((1, 3)));
            assert!(library.prepare_search_counts(&roots, revisions).1.is_none());
        }
    }

    #[test]
    fn search_counts_failure_requires_retry_or_changed_demand() {
        let mut library = LibraryBrowser::default();
        let roots = vec![std::path::PathBuf::from("/music")];
        let revisions = QueryRevisions::default();
        library.refresh_search("needle".into());
        let (_, request) = library.prepare_search_counts(&roots, revisions);
        assert!(library.accept_search_counts(request.unwrap(), &roots, revisions, None));
        assert!(!library.search_counts_pending());
        assert_eq!(library.search_counts(), None);
        assert_eq!(library.root_count_label(&roots[0]), "—");
        assert!(library.root_count_failed(&roots[0]));
        library.cancel_pending_reads();
        assert!(library.prepare_search_counts(&roots, revisions).1.is_none());
        assert!(library.root_count_failed(&roots[0]));
        library.retry_root_counts();
        assert!(!library.root_count_failed(&roots[0]));
        let (_, request) = library.prepare_search_counts(&roots, revisions);
        // Retrying again while pending cannot release the worker slot.
        library.retry_root_counts();
        assert!(library.prepare_search_counts(&roots, revisions).1.is_none());
        assert!(library.accept_search_counts(request.unwrap(), &roots, revisions, None));
        library.refresh_search("other".into());
        let (changed, request) = library.prepare_search_counts(&roots, revisions);
        assert!(changed);
        assert!(!library.root_count_failed(&roots[0]));
        assert!(library.accept_search_counts(
            request.unwrap(),
            &roots,
            revisions,
            Some(search_count_result(&roots[0], (0, 3))),
        ));
        assert_eq!(library.search_counts(), Some((0, 3)));
        assert_eq!(library.root_count_label(&roots[0]), "0 / 3");
        // An accepted payload is invalidated together with its per-root counts.
        library.refresh_search("third".into());
        let (changed, request) = library.prepare_search_counts(&roots, revisions);
        assert!(changed);
        assert!(request.is_some());
        assert_eq!(library.search_counts(), None);
        assert_eq!(library.root_count_label(&roots[0]), "—");
    }

    #[test]
    fn library_locate_expands_ancestry_without_changing_selected_identities() {
        use crate::model::LibraryLocation;
        let mut library = LibraryBrowser::default();
        let folder =
            LibraryNode::Directory(std::sync::Arc::from(std::path::Path::new("/music/other")));
        library.select_node(&LibraryNode::Track(41), false);
        library.select_node(&folder, true);
        let selected = library.selected().clone();
        let nodes = library.selected_nodes().clone();
        let roots = vec!["/music".into(), "/music/Album".into()];
        let (branch, offset) = library
            .reveal_playing(
                0,
                &roots,
                &[
                    LibraryLocation {
                        offset: 4,
                        total: 100,
                        child: Some("/music/Album".into()),
                    },
                    LibraryLocation {
                        offset: 79,
                        total: 80,
                        child: None,
                    },
                ],
            )
            .unwrap();
        assert_eq!(offset, 79);
        assert_eq!(branch.path(), std::path::Path::new("/music/Album"));
        assert_eq!(library.tree().location_index(&branch, offset), Some(85));
        assert_eq!(library.selected(), &selected);
        assert_eq!(library.selected_nodes(), &nodes);
        library.clear_locate_filters();
        assert_eq!(library.selected(), &selected);
        assert_eq!(library.selected_nodes(), &nodes);
    }

    fn history_selection_source(ids: &[i64]) -> Arc<Vec<HistoryEntry>> {
        Arc::new(
            ids.iter()
                .map(|&id| HistoryEntry {
                    track_id: id,
                    title: format!("track {}", 10 - id),
                    play_count: id as u64,
                    played_at: 100 - id,
                    favorite: false,
                })
                .collect(),
        )
    }

    fn select_history(history: &mut HistoryBrowser, id: i64, shift: bool, additive: bool) {
        let commit = history.prepare_selection(id, shift, additive).unwrap();
        assert!(history.commit_selection(commit));
    }

    #[test]
    fn history_search_rebuilds_cached_matches_and_keeps_identity_and_sort() {
        let mut history = HistoryBrowser::default();
        history.sync(&history_selection_source(&[1, 2, 3, 4]), None);
        history.select_all();
        let old_commit = history.prepare_selection(2, false, false).unwrap();
        history.set_query(Some("TRACK 8".into()));
        assert_eq!(history.total(), 4);
        assert_eq!(history.len(), 1);
        assert_eq!(history.get(0).unwrap().track_id, 2);
        assert!(history.ordered_selection().is_empty());
        assert!(!history.commit_selection(old_commit));
        history.select_all();
        assert_eq!(history.ordered_selection(), [2]);
        history.set_sort(HistorySort::Title);
        assert_eq!(history.ordered_selection(), [2]);
        let previous_match = history.prepare_selection(2, false, false).unwrap();
        let mut updated = history_selection_source(&[4, 3, 2, 1]);
        let updated_rows = Arc::make_mut(&mut updated);
        updated_rows[0].title = "Track 8 remastered".into();
        updated_rows[2].title = "Different title".into();
        history.sync(&updated, None);
        assert_eq!(history.len(), 1);
        assert_eq!(history.get(0).unwrap().track_id, 4);
        assert!(history.ordered_selection().is_empty());
        assert!(!history.commit_selection(previous_match));
        history.select_all();
        assert_eq!(history.ordered_selection(), [4]);
        history.sync(&history_selection_source(&[1, 3, 4]), None);
        assert_eq!(history.len(), 0);
        assert_eq!(history.total(), 3);
        assert!(history.ordered_selection().is_empty());
        history.set_query(None);
        assert_eq!(history.len(), 3);
        assert_eq!(history.get(0).unwrap().track_id, 4);
        assert!(history.ordered_selection().is_empty());
    }

    #[test]
    fn history_selection_is_independent_and_ranges_follow_visible_order() {
        let mut browser = BrowserState::default();
        browser.library.select_node(&LibraryNode::Track(99), false);
        browser
            .history
            .sync(&history_selection_source(&[1, 2, 3, 4]), None);
        select_history(&mut browser.history, 2, false, false);
        select_history(&mut browser.history, 4, true, false);
        assert_eq!(browser.history.ordered_selection(), [2, 3, 4]);
        select_history(&mut browser.history, 1, false, true);
        assert_eq!(browser.history.ordered_selection(), [1, 2, 3, 4]);
        select_history(&mut browser.history, 3, false, true);
        assert_eq!(browser.history.ordered_selection(), [1, 2, 4]);
        select_history(&mut browser.history, 4, true, true);
        assert_eq!(browser.history.ordered_selection(), [1, 2, 3, 4]);
        assert_eq!(browser.library.selected(), &HashSet::from([99]));
        browser.history.set_sort(HistorySort::Title);
        assert_eq!(browser.history.ordered_selection(), [4, 3, 2, 1]);
        select_history(&mut browser.history, 2, false, false);
        select_history(&mut browser.history, 4, true, false);
        assert_eq!(browser.history.ordered_selection(), [4, 3, 2]);
    }

    #[test]
    fn history_prepared_selection_is_atomic_and_stale_projection_cannot_commit() {
        let mut history = HistoryBrowser::default();
        let source = history_selection_source(&[1, 2, 3]);
        history.sync(&source, None);
        select_history(&mut history, 1, false, false);
        let canceled = history.prepare_selection(3, true, false).unwrap();
        assert_eq!(history.ordered_selection(), [1]);
        drop(canceled);
        assert_eq!(history.ordered_selection(), [1]);
        let old_sort = history.prepare_selection(2, false, false).unwrap();
        history.set_sort(HistorySort::Title);
        assert!(!history.commit_valid(&old_sort));
        assert!(!history.commit_selection(old_sort));
        let old_source = history.prepare_selection(3, false, false).unwrap();
        history.sync(&history_selection_source(&[1, 2]), None);
        assert!(!history.commit_selection(old_source));
        assert_eq!(history.ordered_selection(), [1]);
        assert!(history.prepare_selection(3, false, false).is_none());
    }

    #[test]
    fn history_selection_changes_and_global_clear_reject_pending_commits() {
        let mut browser = BrowserState::default();
        browser
            .history
            .sync(&history_selection_source(&[1, 2, 3]), None);
        select_history(&mut browser.history, 1, false, false);
        let old_selection = browser.history.prepare_selection(2, true, false).unwrap();
        select_history(&mut browser.history, 3, false, true);
        assert!(!browser.history.commit_selection(old_selection));
        assert_eq!(browser.history.ordered_selection(), [1, 3]);
        let before_all = browser.history.prepare_selection(2, false, false).unwrap();
        browser.history.select_all();
        assert!(!browser.history.commit_selection(before_all));
        let before_clear = browser.history.prepare_selection(2, false, false).unwrap();
        browser.library.select_node(&LibraryNode::Track(99), false);
        browser.clear_selection();
        assert!(browser.history.ordered_selection().is_empty());
        assert!(browser.history.anchor.is_none());
        assert!(browser.library.selected().is_empty());
        assert!(!browser.history.commit_selection(before_clear));
    }

    #[test]
    fn history_refresh_prunes_removed_ids_and_select_all_uses_current_projection() {
        let mut history = HistoryBrowser::default();
        history.sync(&history_selection_source(&[1, 2, 3, 4]), None);
        select_history(&mut history, 2, false, false);
        select_history(&mut history, 4, false, true);
        history.set_sort(HistorySort::Title);
        history.sync(&history_selection_source(&[4, 3, 1]), None);
        assert_eq!(history.ordered_selection(), [4]);
        history.select_all();
        assert_eq!(history.ordered_selection(), [4, 3, 1]);
        history.sync(&Arc::default(), None);
        assert!(history.ordered_selection().is_empty());
        assert!(history.anchor.is_none());
    }

    #[test]
    fn history_drag_freezes_current_group_without_including_unselected_rows() {
        let mut history = HistoryBrowser::default();
        let source = history_selection_source(&[1, 2, 3, 4]);
        history.sync(&source, None);
        let rendered_generation = history.generation();
        select_history(&mut history, 2, false, false);
        select_history(&mut history, 4, false, true);
        // Selection can change after rendering but before the actual drag begins.
        let frozen = history.drag_ids(4, rendered_generation);
        assert_eq!(frozen, [2, 4]);
        assert_eq!(history.drag_ids(3, rendered_generation), [3]);
        assert_eq!(history.ordered_selection(), [2, 4]);
        history.set_sort(HistorySort::Title);
        assert!(history.drag_ids(4, rendered_generation).is_empty());
        assert_eq!(history.drag_ids(4, history.generation()), [4, 2]);
        assert_eq!(frozen, [2, 4]);
        let sorted_generation = history.generation();
        history.sync(&history_selection_source(&[1, 2, 3]), None);
        assert!(history.drag_ids(4, sorted_generation).is_empty());
        assert!(history.drag_ids(4, history.generation()).is_empty());
    }

    #[test]
    fn history_playback_reorder_preserves_selected_ids_but_invalidates_old_drag() {
        let mut history = HistoryBrowser::default();
        let source = history_selection_source(&[1, 2, 3]);
        history.sync(&source, None);
        history.select_all();
        let generation = history.generation();
        history.sync(&source, Some((3, 200)));
        assert_eq!(history.ordered_selection(), [3, 1, 2]);
        assert!(history.drag_ids(1, generation).is_empty());
        assert_eq!(history.drag_ids(1, history.generation()), [3, 1, 2]);
    }

    #[test]
    fn history_sort_survives_reopening_and_tracks_source_replacement() {
        let source = Arc::new(vec![
            HistoryEntry {
                track_id: 1,
                title: "beta".into(),
                play_count: 2,
                played_at: 30,
                favorite: false,
            },
            HistoryEntry {
                track_id: 2,
                title: "ALPHA".into(),
                play_count: 4,
                played_at: 10,
                favorite: true,
            },
            HistoryEntry {
                track_id: 3,
                title: "alpha".into(),
                play_count: 1,
                played_at: 20,
                favorite: false,
            },
        ]);
        let mut history = HistoryBrowser::default();
        history.sync(&source, None);
        let ids = |history: &HistoryBrowser| {
            (0..history.len())
                .map(|index| history.get(index).unwrap().track_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&history), [1, 3, 2]);
        history.set_sort(HistorySort::Title);
        assert_eq!(ids(&history), [2, 3, 1]);
        history.sync(&source, None);
        assert_eq!(ids(&history), [2, 3, 1]);
        history.set_sort(HistorySort::Title);
        assert_eq!(ids(&history), [1, 2, 3]);
        history.set_sort(HistorySort::Plays);
        assert_eq!(ids(&history), [2, 1, 3]);
        let replacement = Arc::new(vec![source[2].clone(), source[0].clone()]);
        history.sync(&replacement, None);
        assert_eq!(ids(&history), [1, 3]);
        history.sync(&Arc::default(), None);
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn history_recency_uses_live_playback_time_without_replacing_history() {
        let source = Arc::new(vec![
            HistoryEntry {
                track_id: 1,
                title: "alpha".into(),
                play_count: 2,
                played_at: 30,
                favorite: false,
            },
            HistoryEntry {
                track_id: 2,
                title: "beta".into(),
                play_count: 1,
                played_at: 10,
                favorite: false,
            },
        ]);
        let mut history = HistoryBrowser::default();
        let ids = |history: &HistoryBrowser| {
            (0..history.len())
                .map(|index| history.get(index).unwrap().track_id)
                .collect::<Vec<_>>()
        };
        history.sync(&source, Some((2, 40)));
        assert_eq!(ids(&history), [2, 1]);
        // Equal-second timestamps still put the current track first in descending order.
        history.sync(&source, Some((2, 30)));
        assert_eq!(ids(&history), [2, 1]);
        history.set_sort(HistorySort::Recent);
        assert_eq!(ids(&history), [1, 2]);
        history.sync(&source, Some((2, 20)));
        assert_eq!(ids(&history), [2, 1]);
        history.set_sort(HistorySort::Recent);
        assert_eq!(ids(&history), [1, 2]);
        history.sync(&source, Some((1, 5)));
        assert_eq!(ids(&history), [2, 1]);
        history.sync(&source, None);
        assert_eq!(ids(&history), [1, 2]);
        history.set_sort(HistorySort::Title);
        history.sync(&source, Some((2, 40)));
        assert_eq!(ids(&history), [1, 2]);
        history.set_sort(HistorySort::Plays);
        assert_eq!(ids(&history), [1, 2]);
    }

    #[test]
    fn accepted_tree_page_locates_selected_track_without_viewport_repair() {
        let mut library = LibraryBrowser::default();
        library.select_node(&LibraryNode::Track(41), false);
        let forest = TreeBranch::default();
        library.prepare_directory(forest.clone(), 0, 20, 1);
        library.accept_directory(
            &forest,
            1,
            Page {
                total: 1,
                rows: vec![DirectoryRow::Track(crate::model::LibraryRow {
                    id: 41,
                    title: "selected".into(),
                    artist: String::new(),
                    album: String::new(),
                    duration: None,
                    codec: String::new(),
                    bitrate_bps: None,
                    bitrate_exact: false,
                    favorite: false,
                    missing: false,
                    play_count: 0,
                })],
            },
        );
        assert_eq!(library.tree().selected_index(), Some(0));
        library.refresh_tree(10);
        assert!(library.selected().contains(&41));
    }
    #[test]
    fn page_retry_preserves_unknown_count_until_an_empty_response_arrives() {
        let mut library = LibraryBrowser::default();
        let forest = TreeBranch::default();
        library.prepare_directory(forest.clone(), 0, 20, 1);
        library.fail_directory(&forest, 1);
        assert_eq!(library.tree().row_count(), 1);
        assert!(library.demand_directories(0..1).is_empty());
        library.retry_directory(&forest);
        assert_eq!(library.demand_directories(0..1), vec![(forest.clone(), 0)]);
        library.prepare_directory(forest.clone(), 0, 20, 2);
        library.accept_directory(
            &forest,
            2,
            Page {
                total: 0,
                rows: Vec::new(),
            },
        );
        assert_eq!(library.tree().row_count(), 0);
        assert!(library.demand_directories(0..1).is_empty());

        let mut playlists = PlaylistBrowser::default();
        playlists.select(41, "empty".into(), 0);
        playlists.prepare_entries(41);
        assert_eq!(playlists.entries().rows.len(), 1);
        playlists.prepare_page(true, 0, 20);
        playlists.fail_page(true, "unavailable".into());
        assert_eq!(playlists.demand_page(true, 0..1, 20), None);
        assert_eq!(playlists.retry_page(true, 20), Some(0));
        playlists.prepare_page(true, 0, 20);
        playlists.accept_entries(Page {
            total: 0,
            rows: Vec::new(),
        });
        assert!(playlists.entries().rows.is_empty());
        assert_eq!(playlists.selected(), Some(41));
        assert_eq!(playlists.retry_page(true, 20), None);
    }

    #[test]
    fn drag_source_resolves_current_mixed_selection_without_duplicates() {
        let mut library = LibraryBrowser::default();
        let folder =
            LibraryNode::Directory(std::sync::Arc::from(std::path::Path::new("/music/album")));
        library.apply_range(
            HashSet::from([folder.clone(), LibraryNode::Track(1), LibraryNode::Track(2)]),
            false,
        );
        for source in [&folder, &LibraryNode::Track(1)] {
            let nodes = library.drag_nodes(source);
            assert_eq!(nodes.len(), 3);
            assert_eq!(
                nodes.into_iter().collect::<HashSet<_>>(),
                HashSet::from([folder.clone(), LibraryNode::Track(1), LibraryNode::Track(2)])
            );
        }
        assert_eq!(
            library.drag_nodes(&LibraryNode::Track(3)),
            [LibraryNode::Track(3)]
        );
        library.select_node(&LibraryNode::Track(2), false);
        assert_eq!(
            library.drag_nodes(&LibraryNode::Track(1)),
            [LibraryNode::Track(1)]
        );
        assert_eq!(
            library.drag_nodes(&LibraryNode::Track(2)),
            [LibraryNode::Track(2)]
        );
        library.clear_selection();
        assert_eq!(library.drag_nodes(&folder), [folder]);
    }

    #[test]
    fn filter_and_view_changes_cancel_deferred_navigation_without_losing_identity() {
        let mut library = LibraryBrowser::default();
        library.select_node(&LibraryNode::Track(41), false);
        library.tree.defer_selection(0);
        library.toggle_missing();
        library.refresh_filter();
        assert_eq!(library.tree.ready_selection(), None);
        assert!(library.tree_active());
        assert_eq!(library.buffer().missing, Some(true));
        assert!(library.selected().contains(&41));
        library.tree.defer_selection(0);
        library.toggle_view();
        assert_eq!(library.tree.ready_selection(), None);
        assert!(!library.tree_active());
        assert_eq!(library.buffer().missing, Some(true));
        library.toggle_view();
        assert!(library.tree_active());
        library.clear_selection();
        assert!(library.selected().is_empty());
    }

    #[test]
    fn refreshed_projection_cancels_old_index_navigation_but_keeps_track_identity() {
        let mut library = LibraryBrowser::default();
        library.select_node(&LibraryNode::Track(41), false);
        library.tree.defer_selection(0);
        library.prepare_query(String::new());
        assert_eq!(library.tree.ready_selection(), None);
        assert!(library.selected().contains(&41));
    }

    #[test]
    fn search_changes_and_clear_keep_view_and_reject_old_directory_requests() {
        for tree_active in [true, false] {
            let mut library = LibraryBrowser::default();
            if !tree_active {
                library.toggle_view();
            }
            library.toggle_favorites();
            library.select_node(&LibraryNode::Track(41), false);
            let branch = TreeBranch::default();
            for (index, query) in ["first", "second", ""].into_iter().enumerate() {
                library.select_node(&LibraryNode::Track(41), false);
                let previous_query = library.buffer().query.clone();
                let generation = index as u64 + 1;
                library.prepare_directory(branch.clone(), 0, 20, generation);
                assert!(library.directory_request_pending(
                    &branch,
                    generation,
                    previous_query.as_deref(),
                    Some(true),
                    tree_active,
                    None,
                ));
                library.tree.defer_selection(0);
                library.refresh_search(query.into());
                assert_eq!(library.tree_active(), tree_active);
                assert_eq!(library.tree.ready_selection(), None);
                assert!(library.selected().is_empty());
                assert!(library.selected_nodes().is_empty());
                assert!(!library.directory_request_pending(
                    &branch,
                    generation,
                    previous_query.as_deref(),
                    Some(true),
                    tree_active,
                    None,
                ));
                let expected = (!query.is_empty()).then_some(query);
                assert_eq!(library.buffer().query.as_deref(), expected);
                library.prepare_directory(branch.clone(), 0, 20, generation + 10);
                assert!(library.directory_request_pending(
                    &branch,
                    generation + 10,
                    expected,
                    Some(true),
                    tree_active,
                    None,
                ));
                assert!(!library.directory_request_pending(
                    &branch,
                    generation + 10,
                    expected,
                    None,
                    tree_active,
                    None,
                ));
                assert_eq!(library.buffer().query.as_deref(), expected);
                assert_eq!(library.buffer().favorite, Some(true));
            }
            library.toggle_missing();
            library.refresh_filter();
            library.prepare_query(String::new());
            assert_eq!(library.tree_active(), tree_active);
            assert_eq!(library.buffer().missing, Some(true));
        }
    }

    #[test]
    fn missing_changes_preserve_views_roots_counts_and_reject_late_pages() {
        for tree_active in [true, false] {
            let mut library = LibraryBrowser::default();
            if !tree_active {
                library.toggle_view();
            }
            library.toggle_favorites();
            library.refresh_search("needle".into());
            library.select_node(&LibraryNode::Track(41), false);
            let roots = vec![std::path::PathBuf::from("/music")];
            let (count_generation, _) = library.begin_root_counts(&roots, 7);
            library.accept_root_counts(
                count_generation,
                0,
                vec![crate::model::LibraryRootCount {
                    path: roots[0].clone(),
                    total: 9,
                }],
            );
            let forest = TreeBranch::default();
            for (generation, missing) in [(1, Some(true)), (2, None), (3, Some(true))] {
                library.prepare_directory(forest.clone(), 0, 20, generation);
                let previous_missing = library.buffer().missing;
                library.toggle_missing();
                library.refresh_filter();
                assert_eq!(library.tree_active(), tree_active);
                assert_eq!(library.buffer().query.as_deref(), Some("needle"));
                assert_eq!(library.buffer().favorite, Some(true));
                assert_eq!(library.buffer().missing, missing);
                assert_eq!(library.root_count(&roots[0]), Some(9));
                assert!(library.counts_current(count_generation, &roots, 7));
                assert!(!library.directory_request_pending(
                    &forest,
                    generation,
                    Some("needle"),
                    Some(true),
                    tree_active,
                    previous_missing,
                ));
                library.accept_directory(
                    &forest,
                    generation,
                    Page {
                        total: 0,
                        rows: vec![],
                    },
                );
                let current = generation + 10;
                library.prepare_directory(forest.clone(), 0, 20, current);
                assert!(library.directory_request_pending(
                    &forest,
                    current,
                    Some("needle"),
                    Some(true),
                    tree_active,
                    missing,
                ));
                assert!(!library.directory_request_pending(
                    &forest,
                    current,
                    Some("needle"),
                    Some(true),
                    tree_active,
                    previous_missing,
                ));
                library.fail_directory(&forest, generation);
                library.accept_directory(
                    &forest,
                    generation,
                    Page {
                        total: 0,
                        rows: vec![],
                    },
                );
                assert!(library.directory_request_pending(
                    &forest,
                    current,
                    Some("needle"),
                    Some(true),
                    tree_active,
                    missing,
                ));
                library.accept_directory(
                    &forest,
                    current,
                    Page {
                        total: 1,
                        rows: vec![DirectoryRow::Directory {
                            path: roots[0].clone(),
                        }],
                    },
                );
                let Some(crate::gui::library::tree::TreeItem::Entry(entry)) =
                    library.tree().item(0)
                else {
                    panic!("expected configured root");
                };
                assert!(matches!(
                    &entry.row,
                    DirectoryRow::Directory { path } if path == &roots[0]
                ));
                let root = entry.branch.unwrap();
                library.expand_directory(&root, true);
                library.prepare_directory(root.clone(), 0, 20, current + 100);
                library.accept_directory(
                    &root,
                    current + 100,
                    Page {
                        total: 0,
                        rows: vec![],
                    },
                );
                assert_eq!(library.tree().row_count(), 1);
                assert_eq!(
                    library.tree().status(&root),
                    crate::gui::library::BranchStatus::Empty
                );
                assert_eq!(library.root_count(&roots[0]), Some(9));
                assert!(library.selected().contains(&41));
            }
        }
    }

    #[test]
    fn root_counts_are_batched_unknown_retryable_and_revision_bound() {
        let mut library = LibraryBrowser::default();
        let roots: Vec<_> = (0..crate::model::MAX_QUERY_ROWS + 1)
            .map(|index| std::path::PathBuf::from(format!("/root/{index}")))
            .collect();
        let (generation, batches) = library.begin_root_counts(&roots, 7);
        assert_eq!(batches, vec![0]);
        assert_eq!(library.root_count(&roots[0]), None);
        library.accept_root_counts(
            generation,
            0,
            roots[..crate::model::MAX_QUERY_ROWS]
                .iter()
                .map(|path| crate::model::LibraryRootCount {
                    path: path.clone(),
                    total: 0,
                })
                .collect(),
        );
        assert_eq!(library.root_count(&roots[0]), Some(0));
        assert_eq!(
            library.begin_root_counts(&roots, 7).1,
            vec![crate::model::MAX_QUERY_ROWS]
        );
        library.fail_root_counts(generation, crate::model::MAX_QUERY_ROWS);
        assert!(!library.root_count_failed(&roots[0]));
        assert!(library.root_count_failed(roots.last().unwrap()));
        assert!(!library.root_count_failed(std::path::Path::new("/other")));
        assert_eq!(library.root_count(roots.last().unwrap()), None);
        assert!(library.begin_root_counts(&roots, 7).1.is_empty());
        assert!(library.root_counts_settled());
        assert!(library.root_count_failed(roots.last().unwrap()));
        library.retry_root_counts();
        assert!(!library.root_counts_settled());
        assert_eq!(
            library.begin_root_counts(&roots, 7).1,
            vec![crate::model::MAX_QUERY_ROWS]
        );
        assert!(!library.root_count_failed(roots.last().unwrap()));
        assert!(library.begin_root_counts(&roots, 7).1.is_empty());
        library.accept_root_counts(
            generation,
            crate::model::MAX_QUERY_ROWS,
            vec![crate::model::LibraryRootCount {
                path: roots.last().unwrap().clone(),
                total: 2,
            }],
        );
        assert_eq!(library.root_count(roots.last().unwrap()), Some(2));
        assert!(library.begin_root_counts(&roots, 7).1.is_empty());
        assert!(library.root_counts_settled());
        library.refresh_search("needle".into());
        assert_eq!(library.root_count(&roots[0]), Some(0));
        let (scanned, _) = library.begin_root_counts(&roots, 8);
        assert_ne!(generation, scanned);
        assert!(!library.counts_current(generation, &roots, 8));
        library.accept_root_counts(
            generation,
            0,
            vec![crate::model::LibraryRootCount {
                path: roots[0].clone(),
                total: 99,
            }],
        );
        assert_eq!(library.root_count(&roots[0]), None);
        library.accept_root_counts(
            scanned,
            0,
            vec![crate::model::LibraryRootCount {
                path: roots[0].clone(),
                total: 3,
            }],
        );
        assert_eq!(library.root_count(&roots[0]), Some(3));
        library.roots_changed();
        assert!(!library.root_counts_settled());
        assert!(!library.counts_current(scanned, &roots, 8));
        assert_eq!(library.root_count(&roots[0]), None);
        let empty = library.begin_root_counts(&[], 8);
        assert!(empty.1.is_empty());
        assert!(library.root_counts_settled());
    }

    #[test]
    fn replacing_roots_rejects_in_flight_counts_at_the_same_structure_revision() {
        let mut library = LibraryBrowser::default();
        let roots = vec![std::path::PathBuf::from("/music")];
        let (generation, batches) = library.begin_root_counts(&roots, 7);
        assert_eq!(batches, vec![0]);
        library.accept_root_counts(
            generation,
            0,
            vec![crate::model::LibraryRootCount {
                path: roots[0].clone(),
                total: 4,
            }],
        );
        assert_eq!(library.root_count(&roots[0]), Some(4));

        let replacement = vec![std::path::PathBuf::from("/other")];
        assert!(!library.counts_current(generation, &replacement, 7));
        let (next, batches) = library.begin_root_counts(&replacement, 7);
        assert_ne!(next, generation);
        assert_eq!(batches, vec![0]);
        library.fail_root_counts(generation, 0);
        library.accept_root_counts(
            generation,
            0,
            vec![crate::model::LibraryRootCount {
                path: roots[0].clone(),
                total: 99,
            }],
        );
        assert!(!library.root_count_failed(&replacement[0]));
        assert_eq!(library.root_count(&roots[0]), None);
        assert!(library.begin_root_counts(&replacement, 7).1.is_empty());
        library.accept_root_counts(
            next,
            0,
            vec![crate::model::LibraryRootCount {
                path: replacement[0].clone(),
                total: 6,
            }],
        );
        assert_eq!(library.root_count(&replacement[0]), Some(6));
    }

    #[test]
    fn replacing_roots_rejects_old_forest_and_group_pages() {
        use crate::gui::library::tree::TreeItem;
        let mut library = LibraryBrowser::default();
        library.toggle_view();
        let forest = TreeBranch::default();
        library.prepare_directory(forest.clone(), 0, 2, 1);
        library.accept_directory(
            &forest,
            1,
            Page {
                total: 1,
                rows: vec![DirectoryRow::Directory {
                    path: "/old".into(),
                }],
            },
        );
        let TreeItem::Entry(entry) = library.tree.item(0).unwrap() else {
            panic!("root header")
        };
        let old_root = entry.branch.unwrap();
        library.prepare_directory(old_root.clone(), 0, 2, 2);
        library.roots_changed();
        library.prepare_directory(forest.clone(), 0, 2, 3);
        library.accept_directory(
            &forest,
            1,
            Page {
                total: 0,
                rows: vec![],
            },
        );
        assert!(library.directory_request_pending(&forest, 3, None, None, false, None));
        library.accept_directory(
            &forest,
            3,
            Page {
                total: 1,
                rows: vec![DirectoryRow::Directory {
                    path: "/new".into(),
                }],
            },
        );
        library.accept_directory(
            &old_root,
            2,
            Page {
                total: 0,
                rows: vec![],
            },
        );
        let TreeItem::Entry(entry) = library.tree.item(0).unwrap() else {
            panic!("current root header")
        };
        assert!(
            matches!(entry.row, DirectoryRow::Directory { path } if path == std::path::Path::new("/new"))
        );
        assert_eq!(library.tree.row_count(), 2);
        assert!(!library.directory_request_pending(&old_root, 2, None, None, false, None));
    }

    fn summary(id: i64, entry_count: usize) -> PlaylistSummary {
        PlaylistSummary {
            id,
            name: format!("playlist {id}"),
            entry_count,
        }
    }

    #[test]
    fn empty_playlist_catalog_reloads_after_creation_and_last_deletion() {
        for query in [None, Some("playlist".into())] {
            let mut playlists = PlaylistBrowser::default();
            playlists.search_summaries(query);
            playlists.prepare_page(false, 0, 20);
            playlists.accept_summaries(Page {
                total: 0,
                rows: vec![],
            });
            assert_eq!(playlists.demand_page(false, 0..1, 20), None);

            for id in [1, 2] {
                playlists.invalidate_summaries();
                let range = 0..playlists.summaries().rows.len();
                assert_eq!(playlists.demand_page(false, range, 20), Some(0));
                playlists.prepare_page(false, 0, 20);
                playlists.accept_summaries(Page {
                    total: 1,
                    rows: vec![summary(id, 0)],
                });
                assert_eq!(playlists.summaries().rows.get(0).unwrap().id, id);

                playlists.invalidate_summaries();
                assert_eq!(playlists.demand_page(false, 0..1, 20), Some(0));
                playlists.prepare_page(false, 0, 20);
                playlists.accept_summaries(Page {
                    total: 0,
                    rows: vec![],
                });
                assert!(playlists.summaries().rows.is_empty());
                assert_eq!(playlists.demand_page(false, 0..1, 20), None);
            }
        }
    }

    #[test]
    fn playlist_batch_resize_preserves_counts_selection_and_viewport() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.prepare_page(false, 0, 20);
        playlists.accept_summaries(Page {
            total: 1,
            rows: vec![summary(1, 100)],
        });
        playlists.prepare_page(true, 80, 20);
        playlists.accept_entries(Page {
            total: 100,
            rows: (80..100)
                .map(|index| PlaylistEntryRow {
                    id: index as i64,
                    track_id: index as i64,
                    position: index,
                    offset: index,
                    title: format!("entry {index}"),
                    artist: String::new(),
                    album: String::new(),
                    duration: None,
                    codec: String::new(),
                    missing: false,
                    favorite: false,
                })
                .collect(),
        });
        playlists.select_entry(83, 83);
        playlists.demand_page(true, 80..90, 20);
        playlists.configure_page_size(25);
        assert_eq!(playlists.summaries().rows.total(), Some(1));
        assert_eq!(playlists.entries().rows.total(), Some(100));
        assert_eq!(playlists.entries().rows.visible(), 80..90);
        assert_eq!(playlists.selected_entry(), Some(83));
        assert!(playlists.entry_ready());
        assert_eq!(playlists.demand_page(true, 80..90, 25), Some(75));
        playlists.prepare_entries(1);
        assert_eq!(playlists.entries().rows.total(), None);
        assert_eq!(playlists.entries().rows.len(), 100);
    }

    #[test]
    fn playlist_lookup_distinguishes_unloaded_empty_and_deleted_playlists() {
        let mut browser = BrowserState::default();
        browser.playlists.select(41, "selected".into(), 40);
        browser.playlists.prepare_entries(41);
        browser.playlists.prepare_page(false, 0, 20);
        browser.playlists.accept_summaries(Page {
            total: 100,
            rows: (1..=20).map(|id| summary(id, 0)).collect(),
        });
        assert_eq!(browser.playlists.selected(), Some(41));
        browser.accept_playlist_lookup(41, Some(summary(41, 0)));
        assert_eq!(browser.playlists.selected(), Some(41));
        browser.playlists.select_entry(99, 0);
        browser.accept_playlist_lookup(41, Some(summary(42, 1)));
        assert_eq!(browser.playlists.selected(), None);
        assert_eq!(browser.playlists.selected_entry(), None);
        assert_eq!(browser.playlists.entries().playlist_id, None);
        assert!(browser.playlists.entries().rows.is_empty());
        assert!(browser.playlists.selected_name.is_empty());
    }

    #[test]
    fn stale_playlist_lookup_cannot_clear_another_selection() {
        let mut browser = BrowserState::default();
        browser.playlists.select(1, "one".into(), 0);
        browser.playlists.select(2, "two".into(), 1);
        browser.playlists.prepare_entries(2);
        browser.playlists.select_entry(20, 0);
        browser.accept_playlist_lookup(1, None);
        assert_eq!(browser.playlists.selected(), Some(2));
        assert_eq!(browser.playlists.selected_entry(), Some(20));
        browser.accept_playlist_lookup(2, None);
        assert_eq!(browser.playlists.selected(), None);
        assert_eq!(browser.playlists.selected_entry(), None);
        assert_eq!(browser.playlists.entries().playlist_id, None);
    }

    #[test]
    fn playlist_switch_and_removal_clear_entry_identity_and_payload() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.select_entry(10, 0);
        assert!(playlists.select(2, "two".into(), 1));
        assert_eq!(playlists.selected_entry(), None);
        playlists.prepare_entries(2);
        playlists.select_entry(20, 0);
        playlists.clear_selection();
        assert_eq!(playlists.selected(), None);
        assert_eq!(playlists.selected_entry(), None);
        assert_eq!(playlists.entries().playlist_id, None);
        assert!(playlists.entries().rows.is_empty());
    }
    #[test]
    fn metadata_row_refresh_does_not_invalidate_occurrence_identity() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.select_entry(41, 3);
        playlists.prepare_entries(1);
        assert_eq!(playlists.selected_entry(), Some(41));
        assert!(playlists.entry_ready());
    }

    #[test]
    fn playlist_reorder_refresh_is_scoped_and_preserves_catalog_payload() {
        use crate::projection::GuiLibrarySnapshot;
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.prepare_page(false, 0, 20);
        playlists.accept_summaries(Page {
            total: 1,
            rows: vec![PlaylistSummary {
                id: 1,
                name: "one".into(),
                entry_count: 10_000,
            }],
        });
        playlists.select_entry(41, 3);
        let previous = GuiLibrarySnapshot::default();
        let mut next = previous.clone();
        Arc::make_mut(&mut next.playlist_entry_revisions).insert(2, 1);
        assert_eq!(
            playlists.refresh_changes(&previous, &next),
            PlaylistRefresh {
                summaries: false,
                entries: false,
            }
        );
        Arc::make_mut(&mut next.playlist_entry_revisions).insert(1, 1);
        let refresh = playlists.refresh_changes(&previous, &next);
        assert_eq!(
            refresh,
            PlaylistRefresh {
                summaries: false,
                entries: true
            }
        );
        if refresh.entries {
            playlists.prepare_entries(1);
            playlists.track_selection.invalidate();
        }
        assert_eq!(playlists.summaries.rows.get(0).unwrap().name, "one");
        assert_eq!(playlists.selected_entry(), Some(41));
        assert!(!playlists.entry_ready());
        let coalesced = next.clone();
        Arc::make_mut(&mut next.playlist_entry_revisions).insert(2, 2);
        assert_eq!(
            playlists.refresh_changes(&coalesced, &next),
            PlaylistRefresh {
                summaries: false,
                entries: false,
            }
        );
        next.playlist_revision += 1;
        next.playlist_entry_revisions = Arc::default();
        assert_eq!(
            playlists.refresh_changes(&coalesced, &next),
            PlaylistRefresh {
                summaries: true,
                entries: true,
            }
        );
        let before_metadata = next.clone();
        next.revision += 1;
        assert_eq!(
            playlists.refresh_changes(&before_metadata, &next),
            PlaylistRefresh {
                summaries: false,
                entries: true,
            }
        );
    }

    #[test]
    fn playlist_switch_generation_rejects_drag_after_switching_back() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        let original = playlists.selection_generation();
        playlists.select(1, "renamed".into(), 0);
        assert_eq!(playlists.selection_generation(), original);
        playlists.select(2, "two".into(), 1);
        playlists.select(1, "one".into(), 0);
        assert_ne!(playlists.selection_generation(), original);
        let returned = playlists.selection_generation();
        playlists.clear_selection();
        playlists.select(1, "one".into(), 0);
        assert_ne!(playlists.selection_generation(), returned);
    }

    #[test]
    fn refreshed_offviewport_entry_resolves_identity_without_cached_pages() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.select_entry(41, 3);
        playlists.prepare_page(true, 8_000, 20);
        playlists.accept_entries(Page {
            total: 10_000,
            rows: (8_000..8_020)
                .map(|index| PlaylistEntryRow {
                    id: index as i64,
                    track_id: index as i64,
                    position: index,
                    offset: index,
                    title: format!("entry {index}"),
                    artist: String::new(),
                    album: String::new(),
                    duration: None,
                    codec: String::new(),
                    missing: false,
                    favorite: false,
                })
                .collect(),
        });
        playlists.demand_page(true, 8_000..8_010, 20);
        playlists.prepare_entries(1);
        playlists.track_selection.invalidate();
        assert_eq!(playlists.selected_entry(), Some(41));
        assert!(!playlists.entry_ready());
        // The one identity result restores actions without restoring offscreen payload.
        playlists.accept_entry_locations(vec![PlaylistEntryLocation {
            id: 41,
            playlist_id: 1,
            position: 7_000,
            offset: 7_000,
            track_id: 99,
            total: 10_000,
        }]);
        assert!(playlists.entry_ready());
        assert_eq!(
            playlists.track_selection.active.map(|entry| entry.track_id),
            Some(99)
        );
        assert_eq!(playlists.entries.rows.visible().start, 8_000);
        assert!(playlists.entries.rows.iter().next().is_none());
        assert_eq!(
            playlists.track_selection.active.map(|entry| entry.position),
            Some(7_000)
        );
        playlists.prepare_entries(1);
        playlists.track_selection.invalidate();
        assert!(!playlists.entry_ready());
        playlists.accept_entry_locations(vec![PlaylistEntryLocation {
            id: 41,
            playlist_id: 1,
            position: 6_000,
            offset: 6_000,
            track_id: 99,
            total: 10_000,
        }]);
        assert_eq!(
            playlists.track_selection.active.map(|entry| entry.position),
            Some(6_000)
        );
        playlists.track_selection.remove(41);
        assert!(!playlists.entry_ready());
    }

    #[test]
    fn missing_entry_clears_selection_not_an_unloaded_page() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.select_entry(41, 3);
        playlists.prepare_entries(1);
        playlists.track_selection.invalidate();
        assert!(!playlists.entry_ready());
        assert_eq!(playlists.selected_entry(), Some(41));
        playlists.accept_entry_locations(Vec::new());
        assert_eq!(playlists.selected_entry(), None);
        assert!(!playlists.entry_ready());
        assert_eq!(playlists.selected(), Some(1));
    }

    #[test]
    fn changed_membership_rejects_stale_entry_identity() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.select_entry(41, 0);
        playlists.accept_entry_locations(vec![PlaylistEntryLocation {
            id: 41,
            playlist_id: 1,
            position: 0,
            offset: 0,
            track_id: 99,
            total: 1,
        }]);
        assert!(playlists.entry_ready());
        playlists.prepare_entries(1);
        playlists.track_selection.invalidate();
        assert!(!playlists.entry_ready());
        playlists.accept_entry_locations(vec![PlaylistEntryLocation {
            id: 41,
            playlist_id: 2,
            position: 0,
            offset: 0,
            track_id: 99,
            total: 1,
        }]);
        assert_eq!(playlists.selected_entry(), None);
    }

    #[test]
    fn snapshot_revision_reconciliation_finishes_root_and_directory_requests_without_rendering() {
        let mut library = LibraryBrowser::default();
        let roots = vec![std::path::PathBuf::from("/music")];
        let (old, _) = library.begin_root_counts(&roots, 7);
        let branch = TreeBranch::default();
        library.prepare_directory(branch.clone(), 0, 20, 1);
        let (current, batches) = library.begin_root_counts(&roots, 8);
        library.refresh_tree(20);
        library.prepare_directory(branch.clone(), 0, 20, 2);
        assert_eq!(batches, [0]);
        assert!(library.begin_root_counts(&roots, 8).1.is_empty());
        assert!(!library.counts_current(old, &roots, 8));
        assert!(!library.directory_request_pending(&branch, 1, None, None, false, None));
        library.fail_root_counts(old, 0);
        library.fail_directory(&branch, 1);
        library.accept_root_counts(
            current,
            0,
            vec![crate::model::LibraryRootCount {
                path: roots[0].clone(),
                total: 0,
            }],
        );
        library.accept_directory(
            &branch,
            2,
            Page {
                total: 0,
                rows: Vec::new(),
            },
        );
        assert_eq!(library.root_count(&roots[0]), Some(0));
        assert!(library.begin_root_counts(&roots, 8).1.is_empty());
        assert!(library.demand_directories(0..1).is_empty());
        assert!(!library.directory_request_pending(&branch, 2, None, None, false, None));
    }
}
