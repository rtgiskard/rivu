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

#[derive(Clone, Debug)]
pub(in crate::gui) struct VisibleDirectoryRow {
    pub(in crate::gui) row: DirectoryRow,
    pub(in crate::gui) depth: usize,
    pub(in crate::gui) expanded: bool,
    pub(in crate::gui) has_children: bool,
    pub(in crate::gui) status: BranchStatus,
}

#[derive(Clone, Debug)]
pub(in crate::gui) enum TreeItem {
    Entry(VisibleDirectoryRow),
    Placeholder {
        path: PathBuf,
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
    parent: Option<(PathBuf, usize)>,
    total: Option<usize>,
    expanded: bool,
    pending: Option<(usize, u64)>,
    generation: u64,
    // Only expanded children need positional anchors. They remain renderable
    // when their parent's payload page is evicted.
    children: BTreeMap<usize, PathBuf>,
    // Retain geometry across revisions, but never expose an unverified
    // directory identity from an old positional anchor.
    stale_children: HashSet<usize>,
}

struct CachedPage {
    rows: Vec<DirectoryRow>,
    failed: bool,
    touched: u64,
}

struct Segment {
    start: usize,
    len: usize,
    path: PathBuf,
    offset: usize,
    depth: usize,
}

/// Sparse preorder tree: segments describe logical ranges, never one object
/// per database row. Only the viewport's payload pages occupy the bounded LRU.
pub(in crate::gui) struct DirectoryTree {
    branches: HashMap<PathBuf, Branch>,
    pages: HashMap<(PathBuf, usize), CachedPage>,
    segments: Vec<Segment>,
    demanded: HashSet<(PathBuf, usize)>,
    page_size: usize,
    clock: u64,
    row_count: usize,
    navigation_target: Option<(PathBuf, usize)>,
    selected_locations: HashMap<super::LibraryNode, (PathBuf, usize)>,
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
            scroll: UniformListScrollHandle::new(),
        }
    }
}

impl DirectoryTree {
    pub(in crate::gui) fn configure_page_size(&mut self, size: usize) {
        let size = size.max(1);
        if self.page_size != size {
            self.page_size = size;
            self.invalidate();
        }
    }

    pub(in crate::gui) fn begin(&mut self, path: PathBuf, offset: usize, generation: u64) {
        if let Some(page) = self.pages.get_mut(&(path.clone(), offset)) {
            page.failed = false;
        }
        let branch = self.branches.entry(path).or_default();
        branch.generation = generation;
        branch.pending = Some((offset, generation));
        self.refresh_visible();
    }

    pub(in crate::gui) fn retry(&mut self, path: &Path) {
        self.pages
            .retain(|(candidate, _), page| candidate != path || !page.failed);
    }

    pub(in crate::gui) fn generation(&self, path: &Path) -> Option<u64> {
        self.branches.get(path).map(|branch| branch.generation)
    }

    pub(in crate::gui) fn request_pending(&self, path: &Path, generation: u64) -> bool {
        self.branches
            .get(path)
            .is_some_and(|branch| branch.pending.is_some_and(|(_, token)| token == generation))
    }

