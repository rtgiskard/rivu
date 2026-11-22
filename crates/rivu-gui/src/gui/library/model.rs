//! Library projection and selection identities, independent of the window shell.
use super::{DirectoryTree, LibraryNode, TreeBranch, selection::LibrarySelection};
use crate::gui::coordination::manual_actions::ManualIntentId;
use gpui::ScrollStrategy;
use rivu_presentation::refresh::RefreshDeadline;
use rivu_protocol::model::{DirectoryRow, LibrarySort, Page, Query, QueryStamp};
use rivu_protocol::query_tracking::{QueryEpoch, QueryTracker};
use rivu_protocol::response::{QueryError, QueryResponse, QueryRevisions, ViewResponse};
use std::{collections::HashSet, ops::Range, rc::Rc, time::Instant};

pub(in crate::gui) fn merge_selection(
    tracks: &mut HashSet<i64>,
    nodes: &mut HashSet<LibraryNode>,
    range: impl IntoIterator<Item = LibraryNode>,
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
            LibraryNode::Directory(_) | LibraryNode::OtherTracks => {
                nodes.insert(node);
            }
        }
    }
}
#[derive(Default)]
struct RootCounts {
    owner: QueryTracker,
    roots: Vec<std::path::PathBuf>,
    totals: std::collections::HashMap<std::path::PathBuf, usize>,
    aggregate: Option<usize>,
    retained: std::collections::HashMap<std::path::PathBuf, usize>,
    retention: RefreshDeadline,
    pending: Option<usize>,
    failed: HashSet<usize>,
    settled: bool,
    retired: bool,
}

impl RootCounts {
    fn invalidate(&mut self) {
        self.owner.cancel();
        self.totals.clear();
        self.aggregate = None;
        self.retained.clear();
        self.retention.clear();
        self.pending = None;
        self.failed.clear();
        self.settled = false;
        self.retired = false;
    }
}

#[derive(Default)]
struct SearchCounts {
    epoch: QueryEpoch,
    // Logical cancellation retires the epoch, not the physical worker slot.
    worker: Option<u64>,
    blocked: Option<CountBlock>,
    scope: Option<Rc<SearchCountScope>>,
    stamp: QueryStamp,
    result: Option<(QueryStamp, SearchCountResult)>,
    retention: RefreshDeadline,
}

#[derive(Default)]
struct OtherCount {
    owner: QueryTracker,
    worker: Option<u64>,
    total: Option<usize>,
    blocked: Option<CountBlock>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CountBlock {
    Failed,
    Retired,
}

pub(super) struct OtherCountRequest {
    pub(super) ticket: rivu_protocol::query_tracking::QueryTicket,
    generation: u64,
}

pub(super) struct SearchCountScope {
    pub(super) roots: Vec<std::path::PathBuf>,
    pub(super) query: Option<String>,
    pub(super) favorite: Option<bool>,
    pub(super) missing: Option<bool>,
}

impl SearchCountScope {
    fn matches(&self, roots: &[std::path::PathBuf], buffer: &LibraryViewBuffer) -> bool {
        self.roots == roots
            && self.query == buffer.query
            && self.favorite == buffer.favorite
            && self.missing == buffer.missing
    }
}

// Both count-only page variants select the same dependencies, regardless of filters.
pub(super) const SEARCH_COUNT_QUERY: Query = Query::OtherTracksPage {
    query: None,
    favorite: None,
    missing: None,
    sort: LibrarySort::Id,
    offset: 0,
    limit: 0,
};

#[derive(Clone)]
pub(super) struct SearchCountRequest {
    pub(super) scope: Rc<SearchCountScope>,
    pub(super) stamp: QueryStamp,
    pub(super) other_empty: bool,
    pub(super) ticket: rivu_protocol::query_tracking::QueryTicket,
    generation: u64,
}

pub(super) struct SearchCountResult {
    pub(super) matched: usize,
    pub(super) roots: std::collections::HashMap<std::path::PathBuf, usize>,
    pub(super) other_matched: usize,
}

#[derive(Default)]
pub(super) struct LibraryViewBuffer {
    pub(super) query: Option<String>,
    pub(super) favorite: Option<bool>,
    pub(super) missing: Option<bool>,
    pub(super) sort: LibrarySort,
}

pub(super) struct LibraryBrowser {
    pub(super) projection: QueryTracker,
    search_counts: SearchCounts,
    other_count: OtherCount,
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
            other_count: OtherCount::default(),
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
    fn reset_search_counts(&mut self) {
        let counts = &mut self.search_counts;
        counts.epoch.cancel();
        counts.result = None;
        counts.blocked = None;
        counts.retention.clear();
    }

    pub(super) fn filtered(&self) -> bool {
        self.buffer.query.is_some()
            || self.buffer.favorite.is_some()
            || self.buffer.missing.is_some()
    }

    /// Reconcile authority even while hidden, independently of the worker slot.
    pub(super) fn sync_search_counts(
        &mut self,
        roots: &[std::path::PathBuf],
        revisions: QueryRevisions,
        now: Instant,
    ) -> bool {
        self.sync_root_counts_at(roots, revisions.structure, now);
        let counts = &mut self.search_counts;
        let roots_changed = counts
            .scope
            .as_ref()
            .map_or(!roots.is_empty(), |scope| scope.roots != roots);
        let same_scope = counts
            .scope
            .as_ref()
            .is_some_and(|scope| scope.matches(roots, &self.buffer));
        let stamp = SEARCH_COUNT_QUERY.stamp(revisions);
        let changed = !same_scope || counts.stamp != stamp;
        let other = &mut self.other_count;
        let other_changed = other.owner.sync(&Query::OtherTracksCount, revisions) || roots_changed;
        if other_changed {
            if roots_changed {
                other.owner.cancel();
            }
            other.total = None;
            other.blocked = None;
        }
        if changed {
            counts.epoch.cancel();
            counts.stamp = stamp;
            if same_scope {
                if counts.result.is_some() {
                    counts.retention.begin(now);
                }
            } else {
                counts.scope = Some(Rc::new(SearchCountScope {
                    roots: roots.to_vec(),
                    query: self.buffer.query.clone(),
                    favorite: self.buffer.favorite,
                    missing: self.buffer.missing,
                }));
                counts.result = None;
                counts.retention.clear();
            }
            counts.blocked = None;
        }
        if changed || other_changed {
            self.reconcile_forest(roots);
        }
        changed
    }

    /// Reconcile complete demand before reserving the sole physical worker.
    /// The boolean reports changed authority, not a reason to erase retained text.
    pub(super) fn prepare_search_counts(
        &mut self,
        roots: &[std::path::PathBuf],
        revisions: QueryRevisions,
    ) -> (bool, Option<SearchCountRequest>) {
        let changed = self.sync_search_counts(roots, revisions, Instant::now());
        if !changed {
            self.reconcile_forest(roots);
        }
        let settled = !self.filtered() || self.total_count() == Some(0);
        let other_empty = self.other_tracks_empty();
        let counts = &mut self.search_counts;
        let request = if settled
            || counts
                .result
                .as_ref()
                .is_some_and(|(stamp, _)| *stamp == counts.stamp)
            || counts.blocked.is_some()
            || counts.worker.is_some()
        {
            None
        } else {
            let generation = counts.epoch.generation();
            counts.worker = Some(generation);
            Some(SearchCountRequest {
                scope: Rc::clone(counts.scope.as_ref().expect("count scope reconciled")),
                stamp: counts.stamp,
                other_empty,
                ticket: counts.epoch.ticket(),
                generation,
            })
        };
        (changed, request)
    }
    pub(super) fn retire_search_counts(&mut self, request: &SearchCountRequest) {
        if self.search_counts.worker == Some(request.generation) {
            self.search_counts.worker = None;
        }
    }

