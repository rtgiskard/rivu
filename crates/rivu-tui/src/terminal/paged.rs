use ratatui::widgets::ListState;
use rivu_presentation::keyboard::Action;
use rivu_presentation::refresh::RefreshDeadline;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

pub(super) struct RetainedPages<T> {
    pages: HashMap<usize, Vec<T>>,
    total: usize,
    fetch_size: usize,
    start: usize,
    selected: Option<usize>,
    identity_scan: bool,
    rows: usize,
    retention: RefreshDeadline,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum ContainerIdentity {
    Directory(PathBuf),
    OtherTracks,
    Playlist(i64),
}

pub(super) enum LookupStep {
    Hint(usize),
    Scan(usize),
}

pub(super) struct PagedList<T> {
    pub(super) pages: HashMap<usize, Vec<T>>,
    pub(super) total: usize,
    pub(super) loaded: bool,
    pub(super) fetch_size: usize,
    pub(super) selection: ListState,
    pub(super) viewport_rows: usize,
    // Target identity, search step, and the viewport to restore on success.
    pub(super) identity_lookup: Option<(ContainerIdentity, LookupStep, usize)>,
    pub(super) columns: Option<super::render::IntrinsicColumns>,
    pub(super) failed_request: Option<std::sync::Arc<super::worker::ReadSpec>>,
    pub(super) retained: Option<RetainedPages<T>>,
    pub(super) reorder_origin: Option<ListState>,
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
            reorder_origin: None,
        }
    }
}

impl<T> PagedList<T> {
    pub(super) fn begin_reorder(&mut self) {
        self.reorder_origin = Some(self.selection);
    }

    pub(super) fn end_reorder(&mut self) {
        if let Some(origin) = self.reorder_origin.take() {
            self.selection = origin;
            self.set_viewport(self.viewport_rows);
        }
    }

    pub(super) fn reorder_indices(&self) -> Option<(usize, usize)> {
        Some((
            self.reorder_origin.as_ref()?.selected()?,
            self.selection.selected()?,
        ))
    }

    fn source_index(&self, index: usize) -> usize {
        let Some((from, to)) = self.reorder_indices() else {
            return index;
        };
        if index == to {
            from
        } else if (from..to).contains(&index) {
            index + 1
        } else if (to < index) && (index <= from) {
            index - 1
        } else {
            index
        }
    }

    pub(super) fn clear(&mut self) {
        self.invalidate(false);
    }

    pub(super) fn clear_pages(&mut self) {
        self.pages.clear();
        self.discard_refresh();
        self.columns = None;
    }

    pub(super) fn invalidate(&mut self, retain_display: bool) {
        self.invalidate_pages(retain_display);
        self.total = 0;
        self.loaded = false;
        self.fetch_size = 0;
        if let Some((_, LookupStep::Scan(offset), _)) = &mut self.identity_lookup {
            *offset = 0;
        }
    }

