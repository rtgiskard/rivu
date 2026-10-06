use crate::model::{DirectoryPage, DirectoryRow};
use gpui::UniformListScrollHandle;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

/// A directory row flattened for rendering and keyboard navigation.
#[derive(Clone, Debug)]
pub(in crate::gui) struct VisibleDirectoryRow {
    pub(in crate::gui) row: DirectoryRow,
    pub(in crate::gui) depth: usize,
    pub(in crate::gui) expanded: bool,
    pub(in crate::gui) has_children: bool,
    pub(in crate::gui) status: BranchStatus,
    pub(in crate::gui) offset: usize,
    pub(in crate::gui) total: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) enum BranchStatus {
    /// The directory has not been requested yet.
    Unloaded,
    Loading,
    Ready,
    Empty,
    Error,
}

struct Branch {
    offset: usize,
    total: usize,
    rows: Option<Vec<DirectoryRow>>,
    expanded: bool,
    pending: bool,
    error: bool,
    generation: u64,
}

impl Branch {
    fn new() -> Self {
        Self {
            offset: 0,
            total: 0,
            rows: None,
            expanded: false,
            pending: false,
            error: false,
            generation: 0,
        }
    }

    fn status(&self) -> BranchStatus {
        if self.pending {
            BranchStatus::Loading
        } else if self.error {
            BranchStatus::Error
        } else if self.rows.as_ref().is_some_and(Vec::is_empty) {
            BranchStatus::Empty
        } else if self.rows.is_some() {
            BranchStatus::Ready
        } else {
            BranchStatus::Unloaded
        }
    }
}

/// Expandable, paginated directory tree. The empty path is a virtual root;
/// every non-root branch owns at most one retained page.
pub(in crate::gui) struct DirectoryTree {
    branches: HashMap<PathBuf, Branch>,
    visible: Vec<VisibleDirectoryRow>,
    pub(in crate::gui) scroll: UniformListScrollHandle,
}

impl Default for DirectoryTree {
    fn default() -> Self {
        Self {
            branches: HashMap::new(),
            visible: Vec::new(),
            scroll: UniformListScrollHandle::new(),
        }
    }
}

impl DirectoryTree {
    fn branch(&self, path: &Path) -> Option<&Branch> {
        self.branches.get(path)
    }

    fn branch_mut(&mut self, path: &Path) -> &mut Branch {
        self.branches
            .entry(path.to_path_buf())
            .or_insert_with(Branch::new)
    }

    fn descendants(&mut self, path: &Path) {
        self.branches.retain(|candidate, _| {
            candidate == path
                || !candidate
                    .strip_prefix(path)
                    .is_ok_and(|rest| !rest.as_os_str().is_empty())
        });
    }

    fn direct_children(page_path: &Path, rows: &[DirectoryRow]) -> HashSet<PathBuf> {
        rows.iter()
            .filter_map(|row| match row {
                DirectoryRow::Directory { path } => Some(path.clone()),
                DirectoryRow::Track(_) => None,
            })
            .filter(|path| page_path.as_os_str().is_empty() || path.strip_prefix(page_path).is_ok())
            .collect()
    }

    /// Start (or restart) a request. Refreshing the same page keeps its
    /// descendants visible; moving to another page prunes only this branch.
    pub(in crate::gui) fn begin(&mut self, path: PathBuf, offset: usize, generation: u64) {
        let same_page = self
            .branch(&path)
            .is_some_and(|branch| branch.rows.is_some() && branch.offset == offset);
        if !same_page {
            self.descendants(&path);
            let branch = self.branch_mut(&path);
            branch.rows = None;
            branch.total = 0;
        }
        let branch = self.branch_mut(&path);
        branch.offset = offset;
        branch.generation = generation;
        branch.pending = true;
        branch.error = false;
        if !path.as_os_str().is_empty() {
            branch.expanded = true;
        }
        self.refresh_visible();
    }

    /// Returns the current token for a branch, including a loaded branch.
    pub(in crate::gui) fn generation(&self, path: &Path) -> Option<u64> {
        self.branch(path).map(|branch| branch.generation)
    }

    /// Accept only the currently outstanding request for this branch.
    pub(in crate::gui) fn accept(
        &mut self,
        path: &Path,
        generation: u64,
        page: DirectoryPage,
    ) -> bool {
        let Some(branch) = self.branches.get(path) else {
            return false;
        };
        if branch.generation != generation || !branch.pending {
            return false;
        }

        let direct = Self::direct_children(path, &page.rows);
        let branch = self.branches.get_mut(path).expect("branch checked above");
        branch.total = page.total;
        branch.rows = Some(page.rows);
        branch.pending = false;
        branch.error = false;

        // A refreshed page may remove a child while retaining the expansion of
        // its siblings. Do not retain descendants hidden by this page.
        self.branches.retain(|candidate, _| {
            if candidate == path {
                return true;
            }
            let Ok(_) = candidate.strip_prefix(path) else {
                return true;
            };
            direct
                .iter()
                .any(|child| candidate == child || candidate.strip_prefix(child).is_ok())
        });
        self.refresh_visible();
        true
    }

