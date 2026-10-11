//! Browser state is independent of the window entity and frame scheduler.
//!
//! Selection identities outlive page eviction. Projection changes cancel range
//! selection and deferred navigation before replacing payloads; only explicit
//! selection/cancellation changes identities. Rendering borrows these owners.

use super::{
    HistorySort, ListFocus, SelectionModel,
    library::{DirectoryTree, LibraryNode, LibrarySelection},
    queries::{LibraryViewBuffer, PlaylistEntriesBuffer, PlaylistSummaryBuffer, ViewRequests},
};
use crate::model::{
    LibraryRow, LibrarySort, LibraryStats, Page, PlaylistEntryRow, PlaylistSummary,
};
use gpui::ScrollStrategy;
use std::collections::HashSet;

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
    focus: Option<ListFocus>,
}

impl BrowserState {
    pub(super) fn focus(&self) -> Option<ListFocus> {
        self.focus
    }

    pub(super) fn focus_list(&mut self, focus: ListFocus) {
        self.focus = Some(focus);
    }

    pub(super) fn clear_focus(&mut self) {
        self.focus = None;
    }

    pub(super) fn remove_playlist_selection(&mut self) {
        self.requests
            .cancel(super::queries::ViewRequestKind::PlaylistEntries);
        self.playlists.remove_selected();
    }

    pub(super) fn clear_selection(&mut self) {
        self.library.clear_selection();
        self.queue.clear();
        self.playlists.clear_selection();
    }
}

pub(super) struct LibraryBrowser {
    pub(super) buffer: LibraryViewBuffer,
    pub(super) tree: DirectoryTree,
    pub(super) range_selection: LibrarySelection,
    navigation: Option<usize>,
    selected: HashSet<i64>,
    selected_nodes: HashSet<LibraryNode>,
    tree_active: bool,
    favorites_only: bool,
    missing_only: bool,
    force_scan: bool,
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
            navigation: None,
            selected: HashSet::new(),
            selected_nodes: HashSet::new(),
            tree_active: true,
            favorites_only: false,
            missing_only: false,
            force_scan: false,
        }
    }
}

