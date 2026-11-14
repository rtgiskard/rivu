use ratatui::widgets::ListState;
use rivu_presentation::keyboard::Action;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const REFRESH_RETENTION: Duration = Duration::from_millis(200);

pub(super) struct RetainedPages<T> {
    pages: HashMap<usize, Vec<T>>,
    total: usize,
    fetch_size: usize,
    start: usize,
    selected: Option<usize>,
    identity_scan: bool,
    rows: usize,
    deadline: Instant,
    expired: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum ContainerIdentity {
    Directory(PathBuf),
    OtherTracks,
    Playlist(i64),
}

pub(super) struct PagedList<T> {
    pub(super) pages: HashMap<usize, Vec<T>>,
    pub(super) total: usize,
    pub(super) loaded: bool,
    pub(super) fetch_size: usize,
    pub(super) selection: ListState,
    pub(super) viewport_rows: usize,
    // Target identity, next scan offset, and the viewport to restore on success.
    pub(super) identity_lookup: Option<(ContainerIdentity, usize, usize)>,
    pub(super) columns: Option<super::render::IntrinsicColumns>,
    pub(super) failed_request: Option<std::sync::Arc<super::worker::ReadSpec>>,
    pub(super) retained: Option<RetainedPages<T>>,
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
            identity_lookup: None,
            columns: None,
            failed_request: None,
            retained: None,
        }
    }
}

impl<T> PagedList<T> {
    pub(super) fn clear(&mut self) {
        self.clear_pages();
        self.total = 0;
        self.loaded = false;
        self.fetch_size = 0;
        if let Some((_, offset, _)) = &mut self.identity_lookup {
            *offset = 0;
        }
    }

    pub(super) fn clear_pages(&mut self) {
        self.pages.clear();
        self.discard_refresh();
        self.columns = None;
    }

    pub(super) fn invalidate(&mut self) {
        self.invalidate_pages();
        self.total = 0;
        self.loaded = false;
        self.fetch_size = 0;
        if let Some((_, offset, _)) = &mut self.identity_lookup {
            *offset = 0;
        }
    }

    pub(super) fn invalidate_pages(&mut self) {
        self.expire_refresh(Instant::now());
        if self.retained.as_ref().is_some_and(|retained| {
            (!retained.identity_scan && retained.start != self.selection.offset())
                || retained.rows != self.viewport_rows
        }) {
            self.discard_refresh();
        }
        let start = self.display_offset();
        if self.retained.is_none() && self.loaded {
            let mut pages = HashMap::new();
            std::mem::swap(&mut pages, &mut self.pages);
            self.retained = Some(RetainedPages {
                pages,
                total: self.total,
                fetch_size: self.fetch_size.max(1),
                start,
                rows: self.viewport_rows,
                selected: self.selection.selected(),
                identity_scan: false,
                deadline: Instant::now() + REFRESH_RETENTION,
                expired: false,
            });
        } else if let Some(retained) = &mut self.retained
            && !retained.expired
        {
            if self.loaded && !retained.identity_scan {
                retained.total = self.total;
            }
            retained.pages.extend(self.pages.drain());
        }
        self.pages.clear();
        if let Some(retained) = &mut self.retained {
            let end = start.saturating_add(retained.rows).min(retained.total);
            retained.pages.retain(|&offset, items| {
                offset < end && offset.saturating_add(items.len()) > start
            });
        }
        self.columns = None;
    }

    pub(super) fn refresh_deadline(&self) -> Option<Instant> {
        self.retained
            .as_ref()
            .filter(|retained| !retained.expired)
            .map(|retained| retained.deadline)
    }

    pub(super) fn expire_refresh(&mut self, now: Instant) -> bool {
        let Some(retained) = &mut self.retained else {
            return false;
        };
        if retained.expired || now < retained.deadline {
            return false;
        }
        retained.pages.clear();
        // Keep the expired interval until fresh visible rows restore authority.
        // Repeated invalidation must not start another stale display interval.
        retained.expired = true;
        self.columns = None;
        true
    }

    pub(super) fn discard_refresh(&mut self) {
        if self.retained.take().is_some() {
            self.columns = None;
        }
    }

    fn retained_display(&self) -> Option<&RetainedPages<T>> {
        self.retained.as_ref().filter(|retained| {
            !retained.expired
                && retained.rows == self.viewport_rows
                && (retained.identity_scan || retained.start == self.selection.offset())
        })
    }

    pub(super) fn display_offset(&self) -> usize {
        self.retained_display()
            .filter(|retained| retained.identity_scan)
            .map_or(self.selection.offset(), |retained| retained.start)
    }

    pub(super) fn display_selected(&self) -> Option<usize> {
        self.retained_display()
            .filter(|retained| retained.identity_scan)
            .map_or(self.selection.selected(), |retained| retained.selected)
    }

    pub(super) fn display_total(&self) -> usize {
        self.retained_display()
            .filter(|retained| !self.loaded || retained.identity_scan)
            .map_or(self.total, |retained| retained.total)
    }

    pub(super) fn display_loaded(&self) -> bool {
        self.loaded || self.retained_display().is_some()
    }

    pub(super) fn display_visible_end(&self) -> usize {
        self.display_offset()
            .saturating_add(self.viewport_rows)
            .min(self.display_total())
    }

    pub(super) fn display(&self, index: usize) -> Option<&T> {
        if index >= self.display_total() {
            return None;
        }
        if let Some(item) = self.get(index) {
            return Some(item);
        }
        let retained = self.retained_display()?;
        if index < retained.start || index >= retained.start.saturating_add(retained.rows) {
            return None;
        }
        let offset = index / retained.fetch_size * retained.fetch_size;
        retained.pages.get(&offset)?.get(index - offset)
    }

