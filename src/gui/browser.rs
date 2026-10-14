//! Browser state is independent of the window entity and frame scheduler.
//!
//! Selection identities outlive page eviction. Projection changes cancel range
//! selection and deferred navigation before replacing payloads; only explicit
//! selection/cancellation changes identities. Rendering and scope capture borrow
//! read-only buffers and tree state. Only the owners prepare, accept, retry, and
//! pin payloads or start and complete range selection.

#[path = "library/selection.rs"]
mod selection;

use super::queue::QueueSelection;
use super::{
    BrowserFocus, HistorySort, ListFocus,
    library::{DirectoryTree, LibraryNode, TreeBranch},
    paged::PagedRows,
    queries::ViewRequests,
};
use crate::model::{
    DirectoryRow, HistoryEntry, LibrarySort, Page, PlaylistEntryLocation, PlaylistEntryRow,
    PlaylistSummary, Query,
};
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
        self.requests
            .cancel(super::queries::ViewRequestKind::EntrySelection);
        self.playlists.clear_selection();
    }

    pub(super) fn accept_playlist_summaries(&mut self, page: Page<PlaylistSummary>) {
        if page.total == 0 {
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
        } else {
            self.remove_playlist_selection();
        }
    }

    pub(super) fn clear_selection(&mut self) {
        self.library.clear_selection();
        self.queue.clear();
        self.remove_playlist_selection();
    }
}

#[derive(Default)]
struct RootCounts {
    generation: u64,
    revision: u64,
    roots: Vec<std::path::PathBuf>,
    totals: std::collections::HashMap<std::path::PathBuf, usize>,
    pending: HashSet<usize>,
    failed: HashSet<usize>,
}

impl RootCounts {
    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.totals.clear();
        self.pending.clear();
        self.failed.clear();
    }
}

#[derive(Default)]
pub(super) struct LibraryViewBuffer {
    pub(super) query: Option<String>,
    pub(super) favorite: Option<bool>,
    pub(super) missing: Option<bool>,
    pub(super) sort: LibrarySort,
}

pub(super) struct LibraryBrowser {
    buffer: LibraryViewBuffer,
    tree: DirectoryTree,
    range_selection: LibrarySelection,
    selected: HashSet<i64>,
    selected_nodes: HashSet<LibraryNode>,
    tree_active: bool,
    favorites_only: bool,
    missing_only: bool,
    force_scan: bool,
    counts: RootCounts,
}

impl Default for LibraryBrowser {
    fn default() -> Self {
        Self {
            buffer: LibraryViewBuffer {
                sort: LibrarySort::Album,
                ..Default::default()
            },
            tree: DirectoryTree::default(),
            range_selection: LibrarySelection::default(),
            selected: HashSet::new(),
            selected_nodes: HashSet::new(),
            tree_active: true,
            favorites_only: false,
            missing_only: false,
            force_scan: false,
            counts: RootCounts::default(),
        }
    }
}

impl LibraryBrowser {
    pub(super) fn begin_root_counts(
        &mut self,
        roots: &[std::path::PathBuf],
        revision: u64,
    ) -> (u64, Vec<usize>) {
        if self.counts.roots != roots || self.counts.revision != revision {
            self.counts.invalidate();
            self.counts.roots = roots.to_vec();
            self.counts.revision = revision;
        }
        let batches = (0..roots.len())
            .step_by(crate::model::MAX_QUERY_ROWS)
            .filter(|offset| {
                !self.counts.pending.contains(offset)
                    && roots[*offset..(*offset + crate::model::MAX_QUERY_ROWS).min(roots.len())]
                        .iter()
                        .any(|root| !self.counts.totals.contains_key(root))
            })
            .collect::<Vec<_>>();
        self.counts.pending.extend(batches.iter().copied());
        self.counts.failed.clear();
        (self.counts.generation, batches)
    }

