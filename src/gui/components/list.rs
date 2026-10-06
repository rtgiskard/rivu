use std::collections::HashSet;

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

impl<T> SelectionModel<T>
where
    T: Eq + std::hash::Hash,
{
    pub(crate) fn contains(&self, id: &T) -> bool {
        self.selected.contains(id)
    }

    pub(crate) fn len(&self) -> usize {
        self.selected.len()
    }

    pub(crate) fn clear(&mut self) {
        self.selected.clear();
    }

    pub(crate) fn insert(&mut self, id: T) -> bool {
        self.selected.insert(id)
    }

    pub(crate) fn remove(&mut self, id: &T) -> bool {
        self.selected.remove(id)
    }

    pub(crate) fn extend<I: IntoIterator<Item = T>>(&mut self, ids: I) {
        self.selected.extend(ids);
    }

    pub(crate) fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.selected.retain(keep);
    }
}
