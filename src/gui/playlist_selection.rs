//! Playlist occurrence selection is independent of bounded metadata pages.
//! Jobs retain compact identities and publish atomically for their original token.
use super::*;
use crate::{
    model::{Page, PlaylistEntryLocation, PlaylistEntryRow, Query},
    response::{PlaylistEntryRevision, ViewResponse},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EntryIdentity {
    pub(super) id: i64,
    pub(super) track_id: i64,
    pub(super) position: usize,
}
impl EntryIdentity {
    pub(super) fn row(row: &PlaylistEntryRow, position: usize) -> Self {
        Self {
            id: row.id,
            track_id: row.track_id,
            position,
        }
    }
}
impl From<&PlaylistEntryLocation> for EntryIdentity {
    fn from(row: &PlaylistEntryLocation) -> Self {
        Self {
            id: row.id,
            track_id: row.track_id,
            position: row.position,
        }
    }
}

#[derive(Default)]
pub(super) struct EntrySelection {
    selected: HashMap<i64, EntryIdentity>,
    pub(super) active: Option<EntryIdentity>,
    anchor: Option<EntryIdentity>,
    generation: u64,
    pub(super) busy: bool,
    pub(super) error: Option<String>,
    navigation: Option<usize>,
}
impl EntrySelection {
    pub(super) fn len(&self) -> usize {
        self.selected.len()
    }
    pub(super) fn contains(&self, id: i64) -> bool {
        self.selected.contains_key(&id)
    }
    pub(super) fn ready(&self) -> bool {
        !self.busy && self.error.is_none()
    }
    pub(super) fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.busy = false;
        self.error = None;
        self.navigation = None;
    }
    pub(super) fn clear(&mut self) {
        self.cancel();
        self.selected.clear();
        self.active = None;
        self.anchor = None;
    }
    pub(super) fn invalidate(&mut self) {
        self.cancel();
        self.busy = !self.selected.is_empty() || self.active.is_some() || self.anchor.is_some();
    }
    pub(super) fn select(&mut self, row: EntryIdentity, multi: bool) {
        self.cancel();
        if !multi {
            self.selected.clear();
        }
        if !multi || self.selected.remove(&row.id).is_none() {
            self.selected.insert(row.id, row);
        }
        self.active = Some(row);
        self.anchor = Some(row);
    }
    pub(super) fn ids(&self) -> Vec<i64> {
        let mut ids: Vec<_> = self.selected.keys().copied().collect();
        for owner in [self.active, self.anchor].into_iter().flatten() {
            if !ids.contains(&owner.id) {
                ids.push(owner.id);
            }
        }
        ids
    }
    pub(super) fn ordered(&self) -> Vec<EntryIdentity> {
        if !self.ready() {
            return Vec::new();
        }
        let mut rows: Vec<_> = self.selected.values().copied().collect();
        rows.sort_unstable_by_key(|row| row.position);
        rows
    }
    pub(super) fn ordered_ids(&self) -> Vec<i64> {
        self.ordered().into_iter().map(|row| row.id).collect()
    }
    pub(super) fn favorite_ids(&self) -> Vec<i64> {
        if !self.ready() {
            return Vec::new();
        }
        let mut ids: Vec<_> = self.selected.values().map(|row| row.track_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
    pub(super) fn drag_ids(&self, source: i64) -> Vec<i64> {
        if !self.ready() {
            Vec::new()
        } else if self.contains(source) {
            self.ordered_ids()
        } else {
            vec![source]
        }
    }
    fn fail(&mut self, message: String) {
        self.busy = false;
        self.error = Some(message);
    }
    #[cfg(test)]
    pub(super) fn remove(&mut self, id: i64) {
        self.cancel();
        self.selected.remove(&id);
        if self.active.is_some_and(|row| row.id == id) {
            self.active = None;
        }
        if self.anchor.is_some_and(|row| row.id == id) {
            self.anchor = None;
        }
    }
    fn current(&self, generation: u64) -> bool {
        self.generation == generation
    }
    pub(super) fn reconcile(&mut self, rows: Vec<EntryIdentity>) {
        self.active = self
            .active
            .and_then(|old| rows.iter().find(|row| row.id == old.id).copied());
        self.anchor = self
            .anchor
            .and_then(|old| rows.iter().find(|row| row.id == old.id).copied());
        self.selected = rows
            .into_iter()
            .filter(|row| self.selected.contains_key(&row.id))
            .map(|row| (row.id, row))
            .collect();
        self.busy = false;
        self.error = None;
    }
    fn complete_range(
        &mut self,
        generation: u64,
        anchor: Option<EntryIdentity>,
        target: EntryIdentity,
        rows: Vec<EntryIdentity>,
        additive: bool,
    ) -> bool {
        if !self.current(generation) {
            return false;
        }
        if !additive {
            self.selected.clear();
        }
        self.selected
            .extend(rows.into_iter().map(|row| (row.id, row)));
        self.anchor = anchor.or(Some(target));
        self.active = Some(target);
        self.busy = false;
        self.navigation = None;
        true
    }
}

/// Only compact identities accumulate; at most one metadata batch is live.
struct EntryRangePages {
    offset: usize,
    end: usize,
    identities: Vec<EntryIdentity>,
}
impl EntryRangePages {
    fn new(start: usize, end: usize) -> Self {
        Self {
            offset: start,
            end,
            identities: Vec::with_capacity(end - start),
        }
    }
    fn next(&self) -> Option<(usize, usize)> {
        (self.offset < self.end).then(|| {
            (
                self.offset,
                (self.end - self.offset).min(crate::model::MAX_QUERY_ROWS),
            )
        })
    }
    fn accept(&mut self, page: Page<PlaylistEntryRow>) -> bool {
        let Some((offset, limit)) = self.next() else {
            return false;
        };
        if page.total < self.end || page.rows.is_empty() || page.rows.len() > limit {
            return false;
        }
        self.identities.extend(
            page.rows
                .iter()
                .enumerate()
                .map(|(index, row)| EntryIdentity::row(row, offset + index)),
        );
        self.offset += page.rows.len();
        true
    }
}

impl GuiApp {
    pub(super) fn playlist_revision(&self, id: i64) -> u64 {
        self.state
            .library
            .playlist_entry_revisions
            .get(&id)
            .copied()
            .unwrap_or(self.state.library.playlist_revision)
    }
    fn entry_job_current(
        &self,
        playlist_id: i64,
        generation: u64,
        revision: u64,
        catalog: u64,
        library: u64,
    ) -> bool {
        self.browser.playlists.selected() == Some(playlist_id)
            && self.browser.playlists.entries().playlist_id == Some(playlist_id)
            && self.browser.playlists.track_selection.current(generation)
            && self.playlist_revision(playlist_id) == revision
            && self.state.library.playlist_revision == catalog
            && self.state.library.revision == library
    }
    fn entry_job_failed(
        &mut self,
        playlist_id: i64,
        generation: u64,
        message: String,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if self.browser.playlists.selected() == Some(playlist_id)
            && self.browser.playlists.track_selection.current(generation)
        {
            // Keep the pre-job selection intact; a failed partial range never becomes visible.
            self.browser.playlists.track_selection.fail(message.clone());
            self.error = Some(message);
            cx.notify();
        }
    }
    pub(super) fn refresh_entry_selection(&mut self, cx: &mut Context<Self>) {
        let Some(playlist_id) = self.browser.playlists.selected() else {
            return;
        };
        let selection = &mut self.browser.playlists.track_selection;
        selection.invalidate();
        let ids = selection.ids();
        if ids.is_empty() {
            return;
        }
        let generation = selection.generation;
        let revision = self.playlist_revision(playlist_id);
        let catalog = self.state.library.playlist_revision;
        let library = self.state.library.revision;
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let mut identities = Vec::with_capacity(ids.len());
            for chunk in ids.chunks(crate::model::MAX_QUERY_ROWS) {
                if !this
                    .update(cx, |this, cx| {
                        this.refresh(cx);
                        this.entry_job_current(playlist_id, generation, revision, catalog, library)
                    })
                    .unwrap_or(false)
                {
                    return;
                }
                let handle = handle.clone();
                let entry_ids = chunk.to_vec();
                let response = cx
                    .background_executor()
                    .spawn(async move {
                        handle.query(Query::PlaylistEntryLocations {
                            playlist_id,
                            entry_ids,
                        })
                    })
                    .await;
                let expected = Some(PlaylistEntryRevision {
                    playlist_id,
                    revision,
                });
                if response.revisions.playlist_entries != expected
                    || response.revisions.playlist != catalog
                    || response.revisions.library != library
                {
                    let _ = this.update(cx, |this, cx| {
                        this.refresh(cx);
                        if this.entry_job_current(
                            playlist_id,
                            generation,
                            revision,
                            catalog,
                            library,
                        ) {
                            this.refresh_entry_selection(cx);
                        }
                    });
                    return;
                }
                match response.result {
                    Ok(ViewResponse::PlaylistEntryLocations(rows)) => identities.extend(
                        rows.into_iter()
                            .filter(|row| row.playlist_id == playlist_id),
                    ),
                    result => {
                        let message = match result {
                            Err(error) => error.to_string(),
                            _ => "Unexpected playlist selection response".into(),
                        };
                        let _ = this.update(cx, |this, cx| {
                            this.entry_job_failed(playlist_id, generation, message, cx)
                        });
                        return;
                    }
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                if !this.entry_job_current(playlist_id, generation, revision, catalog, library) {
                    return;
                }
                this.browser.playlists.accept_entry_locations(identities);
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn select_playlist_entry(
        &mut self,
        id: i64,
        index: usize,
        shift: bool,
        multi: bool,
        cx: &mut Context<Self>,
    ) {
        self.refresh(cx);
        if (shift || multi) && !self.browser.playlists.track_selection.ready() {
            return;
        }
        let Some(row) = self
            .browser
            .playlists
            .entries()
            .rows
            .get(index)
            .filter(|row| row.id == id)
            .map(|row| EntryIdentity::row(row, index))
        else {
            return;
        };
        if shift && let Some(anchor) = self.browser.playlists.track_selection.anchor {
            self.select_playlist_range(
                anchor.position.min(index),
                anchor.position.max(index) + 1,
                Some(anchor),
                row,
                multi,
                cx,
            );
        } else {
            self.browser.playlists.track_selection.select(row, multi);
            self.browser.playlists.sync_active_entry();
        }
        self.request_track(row.track_id, cx);
        cx.notify();
    }
    fn select_playlist_range(
        &mut self,
        start: usize,
        end: usize,
        anchor: Option<EntryIdentity>,
        target: EntryIdentity,
        additive: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(playlist_id) = self.browser.playlists.selected() else {
            return;
        };
        let selection = &mut self.browser.playlists.track_selection;
        selection.cancel();
        selection.busy = true;
        let generation = selection.generation;
        let revision = self.playlist_revision(playlist_id);
        let catalog = self.state.library.playlist_revision;
        let library = self.state.library.revision;
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let mut pages = EntryRangePages::new(start, end);
            while let Some((offset, limit)) = pages.next() {
                if !this
                    .update(cx, |this, cx| {
                        this.refresh(cx);
                        this.entry_job_current(playlist_id, generation, revision, catalog, library)
                    })
                    .unwrap_or(false)
                {
                    return;
                }
                let handle = handle.clone();
                let response = cx
                    .background_executor()
                    .spawn(async move {
                        handle.query(Query::PlaylistEntries {
                            playlist_id,
                            offset,
                            limit,
                        })
                    })
                    .await;
                if response.revisions.playlist_entries
                    != Some(PlaylistEntryRevision {
                        playlist_id,
                        revision,
                    })
                    || response.revisions.playlist != catalog
                    || response.revisions.library != library
                {
                    let _ = this.update(cx, |this, cx| {
                        this.refresh(cx);
                        if this.entry_job_current(
                            playlist_id,
                            generation,
                            revision,
                            catalog,
                            library,
                        ) {
                            this.refresh_entry_selection(cx);
                        }
                    });
                    return;
                }
                match response.result {
                    Ok(ViewResponse::PlaylistEntries(page)) => {
                        if !pages.accept(page) {
                            let _ = this.update(cx, |this, cx| {
                                this.entry_job_failed(
                                    playlist_id,
                                    generation,
                                    "Playlist range changed while loading".into(),
                                    cx,
                                )
                            });
                            return;
                        }
                    }
                    result => {
                        let message = match result {
                            Err(error) => error.to_string(),
                            _ => "Playlist range changed while loading".into(),
                        };
                        let _ = this.update(cx, |this, cx| {
                            this.entry_job_failed(playlist_id, generation, message, cx)
                        });
                        return;
                    }
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                if !this.entry_job_current(playlist_id, generation, revision, catalog, library) {
                    return;
                }
                this.browser.playlists.track_selection.complete_range(
                    generation,
                    anchor,
                    target,
                    pages.identities,
                    additive,
                );
                this.browser.playlists.sync_active_entry();
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn select_all_playlist_entries(&mut self, cx: &mut Context<Self>) {
        if !self.browser.playlists.track_selection.ready() {
            return;
        }
        let total = self.browser.playlists.entries().total;
        if total == 0 {
            return;
        }
        // Fetching the first row also establishes an active occurrence for an empty selection.
        self.navigate_playlist_to(0, false, false, true, cx);
    }
    pub(super) fn navigate_playlist(
        &mut self,
        down: bool,
        shift: bool,
        multi: bool,
        cx: &mut Context<Self>,
    ) {
        let selection = &self.browser.playlists.track_selection;
        let total = self.browser.playlists.entries().total;
        if total == 0 || (!selection.ready() && selection.navigation.is_none()) {
            return;
        }
        let current = selection
            .navigation
            .or(selection.active.map(|row| row.position));
        let offset = match current {
            Some(index) if down => (index + 1).min(total - 1),
            Some(index) => index.saturating_sub(1),
            None if down => 0,
            None => total - 1,
        };
        self.navigate_playlist_to(offset, shift, multi, false, cx);
    }
    fn navigate_playlist_to(
        &mut self,
        offset: usize,
        shift: bool,
        multi: bool,
        all: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(playlist_id) = self.browser.playlists.selected() else {
            return;
        };
        let selection = &mut self.browser.playlists.track_selection;
        selection.cancel();
        selection.busy = true;
        selection.navigation = Some(offset);
        let generation = selection.generation;
        let revision = self.playlist_revision(playlist_id);
        let catalog = self.state.library.playlist_revision;
        let library = self.state.library.revision;
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let response = cx
                .background_executor()
                .spawn(async move {
                    handle.query(Query::PlaylistEntries {
                        playlist_id,
                        offset,
                        limit: 1,
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                if !this.entry_job_current(playlist_id, generation, revision, catalog, library) {
                    return;
                }
                if response.revisions.playlist_entries
                    != Some(PlaylistEntryRevision {
                        playlist_id,
                        revision,
                    })
                    || response.revisions.playlist != catalog
                    || response.revisions.library != library
                {
                    this.refresh_entry_selection(cx);
                    return;
                }
                match response.result {
                    Ok(ViewResponse::PlaylistEntries(page)) => {
                        if let Some(row) = page.rows.first() {
                            let target = EntryIdentity::row(row, offset);
                            let anchor = this.browser.playlists.track_selection.anchor;
                            if all {
                                this.select_playlist_range(
                                    0,
                                    page.total,
                                    Some(target),
                                    target,
                                    false,
                                    cx,
                                );
                            } else if shift && let Some(anchor) = anchor {
                                this.select_playlist_range(
                                    anchor.position.min(offset),
                                    anchor.position.max(offset) + 1,
                                    Some(anchor),
                                    target,
                                    multi,
                                    cx,
                                );
                            } else {
                                this.browser.playlists.track_selection.select(target, false);
                                this.browser.playlists.sync_active_entry();
                            }
                            this.request_track(target.track_id, cx);
                            this.scroll_views.scroll_list_to("playlist-entries", offset);
                        } else {
                            this.refresh_entry_selection(cx);
                        }
                    }
                    Err(error) => {
                        this.entry_job_failed(playlist_id, generation, error.to_string(), cx)
                    }
                    _ => this.entry_job_failed(
                        playlist_id,
                        generation,
                        "Unexpected playlist navigation response".into(),
                        cx,
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn remove_playlist_selection(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
        let Some(playlist_id) = self.browser.playlists.selected() else {
            return;
        };
        let entry_ids = self.browser.playlists.track_selection.ordered_ids();
        if entry_ids.is_empty() {
            return;
        }
        self.send(
            Command::RemovePlaylistEntries {
                playlist_id,
                entry_ids,
                expected_revision: self.playlist_revision(playlist_id),
            },
            cx,
        );
        // Keep identities until the authoritative refresh confirms removal, including command failure.
    }
    pub(super) fn favorite_playlist_selection(&mut self, favorite: bool, cx: &mut Context<Self>) {
        self.refresh(cx);
        let track_ids = self.browser.playlists.track_selection.favorite_ids();
        if !track_ids.is_empty() {
            self.send(
                Command::SetFavorite {
                    track_ids,
                    favorite,
                },
                cx,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EntryIdentity, EntryRangePages, EntrySelection};
    use crate::model::{MAX_QUERY_ROWS, Page, PlaylistEntryRow};

    fn identity(id: i64, position: usize) -> EntryIdentity {
        EntryIdentity {
            id,
            track_id: id + 100,
            position,
        }
    }
    fn page(offset: usize, count: usize, total: usize) -> Page<PlaylistEntryRow> {
        Page {
            total,
            rows: (offset..offset + count)
                .map(|position| PlaylistEntryRow {
                    id: position as i64,
                    track_id: position as i64 + 100,
                    title: String::new(),
                    artist: String::new(),
                    album: String::new(),
                    missing: false,
                    favorite: false,
                })
                .collect(),
        }
    }

    #[test]
    fn control_toggle_keeps_active_and_anchor_separate_from_group_actions() {
        let mut selection = EntrySelection::default();
        selection.select(identity(9, 8), false);
        selection.select(identity(2, 1), true);
        assert_eq!(selection.ordered_ids(), vec![2, 9]);
        selection.select(identity(2, 1), true);
        assert_eq!(selection.ordered_ids(), vec![9]);
        assert_eq!(selection.active, Some(identity(2, 1)));
        assert_eq!(selection.anchor, Some(identity(2, 1)));
        assert_eq!(selection.ids().len(), 2);
        assert_eq!(selection.favorite_ids(), vec![109]);
        assert_eq!(selection.drag_ids(9), vec![9]);
        assert_eq!(selection.drag_ids(2), vec![2]);
    }

    #[test]
    fn inclusive_crosspage_range_replaces_then_adds_without_moving_anchor() {
        let mut selection = EntrySelection::default();
        let anchor = identity(100, MAX_QUERY_ROWS - 1);
        let target = identity(102, MAX_QUERY_ROWS + 1);
        selection.select(anchor, false);
        let rows = vec![anchor, identity(101, MAX_QUERY_ROWS), target];
        assert!(selection.complete_range(selection.generation, Some(anchor), target, rows, false));
        assert_eq!(selection.ordered_ids(), vec![100, 101, 102]);
        assert_eq!(selection.active, Some(target));
        assert_eq!(selection.anchor, Some(anchor));
        let next = identity(1, 0);
        assert!(selection.complete_range(
            selection.generation,
            Some(anchor),
            next,
            vec![next],
            true
        ));
        assert_eq!(selection.ordered_ids(), vec![1, 100, 101, 102]);
        assert_eq!(selection.anchor, Some(anchor));
        assert_eq!(selection.active, Some(next));
    }

    #[test]
    fn range_pages_bound_metadata_and_publish_complete_absolute_identities() {
        let start = MAX_QUERY_ROWS - 1;
        let end = start + MAX_QUERY_ROWS * 2 + 1;
        let mut pages = EntryRangePages::new(start, end);
        let mut calls = 0;
        while let Some((offset, limit)) = pages.next() {
            assert!(limit <= MAX_QUERY_ROWS);
            assert!(pages.accept(page(offset, limit, end)));
            calls += 1;
        }
        assert_eq!(calls, 3);
        assert_eq!(pages.identities.len(), end - start);
        assert_eq!(pages.identities.first().unwrap().position, start);
        assert_eq!(pages.identities.last().unwrap().position, end - 1);
        let mut selection = EntrySelection::default();
        let active = *pages.identities.last().unwrap();
        assert!(selection.complete_range(
            selection.generation,
            None,
            active,
            pages.identities,
            false
        ));
        assert_eq!(selection.len(), end - start);
        assert_eq!(selection.drag_ids(active.id).len(), end - start);
    }

    #[test]
    fn partial_range_failure_rolls_back_and_retry_restores_group_consumers() {
        let mut selection = EntrySelection::default();
        selection.select(identity(7, 20), false);
        selection.busy = true;
        let mut pages = EntryRangePages::new(0, MAX_QUERY_ROWS + 1);
        assert!(pages.accept(page(0, MAX_QUERY_ROWS, MAX_QUERY_ROWS + 1)));
        assert!(!pages.accept(page(MAX_QUERY_ROWS, 0, MAX_QUERY_ROWS + 1)));
        selection.fail("range failed".into());
        assert_eq!(selection.len(), 1);
        assert!(selection.ordered_ids().is_empty());
        assert!(selection.favorite_ids().is_empty());
        assert!(selection.drag_ids(7).is_empty());
        selection.invalidate();
        selection.reconcile(vec![identity(7, 18)]);
        assert_eq!(selection.ordered_ids(), vec![7]);
        assert_eq!(selection.active, Some(identity(7, 18)));
    }

    #[test]
    fn superseded_range_navigation_revision_and_switch_results_never_publish() {
        let mut selection = EntrySelection::default();
        selection.select(identity(7, 20), false);
        let old = selection.generation;
        selection.navigation = Some(21);
        selection.busy = true;
        selection.select(identity(8, 21), false);
        assert!(!selection.complete_range(
            old,
            None,
            identity(9, 22),
            vec![identity(9, 22)],
            false
        ));
        assert_eq!(selection.ordered_ids(), vec![8]);
        let old = selection.generation;
        selection.invalidate();
        assert!(!selection.complete_range(
            old,
            None,
            identity(9, 22),
            vec![identity(9, 22)],
            false
        ));
        selection.reconcile(vec![identity(8, 3)]);
        let old = selection.generation;
        selection.clear();
        selection.select(identity(10, 0), false);
        assert!(!selection.complete_range(old, None, identity(8, 3), vec![identity(8, 3)], false));
        assert_eq!(selection.ordered_ids(), vec![10]);
        assert!(selection.navigation.is_none());
    }

    #[test]
    fn refresh_relocates_compact_ids_removes_missing_and_keeps_source_order() {
        let mut selection = EntrySelection::default();
        selection.select(identity(9, 200_000), false);
        selection.select(identity(2, 5), true);
        selection.select(identity(3, 4), true);
        selection.invalidate();
        assert!(selection.ordered_ids().is_empty());
        selection.reconcile(vec![identity(9, 0), identity(3, 10)]);
        assert_eq!(selection.ordered_ids(), vec![9, 3]);
        assert!(!selection.contains(2));
        assert_eq!(selection.drag_ids(3), vec![9, 3]);
        assert_eq!(selection.drag_ids(4), vec![4]);
        assert_eq!(selection.favorite_ids(), vec![103, 109]);
        assert_eq!(selection.active, Some(identity(3, 10)));
        assert_eq!(selection.anchor, Some(identity(3, 10)));
    }

    #[test]
    fn truncated_or_oversized_range_page_does_not_advance_or_publish() {
        let mut pages = EntryRangePages::new(10, 20);
        assert!(!pages.accept(page(10, 5, 19)));
        assert!(!pages.accept(page(10, 11, 30)));
        assert_eq!(pages.next(), Some((10, 10)));
        assert!(pages.identities.is_empty());
    }
}