    /// Mark a matching request as failed while retaining a previous page, if
    /// one existed, so a transient error never destroys the visible tree.
    pub(in crate::gui) fn fail(&mut self, path: &Path, generation: u64) {
        if let Some(branch) = self.branches.get_mut(path)
            && branch.generation == generation
            && branch.pending
        {
            branch.pending = false;
            branch.error = true;
            self.refresh_visible();
        }
    }

    pub(in crate::gui) fn root_total(&self) -> usize {
        self.branch(Path::new("")).map_or(0, |branch| branch.total)
    }

    pub(in crate::gui) fn root_offset(&self) -> usize {
        self.branch(Path::new("")).map_or(0, |branch| branch.offset)
    }

    /// Invalidate every cached page and every in-flight request.
    pub(in crate::gui) fn clear(&mut self) {
        self.branches.clear();
        self.visible.clear();
    }

    pub(in crate::gui) fn expand(&mut self, path: &Path) {
        self.branch_mut(path).expanded = true;
        self.refresh_visible();
    }

    /// Collapse and prune the whole cached/pending subtree below `path`.
    pub(in crate::gui) fn collapse(&mut self, path: &Path) {
        if let Some(branch) = self.branches.get_mut(path) {
            branch.expanded = false;
        }
        self.descendants(path);
        self.refresh_visible();
    }

    pub(in crate::gui) fn expanded(&self, path: &Path) -> bool {
        self.branch(path).is_some_and(|branch| branch.expanded)
    }

    pub(in crate::gui) fn status(&self, path: &Path) -> BranchStatus {
        self.branch(path)
            .map_or(BranchStatus::Unloaded, Branch::status)
    }

    /// Return root followed by expanded cached branches in parent-before-child
    /// order. Used to refresh visible pages after a library revision.
    pub(in crate::gui) fn refresh_pages(&self) -> Vec<(PathBuf, usize)> {
        let mut result = Vec::new();
        let Some(root) = self.branch(Path::new("")) else {
            return result;
        };
        result.push((PathBuf::new(), root.offset));
        let Some(rows) = root.rows.as_ref() else {
            return result;
        };
        let mut visited = HashSet::new();
        visited.insert(PathBuf::new());
        self.refresh_children(rows, &mut visited, &mut result);
        result
    }

    fn refresh_children(
        &self,
        rows: &[DirectoryRow],
        visited: &mut HashSet<PathBuf>,
        result: &mut Vec<(PathBuf, usize)>,
    ) {
        for path in rows.iter().filter_map(|row| match row {
            DirectoryRow::Directory { path } => Some(path),
            DirectoryRow::Track(_) => None,
        }) {
            if !visited.insert(path.clone()) {
                continue;
            }
            let Some(branch) = self.branch(path) else {
                continue;
            };
            if !branch.expanded {
                continue;
            }
            let Some(rows) = branch.rows.as_ref() else {
                continue;
            };
            result.push((path.clone(), branch.offset));
            self.refresh_children(rows, visited, result);
        }
    }

    /// Rebuild only when branches change; virtual-list rendering borrows the cache.
    fn refresh_visible(&mut self) {
        let mut visible = std::mem::take(&mut self.visible);
        visible.clear();
        if let Some(rows) = self
            .branch(Path::new(""))
            .and_then(|root| root.rows.as_ref())
        {
            let mut visited = HashSet::new();
            self.flatten(rows, 0, &mut visited, &mut visible);
        }
        self.visible = visible;
    }

    pub(in crate::gui) fn visible_rows(&self) -> &[VisibleDirectoryRow] {
        &self.visible
    }

    fn flatten(
        &self,
        rows: &[DirectoryRow],
        depth: usize,
        visited: &mut HashSet<PathBuf>,
        visible: &mut Vec<VisibleDirectoryRow>,
    ) {
        for row in rows {
            let (expanded, has_children, status, offset, total, path) = match row {
                DirectoryRow::Directory { path } => {
                    let branch = self.branch(path);
                    let status = branch.map_or(BranchStatus::Unloaded, Branch::status);
                    let expanded = branch.is_some_and(|branch| branch.expanded);
                    let has_children = status != BranchStatus::Empty;
                    (
                        expanded,
                        has_children,
                        status,
                        branch.map_or(0, |branch| branch.offset),
                        branch.map_or(0, |branch| branch.total),
                        Some(path),
                    )
                }
                DirectoryRow::Track(_) => (false, false, BranchStatus::Ready, 0, 0, None),
            };
            visible.push(VisibleDirectoryRow {
                row: row.clone(),
                depth,
                expanded,
                has_children,
                status,
                offset,
                total,
            });
            let Some(path) = path else { continue };
            if !expanded || !visited.insert(path.clone()) {
                continue;
            }
            if let Some(branch) = self.branch(path)
                && let Some(rows) = branch.rows.as_ref()
            {
                self.flatten(rows, depth + 1, visited, visible);
            }
        }
    }

