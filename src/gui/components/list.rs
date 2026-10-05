#![allow(dead_code)]
use std::collections::HashSet;
use std::hash::Hash;

/// Selection policy shared by queue, playlist, and library lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectionMode {
    Single,
    Multiple,
}

/// Selection state for virtualized lists keyed by stable domain IDs.
pub(crate) struct SelectionModel<T> {
    selected: HashSet<T>,
    anchor: Option<T>,
}

impl<T> Default for SelectionModel<T> {
    fn default() -> Self {
        Self {
            selected: HashSet::new(),
            anchor: None,
        }
    }
}

impl<T> SelectionModel<T> {
    pub(crate) fn anchor(&self) -> Option<&T> {
        self.anchor.as_ref()
    }

    pub(crate) fn set_anchor(&mut self, id: T) {
        self.anchor = Some(id);
    }

    pub(crate) fn clear_anchor(&mut self) {
        self.anchor = None;
    }
}

impl<T> std::ops::Deref for SelectionModel<T> {
    type Target = HashSet<T>;

    fn deref(&self) -> &Self::Target {
        &self.selected
    }
}

impl<T> std::ops::DerefMut for SelectionModel<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.selected
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SelectableListState<T> {
    items: Vec<T>,
    selected: Option<usize>,
    marked: HashSet<usize>,
    mode: SelectionMode,
}

impl<T> SelectableListState<T> {
    pub(crate) fn new(items: impl IntoIterator<Item = T>, mode: SelectionMode) -> Self {
        let items = items.into_iter().collect::<Vec<_>>();
        Self {
            selected: (!items.is_empty()).then_some(0),
            items,
            marked: HashSet::new(),
            mode,
        }
    }

    pub(crate) fn items(&self) -> &[T] {
        &self.items
    }
    pub(crate) fn selected_index(&self) -> Option<usize> {
        self.selected
    }
    pub(crate) fn selected_item(&self) -> Option<&T> {
        self.selected.and_then(|index| self.items.get(index))
    }
    pub(crate) fn is_marked(&self, index: usize) -> bool {
        self.marked.contains(&index)
    }
    pub(crate) fn marked_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.marked.iter().copied()
    }

    pub(crate) fn replace_items(&mut self, items: impl IntoIterator<Item = T>) {
        self.items = items.into_iter().collect();
        self.selected = self.selected.filter(|&index| index < self.items.len());
        if self.selected.is_none() && !self.items.is_empty() {
            self.selected = Some(0);
        }
        self.marked.retain(|index| *index < self.items.len());
    }

    pub(crate) fn move_next(&mut self) {
        if self.items.is_empty() {
            self.selected = None;
            return;
        }
        self.selected = Some(
            self.selected
                .map_or(0, |index| (index + 1).min(self.items.len() - 1)),
        );
    }

    pub(crate) fn move_previous(&mut self) {
        if self.items.is_empty() {
            self.selected = None;
            return;
        }
        self.selected = Some(self.selected.map_or(0, |index| index.saturating_sub(1)));
    }

    pub(crate) fn toggle(&mut self, index: usize, extend: bool) {
        if index >= self.items.len() {
            return;
        }
        if self.mode == SelectionMode::Single || !extend {
            self.select(index, false);
            return;
        }
        if self.selected == Some(index) {
            self.marked.remove(&index);
            self.selected = self.marked.iter().next().copied();
        } else {
            if let Some(selected) = self.selected {
                self.marked.insert(selected);
            }
            self.selected = Some(index);
            self.marked.insert(index);
        }
    }

    pub(crate) fn selected_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.marked.iter().copied().chain(self.selected)
    }

    pub(crate) fn clear_selection(&mut self) {
        self.selected = None;
        self.marked.clear();
    }

    pub(crate) fn select(&mut self, index: usize, extend: bool) {
        if index >= self.items.len() {
            return;
        }
        self.selected = Some(index);
        if self.mode == SelectionMode::Single || !extend {
            self.marked.clear();
        }
        if extend && self.mode == SelectionMode::Multiple {
            self.marked.insert(index);
        }
    }

    pub(crate) fn toggle_marked(&mut self) {
        let Some(index) = self.selected else {
            return;
        };
        if self.mode == SelectionMode::Single {
            return;
        }
        if !self.marked.insert(index) {
            self.marked.remove(&index);
        }
    }

    pub(crate) fn clear_marks(&mut self) {
        self.marked.clear();
    }
}

impl<T: Eq + Hash> SelectableListState<T> {
    pub(crate) fn marked_items(&self) -> impl Iterator<Item = &T> {
        self.marked
            .iter()
            .filter_map(|index| self.items.get(*index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiple_selection_is_bounded_and_toggleable() {
        let mut list = SelectableListState::new([10, 20], SelectionMode::Multiple);
        list.select(1, false);
        list.toggle_marked();
        assert_eq!(list.marked_items().copied().collect::<Vec<_>>(), vec![20]);
        list.move_next();
        assert_eq!(list.selected_index(), Some(1));
    }
}
