use crate::model::{DirectoryRow, Page};
use gpui::UniformListScrollHandle;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
};

// Payloads and failed-page markers share one LRU bound: sixteen pages or
// visible demand plus unverified ancestry. Expansion identities are independent.
const MAX_CACHED_PAGES: usize = 16;

/// A directory occurrence belongs to one configured root, even when roots overlap.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(in crate::gui) struct TreeBranch {
    root: Option<usize>,
    path: PathBuf,
}

impl TreeBranch {
    pub(in crate::gui) fn path(&self) -> &Path {
        &self.path
    }

    pub(in crate::gui) fn is_forest(&self) -> bool {
        self.root.is_none()
    }

    fn child(&self, index: usize, path: &Path) -> Self {
        Self {
            root: Some(self.root.unwrap_or(index)),
            path: path.to_path_buf(),
        }
    }
}

#[derive(Clone, Debug)]
pub(in crate::gui) struct VisibleDirectoryRow {
    pub(in crate::gui) row: DirectoryRow,
    pub(in crate::gui) depth: usize,
    pub(in crate::gui) expanded: bool,
    pub(in crate::gui) has_children: bool,
    pub(in crate::gui) status: BranchStatus,
    pub(in crate::gui) branch: Option<TreeBranch>,
}

#[derive(Clone, Debug)]
pub(in crate::gui) enum TreeItem {
    Entry(VisibleDirectoryRow),
    Placeholder {
        branch: TreeBranch,
        offset: usize,
        depth: usize,
        status: BranchStatus,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) enum BranchStatus {
    Unloaded,
    Loading,
    Ready,
    Empty,
    Error,
}

#[derive(Default)]
struct Branch {
    // Bidirectional positional anchor: parent.children[index] == this path.
    // Detached/collapsed roots have no parent; descendant choices retain theirs.
    parent: Option<(TreeBranch, usize)>,
    total: Option<usize>,
    expanded: bool,
    pending: Option<(usize, u64)>,
    generation: u64,
    // Only expanded children need positional anchors. They remain renderable
    // when their parent's payload page is evicted.
    children: BTreeMap<usize, TreeBranch>,
    // Retain geometry across revisions, but never expose an unverified
    // directory identity from an old positional anchor.
    stale_children: HashSet<usize>,
}

struct CachedPage {
    rows: Vec<DirectoryRow>,
    failed: bool,
    touched: u64,
}

#[derive(Clone, PartialEq, Eq)]
struct Segment {
    start: usize,
    len: usize,
    branch: TreeBranch,
    offset: usize,
    depth: usize,
}

/// Sparse preorder tree: segments describe logical ranges, never one object
/// per database row. Only the viewport's payload pages occupy the bounded LRU.
pub(in crate::gui) struct DirectoryTree {
    branches: HashMap<TreeBranch, Branch>,
    pages: HashMap<(TreeBranch, usize), CachedPage>,
    segments: Vec<Segment>,
    demanded: HashSet<(TreeBranch, usize)>,
    page_size: usize,
    clock: u64,
    row_count: usize,
    navigation_target: Option<(TreeBranch, usize)>,
    selected_locations: HashMap<(super::LibraryNode, TreeBranch), usize>,
    focused: Option<(super::LibraryNode, TreeBranch)>,
    projection_revision: u64,
    pub(in crate::gui) scroll: UniformListScrollHandle,
}

impl Default for DirectoryTree {
    fn default() -> Self {
        Self {
            branches: HashMap::new(),
            pages: HashMap::new(),
            segments: Vec::new(),
            demanded: HashSet::new(),
            page_size: 256,
            clock: 0,
            row_count: 1,
            navigation_target: None,
            selected_locations: HashMap::new(),
            focused: None,
            projection_revision: 0,
            scroll: UniformListScrollHandle::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::gui) struct TreeRangeSpan {
    pub(in crate::gui) branch: TreeBranch,
    pub(in crate::gui) offset: usize,
    pub(in crate::gui) len: usize,
    pub(in crate::gui) total: usize,
}

impl DirectoryTree {
    pub(in crate::gui) fn configure_page_size(&mut self, size: usize) {
        let size = size.max(1);
        if self.page_size != size {
            self.page_size = size;
            self.invalidate();
        }
    }

    pub(in crate::gui) fn begin(&mut self, key: TreeBranch, offset: usize, generation: u64) {
        if let Some(page) = self.pages.get_mut(&(key.clone(), offset)) {
            page.failed = false;
        }
        let branch = self.branches.entry(key).or_default();
        branch.generation = generation;
        branch.pending = Some((offset, generation));
        self.refresh_visible();
    }

    pub(in crate::gui) fn retry(&mut self, key: &TreeBranch) {
        self.pages
            .retain(|(candidate, _), page| candidate != key || !page.failed);
    }

    pub(in crate::gui) fn generation(&self, key: &TreeBranch) -> Option<u64> {
        self.branches.get(key).map(|branch| branch.generation)
    }

    pub(in crate::gui) fn request_pending(&self, key: &TreeBranch, generation: u64) -> bool {
        self.branches
            .get(key)
            .is_some_and(|branch| branch.pending.is_some_and(|(_, token)| token == generation))
    }

    pub(in crate::gui) fn accept(
        &mut self,
        key: &TreeBranch,
        generation: u64,
        page: Page<DirectoryRow>,
    ) -> bool {
        let Some(branch) = self.branches.get_mut(key) else {
            return false;
        };
        let Some((offset, token)) = branch.pending else {
            return false;
        };
        if token != generation {
            return false;
        }
        branch.pending = None;
        branch.total = Some(page.total);
        let end = offset.saturating_add(self.page_size);
        let mut children = std::mem::take(&mut branch.children);
        children.retain(|index, child| {
            if *index >= page.total || (*index >= offset && *index < end) {
                if let Some(branch) = self.branches.get_mut(child) {
                    branch.parent = None;
                }
                false
            } else {
                true
            }
        });
        let branch = self.branches.get_mut(key).expect("request branch exists");
        branch
            .stale_children
            .retain(|index| children.contains_key(index));
        branch.children = children;
        for (index, row) in page.rows.iter().enumerate() {
            if offset + index >= page.total {
                break;
            }
            if let DirectoryRow::Directory { path } = row {
                let child = key.child(offset + index, path);
                if self.expanded(&child) {
                    self.attach(key, offset + index, &child);
                }
            }
        }
        self.selected_locations.retain(|(node, candidate), index| {
            if candidate != key {
                return true;
            }
            if let Some(position) = page
                .rows
                .iter()
                .position(|row| Self::row_matches(node, row))
            {
                *index = offset + position;
                true
            } else {
                *index < page.total && (*index < offset || *index >= end)
            }
        });
        self.pages
            .retain(|(candidate, start), _| candidate != key || *start < page.total);
        self.clock += 1;
        self.pages.insert(
            (key.clone(), offset),
            CachedPage {
                rows: page.rows,
                failed: false,
                touched: self.clock,
            },
        );
        self.evict();
        self.refresh_visible();
        true
    }

    pub(in crate::gui) fn fail(&mut self, key: &TreeBranch, generation: u64) {
        let Some(branch) = self.branches.get_mut(key) else {
            return;
        };
        let Some((offset, token)) = branch.pending else {
            return;
        };
        if token != generation {
            return;
        }
        branch.pending = None;
        self.clock += 1;
        self.pages
            .entry((key.clone(), offset))
            .and_modify(|page| {
                page.failed = true;
                page.touched = self.clock;
            })
            .or_insert(CachedPage {
                rows: Vec::new(),
                failed: true,
                touched: self.clock,
            });
        self.evict();
        self.refresh_visible();
    }

    fn evict(&mut self) {
        while self.pages.len() > MAX_CACHED_PAGES.max(self.demanded.len()) {
            let key = self
                .pages
                .iter()
                .filter(|(key, _)| !self.demanded.contains(*key))
                .min_by_key(|(_, page)| page.touched)
                .or_else(|| self.pages.iter().min_by_key(|(_, page)| page.touched))
                .map(|(key, _)| key.clone());
            if let Some(key) = key {
                self.pages.remove(&key);
            }
        }
    }

    pub(in crate::gui) fn root_total(&self) -> usize {
        self.branches
            .get(&TreeBranch::default())
            .and_then(|branch| branch.total)
            .unwrap_or(0)
    }

    pub(in crate::gui) fn clear(&mut self) {
        self.branches.clear();
        self.selected_locations.clear();
        self.invalidate();
    }

    /// Release revision-specific payloads and tokens, not logical geometry.
    pub(in crate::gui) fn invalidate(&mut self) {
        self.projection_revision = self.projection_revision.wrapping_add(1);
        self.pages.clear();
        self.demanded.clear();
        self.navigation_target = None;
        self.focused = None;
        for branch in self.branches.values_mut() {
            branch.pending = None;
            branch.stale_children = branch.children.keys().copied().collect();
        }
        self.refresh_visible();
    }

    fn detach(&mut self, child: &TreeBranch) {
        let parent = self
            .branches
            .get_mut(child)
            .and_then(|branch| branch.parent.take());
        if let Some((parent, index)) = parent
            && let Some(branch) = self.branches.get_mut(&parent)
        {
            branch.children.remove(&index);
            branch.stale_children.remove(&index);
        }
    }

    fn attach(&mut self, parent: &TreeBranch, index: usize, child: &TreeBranch) {
        self.detach(child);
        let displaced = self
            .branches
            .get(parent)
            .and_then(|branch| branch.children.get(&index))
            .cloned();
        if let Some(displaced) = displaced {
            self.detach(&displaced);
        }
        let branch = self.branches.entry(parent.clone()).or_default();
        branch.children.insert(index, child.clone());
        branch.stale_children.remove(&index);
        self.branches.entry(child.clone()).or_default().parent = Some((parent.clone(), index));
    }

    pub(in crate::gui) fn expand(&mut self, key: &TreeBranch) {
        self.projection_revision = self.projection_revision.wrapping_add(1);
        self.branches.entry(key.clone()).or_default().expanded = true;
        let location = self.pages.iter().find_map(|((parent, offset), page)| {
            page.rows
                .iter()
                .enumerate()
                .find_map(|(index, row)| match row {
                    DirectoryRow::Directory { path }
                        if parent.child(offset + index, path) == *key =>
                    {
                        Some((parent.clone(), offset + index))
                    }
                    _ => None,
                })
        });
        if let Some((parent, index)) = location {
            self.attach(&parent, index, key);
        }
        self.refresh_visible();
    }

    pub(in crate::gui) fn collapse(&mut self, key: &TreeBranch) {
        self.projection_revision = self.projection_revision.wrapping_add(1);
        if let Some(branch) = self.branches.get_mut(key) {
            branch.expanded = false;
        }
        let in_subtree = |candidate: &TreeBranch| {
            candidate.root == key.root && candidate.path.starts_with(&key.path)
        };
        for (candidate, branch) in &mut self.branches {
            if in_subtree(candidate) {
                branch.pending = None;
            }
        }
        self.detach(key);
        self.demanded
            .retain(|(candidate, _)| !in_subtree(candidate));
        self.pages
            .retain(|(candidate, _), _| !in_subtree(candidate));
        self.navigation_target = None;
        if self
            .focused
            .as_ref()
            .is_some_and(|(_, source)| in_subtree(source))
        {
            self.focused = None;
        }
        self.refresh_visible();
    }

    pub(in crate::gui) fn expanded(&self, key: &TreeBranch) -> bool {
        self.branches.get(key).is_some_and(|branch| branch.expanded)
    }

    pub(in crate::gui) fn status(&self, key: &TreeBranch) -> BranchStatus {
        let Some(branch) = self.branches.get(key) else {
            return BranchStatus::Unloaded;
        };
        if branch.pending.is_some() {
            BranchStatus::Loading
        } else if branch.total == Some(0) {
            BranchStatus::Empty
        } else if self
            .pages
            .iter()
            .any(|((candidate, _), page)| candidate == key && page.failed)
        {
            BranchStatus::Error
        } else if branch.total.is_some() {
            BranchStatus::Ready
        } else {
            BranchStatus::Unloaded
        }
    }

    fn refresh_visible(&mut self) {
        let mut segments = Vec::new();
        let mut start = 0;
        self.flatten(
            &TreeBranch::default(),
            0,
            &mut start,
            &mut HashSet::new(),
            &mut segments,
        );
        if self.segments != segments {
            self.projection_revision = self.projection_revision.wrapping_add(1);
        }
        self.segments = segments;
        self.row_count = start;
    }

    fn flatten(
        &self,
        key: &TreeBranch,
        depth: usize,
        start: &mut usize,
        visited: &mut HashSet<TreeBranch>,
        segments: &mut Vec<Segment>,
    ) {
        if !visited.insert(key.clone()) {
            return;
        }
        let branch = self.branches.get(key);
        let total = branch.and_then(|branch| branch.total).unwrap_or(1);
        let mut offset = 0;
        if let Some(branch) = branch {
            for (index, child) in &branch.children {
                if *index >= total || !self.expanded(child) {
                    continue;
                }
                let len = index + 1 - offset;
                segments.push(Segment {
                    start: *start,
                    len,
                    branch: key.clone(),
                    offset,
                    depth,
                });
                *start += len;
                self.flatten(child, depth + 1, start, visited, segments);
                offset = index + 1;
            }
        }
        if offset < total {
            segments.push(Segment {
                start: *start,
                len: total - offset,
                branch: key.clone(),
                offset,
                depth,
            });
            *start += total - offset;
        }
    }

    pub(in crate::gui) fn projection_revision(&self) -> u64 {
        self.projection_revision
    }
    pub(in crate::gui) fn row_count(&self) -> usize {
        self.row_count
    }

    fn segment(&self, index: usize) -> Option<&Segment> {
        let position = self
            .segments
            .partition_point(|segment| segment.start + segment.len <= index);
        self.segments
            .get(position)
            .filter(|segment| index >= segment.start)
    }

    /// Logical sibling counts survive eviction of viewport payload pages.
    pub(super) fn visit_guides(&self, index: usize, mut visit: impl FnMut(usize, bool, bool)) {
        let Some(segment) = self.segment(index) else {
            return;
        };
        let mut key = &segment.branch;
        let mut offset = segment.offset + index - segment.start;
        for depth in (1..=segment.depth).rev() {
            let Some(branch) = self.branches.get(key) else {
                break;
            };
            let continues = offset + 1 < branch.total.unwrap_or(1);
            visit(depth - 1, continues, depth == segment.depth);
            let Some((parent, position)) = &branch.parent else {
                break;
            };
            key = parent;
            offset = *position;
        }
    }

    fn visit_stale_ancestors(&self, key: &TreeBranch, mut visit: impl FnMut(&TreeBranch, usize)) {
        let mut child = key;
        for _ in 0..self.branches.len() {
            let Some((parent, index)) = self
                .branches
                .get(child)
                .and_then(|branch| branch.parent.as_ref())
            else {
                break;
            };
            let Some(branch) = self.branches.get(parent) else {
                break;
            };
            if branch.stale_children.contains(index) {
                visit(parent, index / self.page_size * self.page_size);
            }
            child = parent;
        }
    }
    #[cfg(test)]
    pub(in crate::gui) fn cached_row(
        &self,
        branch: &TreeBranch,
        index: usize,
    ) -> Option<&DirectoryRow> {
        self.cached_rows(branch, index..index + 1)?.first()
    }

    pub(in crate::gui) fn cached_rows(
        &self,
        branch: &TreeBranch,
        range: Range<usize>,
    ) -> Option<&[DirectoryRow]> {
        self.pages.iter().find_map(|((candidate, start), page)| {
            if candidate == branch && range.start >= *start {
                page.rows.get(range.start - start..range.end - start)
            } else {
                None
            }
        })
    }

    pub(in crate::gui) fn item(&self, index: usize) -> Option<TreeItem> {
        let segment = self.segment(index)?;
        let child_index = segment.offset + index - segment.start;
        let offset = child_index / self.page_size * self.page_size;
        let cached = self.pages.get(&(segment.branch.clone(), offset));
        let row = cached
            .and_then(|page| page.rows.get(child_index - offset))
            .cloned()
            .or_else(|| {
                let branch = self.branches.get(&segment.branch)?;
                if branch.stale_children.contains(&child_index) {
                    return None;
                }
                branch
                    .children
                    .get(&child_index)
                    .map(|key| DirectoryRow::Directory {
                        path: key.path.clone(),
                    })
            });
        if let Some(row) = row {
            let branch = match &row {
                DirectoryRow::Directory { path } => Some(segment.branch.child(child_index, path)),
                DirectoryRow::Track(_) => None,
            };
            let (expanded, status) = branch
                .as_ref()
                .map(|key| (self.expanded(key), self.status(key)))
                .unwrap_or((false, BranchStatus::Ready));
            Some(TreeItem::Entry(VisibleDirectoryRow {
                row,
                depth: segment.depth,
                expanded,
                has_children: status != BranchStatus::Empty,
                status,
                branch,
            }))
        } else {
            let mut ancestor_failure = None;
            self.visit_stale_ancestors(&segment.branch, |parent, offset| {
                if self
                    .pages
                    .get(&(parent.clone(), offset))
                    .is_some_and(|page| page.failed)
                {
                    ancestor_failure = Some((parent.clone(), offset));
                }
            });
            if let Some((branch, offset)) = ancestor_failure {
                return Some(TreeItem::Placeholder {
                    branch,
                    offset,
                    depth: segment.depth,
                    status: BranchStatus::Error,
                });
            }
            let status = if cached.is_some_and(|page| page.failed) {
                BranchStatus::Error
            } else if self
                .branches
                .get(&segment.branch)
                .is_some_and(|branch| branch.pending.is_some())
            {
                BranchStatus::Loading
            } else {
                BranchStatus::Unloaded
            };
            Some(TreeItem::Placeholder {
                branch: segment.branch.clone(),
                offset,
                depth: segment.depth,
                status,
            })
        }
    }

    /// Compact query intervals, independent of the bounded payload cache.
    pub(in crate::gui) fn range_spans(&self, range: Range<usize>) -> Option<Vec<TreeRangeSpan>> {
        if range.start >= range.end || range.end > self.row_count {
            return None;
        }
        let mut spans = Vec::new();
        for segment in &self.segments {
            let start = range.start.max(segment.start);
            let end = range.end.min(segment.start + segment.len);
            if start >= end {
                continue;
            }
            let total = self.branches.get(&segment.branch)?.total?;
            spans.push(TreeRangeSpan {
                branch: segment.branch.clone(),
                offset: segment.offset + start - segment.start,
                len: end - start,
                total,
            });
        }
        Some(spans)
    }

    /// Revalidate retained expanded identities before using their frozen geometry.
    pub(in crate::gui) fn range_checks(
        &self,
        range: Range<usize>,
    ) -> Vec<(TreeRangeSpan, TreeBranch)> {
        let mut checks = BTreeMap::new();
        for segment in &self.segments {
            if range.start >= segment.start + segment.len || range.end <= segment.start {
                continue;
            }
            let mut child = &segment.branch;
            for _ in 0..self.branches.len() {
                let Some((parent, index)) = self
                    .branches
                    .get(child)
                    .and_then(|branch| branch.parent.as_ref())
                else {
                    break;
                };
                if let Some(branch) = self.branches.get(parent)
                    && branch.stale_children.contains(index)
                    && let Some(total) = branch.total
                {
                    checks.insert(
                        (parent.clone(), *index),
                        (
                            TreeRangeSpan {
                                branch: parent.clone(),
                                offset: *index,
                                len: 1,
                                total,
                            },
                            child.clone(),
                        ),
                    );
                }
                child = parent;
            }
        }
        checks.into_values().collect()
    }

    /// Demand viewport pages only, at most one active query per occurrence.
    pub(in crate::gui) fn requests_for_range(
        &mut self,
        range: Range<usize>,
    ) -> Vec<(TreeBranch, usize)> {
        self.clock += 1;
        let mut demanded = HashSet::new();
        for segment in &self.segments {
            let start = range.start.max(segment.start);
            let end = range.end.min(segment.start + segment.len);
            if start >= end {
                continue;
            }
            let first = (segment.offset + start - segment.start) / self.page_size * self.page_size;
            let last = (segment.offset + end - segment.start - 1) / self.page_size * self.page_size;
            for offset in (first..=last).step_by(self.page_size) {
                demanded.insert((segment.branch.clone(), offset));
            }
            self.visit_stale_ancestors(&segment.branch, |parent, offset| {
                demanded.insert((parent.clone(), offset));
            });
        }
        self.demanded = demanded;
        let mut keys = self.demanded.iter().cloned().collect::<Vec<_>>();
        keys.sort();
        let mut requested = HashSet::new();
        let requests = keys
            .into_iter()
            .filter(|(key, offset)| {
                if let Some(page) = self.pages.get_mut(&(key.clone(), *offset)) {
                    page.touched = self.clock;
                    return false;
                }
                !self
                    .branches
                    .get(key)
                    .is_some_and(|branch| branch.pending.is_some())
                    && requested.insert(key.clone())
            })
            .collect();
        self.evict();
        requests
    }

    fn row_matches(node: &super::LibraryNode, row: &DirectoryRow) -> bool {
        match (node, row) {
            (super::LibraryNode::Directory(selected), DirectoryRow::Directory { path }) => {
                selected.as_ref() == path
            }
            (super::LibraryNode::Track(id), DirectoryRow::Track(track)) => *id == track.id,
            _ => false,
        }
    }

    pub(in crate::gui) fn remember_selection(
        &mut self,
        nodes: &HashSet<super::LibraryNode>,
        tracks: &HashSet<i64>,
    ) {
        let selected = |node: &super::LibraryNode| {
            nodes.contains(node)
                || matches!(node, super::LibraryNode::Track(id) if tracks.contains(id))
        };
        self.selected_locations
            .retain(|(node, _), _| selected(node));
        if self
            .focused
            .as_ref()
            .is_some_and(|key| !self.selected_locations.contains_key(key))
        {
            self.focused = None;
        }
        for ((key, offset), page) in &self.pages {
            for (index, row) in page.rows.iter().enumerate() {
                let node = match row {
                    DirectoryRow::Directory { path } => {
                        super::LibraryNode::Directory(std::sync::Arc::from(path.as_path()))
                    }
                    DirectoryRow::Track(track) => super::LibraryNode::Track(track.id),
                };
                if selected(&node) {
                    self.selected_locations
                        .insert((node, key.clone()), offset + index);
                }
            }
        }
        for (parent, branch) in &self.branches {
            for (index, key) in &branch.children {
                let node = super::LibraryNode::Directory(std::sync::Arc::from(key.path()));
                if selected(&node) {
                    self.selected_locations
                        .insert((node, parent.clone()), *index);
                }
            }
        }
    }

    fn visible_index(&self, key: &TreeBranch, index: usize) -> Option<usize> {
        self.segments
            .iter()
            .find(|segment| {
                segment.branch == *key
                    && index >= segment.offset
                    && index < segment.offset + segment.len
            })
            .map(|segment| segment.start + index - segment.offset)
    }

    /// Keep the clicked occurrence distinct from the deduplicated action selection.
    pub(in crate::gui) fn focus_selection(&mut self, node: &super::LibraryNode, index: usize) {
        let Some(segment) = self.segment(index) else {
            self.focused = None;
            return;
        };
        let source = segment.branch.clone();
        let offset = segment.offset + index - segment.start;
        self.selected_locations
            .insert((node.clone(), source.clone()), offset);
        self.focused = Some((node.clone(), source));
    }

    pub(in crate::gui) fn selected_index(&self) -> Option<usize> {
        self.focused
            .as_ref()
            .and_then(|key| {
                self.selected_locations
                    .get(key)
                    .and_then(|index| self.visible_index(&key.1, *index))
            })
            .or_else(|| {
                self.selected_locations
                    .iter()
                    .filter_map(|((_, key), index)| self.visible_index(key, *index))
                    .min()
            })
    }

    pub(in crate::gui) fn parent_index(&self, index: usize) -> Option<usize> {
        let depth = self.segment(index)?.depth;
        self.segments
            .iter()
            .rev()
            .find(|segment| segment.start < index && segment.depth < depth)
            .map(|segment| segment.start + segment.len - 1)
    }

    pub(in crate::gui) fn cancel_navigation(&mut self) {
        self.navigation_target = None;
    }

    pub(in crate::gui) fn defer_selection(&mut self, index: usize) {
        self.navigation_target = self.segment(index).map(|segment| {
            (
                segment.branch.clone(),
                segment.offset + index - segment.start,
            )
        });
    }

    pub(in crate::gui) fn ready_selection(&mut self) -> Option<usize> {
        let (key, child_index) = self.navigation_target.as_ref()?;
        let index = self.visible_index(key, *child_index)?;
        if matches!(self.item(index), Some(TreeItem::Entry(_))) {
            self.navigation_target = None;
            Some(index)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(path: &str) -> TreeBranch {
        if path.is_empty() {
            TreeBranch::default()
        } else {
            TreeBranch {
                root: Some(if path == "/b" || path.starts_with("/b/") {
                    1
                } else {
                    0
                }),
                path: path.into(),
            }
        }
    }

    fn page(paths: &[&str], total: usize) -> Page<DirectoryRow> {
        Page {
            total,
            rows: paths
                .iter()
                .map(|path| DirectoryRow::Directory {
                    path: (*path).into(),
                })
                .collect(),
        }
    }

    fn accept(
        tree: &mut DirectoryTree,
        key: &TreeBranch,
        offset: usize,
        generation: u64,
        paths: &[&str],
        total: usize,
    ) {
        tree.begin(key.clone(), offset, generation);
        assert!(tree.accept(key, generation, page(paths, total)));
    }

    #[test]
    fn guides_preserve_sibling_continuations_without_payloads() {
        let mut tree = DirectoryTree::default();
        accept(&mut tree, &branch(""), 0, 1, &["/a"], 1);
        tree.expand(&branch("/a"));
        accept(&mut tree, &branch("/a"), 0, 2, &["/a/first", "/a/last"], 2);
        tree.expand(&branch("/a/first"));
        accept(&mut tree, &branch("/a/first"), 0, 3, &["/a/first/leaf"], 1);
        tree.expand(&branch("/a/last"));
        accept(&mut tree, &branch("/a/last"), 0, 4, &["/a/last/leaf"], 1);
        tree.pages.clear();
        let guides = |index| {
            let mut result = Vec::new();
            tree.visit_guides(index, |column, continues, current| {
                result.push((column, continues, current));
            });
            result
        };
        assert_eq!(guides(0), vec![]);
        assert_eq!(guides(1), vec![(0, true, true)]);
        assert_eq!(guides(2), vec![(1, false, true), (0, true, false)]);
        assert_eq!(guides(3), vec![(0, false, true)]);
        assert_eq!(guides(4), vec![(1, false, true), (0, false, false)]);
    }

    #[test]
    fn nested_revision_retry_relocation_and_collapse_keep_correct_ancestry() {
        let mut tree = DirectoryTree::default();
        tree.configure_page_size(1);
        accept(&mut tree, &branch(""), 0, 1, &["/a"], 1);
        tree.expand(&branch("/a"));
        accept(&mut tree, &branch("/a"), 0, 2, &["/a/b"], 3);
        tree.expand(&branch("/a/b"));
        accept(&mut tree, &branch("/a/b"), 0, 3, &["/a/b/leaf"], 2);
        tree.invalidate();
        assert_eq!(
            tree.requests_for_range(3..4),
            vec![(branch(""), 0), (branch("/a"), 0), (branch("/a/b"), 1)]
        );
        tree.begin(branch("/a"), 0, 4);
        tree.fail(&branch("/a"), 4);
        assert!(matches!(tree.item(3), Some(TreeItem::Placeholder {
            branch: key, offset: 0, status: BranchStatus::Error, ..
        }) if key == branch("/a")));
        assert!(!tree.requests_for_range(3..4).contains(&(branch("/a"), 0)));
        tree.retry(&branch("/a"));
        assert!(tree.requests_for_range(3..4).contains(&(branch("/a"), 0)));
        accept(&mut tree, &branch("/a"), 1, 5, &["/a/b"], 3);
        tree.invalidate();
        assert_eq!(
            tree.requests_for_range(4..5),
            vec![(branch(""), 0), (branch("/a"), 1), (branch("/a/b"), 1)]
        );
        tree.collapse(&branch("/a/b"));
        assert_eq!(tree.row_count(), 4);
        accept(&mut tree, &branch("/a"), 1, 6, &["/a/b"], 3);
        tree.expand(&branch("/a/b"));
        assert_eq!(tree.row_count(), 6);
        accept(&mut tree, &branch("/a"), 0, 7, &["/a/other"], 1);
        tree.invalidate();
        assert_eq!(tree.row_count(), 2);
        tree.clear();
        assert_eq!(tree.row_count(), 1);
        assert!(!tree.expanded(&branch("/a/b")));
    }

    #[test]
    fn logical_rows_survive_many_batches_and_payloads_are_bounded() {
        let mut tree = DirectoryTree::default();
        tree.configure_page_size(1);
        for offset in 0..40 {
            assert_eq!(
                tree.requests_for_range(offset..offset + 1),
                vec![(branch(""), offset)]
            );
            accept(
                &mut tree,
                &branch(""),
                offset,
                offset as u64 + 1,
                &[&format!("/directory-{offset}")],
                40,
            );
            assert_eq!(tree.row_count(), 40);
            assert!(tree.pages.len() <= MAX_CACHED_PAGES);
        }
        assert!(matches!(tree.item(0), Some(TreeItem::Placeholder { .. })));
        assert_eq!(tree.requests_for_range(0..1), vec![(branch(""), 0)]);
    }

    #[test]
    fn expanded_anchor_survives_eviction_and_collapse_reexpansion() {
        let mut tree = DirectoryTree::default();
        tree.configure_page_size(1);
        accept(&mut tree, &branch(""), 0, 1, &["/a"], 40);
        tree.expand(&branch("/a"));
        accept(&mut tree, &branch("/a"), 0, 2, &["/a/b"], 1);
        tree.expand(&branch("/a/b"));
        for offset in 1..40 {
            accept(
                &mut tree,
                &branch(""),
                offset,
                offset as u64 + 2,
                &[&format!("/other-{offset}")],
                40,
            );
        }
        assert!(matches!(tree.item(0), Some(TreeItem::Entry(_))));
        tree.collapse(&branch("/a"));
        assert_eq!(tree.row_count(), 40);
        accept(&mut tree, &branch(""), 0, 100, &["/a"], 40);
        tree.expand(&branch("/a"));
        accept(&mut tree, &branch("/a"), 0, 101, &["/a/b"], 1);
        assert!(tree.expanded(&branch("/a/b")));
        assert_eq!(tree.row_count(), 42);
    }

    #[test]
    fn stale_failed_and_collapsed_responses_do_not_retry_or_apply() {
        let mut tree = DirectoryTree::default();
        tree.begin(branch(""), 0, 1);
        tree.begin(branch(""), 0, 2);
        assert!(!tree.accept(&branch(""), 1, page(&["/stale"], 1)));
        tree.fail(&branch(""), 2);
        assert!(tree.requests_for_range(0..1).is_empty());
        accept(&mut tree, &branch(""), 0, 3, &["/a"], 1);
        tree.expand(&branch("/a"));
        tree.begin(branch("/a"), 0, 4);
        tree.collapse(&branch("/a"));
        assert!(!tree.accept(&branch("/a"), 4, page(&["/a/stale"], 1)));
        tree.invalidate();
        assert!(!tree.accept(&branch(""), 3, page(&["/stale"], 1)));
        assert!(!tree.expanded(&branch("/a")));
    }

    #[test]
    fn revision_reattaches_expanded_paths_at_their_new_positions() {
        let mut tree = DirectoryTree::default();
        accept(&mut tree, &branch(""), 0, 1, &["/a"], 1);
        tree.expand(&branch("/a"));
        accept(&mut tree, &branch("/a"), 0, 2, &["/a/x", "/a/y"], 2);
        tree.expand(&branch("/a/x"));
        tree.invalidate();
        accept(&mut tree, &branch("/a"), 0, 3, &["/a/y", "/a/x"], 2);
        assert_eq!(tree.row_count(), 4);
        assert!(
            matches!(tree.item(3), Some(TreeItem::Placeholder { branch: key, .. }) if key == branch("/a/x"))
        );
    }

    #[test]
    fn branch_scroll_automatically_crosses_four_batch_boundaries() {
        let mut tree = DirectoryTree::default();
        tree.configure_page_size(2);
        accept(&mut tree, &branch(""), 0, 1, &["/a"], 1);
        tree.expand(&branch("/a"));
        for offset in (0..10).step_by(2) {
            assert_eq!(
                tree.requests_for_range(offset + 1..offset + 2),
                vec![(branch("/a"), offset)]
            );
            accept(
                &mut tree,
                &branch("/a"),
                offset,
                offset as u64 + 2,
                &[
                    &format!("/a/child-{offset}"),
                    &format!("/a/child-{}", offset + 1),
                ],
                10,
            );
            assert_eq!(tree.row_count(), 11);
            assert!(matches!(tree.item(0), Some(TreeItem::Entry(_))));
        }
    }

    #[test]
    fn selected_identity_retains_keyboard_position_after_eviction() {
        let mut tree = DirectoryTree::default();
        tree.configure_page_size(1);
        accept(&mut tree, &branch(""), 0, 1, &["/selected"], 40);
        let nodes = HashSet::from([super::super::LibraryNode::Directory(std::sync::Arc::from(
            Path::new("/selected"),
        ))]);
        tree.remember_selection(&nodes, &HashSet::new());
        for offset in 1..40 {
            accept(
                &mut tree,
                &branch(""),
                offset,
                offset as u64 + 1,
                &[&format!("/other-{offset}")],
                40,
            );
        }
        tree.remember_selection(&nodes, &HashSet::new());
        assert_eq!(tree.selected_index(), Some(0));
        assert!(matches!(tree.item(0), Some(TreeItem::Placeholder { .. })));
        assert!(tree.pages.len() <= MAX_CACHED_PAGES);
    }

    #[test]
    fn measurement_row_reads_do_not_replace_far_viewport_demand() {
        let mut tree = DirectoryTree::default();
        tree.configure_page_size(1);
        for offset in 0..40 {
            assert_eq!(
                tree.requests_for_range(offset..offset + 1),
                vec![(branch(""), offset)]
            );
            accept(
                &mut tree,
                &branch(""),
                offset,
                offset as u64 + 1,
                &[&format!("/row-{offset}")],
                40,
            );
        }
        for _ in 0..4 {
            assert!(matches!(tree.item(0), Some(TreeItem::Placeholder { .. })));
        }
        assert_eq!(tree.demanded, HashSet::from([(branch(""), 39)]));
        assert!(tree.requests_for_range(39..40).is_empty());
        assert!(!tree.pages.contains_key(&(branch(""), 0)));
    }

    #[test]
    fn revision_preserves_deep_geometry_and_selected_position_until_revalidated() {
        let mut tree = DirectoryTree::default();
        accept(&mut tree, &branch(""), 0, 1, &["/a", "/b"], 2);
        tree.expand(&branch("/a"));
        accept(&mut tree, &branch("/a"), 0, 2, &["/a/nested"], 1000);
        tree.expand(&branch("/a/nested"));
        accept(
            &mut tree,
            &branch("/a/nested"),
            0,
            3,
            &["/a/nested/leaf"],
            20,
        );
        let node =
            super::super::LibraryNode::Directory(std::sync::Arc::from(Path::new("/a/selected")));
        tree.selected_locations.insert((node, branch("/a")), 500);
        assert_eq!(tree.row_count(), 1022);
        assert_eq!(tree.selected_index(), Some(521));
        tree.invalidate();
        assert_eq!(tree.row_count(), 1022);
        assert_eq!(tree.selected_index(), Some(521));
        assert_eq!(
            tree.requests_for_range(5..6),
            vec![(branch(""), 0), (branch("/a"), 0), (branch("/a/nested"), 0)]
        );
        accept(&mut tree, &branch(""), 0, 4, &["/a", "/b"], 2);
        assert_eq!(tree.selected_index(), Some(521));
        accept(&mut tree, &branch("/a"), 256, 5, &[], 10);
        assert_eq!(tree.row_count(), 32);
        assert_eq!(tree.selected_index(), None);
    }

    #[test]
    fn refreshed_expanded_identity_moves_without_duplicate_old_anchor() {
        let mut tree = DirectoryTree::default();
        tree.configure_page_size(1);
        accept(&mut tree, &branch(""), 0, 1, &["/a"], 1);
        tree.expand(&branch("/a"));
        accept(&mut tree, &branch("/a"), 0, 2, &["/a/x"], 3);
        tree.expand(&branch("/a/x"));
        accept(&mut tree, &branch("/a/x"), 0, 3, &["/a/x/child"], 100);
        tree.invalidate();
        accept(&mut tree, &branch("/a"), 1, 4, &["/a/x"], 3);
        assert_eq!(tree.row_count(), 104);
        assert_eq!(
            tree.branches[&branch("/a")].children,
            BTreeMap::from([(1, branch("/a/x"))])
        );
        assert!(matches!(tree.item(1), Some(TreeItem::Placeholder { .. })));
        assert!(
            matches!(tree.item(2), Some(TreeItem::Entry(VisibleDirectoryRow {
            row: DirectoryRow::Directory { path }, ..
        })) if path == Path::new("/a/x"))
        );
    }

    #[test]
    fn newer_selection_cancels_deferred_navigation() {
        let mut tree = DirectoryTree::default();
        accept(&mut tree, &branch(""), 0, 1, &["/a"], 1);
        tree.expand(&branch("/a"));
        tree.defer_selection(1);
        tree.cancel_navigation();
        accept(&mut tree, &branch("/a"), 0, 2, &["/a/target"], 1);
        assert_eq!(tree.ready_selection(), None);
    }

    #[test]
    fn deferred_navigation_tracks_branch_offset_when_sibling_load_changes_preorder() {
        let mut tree = DirectoryTree::default();
        accept(&mut tree, &branch(""), 0, 1, &["/a", "/b"], 2);
        tree.expand(&branch("/a"));
        tree.expand(&branch("/b"));
        tree.defer_selection(3);
        accept(
            &mut tree,
            &branch("/a"),
            0,
            2,
            &["/a/0", "/a/1", "/a/2"],
            100,
        );
        assert!(matches!(tree.item(3), Some(TreeItem::Entry(_))));
        assert_eq!(tree.ready_selection(), None);
        accept(&mut tree, &branch("/b"), 0, 3, &["/b/target"], 1);
        assert_eq!(tree.ready_selection(), Some(102));
    }

    #[test]
    fn overlapping_roots_have_independent_expansion_requests_and_collapse() {
        let mut tree = DirectoryTree::default();
        accept(
            &mut tree,
            &branch(""),
            0,
            1,
            &["/music", "/music/sub", "/other/sub", "/empty"],
            4,
        );
        let root = TreeBranch::default().child(0, Path::new("/music"));
        let overlap = TreeBranch::default().child(1, Path::new("/music/sub"));
        let nested = root.child(0, Path::new("/music/sub"));
        let empty = TreeBranch::default().child(3, Path::new("/empty"));
        tree.expand(&root);
        accept(&mut tree, &root, 0, 2, &["/music/sub"], 1);
        tree.expand(&nested);
        tree.expand(&overlap);
        tree.expand(&empty);
        tree.begin(nested.clone(), 0, 3);
        tree.begin(overlap.clone(), 0, 4);
        assert!(tree.request_pending(&nested, 3));
        assert!(tree.request_pending(&overlap, 4));
        assert!(tree.accept(&nested, 3, page(&["/music/sub/first"], 1)));
        assert!(tree.accept(&overlap, 4, page(&["/music/sub/second"], 1)));
        accept(&mut tree, &empty, 0, 5, &[], 0);
        assert_eq!(tree.root_total(), 4);
        assert_eq!(tree.row_count(), 7);
        tree.collapse(&root);
        assert_eq!(tree.row_count(), 5);
        assert!(tree.expanded(&overlap));
        assert!(
            matches!(tree.item(2), Some(TreeItem::Entry(VisibleDirectoryRow {
            row: DirectoryRow::Directory { path }, depth: 1, ..
        })) if path == Path::new("/music/sub/second"))
        );
        assert!(matches!(
            tree.item(4),
            Some(TreeItem::Entry(VisibleDirectoryRow {
                status: BranchStatus::Empty,
                ..
            }))
        ));
        tree.invalidate();
        assert!(!tree.accept(&nested, 3, page(&["/stale"], 1)));
        assert!(!tree.accept(&overlap, 4, page(&["/stale"], 1)));
        tree.begin(overlap.clone(), 0, 6);
        tree.fail(&overlap, 6);
        tree.retry(&overlap);
        assert!(tree.requests_for_range(2..3).contains(&(overlap, 0)));
    }

    #[test]
    fn sparse_preorder_range_plan_crosses_evicted_pages_and_expanded_branches() {
        let mut tree = DirectoryTree::default();
        tree.configure_page_size(1);
        accept(&mut tree, &branch(""), 0, 1, &["/a"], 1);
        tree.expand(&branch("/a"));
        accept(&mut tree, &branch("/a"), 0, 2, &["/a/nested"], 40);
        tree.expand(&branch("/a/nested"));
        accept(
            &mut tree,
            &branch("/a/nested"),
            0,
            3,
            &["/a/nested/entry"],
            20,
        );
        for offset in 1..40 {
            accept(
                &mut tree,
                &branch("/a"),
                offset,
                offset as u64 + 4,
                &[&format!("/a/entry-{offset}")],
                40,
            );
        }
        assert!(tree.pages.len() <= MAX_CACHED_PAGES);
        let spans = tree.range_spans(0..61).unwrap();
        assert_eq!(
            spans,
            vec![
                TreeRangeSpan {
                    branch: branch(""),
                    offset: 0,
                    len: 1,
                    total: 1
                },
                TreeRangeSpan {
                    branch: branch("/a"),
                    offset: 0,
                    len: 1,
                    total: 40
                },
                TreeRangeSpan {
                    branch: branch("/a/nested"),
                    offset: 0,
                    len: 20,
                    total: 20
                },
                TreeRangeSpan {
                    branch: branch("/a"),
                    offset: 1,
                    len: 39,
                    total: 40
                },
            ]
        );
        assert!(tree.range_checks(0..61).is_empty());
        let revision = tree.projection_revision();
        tree.invalidate();
        assert_ne!(tree.projection_revision(), revision);
        assert_eq!(tree.range_spans(0..61).unwrap(), spans);
        assert_eq!(tree.range_checks(0..61).len(), 2);
        tree.collapse(&branch("/a/nested"));
        assert!(tree.range_spans(0..61).is_none());
    }

    #[test]
    fn focused_overlapping_occurrence_drives_keyboard_parent_and_survives_other_root_collapse() {
        let mut tree = DirectoryTree::default();
        let forest = TreeBranch::default();
        accept(&mut tree, &forest, 0, 1, &["/music", "/music/sub"], 2);
        let root = forest.child(0, Path::new("/music"));
        let overlap = forest.child(1, Path::new("/music/sub"));
        let nested = root.child(0, Path::new("/music/sub"));
        tree.expand(&root);
        accept(&mut tree, &root, 0, 2, &["/music/sub"], 1);
        tree.expand(&nested);
        accept(&mut tree, &nested, 0, 3, &["/music/sub/leaf"], 1);
        tree.expand(&overlap);
        accept(&mut tree, &overlap, 0, 4, &["/music/sub/leaf"], 1);
        let directory =
            super::super::LibraryNode::Directory(std::sync::Arc::from(Path::new("/music/sub")));
        let leaf = super::super::LibraryNode::Directory(std::sync::Arc::from(Path::new(
            "/music/sub/leaf",
        )));
        let nodes = HashSet::from([directory.clone(), leaf.clone()]);
        tree.remember_selection(&nodes, &HashSet::new());
        tree.focus_selection(&directory, 3);
        assert_eq!(tree.selected_index(), Some(3));
        assert_eq!(tree.parent_index(tree.selected_index().unwrap()), None);
        tree.focus_selection(&leaf, 4);
        assert_eq!(tree.selected_index(), Some(4));
        assert_eq!(tree.parent_index(tree.selected_index().unwrap()), Some(3));
        tree.collapse(&root);
        assert_eq!(tree.selected_index(), Some(2));
        assert_eq!(tree.parent_index(tree.selected_index().unwrap()), Some(1));
        assert!(tree.focused.is_some());
        tree.invalidate();
        assert!(tree.focused.is_none());
        accept(&mut tree, &forest, 0, 5, &["/music", "/music/sub"], 2);
        accept(
            &mut tree,
            &overlap,
            0,
            6,
            &["/music/sub/new", "/music/sub/leaf"],
            2,
        );
        tree.focus_selection(&leaf, 3);
        assert_eq!(tree.selected_index(), Some(3));
        assert_eq!(tree.parent_index(tree.selected_index().unwrap()), Some(1));
        tree.collapse(&overlap);
        assert!(tree.focused.is_none());
        assert_eq!(tree.selected_index(), Some(1));
        tree.remember_selection(&HashSet::new(), &HashSet::new());
        assert_eq!(tree.selected_index(), None);
    }
}
