use std::{ops::Range, time::Instant};

use rivu_protocol::query_tracking::{QueryTicket, QueryTracker};
use rivu_protocol::{
    model::{AlbumKey, AlbumSort, AlbumSummary, LibraryRow, Query},
    response::{QueryError, QueryResponse, QueryRevisions, ViewResponse},
};

use super::super::paged::PagedRows;

const ALBUM_PAGE: usize = 24;

#[derive(Clone, Copy, Default)]
pub(super) struct DataChange {
    pub albums: bool,
    pub tracks: bool,
}

impl DataChange {
    pub fn any(&self) -> bool {
        self.albums || self.tracks
    }
}

pub(super) struct SyncChange {
    pub data: DataChange,
    pub visibility: bool,
    pub demand: bool,
}

pub(super) struct SelectionChange {
    pub data: DataChange,
    pub reset_tracks: bool,
}

pub(super) struct Request {
    pub query: Query,
    pub generation: u64,
    pub ticket: QueryTicket,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) enum Published {
    #[default]
    None,
    Albums,
    Tracks,
}

pub(super) struct Completion {
    pub data: DataChange,
    pub published: Published,
    pub changed: bool,
}

/// Owns both logical listings and their shared physical query slot. Cancelling a
/// listing never releases that slot: only delivery of the detached worker does.
pub(super) struct AlbumModel {
    visible: bool,
    revisions: QueryRevisions,
    albums_projection: QueryTracker,
    tracks_projection: QueryTracker,
    tracks_key: Option<AlbumKey>,
    page_size: usize,
    worker_pending: bool,
    page_timed_out: bool,
    selected: Option<AlbumKey>,
    albums: PagedRows<AlbumSummary>,
    tracks: PagedRows<LibraryRow>,
    query: Option<String>,
    tracks_query: Option<String>,
    sort: AlbumSort,
}

impl Default for AlbumModel {
    fn default() -> Self {
        Self {
            visible: false,
            revisions: QueryRevisions::default(),
            albums_projection: QueryTracker::default(),
            tracks_projection: QueryTracker::default(),
            tracks_key: None,
            page_size: 1,
            worker_pending: false,
            page_timed_out: false,
            selected: None,
            albums: PagedRows::default(),
            tracks: PagedRows::default(),
            query: None,
            tracks_query: None,
            sort: AlbumSort::default(),
        }
    }
}