    pub(in crate::gui) fn accept(
        &mut self,
        path: &Path,
        generation: u64,
        page: Page<DirectoryRow>,
    ) -> bool {
        let Some(branch) = self.branches.get_mut(path) else {
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
        // Move the map out temporarily so reverse anchors can be cleared without
        // cloning removed paths or allocating an intermediate removal list.
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
        let branch = self.branches.get_mut(path).expect("request branch exists");
        branch
            .stale_children
            .retain(|index| children.contains_key(index));
        branch.children = children;
        for (index, row) in page.rows.iter().enumerate() {
            if offset + index >= page.total {
                break;
            }
            if let DirectoryRow::Directory { path: child } = row
                && self.branches.get(child).is_some_and(|child| child.expanded)
            {
                self.attach(path, offset + index, child);
            }
        }
        self.selected_locations.retain(|node, (candidate, index)| {
            if candidate.as_path() != path {
                return true;
            }
            if let Some(position) = page.rows.iter().position(|row| match (node, row) {
                (super::LibraryNode::Directory(selected), DirectoryRow::Directory { path }) => {
                    selected.as_ref() == path.as_path()
                }
                (super::LibraryNode::Track(selected), DirectoryRow::Track(track)) => {
                    *selected == track.id
                }
                _ => false,
            }) {
                *index = offset + position;
                true
            } else {
                *index < page.total && (*index < offset || *index >= end)
            }
        });
        self.pages
            .retain(|(candidate, start), _| candidate != path || *start < page.total);
        self.clock += 1;
        self.pages.insert(
            (path.to_path_buf(), offset),
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

    pub(in crate::gui) fn fail(&mut self, path: &Path, generation: u64) {
        let Some(branch) = self.branches.get_mut(path) else {
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
            .entry((path.to_path_buf(), offset))
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
            .get(Path::new(""))
            .and_then(|branch| branch.total)
            .unwrap_or(0)
    }

    pub(in crate::gui) fn clear(&mut self) {
        self.branches.clear();
        self.selected_locations.clear();
        self.invalidate();
    }

    /// Release revision-specific payloads and tokens, not logical geometry.
    /// Fresh pages revalidate old anchors incrementally without scroll clamping.
    pub(in crate::gui) fn invalidate(&mut self) {
        self.pages.clear();
        self.demanded.clear();
        self.navigation_target = None;
        for branch in self.branches.values_mut() {
            branch.pending = None;
            branch.stale_children = branch.children.keys().copied().collect();
        }
        self.refresh_visible();
    }

    // Mutations maintain both sides together, including replacement and relocation.
    fn detach(&mut self, child: &Path) {
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

    fn attach(&mut self, parent: &Path, index: usize, child: &Path) {
        self.detach(child);
        let displaced = self
            .branches
            .get(parent)
            .and_then(|branch| branch.children.get(&index))
            .cloned();
        if let Some(displaced) = displaced {
            self.detach(&displaced);
        }
        let branch = self.branches.entry(parent.to_path_buf()).or_default();
        branch.children.insert(index, child.to_path_buf());
        branch.stale_children.remove(&index);
        self.branches.entry(child.to_path_buf()).or_default().parent =
            Some((parent.to_path_buf(), index));
    }

    pub(in crate::gui) fn expand(&mut self, path: &Path) {
        self.branches
            .entry(path.to_path_buf())
            .or_default()
            .expanded = true;
        // Locate the row in the bounded payload cache, not by scanning totals.
        let location = self.pages.iter().find_map(|((parent, offset), page)| {
            page.rows
                .iter()
                .position(|row| {
                    matches!(row,
                        DirectoryRow::Directory { path: child } if child == path
                    )
                })
                .map(|index| (parent.clone(), offset + index))
        });
        if let Some((parent, index)) = location {
            self.attach(&parent, index, path);
        }
        self.refresh_visible();
    }

    pub(in crate::gui) fn collapse(&mut self, path: &Path) {
        if let Some(branch) = self.branches.get_mut(path) {
            branch.expanded = false;
        }
        // Reject pending subtree responses and release their payloads, without
        // forgetting the user's descendant expansion choices.
        for (candidate, branch) in &mut self.branches {
            if candidate == path || candidate.starts_with(path) {
                branch.pending = None;
            }
        }
        self.detach(path);
        self.demanded
            .retain(|(candidate, _)| candidate != path && !candidate.starts_with(path));
        self.pages
            .retain(|(candidate, _), _| candidate != path && !candidate.starts_with(path));
        self.navigation_target = None;
        self.refresh_visible();
    }

    pub(in crate::gui) fn expanded(&self, path: &Path) -> bool {
        self.branches
            .get(path)
            .is_some_and(|branch| branch.expanded)
    }

    pub(in crate::gui) fn status(&self, path: &Path) -> BranchStatus {
        let Some(branch) = self.branches.get(path) else {
            return BranchStatus::Unloaded;
        };
        if branch.pending.is_some() {
            BranchStatus::Loading
        } else if branch.total == Some(0) {
            BranchStatus::Empty
        } else if self
            .pages
            .iter()
            .any(|((candidate, _), page)| candidate == path && page.failed)
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
            Path::new(""),
            0,
            &mut start,
            &mut HashSet::new(),
            &mut segments,
        );
        self.segments = segments;
        self.row_count = start;
    }

    fn flatten(
        &self,
        path: &Path,
        depth: usize,
        start: &mut usize,
        visited: &mut HashSet<PathBuf>,
        segments: &mut Vec<Segment>,
    ) {
        if !visited.insert(path.to_path_buf()) {
            return;
        }
        let branch = self.branches.get(path);
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
                    path: path.to_path_buf(),
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
                path: path.to_path_buf(),
                offset,
                depth,
            });
            *start += total - offset;
        }
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

    fn visit_stale_ancestors(&self, path: &Path, mut visit: impl FnMut(&Path, usize)) {
        let mut child = path;
        // Directory ancestry is acyclic; the bound also protects malformed
        // backend paths without allocating a visited set for every row.
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

    pub(in crate::gui) fn item(&self, index: usize) -> Option<TreeItem> {
        let segment = self.segment(index)?;
        let child_index = segment.offset + index - segment.start;
        let offset = child_index / self.page_size * self.page_size;
        let cached = self.pages.get(&(segment.path.clone(), offset));
        let row = cached
            .and_then(|page| page.rows.get(child_index - offset))
            .cloned()
            .or_else(|| {
                let branch = self.branches.get(&segment.path)?;
                if branch.stale_children.contains(&child_index) {
                    return None;
                }
                branch
                    .children
                    .get(&child_index)
                    .map(|path| DirectoryRow::Directory { path: path.clone() })
            });
        if let Some(row) = row {
            let (expanded, status) = match &row {
                DirectoryRow::Directory { path } => (self.expanded(path), self.status(path)),
                DirectoryRow::Track(_) => (false, BranchStatus::Ready),
            };
            Some(TreeItem::Entry(VisibleDirectoryRow {
                row,
                depth: segment.depth,
                expanded,
                has_children: status != BranchStatus::Empty,
                status,
            }))
        } else {
            let mut ancestor_failure = None;
            self.visit_stale_ancestors(&segment.path, |parent, offset| {
                if self
                    .pages
                    .get(&(parent.to_path_buf(), offset))
                    .is_some_and(|page| page.failed)
                {
                    ancestor_failure = Some((parent.to_path_buf(), offset));
                }
            });
            if let Some((path, offset)) = ancestor_failure {
                return Some(TreeItem::Placeholder {
                    path,
                    offset,
                    depth: segment.depth,
                    status: BranchStatus::Error,
                });
            }
            let status = if cached.is_some_and(|page| page.failed) {
                BranchStatus::Error
            } else if self
                .branches
                .get(&segment.path)
                .is_some_and(|branch| branch.pending.is_some())
            {
                BranchStatus::Loading
            } else {
                BranchStatus::Unloaded
            };
            Some(TreeItem::Placeholder {
                path: segment.path.clone(),
                offset,
                depth: segment.depth,
                status,
            })
        }
    }

    /// Demand only intersecting pages. At most one query per branch is active;
    /// completion triggers the next visible miss, never a fetch-all traversal.
    pub(in crate::gui) fn requests_for_range(
        &mut self,
        range: Range<usize>,
    ) -> Vec<(PathBuf, usize)> {
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
                demanded.insert((segment.path.clone(), offset));
            }
            self.visit_stale_ancestors(&segment.path, |parent, offset| {
                demanded.insert((parent.to_path_buf(), offset));
            });
        }
        self.demanded = demanded;
        let mut keys = self.demanded.iter().cloned().collect::<Vec<_>>();
        keys.sort();
        let mut requested = HashSet::new();
        let requests = keys
            .into_iter()
            .filter(|(path, offset)| {
                if let Some(page) = self.pages.get_mut(&(path.clone(), *offset)) {
                    page.touched = self.clock;
                    return false;
                }
                !self
                    .branches
                    .get(path)
                    .is_some_and(|branch| branch.pending.is_some())
                    && requested.insert(path.clone())
            })
            .collect();
        self.evict();
        requests
    }

    /// Keep only compact positions for the user's selection, so keyboard
    /// navigation still has an anchor after its payload page leaves the LRU.
    pub(in crate::gui) fn remember_selection(
        &mut self,
        nodes: &HashSet<super::LibraryNode>,
        tracks: &HashSet<i64>,
    ) {
        let selected = |node: &super::LibraryNode| {
            nodes.contains(node)
                || matches!(node, super::LibraryNode::Track(id) if tracks.contains(id))
        };
        self.selected_locations.retain(|node, _| selected(node));
        for ((path, offset), page) in &self.pages {
            for (index, row) in page.rows.iter().enumerate() {
                let node = match row {
                    DirectoryRow::Directory { path } => nodes.iter().find(|node|
                        matches!(node, super::LibraryNode::Directory(candidate) if candidate.as_ref() == path)
                    ).cloned(),
                    DirectoryRow::Track(track) if tracks.contains(&track.id)
                        || nodes.contains(&super::LibraryNode::Track(track.id)) =>
                            Some(super::LibraryNode::Track(track.id)),
                    DirectoryRow::Track(_) => None,
                };
                if let Some(node) = node {
                    self.selected_locations
                        .insert(node, (path.clone(), offset + index));
                }
            }
        }
        for (parent, branch) in &self.branches {
            for (index, path) in &branch.children {
                if let Some(node) = nodes.iter().find(|node|
                    matches!(node, super::LibraryNode::Directory(candidate) if candidate.as_ref() == path)
                ) {
                    self.selected_locations.insert(node.clone(), (parent.clone(), *index));
                }
            }
        }
    }

    pub(in crate::gui) fn selected_index(&self) -> Option<usize> {
        self.selected_locations
            .values()
            .filter_map(|(path, index)| {
                self.segments
                    .iter()
                    .find(|segment| {
                        segment.path == *path
                            && *index >= segment.offset
                            && *index < segment.offset + segment.len
                    })
                    .map(|segment| segment.start + index - segment.offset)
            })
            .min()
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
        self.navigation_target = self
            .segment(index)
            .map(|segment| (segment.path.clone(), segment.offset + index - segment.start));
    }

    pub(in crate::gui) fn ready_selection(&mut self) -> Option<usize> {
        let (path, child_index) = self.navigation_target.as_ref()?;
        let index = self
            .segments
            .iter()
            .find(|segment| {
                segment.path == *path
                    && *child_index >= segment.offset
                    && *child_index < segment.offset + segment.len
            })
            .map(|segment| segment.start + child_index - segment.offset)?;
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

    fn page(paths: &[&str], total: usize) -> Page<DirectoryRow> {
        Page {
            total,
            rows: paths
                .iter()
                .map(|path| DirectoryRow::Directory {
                    path: PathBuf::from(path),
                })
                .collect(),
        }
    }

    #[test]
    fn nested_revision_retry_relocation_and_collapse_keep_correct_ancestry() {
        let mut tree = DirectoryTree::default();
        tree.configure_page_size(1);
        tree.begin(PathBuf::new(), 0, 1);
        assert!(tree.accept(Path::new(""), 1, page(&["/a"], 3)));
        tree.expand(Path::new("/a"));
        tree.begin(PathBuf::from("/a"), 0, 2);
        assert!(tree.accept(Path::new("/a"), 2, page(&["/a/b"], 1)));
        tree.expand(Path::new("/a/b"));
        tree.begin(PathBuf::from("/a/b"), 0, 3);
        assert!(tree.accept(Path::new("/a/b"), 3, page(&["/a/b/leaf"], 2)));
        tree.invalidate();
        assert_eq!(
            tree.requests_for_range(3..4),
            vec![
                (PathBuf::new(), 0),
                (PathBuf::from("/a"), 0),
                (PathBuf::from("/a/b"), 1),
            ]
        );
        tree.begin(PathBuf::new(), 0, 4);
        tree.fail(Path::new(""), 4);
        assert!(matches!(tree.item(3), Some(TreeItem::Placeholder {
            path, offset: 0, status: BranchStatus::Error, ..
        }) if path == Path::new("")));
        assert!(!tree.requests_for_range(3..4).contains(&(PathBuf::new(), 0)));
        tree.retry(Path::new(""));
        assert!(tree.requests_for_range(3..4).contains(&(PathBuf::new(), 0)));
        tree.begin(PathBuf::new(), 1, 5);
        assert!(tree.accept(Path::new(""), 5, page(&["/a"], 3)));
        tree.invalidate();
        assert_eq!(
            tree.requests_for_range(4..5),
            vec![
                (PathBuf::new(), 1),
                (PathBuf::from("/a"), 0),
                (PathBuf::from("/a/b"), 1),
            ]
        );
        tree.collapse(Path::new("/a"));
        assert_eq!(tree.row_count(), 3);
        assert_eq!(tree.requests_for_range(1..2), vec![(PathBuf::new(), 1)]);
        tree.begin(PathBuf::new(), 1, 6);
        assert!(tree.accept(Path::new(""), 6, page(&["/a"], 3)));
        tree.expand(Path::new("/a"));
        assert!(tree.expanded(Path::new("/a/b")));
        assert_eq!(tree.row_count(), 6);
        assert_eq!(
            tree.requests_for_range(4..5),
            vec![(PathBuf::from("/a"), 0), (PathBuf::from("/a/b"), 1),]
        );
        tree.begin(PathBuf::new(), 0, 7);
        assert!(tree.accept(Path::new(""), 7, page(&["/other"], 1)));
        tree.invalidate();
        assert_eq!(tree.row_count(), 1);
        assert_eq!(tree.requests_for_range(0..1), vec![(PathBuf::new(), 0)]);
        tree.clear();
        assert_eq!(tree.row_count(), 1);
        assert!(!tree.expanded(Path::new("/a/b")));
    }

    #[test]
    fn logical_rows_survive_many_batches_and_payloads_are_bounded() {
        let mut tree = DirectoryTree::default();
        tree.configure_page_size(1);
        for offset in 0..40 {
            assert_eq!(
                tree.requests_for_range(offset..offset + 1),
                vec![(PathBuf::new(), offset)]
            );
            tree.begin(PathBuf::new(), offset, offset as u64 + 1);
            assert!(tree.accept(
                Path::new(""),
                offset as u64 + 1,
                page(&[&format!("/directory-{offset}")], 40)
            ));
            assert_eq!(tree.row_count(), 40);
            assert!(tree.pages.len() <= MAX_CACHED_PAGES);
        }
        assert!(matches!(tree.item(0), Some(TreeItem::Placeholder { .. })));
        assert_eq!(tree.requests_for_range(0..1), vec![(PathBuf::new(), 0)]);
    }

    #[test]
    fn expanded_anchor_survives_eviction_and_collapse_reexpansion() {
        let mut tree = DirectoryTree::default();
        tree.configure_page_size(1);
        tree.begin(PathBuf::new(), 0, 1);
        assert!(tree.accept(Path::new(""), 1, page(&["/a"], 40)));
        tree.expand(Path::new("/a"));
        tree.begin(PathBuf::from("/a"), 0, 2);
        assert!(tree.accept(Path::new("/a"), 2, page(&["/a/b"], 1)));
        tree.expand(Path::new("/a/b"));
        for offset in 1..40 {
            tree.begin(PathBuf::new(), offset, offset as u64 + 2);
            assert!(tree.accept(
                Path::new(""),
                offset as u64 + 2,
                page(&[&format!("/other-{offset}")], 40)
            ));
        }
        assert!(matches!(tree.item(0), Some(TreeItem::Entry(_))));
        tree.collapse(Path::new("/a"));
        assert_eq!(tree.row_count(), 40);
        tree.begin(PathBuf::new(), 0, 100);
        assert!(tree.accept(Path::new(""), 100, page(&["/a"], 40)));
        tree.expand(Path::new("/a"));
        tree.begin(PathBuf::from("/a"), 0, 101);
        assert!(tree.accept(Path::new("/a"), 101, page(&["/a/b"], 1)));
        assert!(tree.expanded(Path::new("/a/b")));
        assert_eq!(tree.row_count(), 42);
    }

    #[test]
    fn stale_failed_and_collapsed_responses_do_not_retry_or_apply() {
        let mut tree = DirectoryTree::default();
        tree.begin(PathBuf::new(), 0, 1);
        tree.begin(PathBuf::new(), 0, 2);
        assert!(!tree.accept(Path::new(""), 1, page(&["/stale"], 1)));
        tree.fail(Path::new(""), 2);
        assert!(tree.requests_for_range(0..1).is_empty());
        tree.begin(PathBuf::new(), 0, 3);
        assert!(tree.accept(Path::new(""), 3, page(&["/a"], 1)));
        tree.expand(Path::new("/a"));
        tree.begin(PathBuf::from("/a"), 0, 4);
        tree.collapse(Path::new("/a"));
        assert!(!tree.accept(Path::new("/a"), 4, page(&["/a/stale"], 1)));
        tree.invalidate();
        assert!(!tree.accept(Path::new(""), 3, page(&["/stale"], 1)));
        assert!(!tree.expanded(Path::new("/a")));
    }

    #[test]
    fn revision_reattaches_expanded_paths_at_their_new_positions() {
        let mut tree = DirectoryTree::default();
        tree.begin(PathBuf::new(), 0, 1);
        assert!(tree.accept(Path::new(""), 1, page(&["/a", "/b"], 2)));
        tree.expand(Path::new("/a"));
        tree.invalidate();
        tree.begin(PathBuf::new(), 0, 2);
        assert!(tree.accept(Path::new(""), 2, page(&["/b", "/a"], 2)));
        assert_eq!(tree.row_count(), 3);
        assert!(
            matches!(tree.item(2), Some(TreeItem::Placeholder { path, .. }) if path == Path::new("/a"))
        );
    }

    #[test]
    fn branch_scroll_automatically_crosses_four_batch_boundaries() {
        let mut tree = DirectoryTree::default();
        tree.configure_page_size(2);
        tree.begin(PathBuf::new(), 0, 1);
        assert!(tree.accept(Path::new(""), 1, page(&["/a"], 1)));
        tree.expand(Path::new("/a"));
        for offset in (0..10).step_by(2) {
            assert_eq!(
                tree.requests_for_range(offset + 1..offset + 2),
                vec![(PathBuf::from("/a"), offset)]
            );
            let generation = offset as u64 + 2;
            tree.begin(PathBuf::from("/a"), offset, generation);
            assert!(tree.accept(
                Path::new("/a"),
                generation,
                page(
                    &[
                        &format!("/a/child-{offset}"),
                        &format!("/a/child-{}", offset + 1),
                    ],
                    10
                )
            ));
            assert_eq!(tree.row_count(), 11);
            assert!(matches!(tree.item(0), Some(TreeItem::Entry(_))));
        }
    }

    #[test]
    fn selected_identity_retains_keyboard_position_after_eviction() {
        let mut tree = DirectoryTree::default();
        tree.configure_page_size(1);
        tree.begin(PathBuf::new(), 0, 1);
        assert!(tree.accept(Path::new(""), 1, page(&["/selected"], 40)));
        let nodes = HashSet::from([super::super::LibraryNode::Directory(std::sync::Arc::from(
            Path::new("/selected"),
        ))]);
        tree.remember_selection(&nodes, &HashSet::new());
        for offset in 1..40 {
            tree.begin(PathBuf::new(), offset, offset as u64 + 1);
            assert!(tree.accept(
                Path::new(""),
                offset as u64 + 1,
                page(&[&format!("/other-{offset}")], 40)
            ));
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
            let requests = tree.requests_for_range(offset..offset + 1);
            assert_eq!(requests, vec![(PathBuf::new(), offset)]);
            tree.begin(PathBuf::new(), offset, offset as u64 + 1);
            assert!(tree.accept(
                Path::new(""),
                offset as u64 + 1,
                page(&[&format!("/row-{offset}")], 40)
            ));
        }
        for _ in 0..4 {
            assert!(matches!(tree.item(0), Some(TreeItem::Placeholder { .. })));
        }
        assert_eq!(tree.demanded, HashSet::from([(PathBuf::new(), 39)]));
        assert!(tree.requests_for_range(39..40).is_empty());
        assert!(!tree.pages.contains_key(&(PathBuf::new(), 0)));
    }

    #[test]
    fn revision_preserves_deep_geometry_and_selected_position_until_revalidated() {
        let mut tree = DirectoryTree::default();
        tree.begin(PathBuf::new(), 0, 1);
        assert!(tree.accept(Path::new(""), 1, page(&["/a", "/b"], 2)));
        tree.expand(Path::new("/a"));
        tree.begin(PathBuf::from("/a"), 0, 2);
        assert!(tree.accept(Path::new("/a"), 2, page(&["/a/nested"], 1000)));
        tree.expand(Path::new("/a/nested"));
        tree.begin(PathBuf::from("/a/nested"), 0, 3);
        assert!(tree.accept(Path::new("/a/nested"), 3, page(&["/a/nested/leaf"], 20)));
        let node =
            super::super::LibraryNode::Directory(std::sync::Arc::from(Path::new("/a/selected")));
        tree.selected_locations
            .insert(node, (PathBuf::from("/a"), 500));
        assert_eq!(tree.row_count(), 1022);
        assert_eq!(tree.selected_index(), Some(521));
        tree.invalidate();
        assert_eq!(tree.row_count(), 1022);
        assert_eq!(tree.selected_index(), Some(521));
        assert!(matches!(tree.item(0), Some(TreeItem::Placeholder { .. })));
        assert_eq!(
            tree.requests_for_range(5..6),
            vec![
                (PathBuf::new(), 0),
                (PathBuf::from("/a"), 0),
                (PathBuf::from("/a/nested"), 0),
            ]
        );
        tree.begin(PathBuf::new(), 0, 4);
        assert!(tree.accept(Path::new(""), 4, page(&["/a", "/b"], 2)));
        assert_eq!(tree.row_count(), 1022);
        assert_eq!(tree.selected_index(), Some(521));
        tree.begin(PathBuf::from("/a"), 256, 5);
        assert!(tree.accept(Path::new("/a"), 5, page(&[], 10)));
        assert_eq!(tree.row_count(), 32);
        assert_eq!(tree.selected_index(), None);
    }

    #[test]
    fn refreshed_expanded_identity_moves_without_duplicate_old_anchor() {
        let mut tree = DirectoryTree::default();
        tree.configure_page_size(1);
        tree.begin(PathBuf::new(), 0, 1);
        assert!(tree.accept(Path::new(""), 1, page(&["/a"], 3)));
        tree.expand(Path::new("/a"));
        tree.begin(PathBuf::from("/a"), 0, 2);
        assert!(tree.accept(Path::new("/a"), 2, page(&["/a/child"], 100)));
        tree.invalidate();
        tree.begin(PathBuf::new(), 1, 3);
        assert!(tree.accept(Path::new(""), 3, page(&["/a"], 3)));
        assert_eq!(tree.row_count(), 103);
        assert_eq!(
            tree.branches[Path::new("")].children,
            BTreeMap::from([(1, PathBuf::from("/a"))])
        );
        assert!(matches!(tree.item(0), Some(TreeItem::Placeholder { .. })));
        assert!(
            matches!(tree.item(1), Some(TreeItem::Entry(VisibleDirectoryRow {
            row: DirectoryRow::Directory { path }, ..
        })) if path == Path::new("/a"))
        );
    }

    #[test]
    fn newer_selection_cancels_deferred_navigation() {
        let mut tree = DirectoryTree::default();
        tree.begin(PathBuf::new(), 0, 1);
        assert!(tree.accept(Path::new(""), 1, page(&["/a"], 1)));
        tree.expand(Path::new("/a"));
        tree.defer_selection(1);
        tree.cancel_navigation();
        tree.begin(PathBuf::from("/a"), 0, 2);
        assert!(tree.accept(Path::new("/a"), 2, page(&["/a/target"], 1)));
        assert_eq!(tree.ready_selection(), None);
    }

    #[test]
    fn deferred_navigation_tracks_branch_offset_when_sibling_load_changes_preorder() {
        let mut tree = DirectoryTree::default();
        tree.begin(PathBuf::new(), 0, 1);
        assert!(tree.accept(Path::new(""), 1, page(&["/a", "/b"], 2)));
        tree.expand(Path::new("/a"));
        tree.expand(Path::new("/b"));
        tree.defer_selection(3);
        tree.begin(PathBuf::from("/a"), 0, 2);
        assert!(tree.accept(Path::new("/a"), 2, page(&["/a/0", "/a/1", "/a/2"], 100)));
        assert!(matches!(tree.item(3), Some(TreeItem::Entry(_))));
        assert_eq!(tree.ready_selection(), None);
        tree.begin(PathBuf::from("/b"), 0, 3);
        assert!(tree.accept(Path::new("/b"), 3, page(&["/b/target"], 1)));
        assert_eq!(tree.ready_selection(), Some(102));
        assert!(
            matches!(tree.item(102), Some(TreeItem::Entry(VisibleDirectoryRow {
            row: DirectoryRow::Directory { path }, ..
        })) if path == Path::new("/b/target"))
        );
    }
}
