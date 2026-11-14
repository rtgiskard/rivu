use rivu_protocol::model::Page;
use rivu_ui::refresh::RefreshDeadline;
use std::{collections::BTreeMap, ops::Range, time::Instant};

const MAX_PAGES: usize = 8;

/// Sparse absolute rows: scrolling changes demand, never the identity or size of the list.
/// Payload is bounded independently of the result total; evicted rows are fetched again.
pub(super) struct PagedRows<T> {
    pages: BTreeMap<usize, Vec<T>>,
    // Same-query display only; never authorizes positions or actions.
    retained: BTreeMap<usize, Vec<T>>,
    retention: RefreshDeadline,
    total: Option<usize>,
    // Retained geometry is not an authoritative count after a data revision.
    layout_len: usize,
    page_size: usize,
    visible: Range<usize>,
    pinned: Option<usize>,
    pending: Option<usize>,
    failures: BTreeMap<usize, String>,
    retired: bool,
}

impl<T> Default for PagedRows<T> {
    fn default() -> Self {
        Self {
            pages: BTreeMap::new(),
            retained: BTreeMap::new(),
            retention: RefreshDeadline::default(),
            total: None,
            layout_len: 1,
            page_size: 1,
            visible: 0..1,
            pending: None,
            failures: BTreeMap::new(),
            pinned: None,
            retired: false,
        }
    }
}

