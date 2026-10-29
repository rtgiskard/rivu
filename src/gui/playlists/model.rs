//! Playlist identities, bounded pages and scroll positions.
//! Request lifetimes are owned by the playlist feature's paired request state.
use crate::gui::{paged::PagedRows, playlists::selection};
use gpui::ScrollStrategy;
use rivu_protocol::model::{Page, PlaylistEntryLocation, PlaylistEntryRow, PlaylistSummary, Query};
use std::ops::Range;

#[path = "requests.rs"]
pub(super) mod requests;

#[derive(Default)]
pub(super) struct PlaylistModel {
    pub(super) playlists: PlaylistBrowser,
    requests: requests::ViewRequests,
    page_size: usize,
    focus: Option<crate::gui::BrowserFocus>,
}

#[derive(Default)]
pub(in crate::gui) struct PlaylistSummaryBuffer {
    pub(in crate::gui) query: Option<String>,
    pub(in crate::gui) offset: usize,
    pub(in crate::gui) rows: PagedRows<PlaylistSummary>,
    pub(in crate::gui) scroll: gpui::UniformListScrollHandle,
}

pub(in crate::gui) struct PlaylistEntriesBuffer {
    pub(in crate::gui) query: Option<String>,
    pub(in crate::gui) playlist_id: Option<i64>,
    pub(in crate::gui) offset: usize,
    pub(in crate::gui) rows: PagedRows<PlaylistEntryRow>,
    pub(in crate::gui) scroll: gpui::UniformListScrollHandle,
}

