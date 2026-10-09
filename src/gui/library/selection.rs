use super::{
    LibraryNode,
    tree::{TreeBranch, TreeRangeSpan},
};
use crate::{
    gui::{GuiApp, ListFocus},
    model::{DirectoryRow, LibrarySort, Query},
    response::{QueryResponse, QueryRevisions, ViewResponse},
};
use gpui::Context;
use std::{
    collections::{HashSet, VecDeque},
    ops::Range,
    sync::Arc,
};

// Query payloads never enter the viewport LRU or survive the current batch.
const MAX_RANGE_BATCH: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Projection {
    Flat {
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: LibrarySort,
        total: usize,
    },
    Tree {
        revision: u64,
        total: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Scope {
    library: u64,
    structure: u64,
    projection: Projection,
}

impl Scope {
    fn capture(app: &GuiApp) -> Self {
        Self {
            library: app.state.library.revision,
            structure: app.state.library.structure_revision,
            projection: if app.library_tree_active {
                Projection::Tree {
                    revision: app.directory_tree.projection_revision(),
                    total: app.directory_tree.row_count(),
                }
            } else {
                Projection::Flat {
                    query: app.library_buffer.query.clone(),
                    favorite: app.library_buffer.favorite,
                    missing: app.library_buffer.missing,
                    sort: app.library_buffer.sort,
                    total: app.library_buffer.total,
                }
            },
        }
    }

    fn matches(&self, app: &GuiApp) -> bool {
        self.library == app.state.library.revision
            && self.structure == app.state.library.structure_revision
            && match &self.projection {
                Projection::Tree { revision, total } => {
                    app.library_tree_active
                        && *revision == app.directory_tree.projection_revision()
                        && *total == app.directory_tree.row_count()
                }
                Projection::Flat {
                    query,
                    favorite,
                    missing,
                    sort,
                    total,
                } => {
                    !app.library_tree_active
                        && query == &app.library_buffer.query
                        && *favorite == app.library_buffer.favorite
                        && *missing == app.library_buffer.missing
                        && *sort == app.library_buffer.sort
                        && *total == app.library_buffer.total
                }
            }
    }

    fn accepts(&self, revisions: QueryRevisions) -> bool {
        self.library == revisions.library && self.structure == revisions.structure
    }

    fn total(&self) -> usize {
        match self.projection {
            Projection::Flat { total, .. } | Projection::Tree { total, .. } => total,
        }
    }
}

#[derive(Clone)]
struct Anchor {
    index: usize,
    scope: Scope,
}

#[derive(Clone, Debug)]
enum Source {
    Flat,
    Tree(TreeBranch),
}

#[derive(Clone, Debug)]
struct Slice {
    source: Source,
    offset: usize,
    len: usize,
    total: usize,
    // A retained expanded ancestor must still occupy its frozen parent position.
    check: Option<TreeBranch>,
}

impl Slice {
    fn tree(span: TreeRangeSpan, check: Option<TreeBranch>) -> Self {
        Self {
            source: Source::Tree(span.branch),
            offset: span.offset,
            len: span.len,
            total: span.total,
            check,
        }
    }
}

struct RangeJob {
    token: u64,
    scope: Scope,
    slices: VecDeque<Slice>,
    selected: HashSet<LibraryNode>,
    endpoint: LibraryNode,
    endpoint_index: usize,
    additive: bool,
    batch_size: usize,
}

impl RangeJob {
    fn next(&self) -> Option<Slice> {
        let mut slice = self.slices.front()?.clone();
        slice.len = slice
            .len
            .min(self.batch_size - slice.offset % self.batch_size);
        Some(slice)
    }

    fn advance(&mut self, count: usize) {
        let slice = self
            .slices
            .front_mut()
            .expect("a consumed range slice exists");
        slice.offset += count;
        slice.len -= count;
        if slice.len == 0 {
            self.slices.pop_front();
        }
    }

    fn add_row(&mut self, slice: &Slice, row: &DirectoryRow) -> Result<(), String> {
        if let Some(expected) = &slice.check {
            if !matches!(row, DirectoryRow::Directory { path } if path == expected.path()) {
                return Err("Library tree changed while selecting; select the range again.".into());
            }
        } else {
            self.selected.insert(match row {
                DirectoryRow::Directory { path } => {
                    LibraryNode::Directory(Arc::from(path.as_path()))
                }
                DirectoryRow::Track(track) => LibraryNode::Track(track.id),
            });
        }
        Ok(())
    }

    fn add_directory(&mut self, slice: &Slice, path: &std::path::Path) -> Result<(), String> {
        if let Some(expected) = &slice.check {
            if path != expected.path() {
                return Err("Library tree changed while selecting; select the range again.".into());
            }
        } else {
            self.selected
                .insert(LibraryNode::Directory(Arc::from(path)));
        }
        Ok(())
    }

    fn accept(&mut self, slice: &Slice, response: QueryResponse) -> Result<(), String> {
        if !self.scope.accepts(response.revisions) {
            return Err("Library changed while selecting; select the range again.".into());
        }
        let result = response.result.map_err(|error| error.to_string())?;
        match (&slice.source, result) {
            (Source::Flat, ViewResponse::LibraryPage(page)) => {
                Self::validate_page(slice, page.total, page.rows.len())?;
                self.selected
                    .extend(page.rows.into_iter().map(|row| LibraryNode::Track(row.id)));
            }
            (Source::Tree(_), ViewResponse::DirectoryPage(page)) => {
                Self::validate_page(slice, page.total, page.rows.len())?;
                for row in &page.rows {
                    self.add_row(slice, row)?;
                }
            }
            _ => return Err("Unexpected library range query response.".into()),
        }
        self.advance(slice.len);
        Ok(())
    }

    fn validate_page(slice: &Slice, total: usize, count: usize) -> Result<(), String> {
        if total != slice.total || count != slice.len {
            Err("Library rows changed while selecting; select the range again.".into())
        } else {
            Ok(())
        }
    }

    fn query(&self, slice: &Slice) -> Query {
        match &slice.source {
            Source::Tree(branch) => Query::DirectoryPage {
                path: branch.path().to_path_buf(),
                offset: slice.offset,
                limit: slice.len,
            },
            Source::Flat => {
                let Projection::Flat {
                    query,
                    favorite,
                    missing,
                    sort,
                    ..
                } = &self.scope.projection
                else {
                    unreachable!("flat ranges have a flat projection")
                };
                Query::LibraryPage {
                    query: query.clone(),
                    favorite: *favorite,
                    missing: *missing,
                    sort: *sort,
                    offset: slice.offset,
                    limit: slice.len,
                }
            }
        }
    }
}

#[derive(Default)]
pub(in crate::gui) struct LibrarySelection {
    anchor: Option<Anchor>,
    token: u64,
    pending: Option<RangeJob>,
    // Cancellation keeps this slot occupied until the outstanding query returns.
    active: Option<u64>,
}

impl LibrarySelection {
    fn cancel(&mut self, clear_anchor: bool) {
        self.token = self.token.wrapping_add(1);
        self.pending = None;
        if clear_anchor {
            self.anchor = None;
        }
    }

    fn interval(&self, scope: &Scope, endpoint: usize) -> Range<usize> {
        let anchor = self
            .anchor
            .as_ref()
            .filter(|anchor| anchor.scope == *scope)
            .map(|anchor| anchor.index)
            .unwrap_or(endpoint);
        anchor.min(endpoint)..anchor.max(endpoint) + 1
    }

    fn accepts_completion(&self, token: u64) -> bool {
        self.token == token && self.pending.as_ref().is_some_and(|job| job.token == token)
    }
}

fn merge_selection(
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

impl GuiApp {
    pub(in crate::gui) fn library_range_pending(&self) -> bool {
        self.library_selection.pending.is_some()
    }

    /// Call before changing view/filter/expansion or a non-row selection.
    pub(in crate::gui) fn invalidate_library_selection(&mut self) {
        self.library_selection.cancel(true);
    }

    pub(in crate::gui) fn select_library_row(
        &mut self,
        node: LibraryNode,
        index: usize,
        shift: bool,
        multi: bool,
        cx: &mut Context<Self>,
    ) {
        let scope = Scope::capture(self);
        if index >= scope.total() {
            return;
        }
        self.library_navigation = None;
        self.directory_tree.cancel_navigation();
        self.list_focus = Some(ListFocus::Library);
        if !shift {
            self.library_selection.cancel(false);
            if !multi {
                self.selected.clear();
                self.library_selected_nodes.clear();
            }
            match &node {
                LibraryNode::Track(id) => {
                    if multi && !self.selected.insert(*id) {
                        self.selected.remove(id);
                    } else if !multi {
                        self.selected.insert(*id);
                    }
                }
                LibraryNode::Directory(_) => {
                    if multi && !self.library_selected_nodes.insert(node.clone()) {
                        self.library_selected_nodes.remove(&node);
                    } else if !multi {
                        self.library_selected_nodes.insert(node.clone());
                    }
                }
            }
            self.library_selection.anchor = Some(Anchor { index, scope });
            self.finish_library_selection(&node, index, cx);
            return;
        }

        let range = self.library_selection.interval(&scope, index);
        // Shift never moves a valid anchor; the first Shift click anchors itself.
        if !self
            .library_selection
            .anchor
            .as_ref()
            .is_some_and(|anchor| anchor.scope == scope)
        {
            self.library_selection.anchor = Some(Anchor {
                index,
                scope: scope.clone(),
            });
        }
        self.library_selection.cancel(false);
        let slices = match &scope.projection {
            Projection::Flat { total, .. } => VecDeque::from([Slice {
                source: Source::Flat,
                offset: range.start,
                len: range.len(),
                total: *total,
                check: None,
            }]),
            Projection::Tree { .. } => {
                let Some(spans) = self.directory_tree.range_spans(range.clone()) else {
                    self.error = Some("Library tree is still loading; select the range again when its folders are ready.".into());
                    cx.notify();
                    return;
                };
                self.directory_tree
                    .range_checks(range)
                    .into_iter()
                    .map(|(span, child)| Slice::tree(span, Some(child)))
                    .chain(spans.into_iter().map(|span| Slice::tree(span, None)))
                    .collect()
            }
        };
        self.library_selection.pending = Some(RangeJob {
            token: self.library_selection.token,
            scope,
            slices,
            selected: HashSet::new(),
            endpoint: node,
            endpoint_index: index,
            additive: multi,
            batch_size: self.view_page_size().clamp(1, MAX_RANGE_BATCH),
        });
        self.pump_library_range(cx);
        cx.notify();
    }

    fn finish_library_selection(
        &mut self,
        endpoint: &LibraryNode,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let focus = match endpoint {
            LibraryNode::Track(id) if self.selected.contains(id) => Some(*id),
            _ => self.selected.iter().copied().min(),
        };
        if let Some(id) = focus {
            self.update_metadata_track(id, cx);
        } else {
            self.clear_metadata_selection(cx);
        }
        if self.library_tree_active {
            self.directory_tree
                .remember_selection(&self.library_selected_nodes, &self.selected);
            if self.library_selected_nodes.contains(endpoint)
                || matches!(endpoint, LibraryNode::Track(id) if self.selected.contains(id))
            {
                self.directory_tree.focus_selection(endpoint, index);
            }
        } else {
            self.library_buffer.rows.pin(Some(index));
        }
        self.sync_waveform(cx);
        cx.notify();
    }

    fn apply_library_range(&mut self, job: RangeJob, cx: &mut Context<Self>) {
        merge_selection(
            &mut self.selected,
            &mut self.library_selected_nodes,
            job.selected,
            job.additive,
        );
        self.finish_library_selection(&job.endpoint, job.endpoint_index, cx);
    }

    fn pump_library_range(&mut self, cx: &mut Context<Self>) {
        if self.library_selection.active.is_some() {
            return;
        }
        while let Some(mut job) = self.library_selection.pending.take() {
            if !job.scope.matches(self) {
                self.library_selection.cancel(true);
                cx.notify();
                return;
            }
            let Some(slice) = job.next() else {
                self.apply_library_range(job, cx);
                return;
            };
            let cached = match &slice.source {
                Source::Flat => (slice.offset..slice.offset + slice.len)
                    .all(|index| self.library_buffer.rows.get(index).is_some()),
                Source::Tree(branch) => {
                    branch.is_forest()
                        || self
                            .directory_tree
                            .cached_rows(branch, slice.offset..slice.offset + slice.len)
                            .is_some()
                }
            };
            if cached {
                let result = match &slice.source {
                    Source::Flat => {
                        job.selected
                            .extend((slice.offset..slice.offset + slice.len).map(|index| {
                                LibraryNode::Track(
                                    self.library_buffer
                                        .rows
                                        .get(index)
                                        .expect("cached range row")
                                        .id,
                                )
                            }));
                        Ok(())
                    }
                    Source::Tree(branch) if branch.is_forest() => {
                        let roots = &self.state.system.config.library_roots;
                        if roots.len() != slice.total || slice.offset + slice.len > roots.len() {
                            Err(
                                "Library roots changed while selecting; select the range again."
                                    .into(),
                            )
                        } else {
                            roots[slice.offset..slice.offset + slice.len]
                                .iter()
                                .try_for_each(|path| job.add_directory(&slice, path))
                        }
                    }
                    Source::Tree(branch) => self
                        .directory_tree
                        .cached_rows(branch, slice.offset..slice.offset + slice.len)
                        .expect("cached range rows")
                        .iter()
                        .try_for_each(|row| job.add_row(&slice, row)),
                };
                if let Err(error) = result {
                    self.library_selection.cancel(true);
                    self.error = Some(error);
                    cx.notify();
                    return;
                }
                job.advance(slice.len);
                self.library_selection.pending = Some(job);
                continue;
            }
            let query = job.query(&slice);
            let token = job.token;
            self.library_selection.active = Some(token);
            self.library_selection.pending = Some(job);
            let handle = self.handle.clone();
            cx.spawn(async move |this, cx| {
                let response = cx
                    .background_executor()
                    .spawn(async move { handle.query(query) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this.library_selection.active == Some(token) {
                        this.library_selection.active = None;
                    }
                    if this.library_selection.accepts_completion(token) {
                        let mut job = this
                            .library_selection
                            .pending
                            .take()
                            .expect("current pending range");
                        if job.scope.matches(this) {
                            match job.accept(&slice, response) {
                                Ok(()) => this.library_selection.pending = Some(job),
                                Err(error) => {
                                    this.library_selection.cancel(true);
                                    this.error = Some(error);
                                }
                            }
                        } else {
                            this.library_selection.cancel(true);
                        }
                    }
                    // A cancelled in-flight batch never applies and cannot overlap its successor.
                    this.pump_library_range(cx);
                    cx.notify();
                });
            })
            .detach();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LibraryRow, Page};

    fn scope() -> Scope {
        Scope {
            library: 7,
            structure: 3,
            projection: Projection::Flat {
                query: Some("filtered".into()),
                favorite: Some(true),
                missing: None,
                sort: LibrarySort::Album,
                total: 10_000,
            },
        }
    }

    fn row(id: i64) -> LibraryRow {
        LibraryRow {
            id,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            duration: None,
            favorite: true,
            missing: false,
            play_count: 0,
        }
    }

    fn job(start: usize, end: usize, additive: bool) -> RangeJob {
        RangeJob {
            token: 1,
            scope: scope(),
            slices: VecDeque::from([Slice {
                source: Source::Flat,
                offset: start,
                len: end - start,
                total: 10_000,
                check: None,
            }]),
            selected: HashSet::new(),
            endpoint: LibraryNode::Track(end as i64 - 1),
            endpoint_index: end - 1,
            additive,
            batch_size: 2,
        }
    }

    #[test]
    fn range_endpoints_reverse_and_repeated_shift_keep_click_anchor() {
        let scope = scope();
        let mut selection = LibrarySelection::default();
        assert_eq!(selection.interval(&scope, 8), 8..9);
        selection.anchor = Some(Anchor {
            index: 4,
            scope: scope.clone(),
        });
        assert_eq!(selection.interval(&scope, 8), 4..9);
        assert_eq!(selection.interval(&scope, 1), 1..5);
        assert_eq!(selection.interval(&scope, 6), 4..7);
        assert_eq!(selection.anchor.as_ref().unwrap().index, 4);
        let mut changed = scope.clone();
        changed.library += 1;
        assert_eq!(selection.interval(&changed, 9), 9..10);
    }

    #[test]
    fn range_accumulates_exact_absolute_interval_across_many_batches() {
        let mut job = job(4001, 4042, false);
        let mut batches = 0;
        while let Some(slice) = job.next() {
            let rows = (slice.offset..slice.offset + slice.len)
                .map(|id| row(id as i64))
                .collect();
            job.accept(
                &slice,
                QueryResponse {
                    revisions: QueryRevisions {
                        library: 7,
                        structure: 3,
                        playlist: 0,
                    },
                    result: Ok(ViewResponse::LibraryPage(Page {
                        total: 10_000,
                        rows,
                    })),
                },
            )
            .unwrap();
            batches += 1;
            assert!(batches <= 21, "bounded range batches must converge");
        }
        assert_eq!(job.selected, (4001..4042).map(LibraryNode::Track).collect());
    }

    #[test]
    fn stale_revision_and_short_batch_never_complete_selection() {
        let mut job = job(2, 8, false);
        let slice = job.next().unwrap();
        let response = |library, rows| QueryResponse {
            revisions: QueryRevisions {
                library,
                structure: 3,
                playlist: 0,
            },
            result: Ok(ViewResponse::LibraryPage(Page {
                total: 10_000,
                rows,
            })),
        };
        assert!(
            job.accept(&slice, response(8, vec![row(2), row(3)]))
                .is_err()
        );
        assert!(job.selected.is_empty());
        assert!(job.accept(&slice, response(7, vec![row(2)])).is_err());
        assert!(job.selected.is_empty());
        assert_eq!(job.next().unwrap().offset, 2);
    }

    #[test]
    fn newer_click_rejects_old_completion_and_keeps_active_batch_slot() {
        let mut selection = LibrarySelection {
            token: 1,
            pending: Some(job(0, 10, false)),
            active: Some(1),
            ..Default::default()
        };
        assert!(selection.accepts_completion(1));
        selection.cancel(false);
        assert!(!selection.accepts_completion(1));
        assert_eq!(selection.active, Some(1));
        let mut next = job(20, 30, true);
        next.token = selection.token;
        selection.pending = Some(next);
        assert!(!selection.accepts_completion(1));
        assert!(selection.accepts_completion(selection.token));
    }

    #[test]
    fn additive_interval_preserves_existing_ids_and_deduplicates_overlap() {
        let mut job = job(3, 6, true);
        job.selected.extend([
            LibraryNode::Track(3),
            LibraryNode::Track(4),
            LibraryNode::Track(5),
        ]);
        let mut tracks = HashSet::from([1, 3]);
        let directory = LibraryNode::Directory(Arc::from(std::path::Path::new("/music")));
        let mut nodes = HashSet::from([directory.clone()]);
        merge_selection(&mut tracks, &mut nodes, job.selected, job.additive);
        assert_eq!(tracks, HashSet::from([1, 3, 4, 5]));
        assert_eq!(nodes, HashSet::from([directory]));
        merge_selection(
            &mut tracks,
            &mut nodes,
            HashSet::from([LibraryNode::Track(9)]),
            false,
        );
        assert_eq!(tracks, HashSet::from([9]));
        assert!(nodes.is_empty());
    }

    #[test]
    fn sparse_tree_range_crosses_evicted_batches_and_deduplicates_overlapping_roots() {
        use super::super::tree::{DirectoryTree, TreeItem};
        use std::path::Path;
        let mut tree = DirectoryTree::default();
        tree.configure_page_size(2);
        let forest = TreeBranch::default();
        tree.begin(forest.clone(), 0, 1);
        assert!(tree.accept(
            &forest,
            1,
            Page {
                total: 2,
                rows: vec![
                    DirectoryRow::Directory {
                        path: "/music".into()
                    },
                    DirectoryRow::Directory {
                        path: "/music/sub".into()
                    },
                ]
            }
        ));
        let occurrence = |tree: &DirectoryTree, index| match tree.item(index).unwrap() {
            TreeItem::Entry(row) => row.branch.unwrap(),
            TreeItem::Placeholder { .. } => panic!("expected loaded directory occurrence"),
        };
        let root = occurrence(&tree, 0);
        let overlapping = occurrence(&tree, 1);
        tree.expand(&root);
        tree.begin(root.clone(), 0, 2);
        assert!(tree.accept(
            &root,
            2,
            Page {
                total: 40,
                rows: vec![
                    DirectoryRow::Directory {
                        path: "/music/sub".into()
                    },
                    DirectoryRow::Track(row(101)),
                ]
            }
        ));
        let nested = occurrence(&tree, 1);
        tree.expand(&nested);
        tree.begin(nested.clone(), 0, 3);
        assert!(tree.accept(
            &nested,
            3,
            Page {
                total: 40,
                rows: vec![DirectoryRow::Track(row(200)), DirectoryRow::Track(row(201)),]
            }
        ));
        tree.expand(&overlapping);
        tree.begin(overlapping.clone(), 0, 4);
        assert!(tree.accept(
            &overlapping,
            4,
            Page {
                total: 40,
                rows: vec![DirectoryRow::Track(row(200)), DirectoryRow::Track(row(201)),]
            }
        ));
        for offset in (2..40).step_by(2) {
            tree.begin(root.clone(), offset, offset as u64 + 10);
            assert!(tree.accept(
                &root,
                offset as u64 + 10,
                Page {
                    total: 40,
                    rows: vec![
                        DirectoryRow::Track(row(100 + offset as i64)),
                        DirectoryRow::Track(row(101 + offset as i64)),
                    ]
                }
            ));
        }
        assert!(tree.cached_row(&nested, 0).is_none());
        assert!(tree.cached_row(&root, 2).is_none());
        let mut job = RangeJob {
            token: 1,
            scope: Scope {
                library: 7,
                structure: 3,
                projection: Projection::Tree {
                    revision: tree.projection_revision(),
                    total: tree.row_count(),
                },
            },
            slices: tree
                .range_spans(0..tree.row_count())
                .unwrap()
                .into_iter()
                .map(|span| Slice::tree(span, None))
                .collect(),
            selected: HashSet::new(),
            endpoint: LibraryNode::Track(239),
            endpoint_index: tree.row_count() - 1,
            additive: false,
            batch_size: 2,
        };
        let mut queries = 0;
        while let Some(slice) = job.next() {
            let Source::Tree(branch) = &slice.source else {
                panic!("tree projection source")
            };
            let rows = if branch.is_forest() {
                ["/music", "/music/sub"][slice.offset..slice.offset + slice.len]
                    .iter()
                    .map(|path| DirectoryRow::Directory {
                        path: (*path).into(),
                    })
                    .collect()
            } else {
                assert!(
                    matches!(job.query(&slice), Query::DirectoryPage { path, offset, limit }
                    if path == branch.path() && offset == slice.offset && limit == slice.len)
                );
                queries += 1;
                (slice.offset..slice.offset + slice.len)
                    .map(|index| {
                        if branch.path() == Path::new("/music") && index == 0 {
                            DirectoryRow::Directory {
                                path: "/music/sub".into(),
                            }
                        } else {
                            DirectoryRow::Track(row(if branch.path() == Path::new("/music") {
                                100 + index as i64
                            } else {
                                200 + index as i64
                            }))
                        }
                    })
                    .collect()
            };
            job.accept(
                &slice,
                QueryResponse {
                    revisions: QueryRevisions {
                        library: 7,
                        structure: 3,
                        playlist: 0,
                    },
                    result: Ok(ViewResponse::DirectoryPage(Page {
                        total: slice.total,
                        rows,
                    })),
                },
            )
            .unwrap();
        }
        assert!(queries > 8);
        assert_eq!(job.selected.len(), 81);
        assert!(job.selected.contains(&LibraryNode::Track(101)));
        assert!(job.selected.contains(&LibraryNode::Track(239)));
    }

    #[test]
    fn retained_tree_ancestor_validation_rejects_relocated_identity() {
        use super::super::tree::{DirectoryTree, TreeItem};
        let mut tree = DirectoryTree::default();
        let forest = TreeBranch::default();
        tree.begin(forest.clone(), 0, 1);
        assert!(tree.accept(
            &forest,
            1,
            Page {
                total: 1,
                rows: vec![DirectoryRow::Directory { path: "/a".into() },]
            }
        ));
        let TreeItem::Entry(visible) = tree.item(0).unwrap() else {
            panic!("root occurrence")
        };
        let expected = visible.branch.unwrap();
        let slice = Slice {
            source: Source::Tree(forest),
            offset: 0,
            len: 1,
            total: 1,
            check: Some(expected),
        };
        let mut job = RangeJob {
            token: 1,
            scope: Scope {
                library: 7,
                structure: 3,
                projection: Projection::Tree {
                    revision: tree.projection_revision(),
                    total: 1,
                },
            },
            slices: VecDeque::from([slice.clone()]),
            selected: HashSet::new(),
            endpoint: LibraryNode::Directory(Arc::from(std::path::Path::new("/a"))),
            endpoint_index: 0,
            additive: false,
            batch_size: 2,
        };
        assert!(
            job.accept(
                &slice,
                QueryResponse {
                    revisions: QueryRevisions {
                        library: 7,
                        structure: 3,
                        playlist: 0,
                    },
                    result: Ok(ViewResponse::DirectoryPage(Page {
                        total: 1,
                        rows: vec![DirectoryRow::Directory { path: "/b".into() },]
                    }))
                }
            )
            .is_err()
        );
        assert!(job.selected.is_empty());
        assert!(job.next().is_some());
        let selection = LibrarySelection {
            anchor: Some(Anchor {
                index: 0,
                scope: job.scope.clone(),
            }),
            ..Default::default()
        };
        let changed = Scope {
            library: 7,
            structure: 3,
            projection: Projection::Tree {
                revision: tree.projection_revision() + 1,
                total: 1,
            },
        };
        assert_ne!(selection.anchor.as_ref().unwrap().scope, changed);
    }
}