    /// Drop directory selections whose rows disappeared when a page was
    /// replaced or a subtree was pruned. Track selections are owned by the
    /// flat library view and are intentionally left untouched here.
    pub(in crate::gui) fn retain_known_directories(
        &self,
        selected: &mut HashSet<super::LibraryNode>,
    ) {
        let known = self
            .branches
            .values()
            .flat_map(|branch| branch.rows.as_deref().unwrap_or(&[]))
            .filter_map(|row| match row {
                DirectoryRow::Directory { path } => Some(path.as_path()),
                DirectoryRow::Track(_) => None,
            })
            .collect::<HashSet<_>>();
        selected.retain(|node| match node {
            super::LibraryNode::Directory(path) => known.contains(path.as_ref()),
            super::LibraryNode::Track(_) => true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DirectoryPage, DirectoryRow, LibraryRow};

    fn directory(path: &str) -> DirectoryRow {
        DirectoryRow::Directory {
            path: PathBuf::from(path),
        }
    }
    fn page(paths: &[&str], total: usize) -> DirectoryPage {
        DirectoryPage {
            total,
            rows: paths.iter().map(|path| directory(path)).collect(),
        }
    }
    fn track(id: i64) -> DirectoryRow {
        DirectoryRow::Track(LibraryRow {
            id,
            title: format!("track-{id}"),
            artist: String::new(),
            album: String::new(),
            duration: None,
            favorite: false,
            missing: false,
            play_count: 0,
        })
    }

    #[test]
    fn lazy_load_and_collapse_prune_subtree() {
        let mut tree = DirectoryTree::default();
        tree.begin(PathBuf::new(), 0, 1);
        assert!(tree.accept(Path::new(""), 1, page(&["/a"], 1)));
        tree.expand(Path::new("/a"));
        tree.begin(PathBuf::from("/a"), 0, 2);
        assert!(tree.accept(Path::new("/a"), 2, page(&["/a/b"], 1)));
        tree.expand(Path::new("/a/b"));
        tree.begin(PathBuf::from("/a/b"), 0, 3);
        assert!(tree.accept(
            Path::new("/a/b"),
            3,
            DirectoryPage {
                total: 1,
                rows: vec![track(1)]
            }
        ));
        assert_eq!(
            tree.visible_rows()
                .iter()
                .map(|row| (&row.row, row.depth))
                .collect::<Vec<_>>(),
            vec![
                (&directory("/a"), 0),
                (&directory("/a/b"), 1),
                (&track(1), 2)
            ]
        );
        tree.collapse(Path::new("/a"));
        assert_eq!(
            tree.visible_rows()
                .iter()
                .map(|row| &row.row)
                .collect::<Vec<_>>(),
            vec![&directory("/a")]
        );
        assert_eq!(tree.generation(Path::new("/a/b")), None);
    }

    #[test]
    fn out_of_order_response_is_ignored() {
        let mut tree = DirectoryTree::default();
        tree.begin(PathBuf::new(), 0, 1);
        tree.begin(PathBuf::new(), 0, 2);
        assert!(!tree.accept(Path::new(""), 1, page(&["/stale"], 1)));
        assert!(tree.accept(Path::new(""), 2, page(&["/fresh"], 1)));
        assert!(
            matches!(tree.visible_rows()[0].row, DirectoryRow::Directory { ref path } if path == &PathBuf::from("/fresh"))
        );
    }

    #[test]
    fn paging_prunes_only_changed_branch() {
        let mut tree = DirectoryTree::default();
        tree.begin(PathBuf::new(), 0, 1);
        assert!(tree.accept(Path::new(""), 1, page(&["/a", "/b"], 2)));
        for (path, generation) in [("/a", 2), ("/b", 3)] {
            tree.expand(Path::new(path));
            tree.begin(PathBuf::from(path), 0, generation);
            assert!(tree.accept(
                Path::new(path),
                generation,
                page(&[&format!("{path}/child")], 2)
            ));
            let child = PathBuf::from(format!("{path}/child"));
            tree.begin(child.clone(), 0, generation + 10);
            assert!(tree.accept(
                &child,
                generation + 10,
                DirectoryPage {
                    total: 1,
                    rows: vec![track(generation as i64)]
                }
            ));
        }
        tree.begin(PathBuf::from("/a"), 256, 4);
        assert_eq!(tree.generation(Path::new("/a/child")), None);
        assert!(tree.generation(Path::new("/b/child")).is_some());
    }

    #[test]
    fn same_page_refresh_keeps_children_but_removes_missing_rows() {
        let mut tree = DirectoryTree::default();
        tree.begin(PathBuf::new(), 0, 1);
        assert!(tree.accept(Path::new(""), 1, page(&["/a"], 1)));
        tree.expand(Path::new("/a"));
        tree.begin(PathBuf::from("/a"), 0, 2);
        assert!(tree.accept(Path::new("/a"), 2, page(&["/a/b"], 1)));
        tree.expand(Path::new("/a/b"));
        tree.begin(PathBuf::from("/a"), 0, 3);
        assert!(tree.accept(Path::new("/a"), 3, page(&["/a/c"], 1)));
        assert_eq!(tree.generation(Path::new("/a/b")), None);
        assert!(tree.generation(Path::new("/a/c")).is_none());
    }
}
