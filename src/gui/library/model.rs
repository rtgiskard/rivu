//! Library projection and selection identities, independent of the window shell.
use super::{DirectoryTree, LibraryNode, TreeBranch, selection::LibrarySelection};
use crate::gui::coordination::manual_actions::ManualIntentId;
use crate::model::{DirectoryRow, LibrarySort, Page, Query};
use crate::{projection::QueryProjection, response::QueryRevisions};
use gpui::ScrollStrategy;
use std::{collections::HashSet, ops::Range};

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
    pub(super) projection: QueryProjection,
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
    pub(super) buffer: LibraryViewBuffer,
    pub(super) tree: DirectoryTree,
    pub(super) range_selection: LibrarySelection,
    pub(super) navigation_intent: Option<ManualIntentId>,
    pub(super) selected: HashSet<i64>,
    pub(super) selected_nodes: HashSet<LibraryNode>,
    tree_active: bool,
    favorites_only: bool,
    missing_only: bool,
    force_scan: bool,
    counts: RootCounts,
    pub(super) locate: super::LibraryLocateState,
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

    pub(super) fn manual_navigation_index(&self) -> Option<usize> {
        self.range_selection
            .target
            .or_else(|| self.tree.selected_index())
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

    #[cfg(test)]
    fn directory_request_pending(
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
                super::BranchStatus::Unloaded | super::BranchStatus::Error
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
        let Some(super::tree::TreeItem::Entry(visible)) = self.tree.item(index) else {
            self.range_selection.target = Some(index);
            self.tree.defer_selection(index);
            return None;
        };
        self.cancel_navigation();
        self.range_selection.target = Some(index);
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
        self.range_selection.target = None;
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