    /// Completion releases its physical slot even when its payload is obsolete.
    /// Failure or lost authority parks the demand until explicit retry or a new stamp.
    pub(super) fn accept_search_counts(
        &mut self,
        request: SearchCountRequest,
        roots: &[std::path::PathBuf],
        revisions: QueryRevisions,
        result: Result<SearchCountResult, CountBlock>,
    ) -> bool {
        let published = self.publish_search_counts(&request, roots, revisions, result);
        self.retire_search_counts(&request);
        published
    }

    pub(super) fn publish_search_counts(
        &mut self,
        request: &SearchCountRequest,
        roots: &[std::path::PathBuf],
        revisions: QueryRevisions,
        result: Result<SearchCountResult, CountBlock>,
    ) -> bool {
        let counts = &mut self.search_counts;
        if counts.worker != Some(request.generation)
            || request.generation != counts.epoch.generation()
            || !request.scope.matches(roots, &self.buffer)
            || request.stamp != SEARCH_COUNT_QUERY.stamp(revisions)
        {
            return false;
        }
        match result {
            Ok(result) => {
                counts.blocked = None;
                counts.retention.clear();
                counts.result = Some((request.stamp, result));
            }
            Err(CountBlock::Retired) => {
                counts.epoch.cancel();
                counts.blocked = Some(CountBlock::Retired);
            }
            Err(CountBlock::Failed) => {
                counts.blocked = Some(CountBlock::Failed);
                counts.retention.clear();
                counts.result = None;
            }
        }
        true
    }

    pub(super) fn search_counts_pending(&self) -> bool {
        self.search_counts.worker.is_some()
    }

    pub(super) fn count_reads_pending(&self) -> bool {
        self.search_counts_pending() || self.other_count.worker.is_some()
    }

    #[cfg(test)]
    pub(super) fn search_counts(&self) -> Option<(usize, usize)> {
        let (matched, total) = self.search_count_parts();
        Some((matched?, total?))
    }

    fn total_count(&self) -> Option<usize> {
        let root_total = if self.counts.roots.is_empty() {
            Some(0)
        } else {
            self.counts.aggregate
        };
        root_total
            .zip(self.other_count.total)
            .map(|(roots, other)| roots + other)
    }

    pub(super) fn search_count_parts(&self) -> (Option<usize>, Option<usize>) {
        self.search_count_parts_at(Instant::now())
    }

    fn search_count_parts_at(&self, now: Instant) -> (Option<usize>, Option<usize>) {
        let total = self.total_count();
        let matched = if !self.filtered() || total == Some(0) {
            total
        } else {
            self.visible_matches_at(total, now)
                .map(|result| result.matched)
        };
        (matched, total)
    }

    fn visible_matches_at(&self, total: Option<usize>, now: Instant) -> Option<&SearchCountResult> {
        let counts = &self.search_counts;
        let (stamp, result) = counts.result.as_ref()?;
        let current = *stamp == counts.stamp;
        // A retained numerator can stand on its own, but not next to a
        // denominator from a different structure revision.
        (current
            || (counts.retention.visible(now)
                && (total.is_none()
                    || stamp.revisions().structure == counts.stamp.revisions().structure)))
            .then_some(result)
    }

    pub(super) fn prepare_other_count(&mut self) -> Option<OtherCountRequest> {
        let other = &mut self.other_count;
        if other.total.is_some() || other.blocked.is_some() || other.worker.is_some() {
            return None;
        }
        let generation = other.owner.generation();
        other.worker = Some(generation);
        Some(OtherCountRequest {
            generation,
            ticket: other.owner.ticket(),
        })
    }

    pub(super) fn retire_other_count(&mut self, request: &OtherCountRequest) {
        if self.other_count.worker == Some(request.generation) {
            self.other_count.worker = None;
        }
    }

    pub(super) fn publish_other_count(
        &mut self,
        request: &OtherCountRequest,
        roots: &[std::path::PathBuf],
        revisions: QueryRevisions,
        response: &QueryResponse,
    ) -> bool {
        let other = &mut self.other_count;
        let query = Query::OtherTracksCount;
        if other.worker != Some(request.generation)
            || self
                .search_counts
                .scope
                .as_ref()
                .is_none_or(|scope| scope.roots != roots)
            || !other.owner.accepts(&query, request.generation, revisions)
        {
            return false;
        }
        match &response.result {
            Ok(ViewResponse::OtherTracksCount(total))
                if query.stamp(response.revisions) == query.stamp(revisions) =>
            {
                other.total = Some(*total);
                self.reconcile_forest(roots);
            }
            Ok(ViewResponse::OtherTracksCount(_))
            | Err(QueryError::RevisionChanged | QueryError::Cancelled) => {
                // Park this logical demand without releasing the physical worker.
                other.owner.cancel();
                other.blocked = Some(CountBlock::Retired);
                return false;
            }
            _ => other.blocked = Some(CountBlock::Failed),
        }
        true
    }

    pub(super) fn other_tracks_empty(&self) -> bool {
        self.other_count.total == Some(0)
    }

    pub(super) fn forest_page(
        &self,
        roots: &[std::path::PathBuf],
        offset: usize,
        limit: usize,
    ) -> Page<DirectoryRow> {
        super::tree::forest_page(roots, !self.other_tracks_empty(), offset, limit)
    }

    fn reconcile_forest(&mut self, roots: &[std::path::PathBuf]) {
        let show_other = !self.other_tracks_empty();
        if self.tree.sync_forest(roots, show_other) {
            self.invalidate_selection();
            if !show_other {
                self.selected_nodes.remove(&LibraryNode::OtherTracks);
            }
            self.remember_selection();
        }
    }

    #[cfg(test)]
    pub(super) fn other_count_failed(&self) -> bool {
        self.other_count.blocked == Some(CountBlock::Failed)
            || (self.filtered() && self.search_counts.blocked == Some(CountBlock::Failed))
    }

    #[cfg(test)]
    pub(super) fn other_count_retired(&self) -> bool {
        self.other_count.blocked == Some(CountBlock::Retired)
    }

    pub(super) fn other_count_label(&self) -> String {
        self.other_count_label_at(Instant::now())
    }

    fn other_count_label_at(&self, now: Instant) -> String {
        let total = self.other_count.total;
        if self.filtered() {
            let matched = if total == Some(0) {
                total
            } else {
                self.visible_matches_at(total, now)
                    .map(|result| result.other_matched)
            };
            super::super::components::search::filtered_count_label(matched, total)
        } else {
            total.map_or_else(|| "—".to_owned(), |total| total.to_string())
        }
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
        self.sync_projection_at(revisions, Instant::now())
    }

    pub(super) fn sync_projection_at(&mut self, revisions: QueryRevisions, now: Instant) -> bool {
        let query = self.projection_query();
        let impact = self.projection.impact(&query, revisions);
        if !self.projection.sync(&query, revisions) {
            return false;
        }
        if !impact.coordinates_changed {
            self.tree.invalidate_payloads_at(now);
        } else {
            self.invalidate_projection();
            if !impact.identity_changed {
                self.tree.refresh(now);
            } else {
                self.tree.invalidate();
            }
        }
        true
    }