impl LibraryBrowser {
    pub(super) fn selected(&self) -> &HashSet<i64> {
        &self.selected
    }
    pub(super) fn selected_nodes(&self) -> &HashSet<LibraryNode> {
        &self.selected_nodes
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
    pub(super) fn navigation(&self) -> Option<usize> {
        self.navigation
    }
    pub(super) fn defer_navigation(&mut self, index: usize) {
        self.navigation = Some(index);
    }
    pub(super) fn cancel_navigation(&mut self) {
        self.navigation = None;
        self.tree.cancel_navigation();
    }

    pub(super) fn invalidate_selection(&mut self) {
        self.range_selection.cancel(true);
    }

    fn invalidate_projection(&mut self) {
        self.invalidate_selection();
        self.navigation = None;
        self.tree.cancel_navigation();
    }

    pub(super) fn clear_selection(&mut self) {
        self.invalidate_projection();
        self.selected.clear();
        self.selected_nodes.clear();
        self.remember_selection();
        self.buffer.rows.pin(None);
    }

    pub(super) fn select_node(&mut self, node: &LibraryNode, multi: bool) {
        self.navigation = None;
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
        self.selected_nodes.clear();
    }

    pub(super) fn toggle_view(&mut self) {
        self.invalidate_projection();
        self.tree_active = !self.tree_active;
    }

    pub(super) fn toggle_favorites(&mut self) {
        self.favorites_only = !self.favorites_only;
        self.invalidate_projection();
        self.tree.invalidate();
        self.clear_flat_payload();
    }

    pub(super) fn toggle_missing(&mut self) {
        self.missing_only = !self.missing_only;
    }

    pub(super) fn toggle_force_scan(&mut self) {
        self.force_scan = !self.force_scan;
    }

    pub(super) fn refresh_filter(&mut self) {
        self.invalidate_projection();
        self.tree_active = false;
        self.clear_flat_payload();
        self.buffer.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    pub(super) fn clear_flat_payload(&mut self) {
        self.buffer.rows.clear();
        self.buffer.total = 0;
    }

    pub(super) fn prepare_query(&mut self, query: String) {
        self.invalidate_selection();
        self.buffer.query = (!query.is_empty()).then_some(query);
        self.buffer.favorite = self.favorites_only.then_some(true);
        self.buffer.missing = self.missing_only.then_some(true);
        self.buffer.rows.invalidate();
    }

    pub(super) fn accept_page(&mut self, page: Page<LibraryRow>) {
        self.buffer.total = page.total;
        self.buffer.rows.accept(page);
        if let Some(index) = self
            .buffer
            .rows
            .position(|row| self.selected.contains(&row.id))
        {
            self.buffer.rows.pin(Some(index));
        }
    }
}

#[derive(Default)]
pub(super) struct PlaylistBrowser {
    pub(super) summaries: PlaylistSummaryBuffer,
    pub(super) entries: PlaylistEntriesBuffer,
    selected: Option<i64>,
    selected_entry: Option<i64>,
    selected_name: String,
}

impl PlaylistBrowser {
    pub(super) fn selected(&self) -> Option<i64> {
        self.selected
    }
    pub(super) fn selected_entry(&self) -> Option<i64> {
        self.selected_entry
    }
    pub(super) fn selected_name(&self) -> &str {
        &self.selected_name
    }

    pub(super) fn select(&mut self, id: i64, name: String, index: usize) -> bool {
        let changed = self.selected != Some(id);
        if changed {
            self.selected_entry = None;
        }
        self.selected = Some(id);
        self.selected_name = name;
        self.summaries.rows.pin(Some(index));
        changed
    }

    pub(super) fn clear_selection(&mut self) {
        self.selected = None;
        self.selected_entry = None;
        self.selected_name.clear();
        self.summaries.rows.pin(None);
    }

    pub(super) fn remove_selected(&mut self) {
        self.clear_selection();
        self.entries = PlaylistEntriesBuffer::default();
    }

    pub(super) fn take_entry(&mut self) -> Option<i64> {
        self.entries.rows.pin(None);
        self.selected_entry.take()
    }

    pub(super) fn select_entry(&mut self, id: i64, index: usize) {
        self.selected_entry = Some(id);
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
        self.entries.playlist_id = Some(id);
    }

    pub(super) fn reconcile_selection(&mut self) {
        // Eviction is not deletion; identities survive unloaded pages.
        if self.selected.is_none() || self.entries.playlist_id != self.selected {
            self.selected_entry = None;
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
}

#[derive(Default)]
pub(super) struct QueueSelection {
    selection: SelectionModel<u64>,
}

impl std::ops::Deref for QueueSelection {
    type Target = SelectionModel<u64>;
    fn deref(&self) -> &Self::Target {
        &self.selection
    }
}

impl QueueSelection {
    pub(super) fn clear(&mut self) {
        self.selection.clear();
        self.selection.clear_anchor();
    }

    pub(super) fn select(&mut self, id: u64, multi: bool) {
        if !multi {
            self.selection.clear();
        }
        if multi && !self.selection.insert(id) {
            self.selection.remove(&id);
        } else if !multi {
            self.selection.insert(id);
        }
        self.selection.set_anchor(id);
    }

    pub(super) fn select_range(
        &mut self,
        ids: impl IntoIterator<Item = u64>,
        anchor: u64,
        additive: bool,
    ) {
        if !additive {
            self.selection.clear();
        }
        self.selection.extend(ids);
        self.selection.set_anchor(anchor);
    }

    pub(super) fn reconcile(&mut self, valid: impl FnMut(&u64) -> bool) {
        self.selection.retain(valid);
        self.selection.clear_anchor();
    }
}

pub(super) struct HistoryBrowser {
    rows: Vec<LibraryRow>,
    total: usize,
    offset: usize,
    sort: HistorySort,
    descending: bool,
    stats: Option<LibraryStats>,
}

impl Default for HistoryBrowser {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            total: 0,
            offset: 0,
            sort: HistorySort::Recent,
            descending: true,
            stats: None,
        }
    }
}

impl HistoryBrowser {
    pub(super) fn rows(&self) -> &[LibraryRow] {
        &self.rows
    }
    pub(super) fn offset(&self) -> usize {
        self.offset
    }
    pub(super) fn sort(&self) -> HistorySort {
        self.sort
    }
    pub(super) fn descending(&self) -> bool {
        self.descending
    }
    pub(super) fn begin(&mut self, offset: usize) {
        self.offset = offset;
        self.rows.clear();
    }
    pub(super) fn accept(&mut self, page: Page<LibraryRow>) {
        self.total = page.total;
        self.rows = page.rows;
    }
    pub(super) fn accept_stats(&mut self, stats: LibraryStats) {
        self.stats = Some(stats);
    }
    pub(super) fn set_sort(&mut self, sort: HistorySort) {
        if self.sort == sort {
            self.descending = !self.descending;
        } else {
            self.sort = sort;
            self.descending = matches!(sort, HistorySort::Recent | HistorySort::Plays);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_and_view_changes_cancel_deferred_navigation_without_losing_identity() {
        let mut library = LibraryBrowser::default();
        library.select_node(&LibraryNode::Track(41), false);
        library.navigation = Some(123);
        library.refresh_filter();
        assert_eq!(library.navigation, None);
        assert!(!library.tree_active());
        assert!(library.selected().contains(&41));
        library.navigation = Some(7);
        library.toggle_view();
        assert_eq!(library.navigation, None);
        assert!(library.tree_active());
        library.clear_selection();
        assert!(library.selected().is_empty());
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
        playlists.remove_selected();
        assert_eq!(playlists.selected(), None);
        assert_eq!(playlists.selected_entry(), None);
        assert_eq!(playlists.entries.playlist_id, None);
    }

    #[test]
    fn queue_reconciliation_retains_selection_and_drops_obsolete_anchor() {
        let mut queue = QueueSelection::default();
        queue.select_range([1, 2, 3], 1, false);
        queue.reconcile(|id| *id != 1);
        assert!(!queue.contains(&1));
        assert!(queue.contains(&2));
        assert_eq!(queue.anchor(), None);
        queue.select(2, true);
        assert!(!queue.contains(&2));
        queue.clear();
        assert_eq!(queue.anchor(), None);
    }
}
