use crate::model::Page;
use gpui::{
    AnyElement, App, Bounds, IntoElement, Pixels, Point, UniformListDecoration, Window, div,
};
use std::{collections::BTreeMap, ops::Range};

/// Measurement renders a sample row too. Only the viewport decoration is a demand signal.
pub(super) fn viewport_demand(
    callback: impl Fn(Range<usize>, &mut Window, &mut App) + 'static,
) -> impl UniformListDecoration {
    ViewportDemand(callback)
}
struct ViewportDemand<F>(F);
impl<F: Fn(Range<usize>, &mut Window, &mut App)> UniformListDecoration for ViewportDemand<F> {
    fn compute(
        &self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: Point<Pixels>,
        _: Pixels,
        _: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        (self.0)(range, window, cx);
        div().into_any_element()
    }
}

const MAX_PAGES: usize = 8;

/// Sparse absolute rows: scrolling changes demand, never the identity or size of the list.
/// Payload is bounded independently of the result total; evicted rows are fetched again.
pub(super) struct PagedRows<T> {
    pages: BTreeMap<usize, Vec<T>>,
    total: Option<usize>,
    page_size: usize,
    visible: Range<usize>,
    pinned: Option<usize>,
    pending: Option<usize>,
    failures: BTreeMap<usize, String>,
}

impl<T> Default for PagedRows<T> {
    fn default() -> Self {
        Self {
            pages: BTreeMap::new(),
            total: None,
            page_size: 1,
            visible: 0..1,
            pending: None,
            failures: BTreeMap::new(),
            pinned: None,
        }
    }
}

