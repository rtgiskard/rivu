#![allow(dead_code)]
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// A flattened source row used by the reusable tree navigator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TreeRow<T> {
    pub(crate) id: T,
    pub(crate) parent: Option<T>,
    pub(crate) depth: usize,
    pub(crate) label: String,
    pub(crate) has_children: bool,
}

impl<T> TreeRow<T> {
    pub(crate) fn new(
        id: T,
        parent: Option<T>,
        depth: usize,
        label: impl Into<String>,
        has_children: bool,
    ) -> Self {
        Self {
            id,
            parent,
            depth,
            label: label.into(),
            has_children,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TreeKey {
    Up,
    Down,
    Left,
    Right,
    Toggle,
}

#[derive(Clone, Debug)]
pub(crate) struct TreeState<T> {
    rows: Vec<TreeRow<T>>,
    parents: Vec<Option<usize>>,
    visible: Vec<usize>,
    expanded: HashSet<T>,
    selected: Option<usize>,
    query: String,
}

impl<T: Clone + Eq + Hash> TreeState<T> {
    pub(crate) fn new(rows: impl IntoIterator<Item = TreeRow<T>>) -> Self {
        let mut tree = Self {
            selected: None,
            rows: Vec::new(),
            parents: Vec::new(),
            visible: Vec::new(),
            expanded: HashSet::new(),
            query: String::new(),
        };
        tree.set_rows(rows);
        tree
    }

    pub(crate) fn rows(&self) -> &[TreeRow<T>] {
        &self.rows
    }
    pub(crate) fn selected(&self) -> Option<&TreeRow<T>> {
        self.selected.and_then(|index| {
            self.visible_indices()
                .get(index)
                .and_then(|row| self.rows.get(*row))
        })
    }
    pub(crate) fn selected_index(&self) -> Option<usize> {
        self.selected
    }
    pub(crate) fn expanded(&self) -> &HashSet<T> {
        &self.expanded
    }

    pub(crate) fn select(&mut self, id: &T) {
        if let Some(index) = self
            .visible_indices()
            .iter()
            .position(|index| &self.rows[*index].id == id)
        {
            self.selected = Some(index);
        }
    }
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) fn set_rows(&mut self, rows: impl IntoIterator<Item = TreeRow<T>>) {
        let selected = self.selected().map(|row| row.id.clone());
        self.rows = rows.into_iter().collect();
        let ids = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| (row.id.clone(), index))
            .collect::<HashMap<_, _>>();
        self.parents = self
            .rows
            .iter()
            .map(|row| row.parent.as_ref().and_then(|id| ids.get(id).copied()))
            .collect();
        self.expanded.retain(|id| ids.contains_key(id));
        self.refresh_visible();
        let visible = self.visible_indices();
        self.selected = selected
            .and_then(|id| visible.iter().position(|index| self.rows[*index].id == id))
            .or_else(|| (!visible.is_empty()).then_some(0));
    }

    pub(crate) fn set_query(&mut self, query: impl Into<String>) {
        self.query = query.into();
        self.refresh_visible();
        self.selected = self
            .selected
            .filter(|index| *index < self.visible_indices().len());
        if self.selected.is_none() && !self.visible_indices().is_empty() {
            self.selected = Some(0);
        }
    }

    pub(crate) fn visible_indices(&self) -> &[usize] {
        &self.visible
    }

    fn refresh_visible(&mut self) {
        self.visible.clear();
        if !self.query.is_empty() {
            let query = self.query.to_lowercase();
            let mut keep = vec![false; self.rows.len()];
            for (index, row) in self.rows.iter().enumerate() {
                if row.label.to_lowercase().contains(&query) {
                    let mut ancestor = Some(index);
                    while let Some(index) = ancestor {
                        if keep[index] {
                            break;
                        }
                        keep[index] = true;
                        ancestor = self.parents[index];
                    }
                }
            }
            self.visible.extend(
                keep.into_iter()
                    .enumerate()
                    .filter_map(|(index, keep)| keep.then_some(index)),
            );
            return;
        }
        for index in 0..self.rows.len() {
            let mut parent = self.parents[index];
            let mut visible = true;
            while let Some(index) = parent {
                if !self.expanded.contains(&self.rows[index].id) {
                    visible = false;
                    break;
                }
                parent = self.parents[index];
            }
            if visible {
                self.visible.push(index);
            }
        }
    }

    pub(crate) fn handle_key(&mut self, key: TreeKey) {
        let visible = &self.visible;
        let Some(selected) = self.selected.filter(|index| *index < visible.len()) else {
            return;
        };
        let row_index = visible[selected];
        match key {
            TreeKey::Up => self.selected = Some(selected.saturating_sub(1)),
            TreeKey::Down => self.selected = Some((selected + 1).min(visible.len() - 1)),
            TreeKey::Toggle => self.toggle(row_index),
            TreeKey::Right => {
                if self.rows[row_index].has_children
                    && self.expanded.insert(self.rows[row_index].id.clone())
                {
                    self.refresh_visible();
                    return;
                }
                if selected + 1 < visible.len()
                    && self.rows[visible[selected + 1]].parent.as_ref()
                        == Some(&self.rows[row_index].id)
                {
                    self.selected = Some(selected + 1);
                }
            }
            TreeKey::Left => {
                if self.rows[row_index].has_children
                    && self.expanded.remove(&self.rows[row_index].id)
                {
                    self.refresh_visible();
                    return;
                }
                if let Some(parent) = &self.rows[row_index].parent
                    && let Some(parent_index) = visible
                        .iter()
                        .position(|index| self.rows[*index].id == *parent)
                {
                    self.selected = Some(parent_index);
                }
            }
        }
    }

    pub(crate) fn toggle_selected(&mut self) {
        if let Some(index) = self
            .selected
            .and_then(|selected| self.visible_indices().get(selected).copied())
        {
            self.toggle(index);
        }
    }

    fn toggle(&mut self, index: usize) {
        if self.rows[index].has_children {
            let id = self.rows[index].id.clone();
            if !self.expanded.insert(id.clone()) {
                self.expanded.remove(&id);
            }
            self.refresh_visible();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> TreeState<&'static str> {
        TreeState::new([
            TreeRow::new("root", None, 0, "Music", true),
            TreeRow::new("album", Some("root"), 1, "Album", true),
            TreeRow::new("track", Some("album"), 2, "Track.flac", false),
        ])
    }

    #[test]
    fn right_and_left_navigate_tree() {
        let mut tree = tree();
        tree.handle_key(TreeKey::Right);
        assert_eq!(tree.visible_indices(), vec![0, 1]);
        tree.handle_key(TreeKey::Right);
        tree.handle_key(TreeKey::Right);
        assert_eq!(tree.visible_indices(), vec![0, 1, 2]);
        tree.handle_key(TreeKey::Left);
        assert_eq!(tree.selected().map(|row| row.id), Some("album"));
    }

    #[test]
    fn search_keeps_matching_ancestors_visible() {
        let mut tree = tree();
        tree.set_query("track");
        assert_eq!(tree.visible_indices(), vec![0, 1, 2]);
    }

    #[test]
    fn refresh_preserves_identity_and_prunes_removed_nodes() {
        let mut tree = tree();
        tree.handle_key(TreeKey::Right);
        tree.select(&"album");
        tree.handle_key(TreeKey::Right);
        tree.select(&"track");
        tree.set_rows([
            TreeRow::new("new", None, 0, "New", false),
            TreeRow::new("root", None, 0, "Music", true),
            TreeRow::new("album", Some("root"), 1, "Album", true),
            TreeRow::new("track", Some("album"), 2, "Track.flac", false),
        ]);
        assert_eq!(tree.selected().map(|row| row.id), Some("track"));
        assert_eq!(tree.selected_index(), Some(3));
        tree.set_rows([TreeRow::new("new", None, 0, "New", false)]);
        assert_eq!(tree.selected().map(|row| row.id), Some("new"));
        assert!(tree.expanded().is_empty());
        tree.set_rows([]);
        assert!(tree.selected().is_none());
        assert!(tree.visible_indices().is_empty());
    }

    #[test]
    fn mouse_selection_and_toggle_share_keyboard_state() {
        let mut tree = tree();
        tree.handle_key(TreeKey::Toggle);
        tree.select(&"album");
        tree.handle_key(TreeKey::Toggle);
        tree.handle_key(TreeKey::Down);
        assert_eq!(tree.selected().map(|row| row.id), Some("track"));
        tree.handle_key(TreeKey::Left);
        assert_eq!(tree.selected().map(|row| row.id), Some("album"));
        tree.handle_key(TreeKey::Toggle);
        assert_eq!(tree.visible_indices(), &[0, 1]);
        tree.handle_key(TreeKey::Up);
        assert_eq!(tree.selected().map(|row| row.id), Some("root"));
    }
}