impl AlbumModel {
    pub fn visible(&self) -> bool {
        self.visible
    }
    pub fn selected(&self) -> Option<&AlbumKey> {
        self.selected.as_ref()
    }
    pub fn albums(&self) -> &PagedRows<AlbumSummary> {
        &self.albums
    }
    pub fn tracks(&self) -> &PagedRows<LibraryRow> {
        &self.tracks
    }
    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }
    pub fn tracks_query(&self) -> Option<&str> {
        self.tracks_query.as_deref()
    }
    pub fn sort(&self) -> AlbumSort {
        self.sort
    }
    pub fn generation(&self) -> u64 {
        self.active_projection().generation()
    }

    pub fn refresh_deadline(&self) -> Option<Instant> {
        [
            self.albums.refresh_deadline(),
            self.tracks.refresh_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub fn expire_refresh(&mut self, now: Instant) -> DataChange {
        DataChange {
            albums: self.albums.expire(now),
            tracks: self.tracks.expire(now),
        }
    }

    /// Lets the view preserve its anchor before a revision invalidates rows.
    /// Search/sort transitions reset that anchor explicitly instead.
    pub fn albums_need_refresh(&self, revisions: QueryRevisions) -> bool {
        let stamp = Query::Albums {
            query: None,
            sort: self.sort,
            offset: 0,
            limit: ALBUM_PAGE,
        }
        .stamp(revisions);
        self.albums_projection.stamp() != Some(stamp)
    }

    /// The active listing's count, including its retained presentation during refresh.
    pub fn display_total(&self, now: Instant) -> Option<usize> {
        if self.selected.is_some() {
            self.tracks.display_total(now)
        } else {
            self.albums.display_total(now)
        }
    }

    pub fn sync(
        &mut self,
        visible: bool,
        revisions: QueryRevisions,
        page_size: usize,
        now: Instant,
    ) -> SyncChange {
        let visibility = self.visible != visible;
        let demand = self.page_size != page_size.max(1);
        if demand {
            self.tracks_projection.cancel();
            self.tracks.configure_page_size(page_size.max(1));
        }
        self.page_size = page_size.max(1);
        let data = self.sync_data(revisions, now);
        if visibility && !visible {
            self.cancel();
        }
        self.visible = visible;
        SyncChange {
            data,
            visibility,
            demand,
        }
    }

    pub fn select(&mut self, selected: Option<AlbumKey>, now: Instant) -> SelectionChange {
        let reset_tracks = selected
            .as_ref()
            .is_some_and(|key| self.tracks_key.as_ref() != Some(key));
        if reset_tracks {
            self.tracks_key = selected.clone();
            self.tracks_query = None;
            self.tracks.clear();
        }
        self.selected = selected;
        self.cancel();
        SelectionChange {
            data: self.sync_data(self.revisions, now),
            reset_tracks,
        }
    }

    pub fn set_query(&mut self, query: Option<String>, now: Instant) -> Option<DataChange> {
        if self.query == query {
            return None;
        }
        self.query = query;
        self.albums.clear();
        self.cancel();
        Some(self.sync_data(self.revisions, now))
    }

    pub fn set_tracks_query(&mut self, query: Option<String>, now: Instant) -> Option<DataChange> {
        if self.tracks_query == query {
            return None;
        }
        self.tracks_query = query;
        self.tracks.clear();
        self.cancel();
        Some(self.sync_data(self.revisions, now))
    }

    pub fn toggle_sort(&mut self, now: Instant) -> DataChange {
        self.sort = match self.sort {
            AlbumSort::Title => AlbumSort::Artist,
            AlbumSort::Artist => AlbumSort::Title,
        };
        self.albums.clear();
        self.cancel();
        self.sync_data(self.revisions, now)
    }

    pub fn cancel(&mut self) {
        cancel_pages(&mut self.albums_projection, &mut self.albums);
        cancel_pages(&mut self.tracks_projection, &mut self.tracks);
    }

    pub fn demand_albums(&mut self, range: Range<usize>) {
        let _ = self.albums.demand(range, ALBUM_PAGE);
    }

    pub fn demand_tracks(&mut self, range: Range<usize>, generation: u64) -> bool {
        if !self.visible || self.selected.is_none() || generation != self.generation() {
            return false;
        }
        let _ = self.tracks.demand(range, self.page_size);
        true
    }

    pub fn retired(&self) -> bool {
        if self.selected.is_some() {
            self.tracks.retired()
        } else {
            self.albums.retired()
        }
    }

    pub fn retry(&mut self, now: Instant) {
        if self.selected.is_some() {
            if self.tracks.retired() {
                self.tracks.refresh_at(now);
            }
            self.tracks.retry();
        } else {
            if self.albums.retired() {
                self.albums.refresh_at(now);
            }
            self.albums.retry();
        }
    }

    pub fn prepare(&mut self) -> Option<Request> {
        if !self.visible || self.worker_pending {
            return None;
        }
        let query = match &self.selected {
            Some(key) => {
                let offset = self.tracks.demand(self.tracks.visible(), self.page_size)?;
                self.tracks.begin(offset, self.page_size);
                Query::AlbumTracks {
                    key: key.clone(),
                    query: self.tracks_query.clone(),
                    offset,
                    limit: self.page_size,
                }
            }
            None => {
                let offset = self.albums.demand(self.albums.visible(), ALBUM_PAGE)?;
                self.albums.begin(offset, ALBUM_PAGE);
                Query::Albums {
                    query: self.query.clone(),
                    sort: self.sort,
                    offset,
                    limit: ALBUM_PAGE,
                }
            }
        };
        self.worker_pending = true;
        self.page_timed_out = false;
        let generation = self.generation();
        let ticket = if self.selected.is_some() {
            self.tracks_projection.ticket()
        } else {
            self.albums_projection.ticket()
        };
        Some(Request {
            query,
            generation,
            ticket,
        })
    }

    pub fn timeout(
        &mut self,
        query: &Query,
        generation: u64,
        response: QueryResponse,
        current_revisions: QueryRevisions,
        now: Instant,
    ) -> DataChange {
        let data = self.sync_data(current_revisions, now);
        // Timeout consumes this worker's publication rights, not its physical slot.
        self.page_timed_out = true;
        if self.visible
            && self
                .active_projection()
                .accepts(query, generation, current_revisions)
            && let Err(error) = response.result
        {
            self.fail_page(error.to_string());
        }
        data
    }

    pub fn complete_page(
        &mut self,
        query: &Query,
        generation: u64,
        response: QueryResponse,
        current_revisions: QueryRevisions,
        unexpected_response: &str,
        now: Instant,
    ) -> Completion {
        // Re-read authority at delivery, not merely the worker's captured revision.
        let data = self.sync_data(current_revisions, now);
        let mut completion = Completion {
            data,
            published: Published::None,
            changed: false,
        };
        if self.page_timed_out {
            return completion;
        }
        if !self.visible
            || !self
                .active_projection()
                .accepts(query, generation, current_revisions)
        {
            return completion;
        }
        if matches!(
            &response.result,
            Err(QueryError::RevisionChanged | QueryError::Cancelled)
        ) || (matches!(
            (query, &response.result),
            (Query::Albums { .. }, Ok(ViewResponse::Albums(_)))
                | (Query::AlbumTracks { .. }, Ok(ViewResponse::AlbumTracks(_)))
        ) && !self
            .active_projection()
            .accepts(query, generation, response.revisions))
        {
            self.retire_page();
            completion.changed = true;
            return completion;
        }
        match response.result {
            Ok(ViewResponse::Albums(page)) if matches!(query, Query::Albums { .. }) => {
                completion.changed = self.albums.accept_at(page, now);
                completion.published = Published::Albums;
            }
            Ok(ViewResponse::AlbumTracks(page)) if matches!(query, Query::AlbumTracks { .. }) => {
                self.tracks.accept(page);
                completion.published = Published::Tracks;
            }
            Err(error) => {
                self.fail_page(error.to_string());
                completion.changed = true;
            }
            _ => {
                self.fail_page(unexpected_response.to_owned());
                completion.changed = true;
            }
        }
        completion
    }

    /// Only physical completion releases the shared slot, including obsolete work.
    pub fn retire_worker(&mut self) {
        self.worker_pending = false;
        self.page_timed_out = false;
    }

    fn albums_query(&self) -> Query {
        Query::Albums {
            query: self.query.clone(),
            sort: self.sort,
            offset: 0,
            limit: ALBUM_PAGE,
        }
    }

    fn sync_data(&mut self, revisions: QueryRevisions, now: Instant) -> DataChange {
        self.revisions = revisions;
        let albums_query = self.albums_query();
        let albums = sync_pages(
            &mut self.albums_projection,
            &mut self.albums,
            &albums_query,
            revisions,
            now,
        );
        let tracks = self.tracks_key.as_ref().is_some_and(|key| {
            sync_pages(
                &mut self.tracks_projection,
                &mut self.tracks,
                &Query::AlbumTracks {
                    key: key.clone(),
                    query: self.tracks_query.clone(),
                    offset: 0,
                    limit: self.page_size,
                },
                revisions,
                now,
            )
        });
        DataChange { albums, tracks }
    }

    fn active_projection(&self) -> &QueryTracker {
        if self.selected.is_some() {
            &self.tracks_projection
        } else {
            &self.albums_projection
        }
    }

    fn retire_page(&mut self) {
        if self.selected.is_some() {
            self.tracks_projection.cancel();
            self.tracks.retire_pending();
        } else {
            self.albums_projection.cancel();
            self.albums.retire_pending();
        }
    }

    fn fail_page(&mut self, message: String) {
        if self.selected.is_some() {
            self.tracks.fail(message);
        } else {
            self.albums.fail(message);
        }
    }
}

fn sync_pages<T>(
    projection: &mut QueryTracker,
    pages: &mut PagedRows<T>,
    query: &Query,
    revisions: QueryRevisions,
    now: Instant,
) -> bool {
    let impact = projection.impact(query, revisions);
    if !projection.sync(query, revisions) {
        return false;
    }
    if impact.identity_changed {
        pages.clear();
    } else if impact.coordinates_changed {
        // Same-query rows remain display-only until replacement or the owner wake.
        pages.refresh_at(now);
    } else {
        pages.invalidate_payload(now);
    }
    true
}

fn cancel_pages<T>(projection: &mut QueryTracker, pages: &mut PagedRows<T>) {
    projection.cancel();
    pages.cancel_pending();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::paged;
    use rivu_protocol::model::Page;

    #[test]
    fn refresh_wake_expires_each_listing_at_its_original_deadline() {
        let now = Instant::now();
        let mut model = AlbumModel::default();
        model.sync(true, QueryRevisions::default(), 2, now);
        model.select(Some(key("A")), now);
        model.albums.begin(0, ALBUM_PAGE);
        model.albums.accept(Page {
            total: 1,
            rows: vec![summary(1)],
        });
        model.tracks.begin(0, 2);
        model.tracks.accept(Page {
            total: 1,
            rows: vec![row(1)],
        });
        let start = now;
        model.albums.refresh_at(start);
        model
            .tracks
            .refresh_at(start + std::time::Duration::from_millis(10));
        let first = model.albums.refresh_deadline().unwrap();
        let second = model.tracks.refresh_deadline().unwrap();
        assert_eq!(model.refresh_deadline(), Some(first));
        let before_first = first - std::time::Duration::from_nanos(1);
        assert!(!model.expire_refresh(before_first).any());
        model.sync(
            true,
            QueryRevisions {
                album: 1,
                ..model.revisions
            },
            2,
            before_first,
        );
        assert_eq!(
            model.refresh_deadline(),
            Some(first),
            "revisions cannot extend retention"
        );
        assert_eq!(model.albums.display_total(before_first), Some(1));
        assert_eq!(model.tracks.display_total(before_first), Some(1));
        let expired = model.expire_refresh(first);
        assert!(expired.albums);
        assert!(!expired.tracks);
        assert!(model.albums.display(0, first).is_none());
        assert!(model.tracks.display(0, first).is_some());
        assert_eq!(model.albums.display_total(first), None);
        assert_eq!(model.tracks.display_total(first), Some(1));
        assert!(model.tracks.get(0).is_none());
        assert_eq!(model.refresh_deadline(), Some(second));
        let expired = model.expire_refresh(second);
        assert!(!expired.albums);
        assert!(expired.tracks);
        assert!(model.tracks.display(0, second).is_none());
        assert_eq!(model.tracks.display_total(second), None);
        assert_eq!(model.refresh_deadline(), None);
        assert!(!model.expire_refresh(second).any());
    }

    #[test]
    fn equal_refresh_restores_authority_without_republishing_visual_changes() {
        let now = Instant::now();
        for tracks in [false, true] {
            for payload_only in [false, true] {
                for changed in [false, true] {
                    if payload_only && !tracks {
                        // Bitrate refreshes member payloads only, never cards.
                        continue;
                    }
                    let mut model = AlbumModel::default();
                    model.sync(true, QueryRevisions::default(), 2, now);
                    if tracks {
                        model.select(Some(key("A")), now);
                    }
                    let reply = |revisions, id| QueryResponse {
                        revisions,
                        result: Ok(if tracks {
                            ViewResponse::AlbumTracks(Page {
                                total: 1,
                                rows: vec![row(id)],
                            })
                        } else {
                            ViewResponse::Albums(Page {
                                total: 1,
                                rows: vec![summary(id)],
                            })
                        }),
                    };
                    let first = model.prepare().unwrap();
                    let completion = model.complete_page(
                        &first.query,
                        first.generation,
                        reply(model.revisions, 1),
                        model.revisions,
                        "wrong response",
                        now,
                    );
                    if !tracks {
                        assert!(completion.changed);
                    }
                    model.retire_worker();
                    let revisions = if payload_only {
                        QueryRevisions {
                            bitrate: 1,
                            ..model.revisions
                        }
                    } else {
                        QueryRevisions {
                            album: 1,
                            ..model.revisions
                        }
                    };
                    model.sync(true, revisions, 2, now);
                    let before =
                        model.refresh_deadline().unwrap() - std::time::Duration::from_nanos(1);
                    if tracks {
                        assert!(model.tracks.display(0, before).is_some());
                        assert!(model.tracks.get(0).is_none());
                        assert!(model.tracks.iter().next().is_none());
                        assert!(model.tracks.position(|track| track.id == 1).is_none());
                    } else {
                        assert!(model.albums.display(0, before).is_some());
                        assert!(model.albums.get(0).is_none());
                    }
                    let refresh = model.prepare().unwrap();
                    let completion = model.complete_page(
                        &refresh.query,
                        refresh.generation,
                        reply(revisions, if changed { 2 } else { 1 }),
                        revisions,
                        "wrong response",
                        before,
                    );
                    if !tracks {
                        assert_eq!(completion.changed, changed);
                    }
                    assert_eq!(
                        completion.published,
                        if tracks {
                            Published::Tracks
                        } else {
                            Published::Albums
                        }
                    );
                    assert_eq!(model.display_total(before), Some(1));
                    if tracks {
                        assert_eq!(model.tracks.get(0).unwrap().id, if changed { 2 } else { 1 });
                        assert_eq!(model.tracks.refresh_deadline(), None);
                    } else {
                        assert!(model.albums.get(0).is_some());
                        assert_eq!(model.albums.refresh_deadline(), None);
                    }
                }
            }
        }
    }

    #[test]
    fn both_listings_retain_read_only_presentation_across_data_revisions() {
        let now = Instant::now();
        for query in queries() {
            let mut projection = QueryTracker::default();
            let mut pages = paged::PagedRows::default();
            let revisions = QueryRevisions::default();
            sync_pages(&mut projection, &mut pages, &query, revisions, now);
            pages.begin(0, 2);
            pages.accept(Page {
                total: 4,
                rows: vec![7, 9],
            });
            pages.begin(2, 2);
            pages.accept(Page {
                total: 4,
                rows: vec![11, 13],
            });
            pages.demand(0..4, 2);
            sync_pages(
                &mut projection,
                &mut pages,
                &query,
                QueryRevisions {
                    album: 1,
                    ..revisions
                },
                now,
            );
            assert_eq!(pages.get(0), None);
            assert_eq!(pages.display(0, now), Some(&7));
            assert_eq!(pages.display(2, now), Some(&11));
            assert_eq!(pages.get(2), None);
            assert!(pages.iter().next().is_none());
            assert_eq!(pages.position(|row| *row == 11), None);
            assert_eq!(pages.demand(0..4, 2), Some(0));
            let changed = Query::Albums {
                query: Some("another album".into()),
                sort: AlbumSort::default(),
                offset: 0,
                limit: 2,
            };
            sync_pages(&mut projection, &mut pages, &changed, revisions, now);
            assert_eq!(pages.display(0, now), None);
            assert_eq!(pages.display(2, now), None);
            assert_eq!(pages.refresh_deadline(), None);
        }
    }

    #[test]
    fn obsolete_pages_park_until_fresh_authority_or_retry_without_releasing_worker() {
        let now = Instant::now();
        for tracks in [false, true] {
            for error in [QueryError::RevisionChanged, QueryError::Cancelled] {
                for explicit in [false, true] {
                    let mut model = AlbumModel::default();
                    let revisions = QueryRevisions {
                        album: 7,
                        ..Default::default()
                    };
                    model.sync(true, revisions, 2, now);
                    if tracks {
                        model.select(Some(key("A")), now);
                    }
                    let old = model.prepare().unwrap();
                    let completion = model.complete_page(
                        &old.query,
                        old.generation,
                        QueryResponse {
                            revisions: Default::default(),
                            result: Err(error.clone()),
                        },
                        revisions,
                        "wrong response",
                        now,
                    );
                    assert_eq!(completion.published, Published::None);
                    assert!(model.retired());
                    assert!(!old.ticket.is_current());
                    assert!(model.albums.error(0).is_none());
                    assert!(model.tracks.error(0).is_none());
                    for _ in 0..3 {
                        model.sync(true, revisions, 2, now);
                        assert!(model.prepare().is_none());
                    }
                    let fresh = if explicit {
                        model.retry(now);
                        revisions
                    } else {
                        let fresh = QueryRevisions {
                            album: 8,
                            ..revisions
                        };
                        model.sync(true, fresh, 2, now);
                        fresh
                    };
                    assert!(
                        model.prepare().is_none(),
                        "retirement keeps the physical slot"
                    );
                    model.retire_worker();
                    let next = model.prepare().unwrap();
                    assert_ne!(next.generation, old.generation);
                    model.complete_page(
                        &old.query,
                        old.generation,
                        response(&old, 0, revisions),
                        fresh,
                        "wrong response",
                        now,
                    );
                    let completion = finish(&mut model, next, 2, now);
                    assert_eq!(
                        completion.published,
                        if tracks {
                            Published::Tracks
                        } else {
                            Published::Albums
                        }
                    );
                    assert_eq!(model.display_total(now), Some(2));
                    assert!(!model.retired());
                }
            }
        }
    }

    fn key(title: &str) -> AlbumKey {
        AlbumKey {
            directory: "/music".into(),
            title: title.into(),
        }
    }

    fn row(id: i64) -> LibraryRow {
        LibraryRow {
            title: id.to_string(),
            ..crate::test_support::library_row(id)
        }
    }

    fn response(request: &Request, total: usize, revisions: QueryRevisions) -> QueryResponse {
        let result = match &request.query {
            Query::Albums { offset, limit, .. } => ViewResponse::Albums(Page {
                total,
                rows: (*offset..(*offset + *limit).min(total))
                    .map(|id| summary(id as i64))
                    .collect(),
            }),
            Query::AlbumTracks { offset, limit, .. } => ViewResponse::AlbumTracks(Page {
                total,
                rows: (*offset..(*offset + *limit).min(total))
                    .map(|id| row(id as i64))
                    .collect(),
            }),
            _ => unreachable!(),
        };
        QueryResponse {
            revisions,
            result: Ok(result),
        }
    }

    fn finish(model: &mut AlbumModel, request: Request, total: usize, now: Instant) -> Completion {
        let response = response(&request, total, model.revisions);
        let completion = model.complete_page(
            &request.query,
            request.generation,
            response,
            model.revisions,
            "wrong response",
            now,
        );
        model.retire_worker();
        completion
    }

    fn summary(id: i64) -> AlbumSummary {
        AlbumSummary {
            key: key("A"),
            artist: "artist".into(),
            track_count: 3,
            representative: rivu_protocol::model::ArtworkSource {
                id,
                path: "/music/a.flac".into(),
                fingerprint: None,
            },
        }
    }

    #[test]
    fn stats_revision_refreshes_member_payload_without_invalidating_cards_or_count() {
        let now = Instant::now();
        let mut model = AlbumModel::default();
        let previous = QueryRevisions::default();
        model.sync(true, previous, 3, now);
        let cards = model.prepare().unwrap();
        let card = summary(7);
        model.complete_page(
            &cards.query,
            cards.generation,
            QueryResponse {
                revisions: previous,
                result: Ok(ViewResponse::Albums(Page {
                    total: 1,
                    rows: vec![card.clone()],
                })),
            },
            previous,
            "wrong response",
            now,
        );
        model.retire_worker();
        model.select(Some(key("A")), now);
        let tracks = model.prepare().unwrap();
        finish(&mut model, tracks, 60, now);
        model.set_tracks_query(Some("needle".into()), now);
        let filtered = model.prepare().unwrap();
        finish(&mut model, filtered, 30, now);
        assert!(model.demand_tracks(24..27, model.generation()));
        let visible = model.prepare().unwrap();
        finish(&mut model, visible, 30, now);
        model.tracks.pin(Some(24));
        let previous_row = model.tracks.get(24).unwrap().clone();
        let cards_generation = model.albums_projection.generation();
        let generation = model.generation();
        let current = QueryRevisions {
            library: 1,
            ..previous
        };
        assert!(!model.albums_need_refresh(current));
        let change = model.sync(true, current, 3, now);
        assert!(!change.data.albums && change.data.tracks);
        assert_eq!(model.albums_projection.generation(), cards_generation);
        assert_eq!(model.albums.get(0), Some(&card));
        assert_eq!(model.albums.refresh_deadline(), None);
        assert_ne!(model.generation(), generation);
        assert_eq!(model.selected(), Some(&key("A")));
        assert_eq!(model.tracks_query(), Some("needle"));
        assert_eq!(model.tracks.len(), 30);
        assert_eq!(model.tracks.total(), Some(30));
        assert_eq!(model.tracks.visible(), 24..27);
        assert_eq!(model.display_total(now), Some(30));
        assert!(model.tracks.get(24).is_none());
        assert_eq!(model.tracks.display(24, now), Some(&previous_row));
        let refresh = model.prepare().unwrap();
        assert!(matches!(
            &refresh.query,
            Query::AlbumTracks {
                offset: 24,
                limit: 3,
                ..
            }
        ));
        let mut response = response(&refresh, 30, current);
        let Ok(ViewResponse::AlbumTracks(page)) = &mut response.result else {
            unreachable!()
        };
        for track in &mut page.rows {
            track.play_count = 5;
        }
        let completion = model.complete_page(
            &refresh.query,
            refresh.generation,
            response,
            current,
            "wrong response",
            now,
        );
        model.retire_worker();
        assert_eq!(completion.published, Published::Tracks);
        assert_eq!(model.display_total(now), Some(30));
        assert_eq!(model.tracks.get(24).unwrap().play_count, 5);
        assert_eq!(model.albums.get(0), Some(&card));
        model.select(None, now);
        assert!(model.prepare().is_none());
    }

    #[test]
    fn bitrate_revision_refreshes_only_members_preserving_counts_selection_and_geometry() {
        let now = Instant::now();
        let mut model = AlbumModel::default();
        model.sync(true, QueryRevisions::default(), 3, now);
        let albums = model.prepare().unwrap();
        finish(&mut model, albums, 200, now);
        model.set_query(Some("album search".into()), now);
        model.toggle_sort(now);
        model.select(Some(key("A")), now);
        let tracks = model.prepare().unwrap();
        finish(&mut model, tracks, 60, now);
        model.set_tracks_query(Some("track search".into()), now);
        model.albums.begin(0, ALBUM_PAGE);
        model.albums.accept(Page {
            total: 100,
            rows: vec![summary(1)],
        });
        model.albums.demand(72..80, ALBUM_PAGE);
        model.tracks.begin(0, 3);
        model.tracks.accept(Page {
            total: 30,
            rows: vec![row(1), row(2), row(1)],
        });
        model.tracks.demand(24..27, 3);
        let selected = model.selected.clone();
        let sort = model.sort();
        let generation = model.generation();
        let cards_generation = model.albums_projection.generation();
        let revisions = QueryRevisions {
            bitrate: 1,
            ..model.revisions
        };
        let change = model.sync(true, revisions, 3, now);
        assert!(!change.data.albums && change.data.tracks);
        assert_eq!(model.albums.get(0), Some(&summary(1)));
        assert_eq!(model.albums.display(0, now), Some(&summary(1)));
        assert_eq!(model.albums_projection.generation(), cards_generation);
        assert!(model.tracks.get(0).is_none());
        assert_eq!(model.albums.total(), Some(100));
        assert_eq!(model.tracks.total(), Some(30));
        assert_eq!(model.display_total(now), Some(30));
        assert_eq!(model.albums.len(), 100);
        assert_eq!(model.tracks.len(), 30);
        assert_eq!(model.albums.visible(), 72..80);
        assert_eq!(model.tracks.visible(), 24..27);
        assert_eq!(model.selected, selected);
        assert_eq!(model.query(), Some("album search"));
        assert_eq!(model.tracks_query(), Some("track search"));
        assert_eq!(model.sort(), sort);
        assert_ne!(model.generation(), generation);
        let tracks = model.prepare().unwrap();
        finish(&mut model, tracks, 30, now);
        assert_eq!(model.display_total(now), Some(30));
        model.select(None, now);
        assert_eq!(model.display_total(now), Some(100));
        // The old viewport is still cold; this is its existing demand, not a card refresh.
        let albums = model.prepare().unwrap();
        assert!(matches!(&albums.query, Query::Albums { offset: 72, .. }));
        finish(&mut model, albums, 100, now);
        assert_eq!(model.display_total(now), Some(100));
    }

    #[test]
    fn bitrate_revision_preserves_empty_counts_without_bootstrapping_either_listing() {
        let now = Instant::now();
        for tracks in [false, true] {
            for filtered in [false, true] {
                let mut model = AlbumModel::default();
                model.sync(true, QueryRevisions::default(), 2, now);
                if tracks {
                    model.select(Some(key("A")), now);
                }
                let unfiltered = model.prepare().unwrap();
                finish(&mut model, unfiltered, if filtered { 10 } else { 0 }, now);
                if filtered {
                    if tracks {
                        model.set_tracks_query(Some("no matches".into()), now);
                    } else {
                        model.set_query(Some("no matches".into()), now);
                    }
                    let empty = model.prepare().unwrap();
                    finish(&mut model, empty, 0, now);
                }
                assert_eq!(model.display_total(now), Some(0));
                let revisions = QueryRevisions {
                    bitrate: 1,
                    ..model.revisions
                };
                model.sync(true, revisions, 2, now);
                let (matched, len) = if tracks {
                    (model.tracks().total(), model.tracks().len())
                } else {
                    (model.albums().total(), model.albums().len())
                };
                assert_eq!(matched, Some(0));
                assert_eq!(len, 0);
                assert_eq!(model.display_total(now), Some(0));
                assert!(model.prepare().is_none());

                model.sync(
                    true,
                    QueryRevisions {
                        album: 1,
                        ..revisions
                    },
                    2,
                    now,
                );
                assert_eq!(model.display_total(now), None);
                let len = if tracks {
                    model.tracks().len()
                } else {
                    model.albums().len()
                };
                assert_eq!(len, 1);
                assert!(model.prepare().is_some());
            }
        }
    }

    #[test]
    fn stale_bitrate_delivery_rejects_member_rows_but_accepts_unchanged_cards() {
        let now = Instant::now();
        for tracks in [false, true] {
            let mut model = AlbumModel::default();
            model.sync(true, QueryRevisions::default(), 2, now);
            if tracks {
                model.select(Some(key("A")), now);
                model.set_tracks_query(Some("needle".into()), now);
            } else {
                model.set_query(Some("needle".into()), now);
            }
            let old = model.prepare().unwrap();
            let previous = model.revisions;
            let revisions = QueryRevisions {
                bitrate: 1,
                ..previous
            };
            let completion = model.complete_page(
                &old.query,
                old.generation,
                response(&old, 10, previous),
                revisions,
                "wrong response",
                now,
            );
            if !tracks {
                assert_eq!(completion.published, Published::Albums);
                assert_eq!(model.display_total(now), Some(10));
                model.retire_worker();
                assert!(model.prepare().is_none(), "card payload remains current");
                continue;
            }
            model.retire_worker();
            assert_eq!(completion.published, Published::None);
            assert!(model.display_total(now).is_none());
            assert!(model.albums.get(0).is_none());
            assert!(model.tracks.get(0).is_none());
            let fresh = model.prepare().unwrap();
            assert_eq!(
                finish(&mut model, fresh, 2, now).published,
                Published::Tracks
            );
        }
    }

    #[test]
    fn hidden_bitrate_revision_preserves_viewport_and_physical_slot_until_delivery() {
        let now = Instant::now();
        for tracks in [false, true] {
            let mut model = AlbumModel::default();
            model.sync(true, QueryRevisions::default(), 2, now);
            if tracks {
                model.select(Some(key("A")), now);
            }
            let first = model.prepare().unwrap();
            finish(&mut model, first, 100, now);
            if tracks {
                assert!(model.demand_tracks(80..82, model.generation()));
            } else {
                model.demand_albums(80..82);
            }
            let pending = model.prepare().unwrap();
            let selected = model.selected.clone();
            model.sync(false, model.revisions, 2, now);
            let revisions = QueryRevisions {
                bitrate: 1,
                ..model.revisions
            };
            model.sync(false, revisions, 2, now);
            assert!(model.prepare().is_none());
            let pages_len = if tracks {
                model.tracks.len()
            } else {
                model.albums.len()
            };
            let visible = if tracks {
                model.tracks.visible()
            } else {
                model.albums.visible()
            };
            assert_eq!(pages_len, 100);
            assert_eq!(visible, 80..82);
            assert_eq!(model.selected, selected);
            model.sync(true, revisions, 2, now);
            assert!(model.prepare().is_none());
            assert_eq!(
                finish(&mut model, pending, 100, now).published,
                Published::None
            );
            let fresh = model.prepare().unwrap();
            assert!(matches!(
                fresh.query,
                Query::AlbumTracks { offset: 80, .. } | Query::Albums { offset: 72, .. }
            ));
        }
    }

    #[test]
    fn timed_out_page_keeps_slot_and_never_publishes_late_rows_or_counts() {
        let now = Instant::now();
        for replacement in [false, true] {
            let mut model = AlbumModel::default();
            model.sync(true, QueryRevisions::default(), 2, now);
            let first = model.prepare().unwrap();
            model.timeout(
                &first.query,
                first.generation,
                QueryResponse {
                    revisions: model.revisions,
                    result: Err(rivu_protocol::response::QueryError::Message(
                        "timed out".into(),
                    )),
                },
                model.revisions,
                now,
            );
            assert!(model.albums.error(0).is_some());
            assert!(model.prepare().is_none());
            if replacement {
                model.select(Some(key("latest")), now);
            } else {
                model.retry(now);
            }
            assert!(model.prepare().is_none());
            let completion = finish(&mut model, first, 100, now);
            assert_eq!(completion.published, Published::None);
            assert!(model.albums().display_total(now).is_none());
            assert!(model.prepare().is_some());
        }
    }

    #[test]
    fn cancellation_retains_physical_slot_until_obsolete_delivery() {
        let now = Instant::now();
        for transition in 0..3 {
            let mut model = AlbumModel::default();
            model.sync(true, QueryRevisions::default(), 2, now);
            let request = model.prepare().unwrap();
            match transition {
                0 => {
                    model.sync(false, model.revisions, 2, now);
                }
                1 => {
                    model.set_query(Some("needle".into()), now);
                }
                2 => {
                    model.select(Some(key("Selected")), now);
                }
                _ => unreachable!(),
            }
            assert!(model.worker_pending);
            assert!(model.prepare().is_none());
            let completion = finish(&mut model, request, 0, now);
            assert_eq!(completion.published, Published::None);
            assert!(!model.worker_pending);
            assert_eq!(model.albums.total(), None);
            if transition == 0 {
                assert!(model.prepare().is_none());
                model.sync(true, model.revisions, 2, now);
            }
            assert!(model.prepare().is_some());
        }
    }

    #[test]
    fn delivery_checks_current_revision_before_publishing_listing() {
        let now = Instant::now();
        let mut model = AlbumModel::default();
        model.set_query(Some("needle".into()), now);
        model.sync(true, QueryRevisions::default(), 2, now);
        let request = model.prepare().unwrap();
        let old = model.revisions;
        let current = QueryRevisions { album: 1, ..old };
        let completion = model.complete_page(
            &request.query,
            request.generation,
            response(&request, 1, old),
            current,
            "wrong response",
            now,
        );
        assert!(completion.data.albums);
        assert_eq!(completion.published, Published::None);
        assert!(model.display_total(now).is_none());
        assert!(model.prepare().is_none());
        model.retire_worker();
        assert!(model.prepare().is_some());
    }

    #[test]
    fn page_retry_preserves_successful_rows_and_count() {
        let now = Instant::now();
        for tracks in [false, true] {
            let mut model = AlbumModel::default();
            let revisions = QueryRevisions::default();
            model.sync(true, revisions, 2, now);
            if tracks {
                model.select(Some(key("A")), now);
            }
            let first = model.prepare().unwrap();
            let total = if tracks { 4 } else { 26 };
            finish(&mut model, first, total, now);
            if tracks {
                model.demand_tracks(2..4, model.generation());
            } else {
                model.demand_albums(24..26);
            }
            let failed = model.prepare().unwrap();
            model.complete_page(
                &failed.query,
                failed.generation,
                QueryResponse {
                    revisions,
                    result: Err(QueryError::Message("page failed".into())),
                },
                revisions,
                "wrong response",
                now,
            );
            model.retire_worker();
            assert!(
                model.prepare().is_none(),
                "page errors do not retry automatically"
            );
            if !tracks {
                model.select(Some(key("A")), now);
                model.retry(now);
                model.select(None, now);
                assert_eq!(model.albums.error(24), Some("page failed"));
                assert!(model.prepare().is_none());
            }
            model.retry(now);
            assert_eq!(model.display_total(now), Some(total));
            if tracks {
                assert_eq!(model.tracks.get(0).unwrap().id, 0);
                assert_eq!(model.tracks.error(2), None);
            } else {
                assert_eq!(model.albums.get(0).unwrap().representative.id, 0);
                assert_eq!(model.albums.error(24), None);
            }
            let retry = model.prepare().unwrap();
            assert_eq!(retry.query, failed.query);
        }
    }

    #[test]
    fn listing_count_follows_current_search_and_clearing() {
        let now = Instant::now();
        for tracks in [false, true] {
            let mut model = AlbumModel::default();
            model.sync(true, QueryRevisions::default(), 2, now);
            if tracks {
                model.select(Some(key("A")), now);
            }
            for (query, count) in [
                (None, 2),
                (Some("match"), 1),
                (Some("absent"), 0),
                (None, 2),
            ] {
                let query = query.map(str::to_owned);
                if tracks {
                    model.set_tracks_query(query.clone(), now);
                } else {
                    model.set_query(query.clone(), now);
                }
                assert_eq!(model.display_total(now), None);
                let request = model.prepare().unwrap();
                let submitted = match &request.query {
                    Query::Albums { query, .. } | Query::AlbumTracks { query, .. } => query,
                    _ => unreachable!(),
                };
                assert_eq!(submitted, &query);
                assert_eq!(
                    finish(&mut model, request, count, now).published,
                    if tracks {
                        Published::Tracks
                    } else {
                        Published::Albums
                    }
                );
                assert_eq!(model.display_total(now), Some(count));
                assert!(model.prepare().is_none());
            }
        }
    }

    #[test]
    fn selection_reuses_same_album_and_independently_invalidates_searches() {
        let now = Instant::now();
        let mut model = AlbumModel::default();
        model.sync(true, QueryRevisions::default(), 2, now);
        let albums = model.prepare().unwrap();
        finish(&mut model, albums, 0, now);
        assert!(model.select(Some(key("A")), now).reset_tracks);
        model.set_tracks_query(Some("needle".into()), now);
        let tracks = model.prepare().unwrap();
        finish(&mut model, tracks, 2, now);
        assert_eq!(model.albums.total(), Some(0));
        model.select(None, now);
        assert!(!model.select(Some(key("A")), now).reset_tracks);
        assert_eq!(model.tracks_query(), Some("needle"));
        assert_eq!(model.tracks.get(1).unwrap().id, 1);
        assert!(model.prepare().is_none());
        let change = model.set_query(Some("other".into()), now).unwrap();
        assert!(change.albums);
        assert!(!change.tracks);
        assert_eq!(model.tracks.get(1).unwrap().id, 1);
        assert!(model.select(Some(key("B")), now).reset_tracks);
        assert!(model.tracks_query().is_none());
        assert!(model.tracks.get(1).is_none());
        assert!(model.tracks.total().is_none());
    }

    #[test]
    fn track_switch_and_page_resize_keep_old_worker_from_publishing() {
        let now = Instant::now();
        let mut model = AlbumModel::default();
        model.sync(true, QueryRevisions::default(), 2, now);
        model.select(Some(key("A")), now);
        let old = model.prepare().unwrap();
        model.select(Some(key("B")), now);
        model.sync(true, model.revisions, 4, now);
        assert!(model.prepare().is_none());
        assert_eq!(finish(&mut model, old, 2, now).published, Published::None);
        let request = model.prepare().unwrap();
        assert!(
            matches!(&request.query, Query::AlbumTracks { key: album, limit: 4, .. } if album == &key("B"))
        );
        finish(&mut model, request, 100, now);
        assert!(model.demand_tracks(80..84, model.generation()));
        let distant = model.prepare().unwrap();
        assert!(matches!(
            distant.query,
            Query::AlbumTracks { offset: 80, .. }
        ));
        finish(&mut model, distant, 100, now);
        assert_eq!(model.tracks.len(), 100);
        assert_eq!(model.tracks.get(83).unwrap().id, 83);
    }

    #[test]
    fn failures_survive_hide_and_cancel_until_explicit_retry() {
        let now = Instant::now();
        let mut model = AlbumModel::default();
        model.sync(true, QueryRevisions::default(), 2, now);
        model.select(Some(key("A")), now);
        let request = model.prepare().unwrap();
        model.complete_page(
            &request.query,
            request.generation,
            QueryResponse {
                revisions: model.revisions,
                result: Err(rivu_protocol::response::QueryError::Message(
                    "offline".into(),
                )),
            },
            model.revisions,
            "wrong response",
            now,
        );
        model.retire_worker();
        model.sync(false, model.revisions, 2, now);
        model.sync(true, model.revisions, 2, now);
        assert!(model.tracks.error(0).is_some());
        assert!(model.prepare().is_none());
        model.retry(now);
        let request = model.prepare().unwrap();
        finish(&mut model, request, 1, now);
        assert_eq!(model.tracks.error(0), None);
        assert_eq!(model.tracks.get(0).unwrap().id, 0);
    }

    fn queries() -> [Query; 2] {
        [
            Query::Albums {
                query: None,
                sort: AlbumSort::default(),
                offset: 0,
                limit: 2,
            },
            Query::AlbumTracks {
                key: AlbumKey {
                    directory: "/music".into(),
                    title: "Album".into(),
                },
                query: None,
                offset: 0,
                limit: 2,
            },
        ]
    }

    #[test]
    fn hidden_data_changes_discard_pages_and_bootstrap_previous_empty_results() {
        let now = Instant::now();
        for query in queries() {
            let revisions = QueryRevisions {
                album: 1,
                ..Default::default()
            };
            for total in [0, 2] {
                let mut projection = QueryTracker::default();
                let mut pages = paged::PagedRows::default();
                sync_pages(
                    &mut projection,
                    &mut pages,
                    &query,
                    QueryRevisions::default(),
                    now,
                );
                pages.begin(0, 2);
                pages.accept(Page {
                    total,
                    rows: if total == 0 { vec![] } else { vec![1, 2] },
                });
                cancel_pages(&mut projection, &mut pages);
                assert!(sync_pages(
                    &mut projection,
                    &mut pages,
                    &query,
                    revisions,
                    now
                ));
                assert_eq!(pages.total(), None);
                assert!(!pages.is_empty());
                assert!(pages.get(0).is_none());
                assert_eq!(pages.demand(pages.visible(), 2), Some(0));
                pages.begin(0, 2);
                pages.accept(Page {
                    total: 1,
                    rows: vec![3],
                });
                assert_eq!(pages.total(), Some(1));
                assert_eq!(pages.get(0), Some(&3));
            }
        }
    }

    #[test]
    fn filtered_empty_album_results_survive_hide_and_page_boundary_changes() {
        let now = Instant::now();
        for mut query in queries() {
            match &mut query {
                Query::Albums { query, .. } | Query::AlbumTracks { query, .. } => {
                    *query = Some("needle".into());
                }
                _ => unreachable!(),
            }
            let mut projection = QueryTracker::default();
            let mut pages = paged::PagedRows::<usize>::default();
            let revisions = QueryRevisions::default();
            sync_pages(&mut projection, &mut pages, &query, revisions, now);
            pages.begin(0, 2);
            pages.accept(Page {
                total: 0,
                rows: vec![],
            });
            cancel_pages(&mut projection, &mut pages);
            let generation = projection.generation();
            projection.cancel();
            pages.configure_page_size(4);
            match &mut query {
                Query::Albums { limit, .. } | Query::AlbumTracks { limit, .. } => *limit = 4,
                _ => unreachable!(),
            }
            assert!(!sync_pages(
                &mut projection,
                &mut pages,
                &query,
                revisions,
                now
            ));
            assert!(!projection.accepts(&query, generation, revisions));
            assert_eq!(pages.total(), Some(0));
            assert!(pages.demand(pages.visible(), 4).is_none());

            let refreshed = QueryRevisions {
                album: 1,
                ..revisions
            };
            assert!(sync_pages(
                &mut projection,
                &mut pages,
                &query,
                refreshed,
                now
            ));
            assert_eq!(pages.total(), None);
            assert_eq!(pages.demand(pages.visible(), 4), Some(0));
        }
    }

    #[test]
    fn paging_and_unrelated_revisions_do_not_change_album_data_keys() {
        let now = Instant::now();
        let mut projection = QueryTracker::default();
        let mut pages = paged::PagedRows::default();
        let query = queries().into_iter().next().unwrap();
        sync_pages(
            &mut projection,
            &mut pages,
            &query,
            QueryRevisions::default(),
            now,
        );
        pages.begin(0, 2);
        pages.accept(Page {
            total: 2,
            rows: vec![1, 2],
        });
        let generation = projection.generation();
        let paging = Query::Albums {
            query: None,
            sort: AlbumSort::default(),
            offset: 24,
            limit: 48,
        };
        let revisions = QueryRevisions {
            playlist: 1,
            queue: 1,
            duplicate_members: 1,
            duplicate_groups: 1,
            ..Default::default()
        };
        assert!(!sync_pages(
            &mut projection,
            &mut pages,
            &paging,
            revisions,
            now
        ));
        assert_eq!(projection.generation(), generation);
        assert_eq!(pages.get(0), Some(&1));
        assert!(projection.accepts(&paging, generation, revisions));
        let changed = Query::Albums {
            query: Some("new search".into()),
            sort: AlbumSort::default(),
            offset: 0,
            limit: 2,
        };
        assert!(sync_pages(
            &mut projection,
            &mut pages,
            &changed,
            revisions,
            now
        ));
        assert!(pages.get(0).is_none());
        assert_eq!(pages.demand(pages.visible(), 2), Some(0));
    }
}