impl<T> PagedRows<T> {
    pub(super) fn len(&self) -> usize {
        self.total.unwrap_or(1)
    }
    pub(super) fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub(super) fn get(&self, index: usize) -> Option<&T> {
        let offset = index / self.page_size * self.page_size;
        self.pages.get(&offset)?.get(index - offset)
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = &T> {
        self.pages.values().flatten()
    }
    pub(super) fn position(&self, mut predicate: impl FnMut(&T) -> bool) -> Option<usize> {
        self.pages.iter().find_map(|(offset, rows)| {
            rows.iter()
                .position(&mut predicate)
                .map(|index| offset + index)
        })
    }
    pub(super) fn visible(&self) -> Range<usize> {
        self.visible.clone()
    }
    pub(super) fn pin(&mut self, index: Option<usize>) {
        self.pinned = index;
    }
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
    pub(super) fn invalidate(&mut self) {
        // A data revision can reorder IDs; a positional pin is no longer authoritative.
        self.pinned = None;
        self.pages.clear();
        self.pending = None;
        self.failures.clear();
    }
    fn configure_page_size(&mut self, page_size: usize) {
        if self.page_size != page_size {
            // Changing query boundaries alone does not reorder the underlying collection.
            let pinned = self.pinned;
            self.invalidate();
            self.pinned = pinned;
            self.page_size = page_size;
        }
    }
    pub(super) fn begin(&mut self, offset: usize, page_size: usize) {
        self.configure_page_size(page_size);
        self.pending = Some(offset);
    }
    pub(super) fn accept(&mut self, page: Page<T>) {
        let Some(offset) = self.pending.take() else {
            return;
        };
        self.total = Some(page.total);
        self.failures.remove(&offset);
        self.pages.retain(|start, _| *start < page.total);
        if offset < page.total {
            self.pages.insert(offset, page.rows);
        }
        if self.visible.start >= page.total {
            let start = page.total.saturating_sub(1);
            self.visible = start..page.total;
        }
        self.trim();
    }
    pub(super) fn fail(&mut self, error: String) {
        if let Some(offset) = self.pending.take() {
            self.failures.insert(offset, error);
            // Failed requests are bounded too; a new viewport may retry an evicted failure.
            while self.failures.len() > self.capacity() {
                let key = self.farthest(self.failures.keys().copied()).unwrap();
                self.failures.remove(&key);
            }
        }
    }
    pub(super) fn error(&self, index: usize) -> Option<&str> {
        self.failures
            .get(&(index / self.page_size * self.page_size))
            .map(String::as_str)
    }
    pub(super) fn retry(&mut self) {
        self.failures.clear();
    }
    pub(super) fn demand(&mut self, range: Range<usize>, page_size: usize) -> Option<usize> {
        self.configure_page_size(page_size);
        self.visible = range;
        if self.pending.is_some() || self.visible.is_empty() {
            return None;
        }
        let total = self.len();
        let start = self.visible.start.min(total.saturating_sub(1)) / page_size * page_size;
        let end = self.visible.end.min(total).max(start.saturating_add(1));
        // Visible rows precede one adjacent prefetch in each direction.
        let candidates = (start..end)
            .step_by(page_size)
            .chain(self.pinned.map(|index| index / page_size * page_size))
            .chain(std::iter::once(end.div_ceil(page_size) * page_size))
            .chain(start.checked_sub(page_size));
        candidates.into_iter().find(|offset| {
            *offset < total
                && !self.pages.contains_key(offset)
                && !self.failures.contains_key(offset)
        })
    }
    fn farthest(&self, offsets: impl Iterator<Item = usize>) -> Option<usize> {
        offsets
            .filter(|offset| !self.wanted(*offset))
            .max_by_key(|offset| offset.abs_diff(self.visible.start))
    }
    fn wanted(&self, offset: usize) -> bool {
        if self
            .pinned
            .is_some_and(|index| index / self.page_size * self.page_size == offset)
        {
            return true;
        }
        if self.visible.is_empty() {
            return false;
        }
        let first =
            (self.visible.start / self.page_size * self.page_size).saturating_sub(self.page_size);
        let last = self.visible.end.div_ceil(self.page_size) * self.page_size;
        (first..=last).contains(&offset)
    }
    fn capacity(&self) -> usize {
        let visible_pages = if self.visible.is_empty() {
            0
        } else {
            (self.visible.end - 1) / self.page_size - self.visible.start / self.page_size + 1
        };
        MAX_PAGES.max(visible_pages + 3)
    }
    fn trim(&mut self) {
        while self.pages.len() > self.capacity() {
            let offset = self.farthest(self.pages.keys().copied()).unwrap();
            self.pages.remove(&offset);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fill(rows: &mut PagedRows<usize>, offset: usize, total: usize) {
        rows.begin(offset, 20);
        rows.accept(Page {
            total,
            rows: (offset..(offset + 20).min(total)).collect(),
        });
    }
    #[test]
    fn distant_scroll_and_return_preserve_absolute_positions_with_bounded_payload() {
        let mut rows = PagedRows::default();
        fill(&mut rows, 0, 10_000);
        for offset in (20..400).step_by(20) {
            assert_eq!(rows.demand(offset..offset + 10, 20), Some(offset));
            fill(&mut rows, offset, 10_000);
            assert_eq!(rows.get(offset + 3), Some(&(offset + 3)));
            assert!(rows.pages.len() <= MAX_PAGES);
        }
        assert_eq!(rows.len(), 10_000);
        assert_eq!(rows.get(0), None);
        assert_eq!(rows.demand(0..10, 20), Some(0));
        fill(&mut rows, 0, 10_000);
        assert_eq!(rows.position(|row| *row == 3), Some(3));
    }

    #[test]
    fn tall_viewport_and_selected_page_converge_without_eviction_loops() {
        let mut rows = PagedRows::default();
        fill(&mut rows, 0, 10_000);
        rows.pin(Some(3));
        for offset in (20..400).step_by(20) {
            rows.demand(offset..offset + 10, 20);
            fill(&mut rows, offset, 10_000);
        }
        assert_eq!(rows.get(3), Some(&3));
        let mut requests = 0;
        while let Some(offset) = rows.demand(400..600, 20) {
            fill(&mut rows, offset, 10_000);
            requests += 1;
            assert!(requests <= 13, "viewport requests must converge");
        }
        assert!((400..600).all(|index| rows.get(index) == Some(&index)));
        assert_eq!(rows.get(3), Some(&3));
        assert!(rows.pages.len() <= 13);
    }
    #[test]
    fn errors_require_retry_and_do_not_block_other_visible_ranges() {
        let mut rows = PagedRows::<usize>::default();
        rows.begin(0, 20);
        rows.fail("offline".into());
        assert_eq!(rows.demand(0..1, 20), None);
        rows.retry();
        assert_eq!(rows.demand(0..1, 20), Some(0));
        fill(&mut rows, 0, 200);
        rows.begin(20, 20);
        rows.fail("offline".into());
        assert_eq!(rows.demand(100..110, 20), Some(100));
        assert_eq!(rows.error(23), Some("offline"));
        rows.begin(40, 20);
        rows.fail("offline".into());
        rows.retry();
        assert_eq!(rows.error(23), None);
        assert_eq!(rows.error(43), None);
        assert_eq!(rows.demand(20..60, 20), Some(20));
    }
    #[test]
    fn data_revision_discards_old_position_until_selection_is_relocated() {
        let mut rows = PagedRows::default();
        fill(&mut rows, 180, 400);
        rows.pin(Some(183));
        rows.invalidate();
        rows.begin(0, 20);
        rows.accept(Page {
            total: 400,
            rows: (0..20)
                .map(|index| if index == 3 { 183 } else { 1000 + index })
                .collect(),
        });
        // The old index is unrelated after reordering; only the visible neighbor is needed.
        assert_eq!(rows.demand(0..10, 20), Some(20));
        let selected = rows.position(|id| *id == 183);
        assert_eq!(selected, Some(3));
        rows.pin(selected);
        for offset in (20..400).step_by(20) {
            rows.demand(offset..offset + 10, 20);
            fill(&mut rows, offset, 400);
        }
        assert_eq!(rows.get(3), Some(&183));
        assert!(rows.pages.len() <= MAX_PAGES);
    }

    #[test]
    fn query_batch_resize_preserves_the_selected_absolute_position() {
        let mut rows = PagedRows::default();
        fill(&mut rows, 180, 400);
        rows.pin(Some(183));
        rows.begin(0, 25);
        rows.accept(Page {
            total: 400,
            rows: (0..25).collect(),
        });
        assert_eq!(rows.demand(0..10, 25), Some(175));
        rows.begin(175, 25);
        rows.accept(Page {
            total: 400,
            rows: (175..200).collect(),
        });
        assert_eq!(rows.get(183), Some(&183));
        assert_eq!(rows.demand(0..10, 25), Some(25));
    }

    #[test]
    fn shrinking_results_drop_stale_rows_and_clamp_demand() {
        let mut rows = PagedRows::default();
        fill(&mut rows, 80, 100);
        rows.demand(80..90, 20);
        rows.invalidate();
        fill(&mut rows, 80, 7);
        assert_eq!(rows.len(), 7);
        assert_eq!(rows.get(80), None);
        assert_eq!(rows.demand(80..90, 20), Some(0));
        fill(&mut rows, 0, 0);
        assert_eq!(rows.len(), 0);
        assert_eq!(rows.demand(0..1, 20), None);
    }
}