    pub(super) fn invalidate_pages(&mut self, retain_display: bool) {
        if !retain_display {
            self.clear_pages();
            return;
        }
        let now = Instant::now();
        self.expire_refresh(now);
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
                retention: {
                    let mut retention = RefreshDeadline::default();
                    retention.begin(now);
                    retention
                },
            });
        } else if let Some(retained) = &mut self.retained
            && retained.retention.visible(now)
        {
            if self.loaded && !retained.identity_scan {
                retained.total = self.total;
            }
            retained.pages.extend(self.pages.drain());
        }
        self.pages.clear();
        let origin = self.reorder_indices().map(|(from, _)| from);
        if let Some(retained) = &mut self.retained {
            let end = start.saturating_add(retained.rows).min(retained.total);
            let (start, end) = if origin.is_some() {
                (start.saturating_sub(1), end.saturating_add(1))
            } else {
                (start, end)
            };
            retained.pages.retain(|&offset, items| {
                let page_end = offset.saturating_add(items.len());
                (offset < end && page_end > start)
                    || origin.is_some_and(|from| (offset..page_end).contains(&from))
            });
        }
        self.columns = None;
    }

    pub(super) fn refresh_deadline(&self) -> Option<Instant> {
        self.retained
            .as_ref()
            .and_then(|retained| retained.retention.deadline())
    }

    pub(super) fn expire_refresh(&mut self, now: Instant) -> bool {
        let Some(retained) = &mut self.retained else {
            return false;
        };
        if !retained.retention.expire(now) {
            return false;
        }
        retained.pages.clear();
        // Keep the expired interval until fresh visible rows restore authority.
        // Repeated invalidation must not start another stale display interval.
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
            retained.retention.visible(Instant::now())
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
        let source = self.source_index(index);
        if let Some(item) = self.get(source) {
            return Some(item);
        }
        let retained = self.retained_display()?;
        if index < retained.start || index >= retained.start.saturating_add(retained.rows) {
            return None;
        }
        let offset = source / retained.fetch_size * retained.fetch_size;
        retained.pages.get(&offset)?.get(source - offset)
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
                        let source = offset.saturating_add(index);
                        let visible = (start..end).contains(&source);
                        let shifted = self.reorder_indices().is_some_and(|(from, _)| {
                            source == from || (start.saturating_sub(1)..=end).contains(&source)
                        });
                        (visible || shifted).then_some(item)
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
        if let Some((_, step, _)) = &self.identity_lookup {
            return Some(match *step {
                LookupStep::Hint(row) => row / fetch_size * fetch_size,
                LookupStep::Scan(offset) => offset,
            });
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
        // The held row can be outside the viewport. Keep only its batch.
        if let Some((from, _)) = self.reorder_indices() {
            let origin = from / fetch_size * fetch_size;
            if missing(origin) {
                return Some(origin);
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
        let held = self
            .reorder_indices()
            .map(|(from, _)| from / fetch_size * fetch_size);
        // Keep the viewport, its neighbors, and at most one held-row batch.
        self.retain_pages(|offset, _| (first..=last).contains(&offset) || Some(offset) == held);
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
            && (self.selection.offset()..self.visible_end())
                .all(|index| self.get(self.source_index(index)).is_some())
        {
            self.discard_refresh();
        }
    }

    pub(super) fn invalidate_identity(
        &mut self,
        identity: Option<ContainerIdentity>,
        retain_display: bool,
    ) {
        let scroll = self
            .identity_lookup
            .as_ref()
            .map_or(self.selection.offset(), |(_, _, scroll)| *scroll);
        self.invalidate(retain_display);
        if let Some(retained) = &mut self.retained {
            retained.identity_scan = true;
        }
        self.identity_lookup = identity.map(|identity| {
            let step = match self.identity_lookup.take() {
                Some((target, step @ LookupStep::Hint(_), _)) if target == identity => step,
                _ => LookupStep::Scan(0),
            };
            (identity, step, scroll)
        });
        self.selection.select(Some(0));
        *self.selection.offset_mut() = 0;
    }

    pub(super) fn retain_identity(&mut self, identity: Option<ContainerIdentity>) {
        let scroll = self
            .identity_lookup
            .as_ref()
            .map_or(self.selection.offset(), |(_, _, scroll)| *scroll);
        self.clear();
        self.identity_lookup = identity.map(|identity| (identity, LookupStep::Scan(0), scroll));
        self.selection.select(Some(0));
    }

    pub(super) fn locate_identity(
        &mut self,
        locate: impl FnOnce(&ContainerIdentity) -> Option<usize>,
    ) -> bool {
        let Some((identity, _, scroll)) = self.identity_lookup.take() else {
            return false;
        };
        if let Some(index) = locate(&identity) {
            self.selection.select(Some(index));
            *self.selection.offset_mut() = scroll;
        } else {
            self.selection.select(Some(0));
            *self.selection.offset_mut() = 0;
            self.discard_refresh();
        }
        true
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
        let Some((identity, step, scroll)) = &mut self.identity_lookup else {
            return true;
        };
        if let Some(index) = items.iter().position(|item| matches(item, identity)) {
            self.selection.select(Some(offset + index));
            *self.selection.offset_mut() = *scroll;
            self.identity_lookup = None;
            return true;
        }
        // A hint may land beyond the end or skip earlier pages. On a miss,
        // scan from the beginning, without requesting page zero twice.
        if matches!(step, LookupStep::Hint(_)) && offset != 0 {
            *step = LookupStep::Scan(0);
            return false;
        }
        if !items.is_empty() && offset + items.len() < total {
            *step = LookupStep::Scan(offset + items.len());
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
        self.get(self.source_index(self.selection.selected()?))
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