    pub(super) fn counts_current(
        &self,
        generation: u64,
        roots: &[std::path::PathBuf],
        revision: u64,
    ) -> bool {
        self.counts.generation == generation
            && self.counts.roots == roots
            && self.counts.revision == revision
    }

    pub(super) fn accept_root_counts(
        &mut self,
        generation: u64,
        offset: usize,
        counts: Vec<crate::model::LibraryRootCount>,
    ) {
        if self.counts.generation == generation && self.counts.pending.remove(&offset) {
            self.counts
                .totals
                .extend(counts.into_iter().map(|count| (count.path, count.total)));
        }
    }

    pub(super) fn fail_root_counts(&mut self, generation: u64, offset: usize) {
        if self.counts.generation == generation && self.counts.pending.remove(&offset) {
            self.counts.failed.insert(offset);
        }
    }

    pub(super) fn root_count(&self, path: &std::path::Path) -> Option<usize> {
        self.counts.totals.get(path).copied()
    }

    pub(super) fn root_counts_failed(&self) -> bool {
        !self.counts.failed.is_empty()
    }

    pub(super) fn buffer(&self) -> &LibraryViewBuffer {
        &self.buffer
    }

    pub(super) fn tree(&self) -> &DirectoryTree {
        &self.tree
    }

    pub(super) fn take_ready_selection(&mut self) -> Option<usize> {
        self.tree.ready_selection()
    }