    pub(super) fn display_iter(&self) -> impl Iterator<Item = &T> {
        let start = self.display_offset();
        let end = self.display_visible_end();
        self.pages.values().flatten().chain(
            self.retained_display()
                .into_iter()
                .flat_map(|retained| retained.pages.iter())
                .filter(move |&(offset, _)| !self.pages.contains_key(offset))
                .flat_map(move |(&offset, items)| {
                    items.iter().enumerate().filter_map(move |(index, item)| {
                        let index = offset.saturating_add(index);
                        (index >= start && index < end).then_some(item)
                    })
                }),
        )
    }

    pub(super) fn retain_pages(&mut self, mut keep: impl FnMut(usize, &[T]) -> bool) {
        let previous_len = self.pages.len();
        self.pages.retain(|&offset, rows| keep(offset, rows));
        if self.pages.len() != previous_len {
            self.columns = None;
        }
    }

    pub(super) fn next_offset(&self, fetch_size: usize) -> Option<usize> {
        let fetch_size = fetch_size.max(1);
        if let Some((_, offset, _)) = &self.identity_lookup {
            return Some(*offset);
        }
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
        if self.retained.as_ref().is_some_and(|retained| {
            (!retained.identity_scan && retained.start != start)
                || retained.rows != self.viewport_rows
        }) {
            self.discard_refresh();
        }
        self.trim_pages();
    }

    fn trim_pages(&mut self) {
        let fetch_size = self.fetch_size.max(1);
        let first = (self.selection.offset() / fetch_size * fetch_size).saturating_sub(fetch_size);
        let last = self.visible_end().saturating_sub(1) / fetch_size * fetch_size;
        let last = last.saturating_add(fetch_size);
        // At most ceil(viewport_rows / fetch_size) + 3 batches, independent of list length.
        self.retain_pages(|offset, _| (first..=last).contains(&offset));
    }

    pub(super) fn navigate_action(&mut self, action: Action, page_size: usize) -> bool {
        let mut discarded = false;
        if matches!(
            action,
            Action::Up
                | Action::Down
                | Action::PageUp
                | Action::PageDown
                | Action::First
                | Action::Last
        ) {
            discarded = self.retained_display().is_some();
            self.discard_refresh();
        }
        if !self.loaded {
            return discarded;
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
        changed || discarded
    }

    pub(super) fn insert(&mut self, offset: usize, fetch_size: usize, total: usize, items: Vec<T>) {
        if self.fetch_size != fetch_size {
            self.pages.clear();
            if self
                .retained
                .as_ref()
                .is_some_and(|retained| retained.fetch_size != fetch_size.max(1))
            {
                self.discard_refresh();
            }
        }
        self.fetch_size = fetch_size;
        self.pages.insert(offset, items);
        self.columns = None;
        self.total = total;
        self.loaded = true;
        let selected = self.selection.selected().unwrap_or(0);
        self.selection
            .select((total != 0).then(|| selected.min(total - 1)));
        if self.identity_lookup.is_none()
            && let Some(retained) = &mut self.retained
        {
            retained.identity_scan = false;
        }
        self.set_viewport(self.viewport_rows);
        if let Some(retained) = &mut self.retained {
            retained.pages.remove(&offset);
        }
        if self
            .retained
            .as_ref()
            .is_some_and(|retained| !retained.identity_scan)
            && (self.selection.offset()..self.visible_end()).all(|index| self.get(index).is_some())
        {
            self.discard_refresh();
        }
    }

    pub(super) fn invalidate_identity(&mut self, identity: Option<ContainerIdentity>) {
        let scroll = self
            .identity_lookup
            .as_ref()
            .map_or(self.selection.offset(), |(_, _, scroll)| *scroll);
        self.invalidate();
        if let Some(retained) = &mut self.retained {
            retained.identity_scan = true;
        }
        self.identity_lookup = identity.map(|identity| (identity, 0, scroll));
        self.selection.select(Some(0));
        *self.selection.offset_mut() = 0;
    }

    pub(super) fn retain_identity(&mut self, identity: Option<ContainerIdentity>) {
        let scroll = self
            .identity_lookup
            .as_ref()
            .map_or(self.selection.offset(), |(_, _, scroll)| *scroll);
        self.clear();
        self.identity_lookup = identity.map(|identity| (identity, 0, scroll));
        self.selection.select(Some(0));
    }

    // Catalogs and directory containers have no protocol locate endpoint. Keep
    // the first page navigable; subsequent scan batches are transient.
    pub(super) fn resolve_identity(
        &mut self,
        offset: usize,
        total: usize,
        items: &[T],
        matches: impl Fn(&T, &ContainerIdentity) -> bool,
    ) -> bool {
        let Some((identity, next, scroll)) = &mut self.identity_lookup else {
            return true;
        };
        if let Some(index) = items.iter().position(|item| matches(item, identity)) {
            self.selection.select(Some(offset + index));
            *self.selection.offset_mut() = *scroll;
            self.identity_lookup = None;
            return true;
        }
        if !items.is_empty() && offset + items.len() < total {
            *next = offset + items.len();
            return offset == 0;
        }
        self.identity_lookup = None;
        self.selection.select((total != 0).then_some(0));
        self.discard_refresh();
        offset == 0
    }

    pub(super) fn get(&self, index: usize) -> Option<&T> {
        let fetch_size = self.fetch_size.max(1);
        let offset = index / fetch_size * fetch_size;
        self.pages.get(&offset)?.get(index - offset)
    }

    pub(super) fn selected(&self) -> Option<&T> {
        if self.display_offset() != self.selection.offset()
            || self.display_selected() != self.selection.selected()
        {
            return None;
        }
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