    pub(super) fn refresh_deadline(&self) -> Option<Instant> {
        [
            self.tree.refresh_deadline(),
            self.counts.retention.deadline(),
            self.search_counts.retention.deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub(super) fn expire_refresh(&mut self, now: Instant) -> bool {
        let mut changed = self.tree.expire_refresh(now);
        if self.counts.retention.expire(now) {
            changed |= !self.counts.retained.is_empty();
            self.counts.retained.clear();
        }
        if self.search_counts.retention.expire(now) {
            changed |= self.search_counts.result.take().is_some();
        }
        changed
    }

    pub(super) fn sync_root_counts(
        &mut self,
        roots: &[std::path::PathBuf],
        structure_revision: u64,
    ) {
        self.sync_root_counts_at(roots, structure_revision, Instant::now());
    }

    fn sync_root_counts_at(
        &mut self,
        roots: &[std::path::PathBuf],
        structure_revision: u64,
        now: Instant,
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
            self.counts.aggregate = None;
            if self.counts.roots == roots {
                self.counts.retention.expire(now);
                if !self.counts.totals.is_empty() {
                    self.counts.retention.begin(now);
                    if self.counts.retention.visible(now) {
                        self.counts.retained = std::mem::take(&mut self.counts.totals);
                    } else {
                        self.counts.totals.clear();
                    }
                }
                if self.counts.retention.expired(now) {
                    self.counts.retained.clear();
                }
            } else {
                self.counts.totals.clear();
                self.counts.retained.clear();
                self.counts.retention.clear();
            }
            self.counts.pending = None;
            self.counts.failed.clear();
            self.counts.settled = false;
            self.counts.retired = false;
            self.counts.roots = roots.to_vec();
        }
    }

    pub(super) fn root_count_ticket(&mut self) -> rivu_protocol::query_tracking::QueryTicket {
        self.counts.owner.ticket()
    }

    pub(super) fn cancel_pending_reads(&mut self) {
        self.projection.cancel();
        self.tree.cancel_pending();
        if self.search_counts_pending() {
            self.search_counts.epoch.cancel();
        }
        if self.other_count.worker.is_some() {
            self.other_count.owner.cancel();
        }
        if self.counts.pending.is_some() {
            self.counts.owner.cancel();
            self.counts.pending = None;
            self.counts.settled = false;
        }
    }

    pub(super) fn retry_root_counts(&mut self) -> bool {
        let search_blocked = self.search_counts.blocked.take().is_some();
        let other_blocked = self.other_count.blocked.take().is_some();
        let roots_blocked = self.counts.retired || !self.counts.failed.is_empty();
        if roots_blocked {
            self.counts.failed.clear();
            self.counts.settled = false;
            self.counts.retired = false;
        }
        search_blocked || other_blocked || roots_blocked
    }

    pub(super) fn root_counts_settled(&self) -> bool {
        self.counts.settled
    }
    pub(super) fn begin_root_counts(
        &mut self,
        roots: &[std::path::PathBuf],
        structure_revision: u64,
    ) -> (u64, Option<usize>) {
        self.sync_root_counts(roots, structure_revision);
        let generation = self.counts.owner.generation();
        if self.counts.retired || self.counts.pending.is_some() {
            return (generation, None);
        }
        let offset = (0..roots.len())
            .step_by(rivu_protocol::model::MAX_QUERY_ROWS)
            .find(|offset| {
                !self.counts.failed.contains(offset)
                    && roots
                        [*offset..(*offset + rivu_protocol::model::MAX_QUERY_ROWS).min(roots.len())]
                        .iter()
                        .any(|root| !self.counts.totals.contains_key(root))
            });
        self.counts.pending = offset;
        self.counts.settled = offset.is_none();
        (generation, offset)
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
        counts: Vec<rivu_protocol::model::LibraryRootCount>,
    ) {
        if self.counts.owner.generation() == generation && self.counts.pending == Some(offset) {
            self.counts.pending = None;
            let totals = &mut self.counts.totals;
            totals.extend(counts.into_iter().map(|count| (count.path, count.total)));
            if self.counts.aggregate.is_none() {
                // Validated roots are disjoint. Publish a total only when every root is known.
                self.counts.aggregate = self
                    .counts
                    .roots
                    .iter()
                    .try_fold(0, |total, root| totals.get(root).map(|count| total + count));
            }
            if self.counts.aggregate.is_some() {
                self.counts.retained.clear();
                self.counts.retention.clear();
            }
        }
    }

    pub(super) fn fail_root_counts(&mut self, generation: u64, offset: usize) {
        if self.counts.owner.generation() == generation && self.counts.pending == Some(offset) {
            self.counts.pending = None;
            self.counts.failed.insert(offset);
        }
    }

    pub(super) fn retire_root_counts(&mut self, generation: u64) {
        if self.counts.owner.generation() == generation {
            self.counts.owner.cancel();
            self.counts.pending = None;
            self.counts.retired = true;
        }
    }

    #[cfg(test)]
    pub(super) fn root_counts_retired(&self) -> bool {
        self.counts.retired
            || (self.filtered() && self.search_counts.blocked == Some(CountBlock::Retired))
    }

    pub(super) fn root_count(&self, path: &std::path::Path) -> Option<usize> {
        self.counts.totals.get(path).copied()
    }

    fn root_count_display_at(&self, path: &std::path::Path, now: Instant) -> Option<usize> {
        self.root_count(path).or_else(|| {
            self.counts
                .retention
                .visible(now)
                .then(|| self.counts.retained.get(path).copied())
                .flatten()
        })
    }

    pub(super) fn root_count_label(&self, path: &std::path::Path) -> String {
        self.root_count_label_at(path, Instant::now())
    }

    fn root_count_label_at(&self, path: &std::path::Path, now: Instant) -> String {
        if self.filtered() {
            let total = self.root_count(path);
            super::super::components::search::filtered_count_label(
                if total == Some(0) {
                    total
                } else {
                    self.visible_matches_at(total, now)
                        .and_then(|result| result.roots.get(path).copied())
                },
                total,
            )
        } else {
            self.root_count_display_at(path, now)
                .map_or_else(|| "—".to_owned(), |total| total.to_string())
        }
    }

    #[cfg(test)]
    pub(super) fn root_count_failed(&self, path: &std::path::Path) -> bool {
        (self.filtered() && self.search_counts.blocked == Some(CountBlock::Failed))
            || self.counts.failed.iter().any(|&offset| {
                self.counts.roots[offset
                    ..(offset + rivu_protocol::model::MAX_QUERY_ROWS).min(self.counts.roots.len())]
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
            self.projection.cancel();
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
            self.projection.cancel();
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

    pub(super) fn retire_directory(&mut self, branch: &TreeBranch, generation: u64) {
        self.tree.retire_request(branch, generation);
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
        root_index: Option<usize>,
        roots: &[std::path::PathBuf],
        locations: &[rivu_protocol::model::LibraryLocation],
    ) -> Option<(TreeBranch, usize)> {
        self.invalidate_selection();
        let other_tracks = !self.other_tracks_empty();
        self.tree
            .reveal_playing(root_index, roots, other_tracks, locations)
    }

    pub(super) fn clear_locate_filters(&mut self) {
        if self.buffer.query.is_some()
            || self.buffer.favorite.is_some()
            || self.buffer.missing.is_some()
        {
            self.projection.cancel();
            self.reset_search_counts();
        }
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
            DirectoryRow::OtherTracks => LibraryNode::OtherTracks,
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
            LibraryNode::Directory(_) | LibraryNode::OtherTracks => {
                self.selected_nodes.contains(source)
            }
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

    pub(super) fn drag_track_ids(&self, nodes: &[LibraryNode]) -> Vec<i64> {
        let mut ids = nodes
            .iter()
            .filter_map(|node| match node {
                LibraryNode::Track(id) => Some(*id),
                LibraryNode::Directory(_) | LibraryNode::OtherTracks => None,
            })
            .collect::<Vec<_>>();
        if ids.len() > 1 {
            let indices = self.tree.first_indices();
            ids.sort_unstable_by_key(|id| {
                (
                    indices
                        .get(&LibraryNode::Track(*id))
                        .copied()
                        .unwrap_or(usize::MAX),
                    *id,
                )
            });
        }
        ids.dedup();
        ids
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
            LibraryNode::Directory(_) | LibraryNode::OtherTracks => {
                if multi && !self.selected_nodes.insert(node.clone()) {
                    self.selected_nodes.remove(node);
                } else if !multi {
                    self.selected_nodes.insert(node.clone());
                }
            }
        }
        self.remember_selection();
    }

    pub(super) fn remember_selection(&mut self) {
        self.tree
            .remember_selection(&self.selected_nodes, &self.selected);
        if self.selected.capacity() > 64.max(self.selected.len().saturating_mul(4)) {
            self.selected
                .shrink_to(self.selected.len().saturating_mul(2));
        }
        if self.selected_nodes.capacity() > 64.max(self.selected_nodes.len().saturating_mul(4)) {
            self.selected_nodes
                .shrink_to(self.selected_nodes.len().saturating_mul(2));
        }
    }

    pub(super) fn roots_changed(&mut self) {
        self.invalidate_projection();
        self.projection.cancel();
        self.reset_search_counts();
        self.tree.clear();
        self.counts.invalidate();
        self.other_count.owner.cancel();
        self.other_count.total = None;
        self.other_count.blocked = None;
        self.selected_nodes = HashSet::new();
    }

    pub(super) fn toggle_view(&mut self) {
        self.projection.cancel();
        self.invalidate_projection();
        self.tree_active = !self.tree_active;
        self.tree.set_grouped(!self.tree_active);
    }

    pub(super) fn toggle_favorites(&mut self) {
        self.projection.cancel();
        self.reset_search_counts();
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
        let changed = self.buffer.missing != self.missing_only.then_some(true);
        self.projection.cancel();
        self.invalidate_projection();
        self.buffer.missing = self.missing_only.then_some(true);
        if changed {
            self.reset_search_counts();
            self.tree.invalidate();
        } else {
            self.tree.refresh(Instant::now());
        }
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
        if self.buffer.query != query
            || self.buffer.favorite != self.favorites_only.then_some(true)
            || self.buffer.missing != self.missing_only.then_some(true)
        {
            self.projection.cancel();
            self.reset_search_counts();
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
    #[test]
    fn retained_counts_expire_without_authorizing_new_count_reads() {
        use std::time::Duration;
        let start = Instant::now();
        let roots = vec![std::path::PathBuf::from("/music")];
        let mut library = LibraryBrowser::default();
        let mut revisions = QueryRevisions::default();
        library.refresh_search("needle".into());
        load_root_totals(&mut library, &roots, revisions);
        let request = library.prepare_search_counts(&roots, revisions).1.unwrap();
        let old = request.clone();
        assert!(library.accept_search_counts(
            request,
            &roots,
            revisions,
            Ok(search_count_result(&roots[0], 2))
        ));
        revisions.structure += 1;
        library.sync_search_counts(&roots, revisions, start);
        assert_eq!(library.root_count(&roots[0]), None);
        assert_eq!(
            library.root_count_display_at(&roots[0], start + Duration::from_millis(199)),
            Some(3)
        );
        assert!(library.prepare_search_counts(&roots, revisions).1.is_some());
        let (generation, offsets) = library.begin_root_counts(&roots, revisions.structure);
        assert_eq!(offsets, Some(0));
        let ticket = library.root_count_ticket();
        revisions.counts += 1;
        library.sync_search_counts(&roots, revisions, start + Duration::from_millis(150));
        assert!(!library.publish_search_counts(
            &old,
            &roots,
            revisions,
            Ok(search_count_result(&roots[0], 99))
        ));
        assert!(!library.expire_refresh(start + Duration::from_millis(199)));
        assert!(library.expire_refresh(start + Duration::from_millis(200)));
        assert_eq!(
            library.root_count_display_at(&roots[0], start + Duration::from_millis(200)),
            None
        );
        assert_eq!(library.search_count_parts(), (None, None));
        assert!(library.counts_current(generation, &roots, revisions.structure));
        assert!(ticket.is_current());
        assert_eq!(library.counts.pending, Some(0));
    }

    #[test]
    fn partial_root_counts_after_expiry_cannot_reopen_the_stale_window() {
        let roots = (0..=rivu_protocol::model::MAX_QUERY_ROWS)
            .map(|index| std::path::PathBuf::from(format!("/music/{index}")))
            .collect::<Vec<_>>();
        let mut library = LibraryBrowser::default();
        let start = Instant::now();
        let deadline = start + rivu_presentation::refresh::REFRESH_RETENTION;
        for revision in [0, 2] {
            loop {
                let (generation, offsets) = library.begin_root_counts(&roots, revision);
                let Some(offset) = offsets else { break };
                library.accept_root_counts(
                    generation,
                    offset,
                    roots[offset..(offset + rivu_protocol::model::MAX_QUERY_ROWS).min(roots.len())]
                        .iter()
                        .map(|path| rivu_protocol::model::LibraryRootCount {
                            path: path.clone(),
                            total: 3,
                        })
                        .collect(),
                );
            }
            if revision == 0 {
                library.sync_root_counts_at(&roots, 1, start);
                assert!(library.expire_refresh(deadline));
                let (generation, offsets) = library.begin_root_counts(&roots, 1);
                assert_eq!(offsets, Some(0));
                library.accept_root_counts(
                    generation,
                    0,
                    roots[..rivu_protocol::model::MAX_QUERY_ROWS]
                        .iter()
                        .map(|path| rivu_protocol::model::LibraryRootCount {
                            path: path.clone(),
                            total: 4,
                        })
                        .collect(),
                );
                assert_eq!(library.root_count_display_at(&roots[0], deadline), Some(4));
                library.sync_root_counts_at(&roots, 2, deadline);
                assert_eq!(library.counts.retention.deadline(), None);
                assert!(library.counts.retained.is_empty());
                assert_eq!(library.root_count_display_at(&roots[0], deadline), None);
            }
        }
        library.sync_root_counts_at(&roots, 3, deadline);
        assert_eq!(
            library.counts.retention.deadline(),
            Some(deadline + rivu_presentation::refresh::REFRESH_RETENTION)
        );
        assert_eq!(library.root_count_display_at(&roots[0], deadline), Some(3));
    }

    #[test]
    fn equal_refreshed_counts_clear_retention_and_settle() {
        let roots = vec![std::path::PathBuf::from("/music")];
        let mut library = LibraryBrowser::default();
        let mut revisions = QueryRevisions::default();
        library.refresh_search("needle".into());
        load_root_totals(&mut library, &roots, revisions);
        let request = library.prepare_search_counts(&roots, revisions).1.unwrap();
        assert!(library.accept_search_counts(
            request,
            &roots,
            revisions,
            Ok(search_count_result(&roots[0], 2))
        ));
        revisions.counts += 1;
        let now = Instant::now();
        library.sync_search_counts(&roots, revisions, now);
        let request = library.prepare_search_counts(&roots, revisions).1.unwrap();
        assert_eq!(library.search_count_parts_at(now), (Some(2), Some(3)));
        assert!(library.accept_search_counts(
            request,
            &roots,
            revisions,
            Ok(search_count_result(&roots[0], 2))
        ));
        assert_eq!(library.root_count_label(&roots[0]), "2 / 3");
        assert!(library.refresh_deadline().is_none());
        assert!(library.prepare_search_counts(&roots, revisions).1.is_none());
    }

    fn load_root_totals(
        library: &mut LibraryBrowser,
        roots: &[std::path::PathBuf],
        revisions: QueryRevisions,
    ) {
        let (generation, offsets) = library.begin_root_counts(roots, revisions.structure);
        if let Some(offset) = offsets {
            library.accept_root_counts(
                generation,
                offset,
                roots
                    .iter()
                    .map(|path| rivu_protocol::model::LibraryRootCount {
                        path: path.clone(),
                        total: 3,
                    })
                    .collect(),
            );
        }
        load_other_total(library, roots, revisions, Some(0));
    }

    #[test]
    fn filtered_counts_survive_failed_root_totals_until_explicit_retry() {
        for filter in 0..3 {
            let mut library = LibraryBrowser::default();
            let roots = vec![std::path::PathBuf::from("/music")];
            let revisions = QueryRevisions::default();
            match filter {
                0 => {
                    library.refresh_search("needle".into());
                }
                1 => library.buffer.favorite = Some(true),
                _ => library.buffer.missing = Some(true),
            }
            let request = library.prepare_search_counts(&roots, revisions).1.unwrap();
            assert_eq!(library.root_count_label(&roots[0]), "- / -");
            load_other_total(&mut library, &roots, revisions, Some(0));
            assert!(library.accept_search_counts(
                request,
                &roots,
                revisions,
                Ok(search_count_result(&roots[0], 1))
            ));
            assert_eq!(library.root_count_label(&roots[0]), "1 / -");
            let (generation, offsets) = library.begin_root_counts(&roots, revisions.structure);
            library.fail_root_counts(generation, offsets.unwrap());
            assert_eq!(library.search_count_parts(), (Some(1), None));
            assert_eq!(library.root_count_label(&roots[0]), "1 / -");
            assert!(library.root_count_failed(&roots[0]));
            assert!(!library.root_count_failed(std::path::Path::new("/unrelated")));
            assert!(
                library
                    .begin_root_counts(&roots, revisions.structure)
                    .1
                    .is_none()
            );
            library.retry_root_counts();
            load_root_totals(&mut library, &roots, revisions);
            assert!(library.prepare_search_counts(&roots, revisions).1.is_none());
            assert_eq!(library.root_count_label(&roots[0]), "1 / 3");
            assert!(!library.root_count_failed(&roots[0]));
            assert_eq!(library.search_counts(), Some((1, 3)));
        }
    }

    #[test]
    fn late_totals_and_superseded_filter_results_cannot_publish() {
        let mut library = LibraryBrowser::default();
        let roots = vec![std::path::PathBuf::from("/music")];
        let mut revisions = QueryRevisions::default();
        assert!(library.prepare_search_counts(&roots, revisions).1.is_none());
        assert!(!library.search_counts_pending());
        let (old_generation, offsets) = library.begin_root_counts(&roots, revisions.structure);
        let old_other = library.prepare_other_count().unwrap();
        library.refresh_search("needle".into());
        let old = library.prepare_search_counts(&roots, revisions).1.unwrap();
        library.refresh_search("changed".into());
        revisions.structure += 1;
        assert!(library.prepare_search_counts(&roots, revisions).1.is_none());
        assert!(library.prepare_other_count().is_none());
        assert!(!library.publish_other_count(
            &old_other,
            &roots,
            revisions,
            &other_response(revisions, Some(99)),
        ));
        library.retire_other_count(&old_other);
        library.accept_root_counts(
            old_generation,
            offsets.unwrap(),
            vec![rivu_protocol::model::LibraryRootCount {
                path: roots[0].clone(),
                total: 99,
            }],
        );
        assert!(!library.accept_search_counts(
            old,
            &roots,
            revisions,
            Ok(search_count_result(&roots[0], 99))
        ));
        assert_eq!(library.search_count_parts(), (None, None));
        load_root_totals(&mut library, &roots, revisions);
        let request = library.prepare_search_counts(&roots, revisions).1.unwrap();
        assert_eq!(library.search_count_parts(), (None, Some(3)));
        assert_eq!(library.root_count_label(&roots[0]), "- / 3");
        assert_eq!(request.scope.query.as_deref(), Some("changed"));
    }
    fn load_other_total(
        library: &mut LibraryBrowser,
        roots: &[std::path::PathBuf],
        revisions: QueryRevisions,
        total: Option<usize>,
    ) {
        library.sync_search_counts(roots, revisions, Instant::now());
        if let Some(request) = library.prepare_other_count() {
            assert!(library.publish_other_count(
                &request,
                roots,
                revisions,
                &other_response(revisions, total),
            ));
            library.retire_other_count(&request);
        }
    }

    fn other_response(revisions: QueryRevisions, total: Option<usize>) -> QueryResponse {
        QueryResponse {
            revisions,
            result: total
                .map(ViewResponse::OtherTracksCount)
                .ok_or_else(|| QueryError::Message("Other Tracks total unavailable".into())),
        }
    }

    fn search_count_result(root: &std::path::Path, matched: usize) -> SearchCountResult {
        SearchCountResult {
            matched,
            roots: [(root.to_path_buf(), matched)].into_iter().collect(),
            other_matched: 0,
        }
    }

    #[test]
    fn count_totals_follow_structure_while_sort_and_filter_reuse_membership() {
        let mut library = LibraryBrowser::default();
        let roots = vec![std::path::PathBuf::from("/music")];
        let mut revisions = QueryRevisions::default();
        let (generation, offsets) = library.begin_root_counts(&roots, revisions.structure);
        library.accept_root_counts(
            generation,
            offsets.unwrap(),
            vec![rivu_protocol::model::LibraryRootCount {
                path: roots[0].clone(),
                total: 4,
            }],
        );
        assert!(library.prepare_search_counts(&roots, revisions).1.is_none());
        load_other_total(&mut library, &roots, revisions, Some(0));
        assert_eq!(library.search_counts(), Some((4, 4)));
        library.refresh_search("needle".into());
        let request = library.prepare_search_counts(&roots, revisions).1.unwrap();
        library.buffer.sort = LibrarySort::MostPlayed;
        revisions.library += 1;
        revisions.bitrate += 1;
        assert!(!library.prepare_search_counts(&roots, revisions).0);
        assert!(request.ticket.is_current());
        assert_eq!(library.search_count_parts(), (None, Some(4)));
        assert!(library.accept_search_counts(
            request,
            &roots,
            revisions,
            Ok(search_count_result(&roots[0], 1))
        ));
        library.buffer.sort = LibrarySort::Album;
        assert!(!library.prepare_search_counts(&roots, revisions).0);
        assert_eq!(library.search_counts(), Some((1, 4)));
        revisions.structure += 1;
        let now = Instant::now();
        library.sync_search_counts(&roots, revisions, now);
        assert!(library.prepare_search_counts(&roots, revisions).1.is_some());
        assert!(library.counts.totals.is_empty());
        assert_eq!(library.search_count_parts_at(now), (Some(1), None));
    }

    #[test]
    fn newer_structure_totals_do_not_pair_with_retained_matches() {
        let mut library = LibraryBrowser::default();
        let roots = vec![std::path::PathBuf::from("/music")];
        let mut revisions = QueryRevisions::default();
        library.refresh_search("needle".into());
        let load_root = |library: &mut LibraryBrowser, revisions: QueryRevisions, total| {
            let (generation, offset) = library.begin_root_counts(&roots, revisions.structure);
            library.accept_root_counts(
                generation,
                offset.unwrap(),
                vec![rivu_protocol::model::LibraryRootCount {
                    path: roots[0].clone(),
                    total,
                }],
            );
        };
        load_root(&mut library, revisions, 10);
        load_other_total(&mut library, &roots, revisions, Some(4));
        let read = library.prepare_search_counts(&roots, revisions).1.unwrap();
        assert!(library.accept_search_counts(
            read,
            &roots,
            revisions,
            Ok(SearchCountResult {
                matched: 11,
                roots: [(roots[0].clone(), 8)].into_iter().collect(),
                other_matched: 3,
            }),
        ));
        assert_eq!(library.search_count_parts(), (Some(11), Some(14)));

        revisions.structure += 1;
        let now = Instant::now();
        library.sync_search_counts(&roots, revisions, now);
        let read = library.prepare_search_counts(&roots, revisions).1.unwrap();
        assert_eq!(library.search_count_parts_at(now), (Some(11), None));
        load_root(&mut library, revisions, 2);
        assert_eq!(library.root_count_label_at(&roots[0], now), "- / 2");
        assert_eq!(library.other_count_label_at(now), "3 / -");
        load_other_total(&mut library, &roots, revisions, Some(1));
        assert_eq!(library.other_count_label_at(now), "- / 1");
        assert_eq!(library.search_count_parts_at(now), (None, Some(3)));

        assert!(library.accept_search_counts(
            read,
            &roots,
            revisions,
            Ok(search_count_result(&roots[0], 1)),
        ));
        assert_eq!(library.search_count_parts(), (Some(1), Some(3)));
        assert_eq!(library.root_count_label(&roots[0]), "1 / 2");
        assert_eq!(library.other_count_label(), "0 / 1");
    }

    #[test]
    fn search_count_timeout_keeps_worker_until_latest_demand_can_run() {
        let mut library = LibraryBrowser::default();
        let roots = vec![std::path::PathBuf::from("/music")];
        let revisions = QueryRevisions::default();
        library.refresh_search("needle".into());
        load_root_totals(&mut library, &roots, revisions);
        let old = library.prepare_search_counts(&roots, revisions).1.unwrap();
        assert!(library.publish_search_counts(&old, &roots, revisions, Err(CountBlock::Failed)));
        assert!(library.search_counts_pending());
        assert!(library.prepare_search_counts(&roots, revisions).1.is_none());
        for query in ["one", "latest"] {
            library.refresh_search(query.into());
            assert!(library.prepare_search_counts(&roots, revisions).1.is_none());
        }
        assert!(!library.publish_search_counts(&old, &roots, revisions, Err(CountBlock::Failed)));
        assert!(!library.accept_search_counts(
            old,
            &roots,
            revisions,
            Ok(search_count_result(&roots[0], 99))
        ));
        let latest = library.prepare_search_counts(&roots, revisions).1.unwrap();
        assert!(library.accept_search_counts(
            latest,
            &roots,
            revisions,
            Ok(search_count_result(&roots[0], 2))
        ));
        assert_eq!(library.search_counts(), Some((2, 3)));
        assert!(!library.search_counts_pending());
    }

    #[test]
    fn search_counts_changed_demand_rejects_old_completion_and_releases_worker() {
        // Completion must also reject changes not yet reconciled by viewport demand.
        for reconcile in [false, true] {
            for change in 0..6 {
                let mut library = LibraryBrowser::default();
                let mut roots = vec![std::path::PathBuf::from("/music")];
                let mut revisions = QueryRevisions::default();
                library.refresh_search("initial".into());
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
                    4 => revisions.counts += 1,
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
                let stale = search_count_result(&request.scope.roots[0], 99);
                assert!(!library.accept_search_counts(request, &roots, revisions, Ok(stale)));
                assert!(!library.search_counts_pending());
                load_root_totals(&mut library, &roots, revisions);
                assert_eq!(library.search_counts(), None);
                assert!(!library.root_count_failed(&roots[0]));
                let (_, current) = library.prepare_search_counts(&roots, revisions);
                assert!(library.accept_search_counts(
                    current.unwrap(),
                    &roots,
                    revisions,
                    Ok(search_count_result(&roots[0], 2)),
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
            load_root_totals(&mut library, &roots, revisions);
            let (_, request) = library.prepare_search_counts(&roots, revisions);
            let request = request.unwrap();
            library.cancel_pending_reads();
            assert!(!request.ticket.is_current());
            assert!(library.search_counts_pending());
            // Revealing before physical completion cannot submit another worker.
            assert!(library.prepare_search_counts(&roots, revisions).1.is_none());
            let result = (!failed_completion)
                .then(|| search_count_result(&roots[0], 99))
                .ok_or(CountBlock::Failed);
            assert!(!library.accept_search_counts(request, &roots, revisions, result));
            assert!(!library.search_counts_pending());
            assert_eq!(library.search_counts(), None);
            assert!(!library.root_count_failed(&roots[0]));
            let (_, request) = library.prepare_search_counts(&roots, revisions);
            assert!(library.accept_search_counts(
                request.unwrap(),
                &roots,
                revisions,
                Ok(search_count_result(&roots[0], 1)),
            ));
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
        load_root_totals(&mut library, &roots, revisions);
        let (_, request) = library.prepare_search_counts(&roots, revisions);
        assert!(library.accept_search_counts(
            request.unwrap(),
            &roots,
            revisions,
            Err(CountBlock::Failed),
        ));
        assert!(!library.search_counts_pending());
        assert_eq!(library.search_counts(), None);
        assert_eq!(library.search_count_parts(), (None, Some(3)));
        assert_eq!(library.root_count_label(&roots[0]), "- / 3");
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
        assert!(library.accept_search_counts(
            request.unwrap(),
            &roots,
            revisions,
            Err(CountBlock::Failed),
        ));
        library.refresh_search("other".into());
        let (changed, request) = library.prepare_search_counts(&roots, revisions);
        assert!(changed);
        assert!(!library.root_count_failed(&roots[0]));
        assert!(library.accept_search_counts(
            request.unwrap(),
            &roots,
            revisions,
            Ok(search_count_result(&roots[0], 0)),
        ));
        assert_eq!(library.search_counts(), Some((0, 3)));
        // An accepted payload is invalidated together with its per-root counts.
        library.refresh_search("third".into());
        let (changed, request) = library.prepare_search_counts(&roots, revisions);
        assert!(changed);
        assert!(request.is_some());
        assert_eq!(library.search_count_parts(), (None, Some(3)));
    }

    #[test]
    fn other_counts_failure_requires_explicit_retry_then_recovers_with_or_without_roots() {
        for roots in [vec![], vec![std::path::PathBuf::from("/music")]] {
            for filtered in [false, true] {
                let mut library = LibraryBrowser::default();
                let revisions = QueryRevisions::default();
                if filtered {
                    library.refresh_search("needle".into());
                }
                library.sync_search_counts(&roots, revisions, Instant::now());
                load_other_total(&mut library, &roots, revisions, None);
                assert!(library.other_count_failed());
                assert_eq!(library.search_count_parts(), (None, None));
                if filtered {
                    let request = library.prepare_search_counts(&roots, revisions).1.unwrap();
                    assert!(library.accept_search_counts(
                        request,
                        &roots,
                        revisions,
                        Ok(SearchCountResult {
                            matched: 2,
                            roots: Default::default(),
                            other_matched: 2,
                        })
                    ));
                    assert_eq!(library.other_count_label(), "2 / -");
                }
                assert!(library.prepare_other_count().is_none());
                library.retry_root_counts();
                assert!(!library.other_count_failed());
                let read = library.prepare_other_count().unwrap();
                library.retry_root_counts();
                assert!(library.prepare_other_count().is_none());
                assert!(library.publish_other_count(
                    &read,
                    &roots,
                    revisions,
                    &other_response(revisions, Some(3)),
                ));
                library.retire_other_count(&read);
                assert_eq!(
                    library.other_count_label(),
                    if filtered { "2 / 3" } else { "3" }
                );
                assert!(!library.other_count_failed());
                assert!(!library.search_counts_pending());
                assert!(library.prepare_search_counts(&roots, revisions).1.is_none());
            }
        }
    }

    #[test]
    fn other_membership_preserves_filtered_zero_and_removes_the_last_item_asynchronously() {
        for roots in [vec![], vec![std::path::PathBuf::from("/music")]] {
            for grouped in [false, true] {
                let mut library = LibraryBrowser::default();
                if grouped {
                    library.toggle_view();
                }
                library.refresh_search("needle".into());
                let mut revisions = QueryRevisions::default();
                let (_, request) = library.prepare_search_counts(&roots, revisions);
                assert!(!library.other_tracks_empty());
                assert_eq!(library.tree.root_total(), Some(roots.len() + 1));
                assert!(library.accept_search_counts(
                    request.unwrap(),
                    &roots,
                    revisions,
                    Ok(SearchCountResult {
                        matched: 0,
                        roots: Default::default(),
                        other_matched: 0,
                    }),
                ));
                assert_eq!(library.other_count_label(), "0 / -");
                load_other_total(&mut library, &roots, revisions, Some(3));
                assert_eq!(library.other_count_label(), "0 / 3");
                assert!(!library.other_tracks_empty());
                assert_eq!(library.tree.root_total(), Some(roots.len() + 1));
                library.select_node(&LibraryNode::Track(41), false);
                library.select_node(&LibraryNode::OtherTracks, true);

                revisions.structure += 1;
                library.sync_search_counts(&roots, revisions, Instant::now());
                assert!(!library.other_tracks_empty());
                load_other_total(&mut library, &roots, revisions, Some(0));
                assert!(library.other_tracks_empty());
                assert_eq!(library.tree.root_total(), Some(roots.len()));
                assert!(!library.selected_nodes().contains(&LibraryNode::OtherTracks));
                assert!(library.selected().contains(&41));
                if roots.is_empty() {
                    assert_eq!(library.tree.row_count(), 0);
                    assert!(library.demand_directories(0..1).is_empty());
                }

                // Filter-only changes cannot turn an authoritative zero into unknown.
                library.refresh_search("another".into());
                let (_, request) = library.prepare_search_counts(&roots, revisions);
                assert!(library.other_tracks_empty());
                assert_eq!(library.tree.root_total(), Some(roots.len()));
                if let Some(request) = request {
                    assert!(!roots.is_empty());
                    assert!(library.accept_search_counts(
                        request,
                        &roots,
                        revisions,
                        Err(CountBlock::Failed),
                    ));
                } else {
                    assert!(roots.is_empty());
                    assert_eq!(library.search_count_parts(), (Some(0), Some(0)));
                }
                assert!(library.other_tracks_empty());

                revisions.structure += 1;
                library.sync_search_counts(&roots, revisions, Instant::now());
                assert!(!library.other_tracks_empty());
                load_other_total(&mut library, &roots, revisions, Some(1));
                assert_eq!(library.tree.root_total(), Some(roots.len() + 1));
                assert!(!library.other_tracks_empty());
            }
        }
    }

    #[test]
    fn ahead_other_total_waits_for_structure_or_explicit_retry() {
        for retry in [false, true] {
            let roots = [];
            let mut library = LibraryBrowser::default();
            let mut revisions = QueryRevisions::default();
            library.sync_search_counts(&roots, revisions, Instant::now());
            let read = library.prepare_other_count().unwrap();
            let ahead = other_response(
                QueryRevisions {
                    structure: 1,
                    ..revisions
                },
                Some(0),
            );
            assert!(!library.publish_other_count(&read, &roots, revisions, &ahead));
            assert!(library.other_count_retired());
            assert!(!library.other_count_failed());
            assert_eq!(library.tree().root_total(), Some(1));
            assert_eq!(library.search_count_parts(), (None, None));
            assert!(library.count_reads_pending());
            assert!(library.prepare_other_count().is_none());
            if !retry {
                library.retire_other_count(&read);
                assert!(!library.count_reads_pending());
            }

            // More viewport demand and unrelated revisions must not resubmit it.
            revisions.counts += 1;
            library.sync_search_counts(&roots, revisions, Instant::now());
            assert!(library.prepare_other_count().is_none());
            if retry {
                library.retry_root_counts();
                assert!(library.prepare_other_count().is_none());
                library.retire_other_count(&read);
                assert!(!library.count_reads_pending());
            } else {
                revisions.structure += 1;
                library.sync_search_counts(&roots, revisions, Instant::now());
            }
            assert!(!library.other_count_retired());
            let current = library.prepare_other_count().unwrap();
            assert!(!library.publish_other_count(&read, &roots, revisions, &ahead));
            assert!(library.publish_other_count(
                &current,
                &roots,
                revisions,
                &other_response(revisions, Some(2)),
            ));
            library.retire_other_count(&current);
            assert_eq!(library.search_count_parts(), (Some(2), Some(2)));
            assert!(library.prepare_other_count().is_none());
        }
    }

    #[test]
    fn empty_library_resolves_filtered_caption_after_matching_count_failure() {
        let mut library = LibraryBrowser::default();
        let roots = [];
        let mut revisions = QueryRevisions::default();
        library.refresh_search("needle".into());
        let read = library.prepare_search_counts(&roots, revisions).1.unwrap();
        assert!(library.accept_search_counts(read, &roots, revisions, Err(CountBlock::Failed)));
        assert!(library.other_count_failed());
        assert_eq!(library.search_count_parts(), (None, None));
        load_other_total(&mut library, &roots, revisions, Some(0));
        assert_eq!(library.search_count_parts(), (Some(0), Some(0)));
        assert_eq!(library.tree().row_count(), 0);
        library.refresh_search("another filter".into());
        assert!(library.prepare_search_counts(&roots, revisions).1.is_none());
        assert_eq!(library.search_count_parts(), (Some(0), Some(0)));

        revisions.structure += 1;
        assert!(library.prepare_search_counts(&roots, revisions).1.is_some());
        assert_eq!(library.search_count_parts(), (None, None));
    }

    #[test]
    fn initially_empty_other_counts_remove_the_no_root_placeholder_without_redemand() {
        for grouped in [false, true] {
            let mut library = LibraryBrowser::default();
            if grouped {
                library.toggle_view();
            }
            let revisions = QueryRevisions::default();
            assert!(library.prepare_search_counts(&[], revisions).1.is_none());
            assert_eq!(library.tree.root_total(), Some(1));
            load_other_total(&mut library, &[], revisions, Some(0));
            assert_eq!(library.tree.root_total(), Some(0));
            assert_eq!(library.tree.row_count(), 0);
            assert!(library.demand_directories(0..1).is_empty());
            assert!(library.prepare_search_counts(&[], revisions).1.is_none());
            assert_eq!(library.tree.row_count(), 0);
        }
    }

    #[test]
    fn library_locate_expands_ancestry_without_changing_selected_identities() {
        use rivu_protocol::model::LibraryLocation;
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
                Some(0),
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
                rows: vec![DirectoryRow::Track(rivu_protocol::model::LibraryRow {
                    id: 41,
                    path: Default::default(),
                    title: "selected".into(),
                    artist: String::new(),
                    album: String::new(),
                    duration: None,
                    codec: String::new(),
                    bitrate_bps: None,
                    bitrate_exact: false,
                    channels: 0,
                    sample_rate: 0,
                    bits_per_sample: None,
                    cue_number: None,
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
    fn drag_source_resolves_current_mixed_selection_without_duplicates() {
        let mut library = LibraryBrowser::default();
        let folder =
            LibraryNode::Directory(std::sync::Arc::from(std::path::Path::new("/music/album")));
        library.select_node(&folder, false);
        library.select_node(&LibraryNode::Track(1), true);
        library.select_node(&LibraryNode::Track(2), true);
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
                vec![rivu_protocol::model::LibraryRootCount {
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
        let roots: Vec<_> = (0..rivu_protocol::model::MAX_QUERY_ROWS + 1)
            .map(|index| std::path::PathBuf::from(format!("/root/{index}")))
            .collect();
        let (generation, batches) = library.begin_root_counts(&roots, 7);
        assert_eq!(batches, Some(0));
        assert_eq!(library.root_count(&roots[0]), None);
        library.accept_root_counts(
            generation,
            0,
            roots[..rivu_protocol::model::MAX_QUERY_ROWS]
                .iter()
                .map(|path| rivu_protocol::model::LibraryRootCount {
                    path: path.clone(),
                    total: 0,
                })
                .collect(),
        );
        assert_eq!(library.root_count(&roots[0]), Some(0));
        assert_eq!(
            library.begin_root_counts(&roots, 7).1,
            Some(rivu_protocol::model::MAX_QUERY_ROWS)
        );
        library.fail_root_counts(generation, rivu_protocol::model::MAX_QUERY_ROWS);
        assert!(!library.root_count_failed(&roots[0]));
        assert!(library.root_count_failed(roots.last().unwrap()));
        assert!(!library.root_count_failed(std::path::Path::new("/other")));
        assert_eq!(library.root_count(roots.last().unwrap()), None);
        assert!(library.begin_root_counts(&roots, 7).1.is_none());
        assert!(library.root_counts_settled());
        assert!(library.root_count_failed(roots.last().unwrap()));
        library.retry_root_counts();
        assert!(!library.root_counts_settled());
        assert_eq!(
            library.begin_root_counts(&roots, 7).1,
            Some(rivu_protocol::model::MAX_QUERY_ROWS)
        );
        assert!(!library.root_count_failed(roots.last().unwrap()));
        assert!(library.begin_root_counts(&roots, 7).1.is_none());
        library.accept_root_counts(
            generation,
            rivu_protocol::model::MAX_QUERY_ROWS,
            vec![rivu_protocol::model::LibraryRootCount {
                path: roots.last().unwrap().clone(),
                total: 2,
            }],
        );
        assert_eq!(library.root_count(roots.last().unwrap()), Some(2));
        assert!(library.begin_root_counts(&roots, 7).1.is_none());
        assert!(library.root_counts_settled());
        library.refresh_search("needle".into());
        assert_eq!(library.root_count(&roots[0]), Some(0));
        let (scanned, _) = library.begin_root_counts(&roots, 8);
        assert_ne!(generation, scanned);
        assert!(!library.counts_current(generation, &roots, 8));
        library.accept_root_counts(
            generation,
            0,
            vec![rivu_protocol::model::LibraryRootCount {
                path: roots[0].clone(),
                total: 99,
            }],
        );
        assert_eq!(library.root_count(&roots[0]), None);
        library.accept_root_counts(
            scanned,
            0,
            vec![rivu_protocol::model::LibraryRootCount {
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
        assert!(empty.1.is_none());
        assert!(library.root_counts_settled());
    }

    #[test]
    fn replacing_roots_rejects_in_flight_counts_at_the_same_structure_revision() {
        let mut library = LibraryBrowser::default();
        let roots = vec![std::path::PathBuf::from("/music")];
        let (generation, batches) = library.begin_root_counts(&roots, 7);
        assert_eq!(batches, Some(0));
        library.accept_root_counts(
            generation,
            0,
            vec![rivu_protocol::model::LibraryRootCount {
                path: roots[0].clone(),
                total: 4,
            }],
        );
        assert_eq!(library.root_count(&roots[0]), Some(4));

        let replacement = vec![std::path::PathBuf::from("/other")];
        assert!(!library.counts_current(generation, &replacement, 7));
        let (next, batches) = library.begin_root_counts(&replacement, 7);
        assert_ne!(next, generation);
        assert_eq!(batches, Some(0));
        library.fail_root_counts(generation, 0);
        library.accept_root_counts(
            generation,
            0,
            vec![rivu_protocol::model::LibraryRootCount {
                path: roots[0].clone(),
                total: 99,
            }],
        );
        assert!(!library.root_count_failed(&replacement[0]));
        assert_eq!(library.root_count(&roots[0]), None);
        assert!(library.begin_root_counts(&replacement, 7).1.is_none());
        library.accept_root_counts(
            next,
            0,
            vec![rivu_protocol::model::LibraryRootCount {
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
    fn replacing_large_selection_releases_capacity_and_preserves_additive_semantics() {
        let mut library = LibraryBrowser::default();
        let range = (0..512).flat_map(|id| {
            [
                LibraryNode::Track(id),
                LibraryNode::Directory(std::sync::Arc::from(std::path::Path::new(&format!(
                    "/music/{id}"
                )))),
            ]
        });
        merge_selection(
            &mut library.selected,
            &mut library.selected_nodes,
            range,
            false,
        );
        library.remember_selection();
        assert_eq!(library.selected.len(), 512);
        assert_eq!(library.selected_nodes.len(), 512);
        library.select_node(&LibraryNode::Track(7), false);
        assert_eq!(library.selected, HashSet::from([7]));
        assert!(library.selected_nodes.is_empty());
        assert!(library.selected.capacity() <= 64);
        assert!(library.selected_nodes.capacity() <= 64);
        library.select_node(&LibraryNode::OtherTracks, true);
        library.select_node(&LibraryNode::Track(8), true);
        assert_eq!(library.selected, HashSet::from([7, 8]));
        assert_eq!(
            library.selected_nodes,
            HashSet::from([LibraryNode::OtherTracks])
        );
        library.select_node(&LibraryNode::Track(7), true);
        assert_eq!(library.selected, HashSet::from([8]));
        library.clear_selection();
        assert!(library.selected.is_empty());
        assert!(library.selected_nodes.is_empty());
    }
}