impl Default for PlaylistEntriesBuffer {
    fn default() -> Self {
        // Without a selected playlist there is no unknown page to load.
        let mut rows = PagedRows::default();
        rows.set_total(0);
        Self {
            playlist_id: None,
            query: None,
            offset: 0,
            rows,
            scroll: Default::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) struct PlaylistRefresh {
    pub(in crate::gui) summaries: bool,
    pub(in crate::gui) entries: bool,
    pub(in crate::gui) entry_payload_only: bool,
}

#[derive(Default)]
pub(in crate::gui) struct PlaylistBrowser {
    summaries: PlaylistSummaryBuffer,
    entries: PlaylistEntriesBuffer,
    selected: Option<i64>,
    pub(in crate::gui) track_selection: selection::EntrySelection,
    selected_name: String,
    selected_total: Option<usize>,
    selection_generation: u64,
}

impl PlaylistBrowser {
    pub(super) fn accept_selected_summary(&mut self, summary: PlaylistSummary) {
        self.selected_name = summary.name;
        self.selected_total = Some(summary.entry_count);
    }
    pub(in crate::gui) fn selected_total(&self) -> Option<usize> {
        self.selected_total
    }
    pub(super) fn search_summaries(&mut self, query: Option<String>) {
        self.summaries.query = query;
        self.summaries.rows.clear();
        self.summaries.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }
    pub(super) fn search_entries(&mut self, query: Option<String>) {
        self.track_selection.query = query.clone();
        self.entries.query = query;
        self.track_selection.clear();
        self.entries.rows.clear();
        self.entries.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }
    pub(in crate::gui) fn refresh_changes(
        &self,
        previous: &crate::projection::GuiLibrarySnapshot,
        next: &crate::projection::GuiLibrarySnapshot,
    ) -> PlaylistRefresh {
        let summaries_query = Query::PlaylistSummaries {
            query: self.summaries.query.clone(),
            offset: 0,
            limit: 1,
        };
        let summaries = summaries_query.stamp(previous.query_revisions(None))
            != summaries_query.stamp(next.query_revisions(None));
        let (entries, entry_payload_only) =
            self.entries
                .playlist_id
                .map_or((false, false), |playlist_id| {
                    let query = Query::PlaylistEntries {
                        query: self.entries.query.clone(),
                        playlist_id,
                        offset: 0,
                        limit: 1,
                    };
                    let mut previous = previous.query_revisions(Some(playlist_id));
                    let next = next.query_revisions(Some(playlist_id));
                    let changed = query.stamp(previous) != query.stamp(next);
                    previous.bitrate = next.bitrate;
                    (
                        changed,
                        changed && query.stamp(previous) == query.stamp(next),
                    )
                });
        PlaylistRefresh {
            summaries,
            entries,
            entry_payload_only,
        }
    }

    pub(in crate::gui) fn summaries(&self) -> &PlaylistSummaryBuffer {
        &self.summaries
    }

    pub(in crate::gui) fn entries(&self) -> &PlaylistEntriesBuffer {
        &self.entries
    }

    pub(super) fn invalidate_entry_payload(&mut self) {
        self.entries.rows.invalidate_payload();
    }
    pub(super) fn invalidate_summaries(&mut self) {
        self.summaries.rows.invalidate();
        self.selected_total = None;
    }

    pub(super) fn configure_page_size(&mut self, page_size: usize) {
        self.summaries.rows.configure_page_size(page_size);
        self.entries.rows.configure_page_size(page_size);
    }

    pub(super) fn cancel_page(&mut self, entries: bool) {
        if entries {
            self.entries.rows.cancel_pending();
        } else {
            self.summaries.rows.cancel_pending();
        }
    }

    pub(super) fn prepare_page(&mut self, entries: bool, offset: usize, page_size: usize) -> Query {
        let offset = offset / page_size * page_size;
        if entries {
            self.entries.offset = offset;
            self.entries.rows.begin(offset, page_size);
            Query::PlaylistEntries {
                query: self.entries.query.clone(),
                playlist_id: self
                    .entries
                    .playlist_id
                    .expect("entries prepared for a playlist"),
                offset,
                limit: page_size,
            }
        } else {
            self.summaries.offset = offset;
            self.summaries.rows.begin(offset, page_size);
            Query::PlaylistSummaries {
                query: self.summaries.query.clone(),
                offset,
                limit: page_size,
            }
        }
    }

    pub(super) fn demand_page(
        &mut self,
        entries: bool,
        range: Range<usize>,
        page_size: usize,
    ) -> Option<usize> {
        if entries {
            self.entries.rows.demand(range, page_size)
        } else {
            self.summaries.rows.demand(range, page_size)
        }
    }

    pub(super) fn retry_page(&mut self, entries: bool, page_size: usize) -> Option<usize> {
        let range = if entries {
            self.entries.rows.retry();
            self.entries.rows.visible()
        } else {
            self.summaries.rows.retry();
            self.summaries.rows.visible()
        };
        self.demand_page(entries, range, page_size)
    }

    pub(super) fn fail_page(&mut self, entries: bool, message: String) {
        if entries {
            self.entries.rows.fail(message);
        } else {
            self.summaries.rows.fail(message);
        }
    }

    pub(in crate::gui) fn selected(&self) -> Option<i64> {
        self.selected
    }
    pub(in crate::gui) fn selection_generation(&self) -> u64 {
        self.selection_generation
    }
    pub(in crate::gui) fn selected_entry(&self) -> Option<i64> {
        self.track_selection.active.map(|row| row.id)
    }

    pub(super) fn select(&mut self, id: i64, name: String, index: usize) -> bool {
        let changed = self.selected != Some(id);
        if changed {
            self.selection_generation = self.selection_generation.wrapping_add(1);
            self.track_selection.clear();
        }
        self.selected = Some(id);
        self.selected_name = name;
        self.selected_total = self
            .summaries
            .rows
            .get(index)
            .filter(|row| row.id == id)
            .map(|row| row.entry_count);
        self.summaries.rows.pin(Some(index));
        changed
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.selected = None;
        self.selected_total = None;
        self.track_selection.clear();
        self.selected_name.clear();
        self.summaries.rows.pin(None);
        self.entries = PlaylistEntriesBuffer::default();
    }

    #[cfg(test)]
    pub(super) fn select_entry(&mut self, id: i64, index: usize) {
        let track_id = self
            .entries
            .rows
            .get(index)
            .filter(|row| row.id == id)
            .map(|row| row.track_id)
            .unwrap_or(0);
        self.track_selection.select(
            selection::EntryIdentity {
                id,
                track_id,
                position: index,
                offset: index,
            },
            false,
        );
        self.sync_active_entry();
    }

    pub(super) fn sync_active_entry(&mut self) {
        self.entries.rows.pin(
            self.track_selection
                .active
                .filter(|_| self.track_selection.ready())
                .map(|row| row.offset),
        );
    }

    pub(super) fn prepare_entries(&mut self, id: i64) {
        let changed = self.entries.playlist_id != Some(id);
        if changed {
            self.entries.query = None;
            self.track_selection.query = None;
            self.track_selection.clear();
            self.entries.rows.clear();
            self.entries.scroll.scroll_to_item(0, ScrollStrategy::Top);
        } else {
            self.entries.rows.invalidate();
        }
        self.entries.playlist_id = Some(id);
    }

    pub(super) fn reconcile_selection(&mut self) {
        // Eviction is not deletion; identities survive unloaded pages.
        if self.selected.is_none() || self.entries.playlist_id != self.selected {
            self.track_selection.clear();
        }
    }

    pub(super) fn accept_summaries(&mut self, page: Page<PlaylistSummary>) -> bool {
        let contains_selected =
            if let Some(summary) = page.rows.iter().find(|row| Some(row.id) == self.selected) {
                self.selected_name.clone_from(&summary.name);
                self.selected_total = Some(summary.entry_count);
                true
            } else {
                false
            };
        self.summaries.rows.accept(page);
        if let Some(index) = self
            .summaries
            .rows
            .position(|row| Some(row.id) == self.selected)
        {
            self.summaries.rows.pin(Some(index));
        }
        contains_selected
    }

    pub(super) fn accept_entries(&mut self, page: Page<PlaylistEntryRow>) {
        self.entries.rows.accept(page);
        if let Some(index) = self
            .entries
            .rows
            .position(|row| Some(row.id) == self.selected_entry())
        {
            self.entries.rows.pin(Some(index));
        }
    }

    pub(super) fn accept_entry_locations(&mut self, mut rows: Vec<PlaylistEntryLocation>) {
        rows.retain(|row| Some(row.playlist_id) == self.entries.playlist_id);
        if let Some(row) = rows.first() {
            self.entries.rows.set_total(row.total);
        }
        self.track_selection
            .reconcile(rows.iter().map(selection::EntryIdentity::from).collect());
        self.sync_active_entry();
    }

    #[cfg(test)]
    pub(in crate::gui) fn entry_ready(&self) -> bool {
        self.track_selection.active.is_some() && self.track_selection.ready()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    fn summary(id: i64, entry_count: usize) -> PlaylistSummary {
        PlaylistSummary {
            id,
            name: format!("playlist {id}"),
            entry_count,
        }
    }

    #[test]
    fn empty_playlist_catalog_reloads_after_creation_and_last_deletion() {
        for query in [None, Some("playlist".into())] {
            let mut playlists = PlaylistBrowser::default();
            playlists.search_summaries(query);
            playlists.prepare_page(false, 0, 20);
            playlists.accept_summaries(Page {
                total: 0,
                rows: vec![],
            });
            assert_eq!(playlists.demand_page(false, 0..1, 20), None);

            for id in [1, 2] {
                playlists.invalidate_summaries();
                let range = 0..playlists.summaries().rows.len();
                assert_eq!(playlists.demand_page(false, range, 20), Some(0));
                playlists.prepare_page(false, 0, 20);
                playlists.accept_summaries(Page {
                    total: 1,
                    rows: vec![summary(id, 0)],
                });
                assert_eq!(playlists.summaries().rows.get(0).unwrap().id, id);

                playlists.invalidate_summaries();
                assert_eq!(playlists.demand_page(false, 0..1, 20), Some(0));
                playlists.prepare_page(false, 0, 20);
                playlists.accept_summaries(Page {
                    total: 0,
                    rows: vec![],
                });
                assert!(playlists.summaries().rows.is_empty());
                assert_eq!(playlists.demand_page(false, 0..1, 20), None);
            }
        }
    }

    #[test]
    fn playlist_batch_resize_preserves_counts_selection_and_viewport() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.prepare_page(false, 0, 20);
        playlists.accept_summaries(Page {
            total: 1,
            rows: vec![summary(1, 100)],
        });
        playlists.prepare_page(true, 80, 20);
        playlists.accept_entries(Page {
            total: 100,
            rows: (80..100)
                .map(|index| PlaylistEntryRow {
                    id: index as i64,
                    track_id: index as i64,
                    position: index,
                    offset: index,
                    title: format!("entry {index}"),
                    path: Default::default(),
                    channels: 0,
                    sample_rate: 0,
                    bits_per_sample: None,
                    bitrate_bps: None,
                    bitrate_exact: false,
                    cue_number: None,
                    artist: String::new(),
                    album: String::new(),
                    duration: None,
                    codec: String::new(),
                    missing: false,
                    favorite: false,
                })
                .collect(),
        });
        playlists.select_entry(83, 83);
        playlists.demand_page(true, 80..90, 20);
        playlists.configure_page_size(25);
        assert_eq!(playlists.summaries().rows.total(), Some(1));
        assert_eq!(playlists.entries().rows.total(), Some(100));
        assert_eq!(playlists.entries().rows.visible(), 80..90);
        assert_eq!(playlists.selected_entry(), Some(83));
        assert!(playlists.entry_ready());
        assert_eq!(playlists.demand_page(true, 80..90, 25), Some(75));
        playlists.prepare_entries(1);
        assert_eq!(playlists.entries().rows.total(), None);
        assert_eq!(playlists.entries().rows.len(), 100);
    }

    #[test]
    fn playlist_lookup_distinguishes_unloaded_empty_and_deleted_playlists() {
        let mut browser = PlaylistModel::default();
        browser.playlists.select(41, "selected".into(), 40);
        browser.playlists.prepare_entries(41);
        browser.playlists.prepare_page(false, 0, 20);
        browser.playlists.accept_summaries(Page {
            total: 100,
            rows: (1..=20).map(|id| summary(id, 0)).collect(),
        });
        assert_eq!(browser.playlists.selected(), Some(41));
        browser.accept_playlist_lookup(41, Some(summary(41, 0)));
        assert_eq!(browser.playlists.selected(), Some(41));
        browser.playlists.select_entry(99, 0);
        browser.accept_playlist_lookup(41, Some(summary(42, 1)));
        assert_eq!(browser.playlists.selected(), None);
        assert_eq!(browser.playlists.selected_entry(), None);
        assert_eq!(browser.playlists.entries().playlist_id, None);
        assert!(browser.playlists.entries().rows.is_empty());
        assert!(browser.playlists.selected_name.is_empty());
    }

    #[test]
    fn stale_playlist_lookup_cannot_clear_another_selection() {
        let mut browser = PlaylistModel::default();
        browser.playlists.select(1, "one".into(), 0);
        browser.playlists.select(2, "two".into(), 1);
        browser.playlists.prepare_entries(2);
        browser.playlists.select_entry(20, 0);
        browser.accept_playlist_lookup(1, None);
        assert_eq!(browser.playlists.selected(), Some(2));
        assert_eq!(browser.playlists.selected_entry(), Some(20));
        browser.accept_playlist_lookup(2, None);
        assert_eq!(browser.playlists.selected(), None);
        assert_eq!(browser.playlists.selected_entry(), None);
        assert_eq!(browser.playlists.entries().playlist_id, None);
    }

    #[test]
    fn playlist_switch_and_removal_clear_entry_identity_and_payload() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.select_entry(10, 0);
        assert!(playlists.select(2, "two".into(), 1));
        assert_eq!(playlists.selected_entry(), None);
        playlists.prepare_entries(2);
        playlists.select_entry(20, 0);
        playlists.clear_selection();
        assert_eq!(playlists.selected(), None);
        assert_eq!(playlists.selected_entry(), None);
        assert_eq!(playlists.entries().playlist_id, None);
        assert!(playlists.entries().rows.is_empty());
    }
    #[test]
    fn metadata_row_refresh_does_not_invalidate_occurrence_identity() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.select_entry(41, 3);
        playlists.prepare_entries(1);
        assert_eq!(playlists.selected_entry(), Some(41));
        assert!(playlists.entry_ready());
    }