    pub(super) fn focus_track(&mut self, id: i64) {
        self.remember_selection();
        let node = LibraryNode::Track(id);
        if let Some(index) = self.tree.index_of(&node) {
            self.tree.focus_selection(&node, index);
            self.tree
                .scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
    }

    pub(super) fn refresh_tree(&mut self, page_size: usize) {
        self.invalidate_projection();
        if !self.tree.configure_page_size(page_size) {
            self.tree.invalidate();
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

    pub(super) fn retry_directory(&mut self, branch: &TreeBranch) {
        self.tree.retry(branch);
    }

    pub(super) fn expand_directory(&mut self, branch: &TreeBranch, expanded: bool) -> bool {
        self.invalidate_selection();
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

    pub(super) fn demand_directories(&mut self, range: Range<usize>) -> Vec<(TreeBranch, usize)> {
        self.tree.requests_for_range(range)
    }

    pub(super) fn navigate_tree_row(&mut self, index: usize) -> Option<LibraryNode> {
        self.invalidate_selection();
        self.tree
            .scroll
            .scroll_to_item(index, ScrollStrategy::Nearest);
        let Some(super::library::tree::TreeItem::Entry(visible)) = self.tree.item(index) else {
            self.tree.defer_selection(index);
            return None;
        };
        self.tree.cancel_navigation();
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
        self.tree.cancel_navigation();
    }

    pub(super) fn invalidate_selection(&mut self) {
        self.range_selection.cancel(true);
    }

    pub(super) fn invalidate_projection(&mut self) {
        self.invalidate_selection();
        self.tree.cancel_navigation();
    }

    pub(super) fn clear_selection(&mut self) {
        self.invalidate_projection();
        self.selected.clear();
        self.selected_nodes.clear();
        self.remember_selection();
    }

    pub(super) fn select_node(&mut self, node: &LibraryNode, multi: bool) {
        self.tree.cancel_navigation();
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
        if self.tree_active {
            self.toggle_view();
        }
        self.buffer.missing = self.missing_only.then_some(true);
        self.tree.invalidate();
        self.scroll_filter_to_top();
    }

    pub(super) fn refresh_search(&mut self, query: String) {
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
    pub(super) offset: usize,
    pub(super) total: usize,
    pub(super) rows: PagedRows<PlaylistSummary>,
    pub(super) scroll: gpui::UniformListScrollHandle,
}

pub(super) struct PlaylistEntriesBuffer {
    pub(super) playlist_id: Option<i64>,
    pub(super) offset: usize,
    pub(super) total: usize,
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
            offset: 0,
            total: 0,
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
    selected_entry: Option<i64>,
    entry_details: Option<PlaylistEntryLocation>,
    selected_name: String,
    selection_generation: u64,
}

impl PlaylistBrowser {
    pub(super) fn refresh_changes(
        &self,
        previous: &crate::projection::GuiLibrarySnapshot,
        next: &crate::projection::GuiLibrarySnapshot,
    ) -> PlaylistRefresh {
        let summaries = previous.revision != next.revision
            || previous.structure_revision != next.structure_revision
            || previous.playlist_revision != next.playlist_revision;
        let entries = self.entries.playlist_id.is_some_and(|id| {
            summaries
                || previous.playlist_entry_revisions.get(&id)
                    != next.playlist_entry_revisions.get(&id)
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

    pub(super) fn prepare_page(&mut self, entries: bool, offset: usize, page_size: usize) -> Query {
        let offset = offset / page_size * page_size;
        if entries {
            self.entries.offset = offset;
            self.entries.rows.begin(offset, page_size);
            Query::PlaylistEntries {
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
        self.selected_entry
    }

    pub(super) fn select(&mut self, id: i64, name: String, index: usize) -> bool {
        let changed = self.selected != Some(id);
        if changed {
            self.selection_generation = self.selection_generation.wrapping_add(1);
            self.selected_entry = None;
            self.entry_details = None;
        }
        self.selected = Some(id);
        self.selected_name = name;
        self.summaries.rows.pin(Some(index));
        changed
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.selected = None;
        self.selected_entry = None;
        self.entry_details = None;
        self.selected_name.clear();
        self.summaries.rows.pin(None);
        self.entries = PlaylistEntriesBuffer::default();
    }

    pub(super) fn take_entry(&mut self) -> Option<i64> {
        self.entries.rows.pin(None);
        self.entry_details = None;
        self.selected_entry.take()
    }

    pub(super) fn select_entry(&mut self, id: i64, index: usize) {
        self.selected_entry = Some(id);
        self.entry_details = self
            .entries
            .rows
            .get(index)
            .filter(|row| row.id == id)
            .and_then(|row| {
                self.entries
                    .playlist_id
                    .map(|playlist_id| PlaylistEntryLocation {
                        id,
                        playlist_id,
                        position: index,
                        track_id: row.track_id,
                        total: self.entries.total,
                    })
            });
        self.entries.rows.pin(Some(index));
    }

    pub(super) fn prepare_entries(&mut self, id: i64) {
        let changed = self.entries.playlist_id != Some(id);
        if changed {
            self.selected_entry = None;
            self.entries.rows.clear();
            self.entries.total = 0;
            self.entries.scroll.scroll_to_item(0, ScrollStrategy::Top);
        } else {
            self.entries.rows.invalidate();
        }
        self.entry_details = None;
        self.entries.playlist_id = Some(id);
    }

    pub(super) fn reconcile_selection(&mut self) {
        // Eviction is not deletion; identities survive unloaded pages.
        if self.selected.is_none() || self.entries.playlist_id != self.selected {
            self.selected_entry = None;
            self.entry_details = None;
        }
    }

    pub(super) fn accept_summaries(&mut self, page: Page<PlaylistSummary>) {
        self.summaries.total = page.total;
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
        }
    }

    pub(super) fn accept_entries(&mut self, page: Page<PlaylistEntryRow>) {
        self.entries.total = page.total;
        self.entries.rows.accept(page);
        if let Some(index) = self
            .entries
            .rows
            .position(|row| Some(row.id) == self.selected_entry)
        {
            self.entries.rows.pin(Some(index));
        }
    }

    /// Compact identity lookup survives page eviction without retaining a whole playlist.
    pub(super) fn accept_entry_lookup(&mut self, id: i64, details: Option<PlaylistEntryLocation>) {
        if self.selected_entry != Some(id) {
            return;
        }
        let details = details
            .filter(|entry| entry.id == id && Some(entry.playlist_id) == self.entries.playlist_id);
        self.entries
            .rows
            .pin(details.as_ref().map(|entry| entry.position));
        if let Some(details) = details.as_ref() {
            self.entries.total = details.total;
        } else {
            self.take_entry();
        }
        self.entry_details = details;
    }

    pub(super) fn entry_ready(&self) -> bool {
        self.entry_details.is_some()
    }
}

pub(super) struct HistoryBrowser {
    source: Arc<Vec<HistoryEntry>>,
    indices: Vec<usize>,
    title_keys: Vec<String>,
    sort: HistorySort,
    descending: bool,
}

impl Default for HistoryBrowser {
    fn default() -> Self {
        Self {
            source: Arc::default(),
            indices: Vec::new(),
            title_keys: Vec::new(),
            sort: HistorySort::Recent,
            descending: true,
        }
    }
}

impl HistoryBrowser {
    pub(super) fn sync(&mut self, source: &Arc<Vec<HistoryEntry>>) {
        if Arc::ptr_eq(&self.source, source) {
            return;
        }
        self.source = Arc::clone(source);
        self.title_keys = source
            .iter()
            .map(|entry| entry.title.to_lowercase())
            .collect();
        self.rebuild();
    }

    fn rebuild(&mut self) {
        self.indices.clear();
        self.indices.extend(0..self.source.len());
        self.indices.sort_by(|&a, &b| {
            let ordering = match self.sort {
                HistorySort::Recent => self.source[a].played_at.cmp(&self.source[b].played_at),
                HistorySort::Title => self.title_keys[a].cmp(&self.title_keys[b]),
                HistorySort::Plays => self.source[a].play_count.cmp(&self.source[b].play_count),
            };
            if self.descending {
                ordering.reverse()
            } else {
                ordering
            }
        });
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

    #[test]
    fn history_sort_survives_reopening_and_tracks_source_replacement() {
        let source = Arc::new(vec![
            HistoryEntry {
                track_id: 1,
                title: "beta".into(),
                play_count: 2,
                played_at: 30,
            },
            HistoryEntry {
                track_id: 2,
                title: "ALPHA".into(),
                play_count: 4,
                played_at: 10,
            },
            HistoryEntry {
                track_id: 3,
                title: "alpha".into(),
                play_count: 1,
                played_at: 20,
            },
        ]);
        let mut history = HistoryBrowser::default();
        history.sync(&source);
        let ids = |history: &HistoryBrowser| {
            (0..history.len())
                .map(|index| history.get(index).unwrap().track_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&history), [1, 3, 2]);
        history.set_sort(HistorySort::Title);
        assert_eq!(ids(&history), [2, 3, 1]);
        history.sync(&source);
        assert_eq!(ids(&history), [2, 3, 1]);
        history.set_sort(HistorySort::Title);
        assert_eq!(ids(&history), [1, 2, 3]);
        history.set_sort(HistorySort::Plays);
        assert_eq!(ids(&history), [2, 1, 3]);
        let replacement = Arc::new(vec![source[2].clone(), source[0].clone()]);
        history.sync(&replacement);
        assert_eq!(ids(&history), [1, 3]);
        history.sync(&Arc::default());
        assert_eq!(history.len(), 0);
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
        library.refresh_filter();
        assert_eq!(library.take_ready_selection(), None);
        assert!(!library.tree_active());
        assert!(library.selected().contains(&41));
        library.tree.defer_selection(0);
        library.toggle_view();
        assert_eq!(library.take_ready_selection(), None);
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
        assert_eq!(library.take_ready_selection(), None);
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
                assert_eq!(library.take_ready_selection(), None);
                assert!(library.selected().contains(&41));
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
            assert!(!library.tree_active());
            assert_eq!(library.buffer().missing, Some(true));
        }
    }

    #[test]
    fn root_counts_are_batched_unknown_retryable_and_revision_bound() {
        let mut library = LibraryBrowser::default();
        let roots: Vec<_> = (0..crate::model::MAX_QUERY_ROWS + 1)
            .map(|index| std::path::PathBuf::from(format!("/root/{index}")))
            .collect();
        let (generation, batches) = library.begin_root_counts(&roots, 7);
        assert_eq!(batches, vec![0, crate::model::MAX_QUERY_ROWS]);
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
        library.fail_root_counts(generation, crate::model::MAX_QUERY_ROWS);
        assert!(library.root_counts_failed());
        assert_eq!(library.root_count(roots.last().unwrap()), None);
        assert_eq!(
            library.begin_root_counts(&roots, 7).1,
            vec![crate::model::MAX_QUERY_ROWS]
        );
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
        assert!(!library.counts_current(scanned, &roots, 8));
        assert_eq!(library.root_count(&roots[0]), None);
        let empty = library.begin_root_counts(&[], 8);
        assert!(empty.1.is_empty());
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
                summaries: true,
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
                    title: format!("entry {index}"),
                    artist: String::new(),
                    album: String::new(),
                    missing: false,
                    favorite: false,
                })
                .collect(),
        });
        playlists.demand_page(true, 8_000..8_010, 20);
        playlists.prepare_entries(1);
        assert_eq!(playlists.selected_entry(), Some(41));
        assert!(!playlists.entry_ready());
        // The one identity result restores actions without restoring offscreen payload.
        playlists.accept_entry_lookup(
            41,
            Some(PlaylistEntryLocation {
                id: 41,
                playlist_id: 1,
                position: 7_000,
                track_id: 99,
                total: 10_000,
            }),
        );
        assert!(playlists.entry_ready());
        assert_eq!(
            playlists.entry_details.as_ref().map(|entry| entry.track_id),
            Some(99)
        );
        assert_eq!(playlists.entries.rows.visible().start, 8_000);
        assert!(playlists.entries.rows.iter().next().is_none());
        assert_eq!(
            playlists.entry_details.as_ref().map(|entry| entry.position),
            Some(7_000)
        );
        playlists.prepare_entries(1);
        assert!(!playlists.entry_ready());
        playlists.accept_entry_lookup(
            41,
            Some(PlaylistEntryLocation {
                id: 41,
                playlist_id: 1,
                position: 6_000,
                track_id: 99,
                total: 10_000,
            }),
        );
        assert_eq!(
            playlists.entry_details.as_ref().map(|entry| entry.position),
            Some(6_000)
        );
        assert_eq!(playlists.take_entry(), Some(41));
        assert!(!playlists.entry_ready());
    }

    #[test]
    fn missing_entry_clears_selection_not_an_unloaded_page() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.select_entry(41, 3);
        assert!(!playlists.entry_ready());
        playlists.accept_entry_lookup(40, None);
        assert_eq!(playlists.selected_entry(), Some(41));
        playlists.accept_entry_lookup(41, None);
        assert_eq!(playlists.selected_entry(), None);
        assert_eq!(playlists.take_entry(), None);
        assert!(!playlists.entry_ready());
        assert_eq!(playlists.selected(), Some(1));
    }

    #[test]
    fn changed_membership_rejects_stale_entry_identity() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.select_entry(41, 0);
        playlists.accept_entry_lookup(
            41,
            Some(PlaylistEntryLocation {
                id: 41,
                playlist_id: 1,
                position: 0,
                track_id: 99,
                total: 1,
            }),
        );
        assert!(playlists.entry_ready());
        playlists.prepare_entries(1);
        assert!(!playlists.entry_ready());
        playlists.accept_entry_lookup(
            41,
            Some(PlaylistEntryLocation {
                id: 41,
                playlist_id: 2,
                position: 0,
                track_id: 99,
                total: 1,
            }),
        );
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