impl<T> PagedRows<T> {
    pub(super) fn total(&self) -> Option<usize> {
        self.total
    }
    pub(super) fn len(&self) -> usize {
        self.layout_len
    }
    pub(super) fn set_total(&mut self, total: usize) {
        self.total = Some(total);
        self.layout_len = total;
    }
    pub(super) fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub(super) fn get(&self, index: usize) -> Option<&T> {
        let offset = index / self.page_size * self.page_size;
        self.pages.get(&offset)?.get(index - offset)
    }
    pub(super) fn display(&self, index: usize) -> Option<&T> {
        if index >= self.len() {
            return None;
        }
        self.get(index)
            .or_else(|| self.retained_at(index, Instant::now()))
    }
    #[cfg(test)]
    fn display_at(&self, index: usize, now: Instant) -> Option<&T> {
        if index >= self.len() {
            return None;
        }
        self.get(index).or_else(|| self.retained_at(index, now))
    }
    fn retained_at(&self, index: usize, now: Instant) -> Option<&T> {
        if !self.retention.visible(now) {
            return None;
        }
        let offset = index / self.page_size * self.page_size;
        self.retained.get(&offset)?.get(index - offset)
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
    /// Keep only viewport presentation; get/iter/position remain authoritative.
    pub(super) fn invalidate(&mut self) {
        self.refresh_at(Instant::now());
    }
    pub(super) fn refresh_at(&mut self, now: Instant) {
        self.expire(now);
        self.retention.begin(now);
        self.retained.append(&mut self.pages);
        self.retired = false;
        self.total = None;
        self.layout_len = self.layout_len.max(1);
        if self.visible.is_empty() {
            self.visible = 0..1;
        }
        // A data revision can reorder IDs; a positional pin is no longer authoritative.
        self.pinned = None;
        self.pending = None;
        self.failures.clear();
        self.trim_retained();
    }
    pub(super) fn refresh_deadline(&self) -> Option<Instant> {
        self.retention.deadline()
    }
    pub(super) fn expire(&mut self, now: Instant) -> bool {
        if !self.retention.expired(now) {
            return false;
        }
        self.retention.clear();
        let changed = !self.retained.is_empty();
        self.retained.clear();
        changed
    }
    fn clear_pages(&mut self) {
        self.pages.clear();
        self.retained.clear();
        self.retention.clear();
        self.pending = None;
        self.failures.clear();
    }
    /// Retain stale technical fields briefly without changing list identity or geometry.
    pub(super) fn invalidate_payload(&mut self) {
        let (total, layout_len, pinned) = (self.total, self.layout_len, self.pinned);
        self.invalidate();
        self.total = total;
        self.layout_len = layout_len;
        self.pinned = pinned;
    }
    /// Change batch boundaries without invalidating current counts or selection position.
    pub(super) fn configure_page_size(&mut self, page_size: usize) {
        if self.page_size != page_size {
            self.clear_pages();
            self.page_size = page_size;
        }
    }
    pub(super) fn begin(&mut self, offset: usize, page_size: usize) {
        self.configure_page_size(page_size);
        self.pending = Some(offset);
    }
    /// Retire a logical page request without invalidating payload, total, or failures.
    /// The consumer owns generation checks and retains its physical worker gate.
    pub(super) fn cancel_pending(&mut self) {
        self.pending = None;
    }
    /// Core rejected this snapshot. Quietly park demand until fresh authority or Retry.
    pub(super) fn retire_pending(&mut self) {
        self.pending = None;
        self.retired = true;
        self.retained.clear();
    }
    pub(super) fn accept(&mut self, page: Page<T>) -> bool
    where
        T: PartialEq,
    {
        self.accept_at(page, Instant::now())
    }
    fn accept_at(&mut self, page: Page<T>, now: Instant) -> bool
    where
        T: PartialEq,
    {
        let Some(offset) = self.pending.take() else {
            return false;
        };
        let previous = self.pages.get(&offset).or_else(|| {
            self.retention
                .visible(now)
                .then(|| self.retained.get(&offset))
                .flatten()
        });
        let changed = self.layout_len != page.total || previous != Some(&page.rows);
        self.set_total(page.total);
        self.failures.remove(&offset);
        self.pages.retain(|start, _| *start < page.total);
        self.retained
            .retain(|start, _| *start < page.total && *start != offset);
        if offset < page.total {
            self.pages.insert(offset, page.rows);
        }
        if self.visible.start >= page.total {
            let start = page.total.saturating_sub(1);
            self.visible = start..page.total;
        }
        self.trim();
        if self.retained.is_empty() {
            self.retention.clear();
        }
        changed
    }
    pub(super) fn fail(&mut self, error: String) {
        if let Some(offset) = self.pending.take() {
            self.failures.insert(offset, error);
            self.retained.remove(&offset);
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
    pub(super) fn retired(&self) -> bool {
        self.retired
    }

    pub(super) fn retry(&mut self) {
        self.failures.clear();
        self.retired = false;
    }
    pub(super) fn demand(&mut self, range: Range<usize>, page_size: usize) -> Option<usize> {
        self.configure_page_size(page_size);
        self.visible = range;
        self.trim();
        if self.retired || self.pending.is_some() || self.visible.is_empty() {
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
    fn trim_retained(&mut self) {
        let first = self.visible.start / self.page_size * self.page_size;
        let end = self.visible.end;
        self.retained
            .retain(|offset, _| *offset >= first && *offset < end);
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
        self.trim_retained();
        while self.pages.len() > self.capacity() {
            let offset = self.farthest(self.pages.keys().copied()).unwrap();
            self.pages.remove(&offset);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn fixed_retention_boundary_and_equal_replacement() {
        let mut rows = super::PagedRows::default();
        fill(&mut rows, 0, 20);
        let now = std::time::Instant::now();
        rows.refresh_at(now);
        let deadline = now + rivu_ui::refresh::REFRESH_RETENTION;
        rows.refresh_at(deadline - std::time::Duration::from_millis(1));
        rows.retry();
        assert_eq!(rows.refresh_deadline(), Some(deadline));
        assert_eq!(
            rows.display_at(0, deadline - std::time::Duration::from_nanos(1)),
            Some(&0)
        );
        assert_eq!(rows.get(0), None);
        assert_eq!(rows.position(|row| *row == 0), None);
        assert_eq!(rows.iter().next(), None);
        assert_eq!(rows.display_at(0, deadline), None);
        rows.begin(0, 20);
        assert!(!rows.accept_at(
            rivu_protocol::model::Page {
                total: 20,
                rows: (0..20).collect()
            },
            now
        ));
        rows.refresh_at(now);
        rows.begin(0, 20);
        assert!(rows.accept_at(
            rivu_protocol::model::Page {
                total: 20,
                rows: (0..20).rev().collect()
            },
            now
        ));
        assert_eq!(rows.display_at(0, now), Some(&19));
        rows.refresh_at(now);
        assert!(rows.expire(deadline));
        assert!(!rows.expire(deadline));
    }

    #[test]
    fn failure_and_shrink_remove_retained_display_immediately() {
        let mut rows = PagedRows::default();
        fill(&mut rows, 0, 20);
        let now = Instant::now();
        rows.refresh_at(now);
        rows.begin(0, 20);
        rows.fail("offline".into());
        assert_eq!(rows.display_at(0, now), None);
        assert_eq!(rows.error(0), Some("offline"));
        rows.retry();
        assert_eq!(
            rows.refresh_deadline(),
            Some(now + rivu_ui::refresh::REFRESH_RETENTION)
        );
        rows.begin(0, 20);
        rows.accept_at(
            Page {
                total: 20,
                rows: (0..20).collect(),
            },
            now,
        );
        rows.refresh_at(now);
        rows.begin(0, 20);
        assert!(rows.accept_at(
            Page {
                total: 1,
                rows: vec![0]
            },
            now
        ));
        assert_eq!(rows.display_at(0, now), Some(&0));
        assert_eq!(rows.display_at(1, now), None);
    }

    use super::*;
    fn fill(rows: &mut PagedRows<usize>, offset: usize, total: usize) {
        rows.begin(offset, 20);
        rows.accept(Page {
            total,
            rows: (offset..(offset + 20).min(total)).collect(),
        });
    }

    #[test]
    fn refresh_retains_display_without_authorizing_rows_or_skipping_reload() {
        let mut rows = PagedRows::default();
        fill(&mut rows, 0, 100);
        fill(&mut rows, 20, 100);
        rows.demand(0..30, 20);
        rows.invalidate();
        assert_eq!(rows.display(3), Some(&3));
        assert_eq!(rows.display(23), Some(&23));
        assert_eq!(rows.get(3), None);
        assert_eq!(rows.iter().next(), None);
        assert_eq!(rows.position(|row| *row == 3), None);
        assert_eq!(rows.total(), None);
        assert_eq!(rows.demand(0..30, 20), Some(0));
        rows.begin(0, 20);
        rows.accept(Page {
            total: 100,
            rows: (100..120).collect(),
        });
        assert_eq!(rows.display(3), Some(&103));
        assert_eq!(rows.display(23), Some(&23));
        rows.invalidate();
        assert_eq!(rows.display(3), Some(&103));
        assert_eq!(rows.display(23), Some(&23));
        assert_eq!(rows.demand(60..70, 20), Some(60));
        assert_eq!(rows.display(3), None);
        assert_eq!(rows.display(23), None);
    }

    #[test]
    fn refresh_does_not_mask_failure_removal_or_query_reset() {
        for failure in [false, true] {
            let mut rows = PagedRows::default();
            fill(&mut rows, 0, 100);
            rows.invalidate();
            rows.begin(0, 20);
            if failure {
                rows.fail("unavailable".into());
                assert_eq!(rows.error(3), Some("unavailable"));
            } else {
                rows.accept(Page {
                    total: 0,
                    rows: vec![],
                });
                assert_eq!(rows.total(), Some(0));
            }
            assert_eq!(rows.display(3), None);
        }
        let mut rows = PagedRows::default();
        fill(&mut rows, 0, 100);
        rows.invalidate();
        rows.clear();
        assert_eq!(rows.display(3), None);
        fill(&mut rows, 0, 100);
        rows.invalidate();
        rows.begin(0, 20);
        rows.retire_pending();
        assert_eq!(rows.display(3), None);
    }

    #[test]
    fn retired_page_keeps_rows_and_geometry_without_replaying_unchanged_demand() {
        let mut rows = PagedRows::default();
        fill(&mut rows, 0, 100);
        rows.begin(20, 20);
        rows.retire_pending();
        assert_eq!(rows.get(3), Some(&3));
        assert_eq!(rows.total(), Some(100));
        assert!(rows.error(20).is_none());
        assert_eq!(rows.demand(20..40, 20), None);
        rows.configure_page_size(10);
        assert_eq!(rows.demand(20..40, 10), None);
        rows.retry();
        assert_eq!(rows.demand(20..40, 10), Some(20));
        rows.begin(20, 10);
        rows.retire_pending();
        rows.invalidate();
        assert_eq!(rows.demand(20..40, 10), Some(20));
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
        assert!(rows.pages.len() <= MAX_PAGES);
        assert!(rows.iter().count() <= 160);
        fill(&mut rows, 0, 10_000);
        assert_eq!(rows.position(|row| *row == 3), Some(3));
    }

    #[test]
    fn data_revision_forgets_counts_but_keeps_reloadable_viewport_geometry() {
        for total in [0, 100] {
            let mut rows = PagedRows::default();
            fill(&mut rows, 80, total);
            if total > 0 {
                rows.demand(80..90, 20);
            }
            rows.invalidate();
            assert_eq!(rows.total(), None);
            assert_eq!(rows.len(), total.max(1));
            let offset = if total == 0 { 0 } else { 80 };
            assert_eq!(rows.demand(rows.visible(), 20), Some(offset));
            fill(&mut rows, offset, 101);
            assert_eq!(rows.total(), Some(101));
            assert_eq!(rows.get(offset), Some(&offset));
        }
    }

    #[test]
    fn batch_resize_retains_confirmed_empty_results_until_data_changes() {
        let mut rows = PagedRows::default();
        fill(&mut rows, 0, 0);
        rows.configure_page_size(25);
        assert_eq!(rows.total(), Some(0));
        assert!(rows.is_empty());
        assert_eq!(rows.demand(0..1, 25), None);

        rows.invalidate();
        rows.configure_page_size(30);
        assert_eq!(rows.total(), None);
        assert_eq!(rows.demand(rows.visible(), 30), Some(0));
        rows.set_total(0);
        assert_eq!(rows.demand(0..1, 30), None);
    }

    #[test]
    fn cancelling_pending_demand_retains_payload_identity_and_explicit_failures() {
        let mut rows = PagedRows::default();
        fill(&mut rows, 0, 100);
        rows.pin(Some(3));
        rows.begin(20, 20);
        rows.fail("offline".into());
        rows.begin(40, 20);
        rows.cancel_pending();
        rows.accept(Page {
            total: 0,
            rows: Vec::new(),
        });
        assert_eq!(rows.len(), 100);
        assert_eq!(rows.total(), Some(100));
        assert_eq!(rows.get(3), Some(&3));
        assert_eq!(rows.pinned, Some(3));
        assert!(rows.error(20).is_some());
        assert_eq!(rows.demand(40..50, 20), Some(40));
    }

    #[test]
    fn authoritative_total_exposes_last_queue_row_before_any_page_is_loaded() {
        let mut rows = PagedRows::default();
        rows.set_total(200_000);
        assert_eq!(rows.len(), 200_000);
        assert_eq!(rows.demand(199_990..200_000, 20), Some(199_980));
        fill(&mut rows, 199_980, 200_000);
        assert_eq!(rows.get(199_999), Some(&199_999));
        for offset in (0..400).step_by(20) {
            rows.demand(offset..offset + 10, 20);
            fill(&mut rows, offset, 200_000);
            assert!(rows.pages.len() <= MAX_PAGES);
        }
        assert_eq!(rows.get(199_999), None);
        assert_eq!(rows.demand(199_990..200_000, 20), Some(199_980));
        fill(&mut rows, 199_980, 200_000);
        assert_eq!(rows.get(199_999), Some(&199_999));
        rows.invalidate();
        rows.set_total(3);
        assert_eq!(rows.get(199_999), None);
        assert_eq!(rows.demand(199_990..200_000, 20), Some(0));
        fill(&mut rows, 0, 3);
        assert_eq!(rows.get(2), Some(&2));
        assert_eq!(rows.get(3), None);
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
        assert!(rows.error(23).is_some());
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
        assert_eq!(rows.total(), Some(400));
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