    #[test]
    fn playlist_reorder_refresh_is_scoped_and_preserves_catalog_payload() {
        use crate::projection::GuiLibrarySnapshot;
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.prepare_page(false, 0, 20);
        playlists.accept_summaries(Page {
            total: 1,
            rows: vec![PlaylistSummary {
                id: 1,
                name: "one".into(),
                entry_count: 10_000,
            }],
        });
        playlists.select_entry(41, 3);
        let previous = GuiLibrarySnapshot::default();
        let mut next = previous.clone();
        Arc::make_mut(&mut next.playlist_entry_revisions).insert(2, 1);
        assert_eq!(
            playlists.refresh_changes(&previous, &next),
            PlaylistRefresh {
                summaries: false,
                entries: false,
                entry_payload_only: false,
            }
        );
        Arc::make_mut(&mut next.playlist_entry_revisions).insert(1, 1);
        let refresh = playlists.refresh_changes(&previous, &next);
        assert_eq!(
            refresh,
            PlaylistRefresh {
                summaries: false,
                entries: true,
                entry_payload_only: false,
            }
        );
        if refresh.entries {
            playlists.prepare_entries(1);
            playlists.track_selection.invalidate();
        }
        assert_eq!(playlists.summaries.rows.get(0).unwrap().name, "one");
        assert_eq!(playlists.selected_entry(), Some(41));
        assert!(!playlists.entry_ready());
        let coalesced = next.clone();
        Arc::make_mut(&mut next.playlist_entry_revisions).insert(2, 2);
        assert_eq!(
            playlists.refresh_changes(&coalesced, &next),
            PlaylistRefresh {
                summaries: false,
                entries: false,
                entry_payload_only: false,
            }
        );
        next.playlist_revision += 1;
        next.playlist_entry_revisions = Arc::default();
        assert_eq!(
            playlists.refresh_changes(&coalesced, &next),
            PlaylistRefresh {
                summaries: true,
                entries: true,
                entry_payload_only: false,
            }
        );
        let before_metadata = next.clone();
        next.revision += 1;
        assert_eq!(
            playlists.refresh_changes(&before_metadata, &next),
            PlaylistRefresh {
                summaries: false,
                entries: true,
                entry_payload_only: false,
            }
        );
    }

