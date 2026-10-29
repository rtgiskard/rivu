use crate::keyboard::Action;
use ratatui::widgets::ListState;
use std::collections::HashMap;

pub(super) struct PagedList<T> {
    pub(super) pages: HashMap<usize, Vec<T>>,
    pub(super) total: usize,
    pub(super) loaded: bool,
    pub(super) fetch_size: usize,
    pub(super) selection: ListState,
    pub(super) viewport_rows: usize,
}

impl<T> Default for PagedList<T> {
    fn default() -> Self {
        Self {
            pages: HashMap::new(),
            total: 0,
            loaded: false,
            fetch_size: 0,
            selection: ListState::default(),
            viewport_rows: 1,
        }
    }
}

impl<T> PagedList<T> {
    pub(super) fn clear(&mut self) {
        self.pages.clear();
        self.total = 0;
        self.loaded = false;
        self.fetch_size = 0;
    }

    pub(super) fn next_offset(&self, fetch_size: usize) -> Option<usize> {
        let fetch_size = fetch_size.max(1);
        let selected = self.selection.selected().unwrap_or(0) / fetch_size * fetch_size;
        if !self.loaded || self.fetch_size != fetch_size {
            return Some(selected);
        }
        if self.total == 0 {
            return None;
        }
        let missing = |offset| !self.pages.contains_key(&offset);
        if missing(selected) {
            return Some(selected);
        }
        let first = self.selection.offset() / fetch_size * fetch_size;
        let last = self.visible_end().saturating_sub(1) / fetch_size * fetch_size;
        for offset in (first..=last).step_by(fetch_size) {
            if missing(offset) {
                return Some(offset);
            }
        }
        // One neighboring batch on either side hides boundaries during ordinary scrolling.
        let next = last.saturating_add(fetch_size);
        if next < self.total && missing(next) {
            return Some(next);
        }
        first
            .checked_sub(fetch_size)
            .filter(|offset| missing(*offset))
    }

    pub(super) fn visible_end(&self) -> usize {
        self.selection
            .offset()
            .saturating_add(self.viewport_rows)
            .min(self.total)
    }

    pub(super) fn set_viewport(&mut self, rows: usize) {
        self.viewport_rows = rows.max(1);
        let selected = self.selection.selected().unwrap_or(0);
        let mut start = self.selection.offset();
        if selected < start {
            start = selected;
        } else if selected >= start.saturating_add(self.viewport_rows) {
            start = selected + 1 - self.viewport_rows;
        }
        if self.loaded {
            start = start.min(self.total.saturating_sub(self.viewport_rows));
        }
        *self.selection.offset_mut() = start;
        self.trim_pages();
    }

    fn trim_pages(&mut self) {
        let fetch_size = self.fetch_size.max(1);
        let first = (self.selection.offset() / fetch_size * fetch_size).saturating_sub(fetch_size);
        let last = self.visible_end().saturating_sub(1) / fetch_size * fetch_size;
        let last = last.saturating_add(fetch_size);
        // At most ceil(viewport_rows / fetch_size) + 3 batches, independent of list length.
        self.pages
            .retain(|offset, _| (first..=last).contains(offset));
    }

    pub(super) fn navigate_action(&mut self, action: Action, page_size: usize) -> bool {
        if !self.loaded {
            return false;
        }
        let changed = match action {
            Action::Up | Action::Down => {
                move_selection(&mut self.selection, self.total, action == Action::Down)
            }
            Action::PageUp | Action::PageDown => move_selection_page(
                &mut self.selection,
                self.total,
                page_size,
                action == Action::PageDown,
            ),
            Action::First | Action::Last => {
                let next = if action == Action::First {
                    (self.total != 0).then_some(0)
                } else {
                    self.total.checked_sub(1)
                };
                let changed = next != self.selection.selected();
                self.selection.select(next);
                changed
            }
            _ => false,
        };
        self.set_viewport(self.viewport_rows);
        changed
    }

    pub(super) fn insert(&mut self, offset: usize, fetch_size: usize, total: usize, items: Vec<T>) {
        if self.fetch_size != fetch_size {
            self.clear();
        }
        self.fetch_size = fetch_size;
        self.pages.insert(offset, items);
        self.total = total;
        self.loaded = true;
        let selected = self.selection.selected().unwrap_or(0);
        self.selection
            .select((total != 0).then(|| selected.min(total - 1)));
        self.set_viewport(self.viewport_rows);
    }

    pub(super) fn get(&self, index: usize) -> Option<&T> {
        let fetch_size = self.fetch_size.max(1);
        let offset = index / fetch_size * fetch_size;
        self.pages.get(&offset)?.get(index - offset)
    }

    pub(super) fn selected(&self) -> Option<&T> {
        self.get(self.selection.selected()?)
    }
}
fn move_selection(selection: &mut ListState, len: usize, down: bool) -> bool {
    if len == 0 {
        if selection.selected().is_some() {
            selection.select(None);
            return true;
        }
        return false;
    }
    let index = selection.selected().unwrap_or(0);
    let next = if down {
        index.saturating_add(1).min(len - 1)
    } else {
        index.saturating_sub(1)
    };
    if selection.selected() == Some(next) {
        false
    } else {
        selection.select(Some(next));
        true
    }
}
fn move_selection_page(
    selection: &mut ListState,
    len: usize,
    page_size: usize,
    down: bool,
) -> bool {
    let page_size = page_size.max(1);
    if len == 0 {
        if selection.selected().is_some() {
            selection.select(None);
            return true;
        }
        return false;
    }
    let index = selection.selected().unwrap_or(0);
    let next = if down {
        index.saturating_add(page_size).min(len - 1)
    } else {
        index.saturating_sub(page_size)
    };
    if selection.selected() == Some(next) {
        false
    } else {
        selection.select(Some(next));
        true
    }
}

#[cfg(test)]
mod tests;
