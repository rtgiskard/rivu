use super::LibraryBrowser;
use crate::gui::library::{
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
        revision: u64,
        total: usize,
    },
    Tree {
        query: Option<String>,
        favorite: Option<bool>,
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
    fn capture(browser: &LibraryBrowser, revisions: (u64, u64)) -> Self {
        let view = browser.buffer();
        Self {
            library: revisions.0,
            structure: revisions.1,
            projection: if browser.tree_active() {
                Projection::Tree {
                    query: view.query.clone(),
                    favorite: browser.favorites_only().then_some(true),
                    revision: browser.tree().projection_revision(),
                    total: browser.tree().row_count(),
                }
            } else {
                Projection::Flat {
                    query: view.query.clone(),
                    favorite: view.favorite,
                    missing: view.missing,
                    sort: view.sort,
                    revision: browser.tree().projection_revision(),
                    total: browser.tree().row_count(),
                }
            },
        }
    }

    fn matches(&self, browser: &LibraryBrowser, revisions: (u64, u64)) -> bool {
        let view = browser.buffer();
        self.library == revisions.0
            && self.structure == revisions.1
            && match &self.projection {
                Projection::Tree {
                    query,
                    favorite,
                    revision,
                    total,
                } => {
                    browser.tree_active()
                        && query == &view.query
                        && *favorite == browser.favorites_only().then_some(true)
                        && *revision == browser.tree().projection_revision()
                        && *total == browser.tree().row_count()
                }
                Projection::Flat {
                    query,
                    favorite,
                    missing,
                    sort,
                    revision,
                    total,
                } => {
                    !browser.tree_active()
                        && query == &view.query
                        && *favorite == view.favorite
                        && *missing == view.missing
                        && *sort == view.sort
                        && *revision == browser.tree().projection_revision()
                        && *total == browser.tree().row_count()
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
    Flat(TreeBranch),
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
            (Source::Flat(_), ViewResponse::LibraryPage(page)) => {
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
            Source::Tree(branch) => {
                let Projection::Tree {
                    query, favorite, ..
                } = &self.scope.projection
                else {
                    unreachable!("tree ranges have a tree projection")
                };
                Query::DirectoryPage {
                    path: branch.path().to_path_buf(),
                    query: query.clone(),
                    favorite: *favorite,
                    offset: slice.offset,
                    limit: slice.len,
                }
            }
            Source::Flat(branch) => {
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
                Query::LibraryRootPage {
                    path: branch.path().to_path_buf(),
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
pub(super) struct LibrarySelection {
    anchor: Option<Anchor>,
    token: u64,
    pending: Option<RangeJob>,
    // Cancellation keeps this slot occupied until the outstanding query returns.
    active: Option<u64>,
}

impl LibrarySelection {
    pub(super) fn cancel(&mut self, clear_anchor: bool) {
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

/// Range state stays behind the browser owner; the window only dispatches work.
enum RangeStep {
    Idle,
    Selected(LibraryNode),
    Query {
        token: u64,
        slice: Slice,
        query: Query,
    },
}

impl LibraryBrowser {
    pub(in crate::gui) fn range_pending(&self) -> bool {
        self.range_selection.pending.is_some()
    }

    fn start_selection(
        &mut self,
        node: &LibraryNode,
        index: usize,
        shift: bool,
        multi: bool,
        revisions: (u64, u64),
        page_size: usize,
    ) -> Result<bool, String> {
        let scope = Scope::capture(self, revisions);
        if index >= scope.total() {
            return Ok(false);
        }
        self.cancel_navigation();
        if !shift {
            self.range_selection.cancel(false);
            self.select_node(node, multi);
            self.range_selection.anchor = Some(Anchor { index, scope });
            self.pin_selection(node, index);
            return Ok(true);
        }
        let range = self.range_selection.interval(&scope, index);
        // Shift never moves a valid anchor; the first Shift click anchors itself.
        if !self
            .range_selection
            .anchor
            .as_ref()
            .is_some_and(|anchor| anchor.scope == scope)
        {
            self.range_selection.anchor = Some(Anchor {
                index,
                scope: scope.clone(),
            });
        }
        self.range_selection.cancel(false);
        let spans = self.tree.range_spans(range.clone()).ok_or_else(|| {
            "Library folders are still loading; select the range again when they are ready."
                .to_owned()
        })?;
        let mut slices: VecDeque<_> = self
            .tree
            .range_checks(range)
            .into_iter()
            .map(|(span, child)| Slice::tree(span, Some(child)))
            .chain(spans.into_iter().map(|span| Slice::tree(span, None)))
            .collect();
        if !self.tree_active {
            for slice in &mut slices {
                if let Source::Tree(branch) = &slice.source {
                    if !branch.is_forest() {
                        slice.source = Source::Flat(branch.clone());
                    }
                }
            }
        }
        self.range_selection.pending = Some(RangeJob {
            token: self.range_selection.token,
            scope,
            slices,
            selected: HashSet::new(),
            endpoint: node.clone(),
            endpoint_index: index,
            additive: multi,
            batch_size: page_size.clamp(1, MAX_RANGE_BATCH),
        });
        Ok(true)
    }

    fn pin_selection(&mut self, endpoint: &LibraryNode, index: usize) {
        if self.selected_nodes.contains(endpoint)
            || matches!(endpoint, LibraryNode::Track(id) if self.selected.contains(id))
        {
            self.tree.focus_selection(endpoint, index);
        }
    }

    fn advance_range(
        &mut self,
        revisions: (u64, u64),
        roots: &[std::path::PathBuf],
    ) -> Result<RangeStep, String> {
        if self.range_selection.active.is_some() {
            return Ok(RangeStep::Idle);
        }
        while let Some(mut job) = self.range_selection.pending.take() {
            if !job.scope.matches(self, revisions) {
                self.range_selection.cancel(true);
                return Ok(RangeStep::Idle);
            }
            let Some(slice) = job.next() else {
                self.apply_range(job.selected, job.additive);
                self.pin_selection(&job.endpoint, job.endpoint_index);
                return Ok(RangeStep::Selected(job.endpoint));
            };
            let cached = match &slice.source {
                Source::Tree(branch) | Source::Flat(branch) => {
                    branch.is_forest()
                        || self
                            .tree
                            .cached_rows(branch, slice.offset..slice.offset + slice.len)
                            .is_some()
                }
            };
            if cached {
                let result = match &slice.source {
                    Source::Tree(branch) if branch.is_forest() => {
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
                    Source::Tree(branch) | Source::Flat(branch) => self
                        .tree
                        .cached_rows(branch, slice.offset..slice.offset + slice.len)
                        .expect("cached range rows")
                        .iter()
                        .try_for_each(|row| job.add_row(&slice, row)),
                };
                if let Err(error) = result {
                    self.range_selection.cancel(true);
                    return Err(error);
                }
                job.advance(slice.len);
                self.range_selection.pending = Some(job);
                continue;
            }
            let query = job.query(&slice);
            let token = job.token;
            self.range_selection.active = Some(token);
            self.range_selection.pending = Some(job);
            return Ok(RangeStep::Query {
                token,
                slice,
                query,
            });
        }
        Ok(RangeStep::Idle)
    }

    fn complete_range(
        &mut self,
        token: u64,
        slice: &Slice,
        response: QueryResponse,
        revisions: (u64, u64),
    ) -> Result<(), String> {
        if self.range_selection.active == Some(token) {
            self.range_selection.active = None;
        }
        if !self.range_selection.accepts_completion(token) {
            return Ok(());
        }
        let mut job = self
            .range_selection
            .pending
            .take()
            .expect("current pending range");
        if !job.scope.matches(self, revisions) {
            self.range_selection.cancel(true);
            return Ok(());
        }
        if let Err(error) = job.accept(slice, response) {
            self.range_selection.cancel(true);
            return Err(error);
        }
        self.range_selection.pending = Some(job);
        Ok(())
    }
}

impl GuiApp {
    pub(in crate::gui) fn library_range_pending(&self) -> bool {
        self.browser.library.range_pending()
    }

    pub(in crate::gui) fn select_library_row(
        &mut self,
        node: LibraryNode,
        index: usize,
        shift: bool,
        multi: bool,
        cx: &mut Context<Self>,
    ) {
        let revisions = (
            self.state.library.revision,
            self.state.library.structure_revision,
        );
        let page_size = self.view_page_size();
        match self
            .browser
            .library
            .start_selection(&node, index, shift, multi, revisions, page_size)
        {
            Ok(false) => return,
            Err(error) => self.error = Some(error),
            Ok(true) => {
                self.browser.focus_list(ListFocus::Library);
                if shift {
                    self.pump_library_range(cx);
                } else {
                    self.finish_library_selection(&node, cx);
                }
            }
        }
        cx.notify();
    }

    fn finish_library_selection(&mut self, endpoint: &LibraryNode, cx: &mut Context<Self>) {
        let focus = match endpoint {
            LibraryNode::Track(id) if self.browser.library.selected().contains(id) => Some(*id),
            _ => self.browser.library.selected().iter().copied().min(),
        };
        if let Some(id) = focus {
            self.request_track(id, cx);
        } else {
            self.clear_metadata_selection(cx);
        }
        self.sync_waveform(cx);
        cx.notify();
    }

    fn pump_library_range(&mut self, cx: &mut Context<Self>) {
        let revisions = (
            self.state.library.revision,
            self.state.library.structure_revision,
        );
        match self
            .browser
            .library
            .advance_range(revisions, &self.state.system.config.library_roots)
        {
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
            Ok(RangeStep::Idle) => {}
            Ok(RangeStep::Selected(endpoint)) => self.finish_library_selection(&endpoint, cx),
            Ok(RangeStep::Query {
                token,
                slice,
                query,
            }) => {
                let handle = self.handle.clone();
                cx.spawn(async move |this, cx| {
                    let response = cx
                        .background_executor()
                        .spawn(async move { handle.query(query) })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        let revisions = (
                            this.state.library.revision,
                            this.state.library.structure_revision,
                        );
                        if let Err(error) = this
                            .browser
                            .library
                            .complete_range(token, &slice, response, revisions)
                        {
                            this.error = Some(error);
                        }
                        // A cancelled batch cannot apply or overlap its successor.
                        this.pump_library_range(cx);
                        cx.notify();
                    });
                })
                .detach();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::browser::merge_selection;
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
                revision: 0,
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
            codec: String::new(),
            bitrate_bps: None,
            favorite: true,
            missing: false,
            play_count: 0,
        }
    }
    fn grouped(
        total: usize,
        page_size: usize,
    ) -> (LibraryBrowser, TreeBranch, Vec<std::path::PathBuf>) {
        let mut library = LibraryBrowser::default();
        library.toggle_view();
        let roots = vec![std::path::PathBuf::from("/music")];
        let forest = TreeBranch::default();
        library.prepare_directory(forest.clone(), 0, page_size, 1);
        library.accept_directory(
            &forest,
            1,
            Page {
                total: 1,
                rows: vec![DirectoryRow::Directory {
                    path: roots[0].clone(),
                }],
            },
        );
        let crate::gui::library::tree::TreeItem::Entry(entry) = library.tree.item(0).unwrap()
        else {
            panic!("root header");
        };
        let branch = entry.branch.unwrap();
        load_group(&mut library, &branch, 0, total, page_size, 2);
        (library, branch, roots)
    }

    fn load_group(
        library: &mut LibraryBrowser,
        branch: &TreeBranch,
        offset: usize,
        total: usize,
        size: usize,
        generation: u64,
    ) {
        library.prepare_directory(branch.clone(), offset, size, generation);
        library.accept_directory(
            branch,
            generation,
            Page {
                total,
                rows: (offset..(offset + size).min(total))
                    .map(|id| DirectoryRow::Track(row(id as i64)))
                    .collect(),
            },
        );
    }

    #[test]
    fn cancelled_owner_range_releases_its_batch_before_loading_the_replacement() {
        let (mut library, branch, roots) = grouped(100, 20);
        library
            .start_selection(&LibraryNode::Track(1), 2, false, false, (7, 3), 20)
            .unwrap();
        library
            .start_selection(&LibraryNode::Track(30), 31, true, false, (7, 3), 20)
            .unwrap();
        let RangeStep::Query { token, slice, .. } = library.advance_range((7, 3), &roots).unwrap()
        else {
            panic!("uncached range needs a batch")
        };
        library.prepare_query("replacement".into());
        let forest = TreeBranch::default();
        library.prepare_directory(forest.clone(), 0, 20, 3);
        library.accept_directory(
            &forest,
            3,
            Page {
                total: 1,
                rows: vec![DirectoryRow::Directory {
                    path: roots[0].clone(),
                }],
            },
        );
        library
            .start_selection(&LibraryNode::Track(50), 51, true, false, (7, 3), 20)
            .unwrap();
        assert!(matches!(
            library.advance_range((7, 3), &roots).unwrap(),
            RangeStep::Idle
        ));
        library
            .complete_range(
                token,
                &slice,
                QueryResponse {
                    revisions: QueryRevisions {
                        library: 7,
                        structure: 3,
                        playlist: 0,
                    },
                    result: Ok(ViewResponse::LibraryPage(Page {
                        total: 100,
                        rows: (slice.offset..slice.offset + slice.len)
                            .map(|id| row(id as i64))
                            .collect(),
                    })),
                },
                (7, 3),
            )
            .unwrap();
        assert_eq!(library.selected(), &HashSet::from([1]));
        let RangeStep::Query {
            token,
            slice,
            query,
        } = library.advance_range((7, 3), &roots).unwrap()
        else {
            panic!("replacement starts after old batch")
        };
        assert!(
            matches!(query, Query::LibraryRootPage { path, query: Some(query), offset: 50, limit: 1, .. }
            if path == branch.path() && query == "replacement")
        );
        library
            .complete_range(
                token,
                &slice,
                QueryResponse {
                    revisions: QueryRevisions {
                        library: 7,
                        structure: 3,
                        playlist: 0,
                    },
                    result: Ok(ViewResponse::LibraryPage(Page {
                        total: 100,
                        rows: vec![row(50)],
                    })),
                },
                (7, 3),
            )
            .unwrap();
        assert!(matches!(
            library.advance_range((7, 3), &roots).unwrap(),
            RangeStep::Selected(LibraryNode::Track(50))
        ));
        assert_eq!(library.selected(), &HashSet::from([50]));
    }

    #[test]
    fn owner_range_selection_keeps_evicted_identities_and_rejects_changed_scope() {
        let (mut library, branch, roots) = grouped(1000, 20);
        load_group(&mut library, &branch, 20, 1000, 20, 3);
        library
            .start_selection(&LibraryNode::Track(1), 2, false, false, (7, 3), 20)
            .unwrap();
        library
            .start_selection(&LibraryNode::Track(21), 22, true, false, (7, 3), 20)
            .unwrap();
        assert!(matches!(
            library.advance_range((7, 3), &roots).unwrap(),
            RangeStep::Selected(LibraryNode::Track(21))
        ));
        for offset in (100..500).step_by(20) {
            load_group(&mut library, &branch, offset, 1000, 20, offset as u64);
        }
        assert!(library.tree.cached_row(&branch, 1).is_none());
        assert_eq!(library.selected(), &(1..=21).collect::<HashSet<_>>());
        library
            .start_selection(&LibraryNode::Track(50), 51, true, false, (7, 3), 20)
            .unwrap();
        assert!(matches!(
            library.advance_range((8, 3), &roots).unwrap(),
            RangeStep::Idle
        ));
        assert!(!library.range_pending());
        assert_eq!(library.selected(), &(1..=21).collect::<HashSet<_>>());
    }
    #[test]
    fn grouped_shift_range_includes_headers_across_pages_and_overlapping_roots() {
        let (mut library, root, mut roots) = grouped(3, 2);
        roots.push("/music/sub".into());
        let forest = TreeBranch::default();
        library.prepare_directory(forest.clone(), 0, 2, 3);
        library.accept_directory(
            &forest,
            3,
            Page {
                total: 2,
                rows: roots
                    .iter()
                    .map(|path| DirectoryRow::Directory { path: path.clone() })
                    .collect(),
            },
        );
        let crate::gui::library::tree::TreeItem::Entry(entry) = library.tree.item(4).unwrap()
        else {
            panic!("second root header");
        };
        let overlapping = entry.branch.unwrap();
        library.prepare_directory(overlapping.clone(), 0, 2, 4);
        library.accept_directory(
            &overlapping,
            4,
            Page {
                total: 3,
                rows: vec![DirectoryRow::Track(row(1)), DirectoryRow::Track(row(2))],
            },
        );
        assert_eq!(library.tree.row_count(), 8);
        let header = LibraryNode::Directory(Arc::from(roots[0].as_path()));
        library
            .start_selection(&header, 0, false, false, (7, 3), 2)
            .unwrap();
        library
            .start_selection(&LibraryNode::Track(3), 7, true, false, (7, 3), 2)
            .unwrap();
        let mut requests = Vec::new();
        loop {
            match library.advance_range((7, 3), &roots).unwrap() {
                RangeStep::Query {
                    token,
                    slice,
                    query,
                } => {
                    let Query::LibraryRootPage {
                        path,
                        offset,
                        limit,
                        ..
                    } = query
                    else {
                        panic!("grouped selection recursively pages a root");
                    };
                    requests.push((path.clone(), offset, limit));
                    assert_eq!((offset, limit), (2, 1));
                    let id = if path == root.path() { 2 } else { 3 };
                    library
                        .complete_range(
                            token,
                            &slice,
                            QueryResponse {
                                revisions: QueryRevisions {
                                    library: 7,
                                    structure: 3,
                                    playlist: 0,
                                },
                                result: Ok(ViewResponse::LibraryPage(Page {
                                    total: 3,
                                    rows: vec![row(id)],
                                })),
                            },
                            (7, 3),
                        )
                        .unwrap();
                }
                RangeStep::Selected(LibraryNode::Track(3)) => break,
                _ => panic!("range must converge"),
            }
        }
        assert_eq!(requests.len(), 2);
        assert_eq!(library.selected(), &HashSet::from([0, 1, 2, 3]));
        assert_eq!(
            library.selected_nodes(),
            &roots
                .iter()
                .map(|path| LibraryNode::Directory(Arc::from(path.as_path())))
                .collect()
        );
        library.expand_directory(&root, false);
        assert!(library.tree.expanded(&overlapping));
    }

    fn job(start: usize, end: usize, additive: bool) -> RangeJob {
        RangeJob {
            token: 1,
            scope: scope(),
            slices: VecDeque::from([Slice {
                source: Source::Flat(TreeBranch::default()),
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
    fn tree_range_search_and_favorites_share_query_batches() {
        use crate::{
            library::{ScanRecord, ScanResult},
            model::MediaInfo,
            store::Store,
        };
        use std::path::Path;

        let store = Store::open(Path::new(":memory:")).unwrap();
        store
            .apply_scan(&ScanResult {
                roots: vec!["/music".into()],
                records: (0..6)
                    .map(|index| ScanRecord {
                        path: format!("/music/{index}.wav").into(),
                        size: 1,
                        modified_ns: 0,
                        fingerprint: None,
                        cue: None,
                        media: MediaInfo {
                            title: index.to_string(),
                            artist: String::new(),
                            album: String::new(),
                            duration: Some(1.0),
                            bitrate_bps: None,
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
        let favorites: Vec<_> = [1, 3, 5]
            .into_iter()
            .map(|index| {
                store
                    .track_id_for_source(Path::new(&format!("/music/{index}.wav")), None)
                    .unwrap()
                    .unwrap()
            })
            .collect();
        store.set_favorite(&favorites, true).unwrap();
        let mut tree = crate::gui::library::DirectoryTree::default();
        let forest = TreeBranch::default();
        tree.begin(forest.clone(), 0, 1);
        tree.accept(
            &forest,
            1,
            Page {
                total: 1,
                rows: vec![DirectoryRow::Directory {
                    path: "/music".into(),
                }],
            },
        );
        let Some(crate::gui::library::tree::TreeItem::Entry(entry)) = tree.item(0) else {
            panic!("expected library root")
        };
        let branch = entry.branch.unwrap();
        for (query, expected) in [(None, favorites.clone()), (Some("3"), vec![favorites[1]])] {
            let mut job = RangeJob {
                token: 1,
                scope: Scope {
                    library: 7,
                    structure: 3,
                    projection: Projection::Tree {
                        query: query.map(str::to_owned),
                        favorite: Some(true),
                        revision: tree.projection_revision(),
                        total: expected.len(),
                    },
                },
                slices: VecDeque::from([Slice {
                    source: Source::Tree(branch.clone()),
                    offset: 0,
                    len: expected.len(),
                    total: expected.len(),
                    check: None,
                }]),
                selected: HashSet::new(),
                endpoint: LibraryNode::Track(*expected.last().unwrap()),
                endpoint_index: expected.len() - 1,
                additive: false,
                batch_size: 1,
            };
            while let Some(slice) = job.next() {
                let Query::DirectoryPage {
                    path,
                    query,
                    favorite,
                    offset,
                    limit,
                } = job.query(&slice)
                else {
                    panic!("expected directory range query")
                };
                let page = store
                    .directory_page(&path, query.as_deref(), favorite, offset, limit)
                    .unwrap();
                job.accept(
                    &slice,
                    QueryResponse {
                        revisions: QueryRevisions {
                            library: 7,
                            structure: 3,
                            playlist: 0,
                        },
                        result: Ok(ViewResponse::DirectoryPage(page)),
                    },
                )
                .unwrap();
            }
            assert_eq!(
                job.selected,
                expected.into_iter().map(LibraryNode::Track).collect()
            );
        }
    }

    #[test]
    fn tree_search_owner_cancels_old_range_and_uses_same_query() {
        use crate::gui::library::tree::TreeItem;
        let mut library = LibraryBrowser::default();
        library.toggle_favorites();
        library.refresh_search("needle".into());
        let forest = TreeBranch::default();
        library.prepare_directory(forest.clone(), 0, 2, 1);
        library.accept_directory(
            &forest,
            1,
            Page {
                total: 1,
                rows: vec![DirectoryRow::Directory {
                    path: "/music".into(),
                }],
            },
        );
        let Some(TreeItem::Entry(entry)) = library.tree().item(0) else {
            panic!("expected configured root")
        };
        let branch = entry.branch.unwrap();
        library.expand_directory(&branch, true);
        library.prepare_directory(branch.clone(), 0, 2, 2);
        library.accept_directory(
            &branch,
            2,
            Page {
                total: 4,
                rows: vec![DirectoryRow::Track(row(0)), DirectoryRow::Track(row(1))],
            },
        );
        library
            .start_selection(&LibraryNode::Track(0), 1, false, false, (7, 3), 2)
            .unwrap();
        library
            .start_selection(&LibraryNode::Track(3), 4, true, false, (7, 3), 2)
            .unwrap();
        let RangeStep::Query {
            token,
            slice,
            query,
        } = library.advance_range((7, 3), &["/music".into()]).unwrap()
        else {
            panic!("uncached filtered rows require a range query")
        };
        assert!(matches!(
            query,
            Query::DirectoryPage {
                query: Some(query), favorite: Some(true), offset: 2, limit: 2, ..
            } if query == "needle"
        ));
        let scope = Scope::capture(&library, (7, 3));
        let mut other_query = scope.clone();
        let Projection::Tree { query, .. } = &mut other_query.projection else {
            unreachable!()
        };
        *query = Some("other".into());
        assert!(!other_query.matches(&library, (7, 3)));
        library.refresh_search("replacement".into());
        assert!(library.tree_active());
        assert!(!scope.matches(&library, (7, 3)));
        assert!(!library.range_pending());
        library
            .complete_range(
                token,
                &slice,
                QueryResponse {
                    revisions: QueryRevisions {
                        library: 7,
                        structure: 3,
                        playlist: 0,
                    },
                    result: Ok(ViewResponse::DirectoryPage(Page {
                        total: 4,
                        rows: vec![DirectoryRow::Track(row(2)), DirectoryRow::Track(row(3))],
                    })),
                },
                (7, 3),
            )
            .unwrap();
        assert_eq!(library.selected(), &HashSet::from([0]));
        library.refresh_search(String::new());
        assert!(library.tree_active());
        assert_eq!(library.buffer().query, None);
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
        use crate::gui::library::tree::{DirectoryTree, TreeItem};
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
                    query: None,
                    favorite: None,
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
                    matches!(job.query(&slice), Query::DirectoryPage { path, offset, limit, .. }
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
        use crate::gui::library::tree::{DirectoryTree, TreeItem};
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
                    query: None,
                    favorite: None,
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
                query: None,
                favorite: None,
                revision: tree.projection_revision() + 1,
                total: 1,
            },
        };
        assert_ne!(selection.anchor.as_ref().unwrap().scope, changed);
    }
}