    #[test]
    fn playlist_switch_generation_rejects_drag_after_switching_back() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        let original = playlists.selection_generation();
        playlists.select(1, "renamed".into(), 0);
        assert_eq!(playlists.selection_generation(), original);
        playlists.select(2, "two".into(), 1);
        playlists.select(1, "one".into(), 0);
        assert_ne!(playlists.selection_generation(), original);
        let returned = playlists.selection_generation();
        playlists.clear_selection();
        playlists.select(1, "one".into(), 0);
        assert_ne!(playlists.selection_generation(), returned);
    }

    #[test]
    fn refreshed_offviewport_entry_resolves_identity_without_cached_pages() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.select_entry(41, 3);
        playlists.prepare_page(true, 8_000, 20);
        playlists.accept_entries(Page {
            total: 10_000,
            rows: (8_000..8_020)
                .map(|index| PlaylistEntryRow {
                    id: index as i64,
                    track_id: index as i64,
                    position: index,
                    offset: index,
                    title: format!("entry {index}"),
                    path: Default::default(),
                    channels: 0,
                    sample_rate: 0,
                    bits_per_sample: None,
                    bitrate_bps: None,
                    bitrate_exact: false,
                    cue_number: None,
                    artist: String::new(),
                    album: String::new(),
                    duration: None,
                    codec: String::new(),
                    missing: false,
                    favorite: false,
                })
                .collect(),
        });
        playlists.demand_page(true, 8_000..8_010, 20);
        playlists.prepare_entries(1);
        playlists.track_selection.invalidate();
        assert_eq!(playlists.selected_entry(), Some(41));
        assert!(!playlists.entry_ready());
        // The one identity result restores actions without restoring offscreen payload.
        playlists.accept_entry_locations(vec![PlaylistEntryLocation {
            id: 41,
            playlist_id: 1,
            position: 7_000,
            offset: 7_000,
            track_id: 99,
            total: 10_000,
        }]);
        assert!(playlists.entry_ready());
        assert_eq!(
            playlists.track_selection.active.map(|entry| entry.track_id),
            Some(99)
        );
        assert_eq!(playlists.entries.rows.visible().start, 8_000);
        assert!(playlists.entries.rows.iter().next().is_none());
        assert_eq!(
            playlists.track_selection.active.map(|entry| entry.position),
            Some(7_000)
        );
        playlists.prepare_entries(1);
        playlists.track_selection.invalidate();
        assert!(!playlists.entry_ready());
        playlists.accept_entry_locations(vec![PlaylistEntryLocation {
            id: 41,
            playlist_id: 1,
            position: 6_000,
            offset: 6_000,
            track_id: 99,
            total: 10_000,
        }]);
        assert_eq!(
            playlists.track_selection.active.map(|entry| entry.position),
            Some(6_000)
        );
        playlists.track_selection.remove(41);
        assert!(!playlists.entry_ready());
    }

    #[test]
    fn missing_entry_clears_selection_not_an_unloaded_page() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.select_entry(41, 3);
        playlists.prepare_entries(1);
        playlists.track_selection.invalidate();
        assert!(!playlists.entry_ready());
        assert_eq!(playlists.selected_entry(), Some(41));
        playlists.accept_entry_locations(Vec::new());
        assert_eq!(playlists.selected_entry(), None);
        assert!(!playlists.entry_ready());
        assert_eq!(playlists.selected(), Some(1));
    }

    #[test]
    fn changed_membership_rejects_stale_entry_identity() {
        let mut playlists = PlaylistBrowser::default();
        playlists.select(1, "one".into(), 0);
        playlists.prepare_entries(1);
        playlists.select_entry(41, 0);
        playlists.accept_entry_locations(vec![PlaylistEntryLocation {
            id: 41,
            playlist_id: 1,
            position: 0,
            offset: 0,
            track_id: 99,
            total: 1,
        }]);
        assert!(playlists.entry_ready());
        playlists.prepare_entries(1);
        playlists.track_selection.invalidate();
        assert!(!playlists.entry_ready());
        playlists.accept_entry_locations(vec![PlaylistEntryLocation {
            id: 41,
            playlist_id: 2,
            position: 0,
            offset: 0,
            track_id: 99,
            total: 1,
        }]);
        assert_eq!(playlists.selected_entry(), None);
    }
}
