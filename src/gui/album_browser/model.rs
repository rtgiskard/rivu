use std::ops::Range;

use crate::{core::QueryTicket, projection::QueryProjection};
use rivu_protocol::{
    model::{AlbumKey, AlbumSort, AlbumSummary, LibraryRow, Query, QueryStamp},
    response::{QueryResponse, QueryRevisions, ViewResponse},
};

use super::super::paged::PagedRows;

const ALBUM_PAGE: usize = 24;

#[derive(Default)]
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

pub(super) struct CountRequest {
    pub query: Query,
    pub page_query: Query,
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
    pub counts: Option<(usize, usize)>,
}

/// Owns both logical listings and their shared physical query slot. Cancelling a
/// listing never releases that slot: only delivery of the detached worker does.
pub(super) struct AlbumModel {
    visible: bool,
    revisions: QueryRevisions,
    albums_projection: QueryProjection,
    tracks_projection: QueryProjection,
    tracks_key: Option<AlbumKey>,
    page_size: usize,
    worker_pending: bool,
    page_timed_out: bool,
    albums_total: Option<(QueryStamp, usize)>,
    tracks_total: Option<(QueryStamp, usize)>,
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
            albums_projection: QueryProjection::default(),
            tracks_projection: QueryProjection::default(),
            tracks_key: None,
            page_size: 1,
            worker_pending: false,
            page_timed_out: false,
            albums_total: None,
            tracks_total: None,
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

    pub fn total(&self) -> Option<usize> {
        let total = if self.selected.is_some() {
            self.tracks_total
        } else {
            self.albums_total
        };
        total
            .filter(|(stamp, _)| Some(*stamp) == self.active_projection().stamp())
            .map(|(_, total)| total)
    }

    pub fn sync(
        &mut self,
        visible: bool,
        revisions: QueryRevisions,
        page_size: usize,
    ) -> SyncChange {
        let visibility = self.visible != visible;
        let demand = self.page_size != page_size.max(1);
        if demand {
            self.tracks_projection.cancel();
            self.tracks.configure_page_size(page_size.max(1));
        }
        self.page_size = page_size.max(1);
        let data = self.sync_data(revisions);
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

    pub fn select(&mut self, selected: Option<AlbumKey>) -> SelectionChange {
        let reset_tracks = selected
            .as_ref()
            .is_some_and(|key| self.tracks_key.as_ref() != Some(key));
        if reset_tracks {
            self.tracks_key = selected.clone();
            self.tracks_total = None;
            self.tracks_query = None;
            self.tracks.clear();
        }
        self.selected = selected;
        self.cancel();
        SelectionChange {
            data: self.sync_data(self.revisions),
            reset_tracks,
        }
    }

    pub fn set_query(&mut self, query: Option<String>) -> Option<DataChange> {
        if self.query == query {
            return None;
        }
        self.query = query;
        self.albums.clear();
        self.cancel();
        Some(self.sync_data(self.revisions))
    }

    pub fn set_tracks_query(&mut self, query: Option<String>) -> Option<DataChange> {
        if self.tracks_query == query {
            return None;
        }
        self.tracks_query = query;
        self.tracks.clear();
        self.cancel();
        Some(self.sync_data(self.revisions))
    }

    pub fn toggle_sort(&mut self) -> DataChange {
        self.sort = match self.sort {
            AlbumSort::Title => AlbumSort::Artist,
            AlbumSort::Artist => AlbumSort::Title,
        };
        self.albums.clear();
        self.cancel();
        self.sync_data(self.revisions)
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

    pub fn retry(&mut self) {
        if self.selected.is_some() {
            self.tracks.retry();
        } else {
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
    ) -> DataChange {
        let data = self.sync_data(current_revisions);
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
    ) -> Completion {
        // Re-read authority at delivery, not merely the worker's captured revision.
        let data = self.sync_data(current_revisions);
        let mut completion = Completion {
            data,
            published: Published::None,
            counts: None,
        };
        if self.page_timed_out {
            return completion;
        }
        if !self.visible
            || !self
                .active_projection()
                .accepts(query, generation, response.revisions)
        {
            return completion;
        }
        match response.result {
            Ok(ViewResponse::Albums(page)) => {
                let stamp = Query::Albums {
                    query: None,
                    sort: self.sort,
                    offset: 0,
                    limit: 0,
                }
                .stamp(response.revisions);
                if self.query.is_none() {
                    self.albums_total = Some((stamp, page.total));
                }
                completion.counts = self
                    .albums_total
                    .filter(|(s, _)| *s == stamp)
                    .map(|(_, total)| (page.total, total));
                self.albums.accept(page);
                completion.published = Published::Albums;
            }
            Ok(ViewResponse::AlbumTracks(page)) => {
                let Query::AlbumTracks { key, .. } = query else {
                    unreachable!()
                };
                let stamp = Query::AlbumTracks {
                    key: key.clone(),
                    query: None,
                    offset: 0,
                    limit: 0,
                }
                .stamp(response.revisions);
                if self.tracks_query.is_none() {
                    self.tracks_total = Some((stamp, page.total));
                }
                completion.counts = self
                    .tracks_total
                    .filter(|(s, _)| *s == stamp)
                    .map(|(_, total)| (page.total, total));
                self.tracks.accept(page);
                completion.published = Published::Tracks;
            }
            Err(error) => self.fail_page(error.to_string()),
            _ => self.fail_page(unexpected_response.to_owned()),
        }
        completion
    }

    /// Only physical completion releases the shared slot, including obsolete work.
    pub fn retire_worker(&mut self) {
        self.worker_pending = false;
    }

    pub fn prepare_count(&mut self, page_query: &Query, generation: u64) -> Option<CountRequest> {
        if self.page_timed_out
            || !self.visible
            || !self
                .active_projection()
                .accepts(page_query, generation, self.revisions)
        {
            return None;
        }
        let query = match page_query {
            Query::Albums {
                query: Some(_),
                sort,
                ..
            } => Query::Albums {
                query: None,
                sort: *sort,
                offset: 0,
                limit: 0,
            },
            Query::AlbumTracks {
                key,
                query: Some(_),
                ..
            } => Query::AlbumTracks {
                key: key.clone(),
                query: None,
                offset: 0,
                limit: 0,
            },
            _ => return None,
        };
        let total = if self.selected.is_some() {
            self.tracks_total
        } else {
            self.albums_total
        };
        if total.is_some_and(|(stamp, _)| stamp == query.stamp(self.revisions)) {
            return None;
        }
        let ticket = if self.selected.is_some() {
            self.tracks_projection.ticket()
        } else {
            self.albums_projection.ticket()
        };
        Some(CountRequest {
            query,
            page_query: page_query.clone(),
            generation,
            ticket,
        })
    }

    pub fn complete_count(
        &mut self,
        request: &CountRequest,
        response: QueryResponse,
        current_revisions: QueryRevisions,
    ) -> Completion {
        let data = self.sync_data(current_revisions);
        let mut completion = Completion {
            data,
            published: Published::None,
            counts: None,
        };
        if !self.visible
            || !self.active_projection().accepts(
                &request.page_query,
                request.generation,
                response.revisions,
            )
            || request.query.stamp(response.revisions) != request.query.stamp(current_revisions)
        {
            return completion;
        }
        let (total, matched, published) = match (&request.query, response.result) {
            (Query::Albums { .. }, Ok(ViewResponse::Albums(page))) => {
                self.albums_total = Some((request.query.stamp(response.revisions), page.total));
                (page.total, self.albums.total(), Published::Albums)
            }
            (Query::AlbumTracks { .. }, Ok(ViewResponse::AlbumTracks(page))) => {
                self.tracks_total = Some((request.query.stamp(response.revisions), page.total));
                (page.total, self.tracks.total(), Published::Tracks)
            }
            _ => return completion,
        };
        completion.published = published;
        completion.counts = matched.map(|matched| (matched, total));
        completion
    }

    fn albums_query(&self) -> Query {
        Query::Albums {
            query: self.query.clone(),
            sort: self.sort,
            offset: 0,
            limit: ALBUM_PAGE,
        }
    }

    fn sync_data(&mut self, revisions: QueryRevisions) -> DataChange {
        self.revisions = revisions;
        let albums_query = self.albums_query();
        let albums = sync_pages(
            &mut self.albums_projection,
            &mut self.albums,
            &albums_query,
            revisions,
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
            )
        });
        DataChange { albums, tracks }
    }

    fn active_projection(&self) -> &QueryProjection {
        if self.selected.is_some() {
            &self.tracks_projection
        } else {
            &self.albums_projection
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
    projection: &mut QueryProjection,
    pages: &mut PagedRows<T>,
    query: &Query,
    revisions: QueryRevisions,
) -> bool {
    let identity_changed = !projection.matches_identity(query);
    if !projection.sync(query, revisions) {
        return false;
    }
    if identity_changed {
        pages.clear();
    } else {
        // Keep viewport geometry until the refreshed page supplies its real total.
        pages.invalidate();
    }
    true
}

fn cancel_pages<T>(projection: &mut QueryProjection, pages: &mut PagedRows<T>) {
    projection.cancel();
    pages.cancel_pending();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::paged;
    use rivu_protocol::model::Page;
    fn key(title: &str) -> AlbumKey {
        AlbumKey {
            directory: "/music".into(),
            title: title.into(),
        }
    }

    fn row(id: i64) -> LibraryRow {
        LibraryRow {
            id,
            path: Default::default(),
            title: id.to_string(),
            artist: String::new(),
            album: String::new(),
            duration: None,
            codec: String::new(),
            bitrate_bps: None,
            bitrate_exact: false,
            channels: 0,
            sample_rate: 0,
            bits_per_sample: None,
            cue_number: None,
            favorite: false,
            missing: false,
            play_count: 0,
        }
    }

    fn response(request: &Request, total: usize, revisions: QueryRevisions) -> QueryResponse {
        let result = match &request.query {
            Query::Albums { .. } => ViewResponse::Albums(Page {
                total,
                rows: vec![],
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

    fn finish(model: &mut AlbumModel, request: Request, total: usize) -> Completion {
        let response = response(&request, total, model.revisions);
        let completion = model.complete_page(
            &request.query,
            request.generation,
            response,
            model.revisions,
            "wrong response",
        );
        model.retire_worker();
        completion
    }

    fn summary(id: i64) -> AlbumSummary {
        AlbumSummary {
            key: key("A"),
            artist: "artist".into(),
            track_count: 3,
            representative: rivu_protocol::model::Track {
                id,
                path: "/music/a.flac".into(),
                fingerprint: None,
                cue: None,
                title: "title".into(),
                artist: "artist".into(),
                album: "A".into(),
                duration: Some(10.),
                codec: "flac".into(),
                channels: 2,
                sample_rate: 44100,
                bitrate_bps: None,
                bitrate_exact: false,
                track_number: None,
                disc_number: None,
                bits_per_sample: None,
                release_date: None,
                favorite: false,
                decoder_hint: Default::default(),
                missing: false,
                play_count: 0,
                last_played: None,
            },
        }
    }

    #[test]
    fn bitrate_revision_reloads_both_listings_preserving_selection_queries_and_geometry() {
        let mut model = AlbumModel::default();
        model.sync(true, QueryRevisions::default(), 3);
        model.set_query(Some("album search".into()));
        model.toggle_sort();
        model.select(Some(key("A")));
        model.set_tracks_query(Some("track search".into()));
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
        let revisions = QueryRevisions {
            bitrate: 1,
            ..model.revisions
        };
        let change = model.sync(true, revisions, 3);
        assert!(change.data.albums && change.data.tracks);
        assert!(model.albums.get(0).is_none());
        assert!(model.tracks.get(0).is_none());
        assert_eq!(model.albums.len(), 100);
        assert_eq!(model.tracks.len(), 30);
        assert_eq!(model.albums.visible(), 72..80);
        assert_eq!(model.tracks.visible(), 24..27);
        assert_eq!(model.selected, selected);
        assert_eq!(model.query(), Some("album search"));
        assert_eq!(model.tracks_query(), Some("track search"));
        assert_eq!(model.sort(), sort);
        assert_ne!(model.generation(), generation);
    }

    #[test]
    fn stale_bitrate_delivery_rejects_rows_and_counts_then_requests_current_payload() {
        for tracks in [false, true] {
            let mut model = AlbumModel::default();
            model.sync(true, QueryRevisions::default(), 2);
            if tracks {
                model.select(Some(key("A")));
                model.set_tracks_query(Some("needle".into()));
            } else {
                model.set_query(Some("needle".into()));
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
            );
            assert!(model.prepare_count(&old.query, old.generation).is_none());
            model.retire_worker();
            assert_eq!(completion.published, Published::None);
            assert_eq!(completion.counts, None);
            assert!(model.total().is_none());
            assert!(model.albums.get(0).is_none());
            assert!(model.tracks.get(0).is_none());
            let fresh = model.prepare().unwrap();
            assert_eq!(
                finish(&mut model, fresh, 2).published,
                if tracks {
                    Published::Tracks
                } else {
                    Published::Albums
                },
            );
        }
    }

    #[test]
    fn hidden_bitrate_revision_preserves_viewport_and_physical_slot_until_delivery() {
        for tracks in [false, true] {
            let mut model = AlbumModel::default();
            model.sync(true, QueryRevisions::default(), 2);
            if tracks {
                model.select(Some(key("A")));
            }
            let first = model.prepare().unwrap();
            finish(&mut model, first, 100);
            if tracks {
                assert!(model.demand_tracks(80..82, model.generation()));
            } else {
                model.demand_albums(80..82);
            }
            let pending = model.prepare().unwrap();
            let selected = model.selected.clone();
            model.sync(false, model.revisions, 2);
            let revisions = QueryRevisions {
                bitrate: 1,
                ..model.revisions
            };
            model.sync(false, revisions, 2);
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
            model.sync(true, revisions, 2);
            assert!(model.prepare().is_none());
            assert_eq!(finish(&mut model, pending, 100).published, Published::None);
            let fresh = model.prepare().unwrap();
            assert!(matches!(
                fresh.query,
                Query::AlbumTracks { offset: 80, .. } | Query::Albums { offset: 72, .. }
            ));
        }
    }

    #[test]
    fn timed_out_page_keeps_slot_and_never_publishes_late_rows_or_counts() {
        for replacement in [false, true] {
            let mut model = AlbumModel::default();
            model.sync(true, QueryRevisions::default(), 2);
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
            );
            assert_eq!(model.albums.error(0), Some("timed out"));
            assert!(model.prepare().is_none());
            if replacement {
                model.select(Some(key("latest")));
            } else {
                model.retry();
            }
            assert!(model.prepare().is_none());
            let completion = finish(&mut model, first, 100);
            assert_eq!(completion.published, Published::None);
            assert_eq!(completion.counts, None);
            assert!(model.albums_total.is_none());
            assert!(model.prepare().is_some());
        }
    }

    #[test]
    fn cancellation_retains_physical_slot_until_obsolete_delivery() {
        for transition in 0..3 {
            let mut model = AlbumModel::default();
            model.sync(true, QueryRevisions::default(), 2);
            let request = model.prepare().unwrap();
            match transition {
                0 => {
                    model.sync(false, model.revisions, 2);
                }
                1 => {
                    model.set_query(Some("needle".into()));
                }
                2 => {
                    model.select(Some(key("Selected")));
                }
                _ => unreachable!(),
            }
            assert!(model.worker_pending);
            assert!(model.prepare().is_none());
            let completion = finish(&mut model, request, 0);
            assert_eq!(completion.published, Published::None);
            assert!(!model.worker_pending);
            assert_eq!(model.albums.total(), None);
            if transition == 0 {
                assert!(model.prepare().is_none());
                model.sync(true, model.revisions, 2);
            }
            assert!(model.prepare().is_some());
        }
    }

    #[test]
    fn delivery_checks_current_revision_before_publishing_page_or_count() {
        let mut model = AlbumModel::default();
        model.set_query(Some("needle".into()));
        model.sync(true, QueryRevisions::default(), 2);
        let request = model.prepare().unwrap();
        let old = model.revisions;
        let current = QueryRevisions { album: 1, ..old };
        let completion = model.complete_page(
            &request.query,
            request.generation,
            response(&request, 1, old),
            current,
            "wrong response",
        );
        assert!(completion.data.albums);
        assert_eq!(completion.published, Published::None);
        assert!(completion.counts.is_none());
        assert!(
            model
                .prepare_count(&request.query, request.generation)
                .is_none()
        );
        assert!(model.albums_total.is_none());
        assert!(model.prepare().is_none());
        model.retire_worker();
        assert!(model.prepare().is_some());
    }

    fn count_response(
        request: &CountRequest,
        total: usize,
        revisions: QueryRevisions,
    ) -> QueryResponse {
        QueryResponse {
            revisions,
            result: Ok(match request.query {
                Query::Albums { .. } => ViewResponse::Albums(Page {
                    total,
                    rows: vec![],
                }),
                Query::AlbumTracks { .. } => ViewResponse::AlbumTracks(Page {
                    total,
                    rows: vec![],
                }),
                _ => unreachable!(),
            }),
        }
    }

    #[test]
    fn count_cache_is_stamp_scoped_and_reused_across_searches() {
        let mut model = AlbumModel::default();
        model.set_query(Some("first".into()));
        model.sync(true, QueryRevisions::default(), 2);
        let request = model.prepare().unwrap();
        let completion = model.complete_page(
            &request.query,
            request.generation,
            response(&request, 0, model.revisions),
            model.revisions,
            "wrong response",
        );
        assert_eq!(completion.published, Published::Albums);
        assert_eq!(completion.counts, None);
        let count = model
            .prepare_count(&request.query, request.generation)
            .unwrap();
        let completion = model.complete_count(
            &count,
            count_response(&count, 10, model.revisions),
            model.revisions,
        );
        assert_eq!(completion.counts, Some((0, 10)));
        model.retire_worker();
        model.set_query(Some("second".into()));
        let request = model.prepare().unwrap();
        assert!(
            model
                .prepare_count(&request.query, request.generation)
                .is_none()
        );
        assert_eq!(finish(&mut model, request, 0).counts, Some((0, 10)));
        let current = QueryRevisions {
            album: 1,
            ..model.revisions
        };
        model.sync(true, current, 2);
        assert_eq!(model.total(), None);
        let request = model.prepare().unwrap();
        let completion = model.complete_page(
            &request.query,
            request.generation,
            response(&request, 0, current),
            current,
            "wrong response",
        );
        assert_eq!(completion.published, Published::Albums);
        let count = model
            .prepare_count(&request.query, request.generation)
            .unwrap();
        let completion = model.complete_count(
            &count,
            count_response(&count, 99, QueryRevisions::default()),
            current,
        );
        assert!(completion.counts.is_none());
        assert_eq!(model.total(), None);
        model.retire_worker();
    }

    #[test]
    fn count_failure_preserves_published_rows_and_holds_slot_until_retirement() {
        for tracks in [false, true] {
            let mut model = AlbumModel::default();
            model.sync(true, QueryRevisions::default(), 2);
            if tracks {
                model.select(Some(key("A")));
                model.set_tracks_query(Some("needle".into()));
            } else {
                model.set_query(Some("needle".into()));
            }
            let request = model.prepare().unwrap();
            let mut page = response(&request, 3, model.revisions);
            if !tracks {
                page.result = Ok(ViewResponse::Albums(Page {
                    total: 3,
                    rows: vec![summary(42)],
                }));
            }
            let completion = model.complete_page(
                &request.query,
                request.generation,
                page,
                model.revisions,
                "wrong response",
            );
            assert_ne!(completion.published, Published::None);
            assert_eq!(completion.counts, None);
            let count = model
                .prepare_count(&request.query, request.generation)
                .unwrap();
            assert!(model.prepare().is_none());
            let completion = model.complete_count(
                &count,
                QueryResponse {
                    revisions: model.revisions,
                    result: Err(rivu_protocol::response::QueryError::Message(
                        "timed out".into(),
                    )),
                },
                model.revisions,
            );
            assert_eq!(completion.counts, None);
            assert!(model.prepare().is_none());
            if tracks {
                assert_eq!(model.tracks.get(0).unwrap().id, 0);
                assert_eq!(model.tracks.error(0), None);
                model.demand_tracks(2..3, model.generation());
            } else {
                assert_eq!(model.albums.get(0).unwrap().representative.id, 42);
                assert_eq!(model.albums.error(0), None);
                model.demand_albums(24..25);
            }
            assert!(model.prepare().is_none());
            model.retire_worker();
            if tracks {
                assert!(model.prepare().is_some());
            }
        }
    }

    #[test]
    fn obsolete_counts_cannot_update_replacement_or_reopened_listing() {
        for transition in 0..4 {
            let mut model = AlbumModel::default();
            model.sync(true, QueryRevisions::default(), 2);
            model.select(Some(key("A")));
            model.set_tracks_query(Some("first".into()));
            let request = model.prepare().unwrap();
            model.complete_page(
                &request.query,
                request.generation,
                response(&request, 3, model.revisions),
                model.revisions,
                "wrong response",
            );
            let count = model
                .prepare_count(&request.query, request.generation)
                .unwrap();
            let old = model.revisions;
            match transition {
                0 => {
                    model.set_tracks_query(Some("second".into()));
                }
                1 => {
                    model.select(Some(key("B")));
                }
                2 => {
                    model.sync(false, old, 2);
                    model.sync(true, old, 2);
                }
                3 => {
                    model.sync(true, QueryRevisions { album: 1, ..old }, 2);
                }
                _ => unreachable!(),
            }
            let completion =
                model.complete_count(&count, count_response(&count, 99, old), model.revisions);
            assert_eq!(completion.published, Published::None);
            assert_eq!(completion.counts, None);
            assert_eq!(model.total(), None);
            assert!(model.prepare().is_none());
            model.retire_worker();
        }
    }

    #[test]
    fn selection_reuses_same_album_and_independently_invalidates_searches() {
        let mut model = AlbumModel::default();
        model.sync(true, QueryRevisions::default(), 2);
        let albums = model.prepare().unwrap();
        finish(&mut model, albums, 0);
        assert!(model.select(Some(key("A"))).reset_tracks);
        model.set_tracks_query(Some("needle".into()));
        let tracks = model.prepare().unwrap();
        finish(&mut model, tracks, 2);
        assert_eq!(model.albums.total(), Some(0));
        model.select(None);
        assert!(!model.select(Some(key("A"))).reset_tracks);
        assert_eq!(model.tracks_query(), Some("needle"));
        assert_eq!(model.tracks.get(1).unwrap().id, 1);
        assert!(model.prepare().is_none());
        let change = model.set_query(Some("other".into())).unwrap();
        assert!(change.albums);
        assert!(!change.tracks);
        assert_eq!(model.tracks.get(1).unwrap().id, 1);
        assert!(model.select(Some(key("B"))).reset_tracks);
        assert!(model.tracks_query().is_none());
        assert!(model.tracks.get(1).is_none());
        assert!(model.tracks_total.is_none());
    }

    #[test]
    fn track_switch_and_page_resize_keep_old_worker_from_publishing() {
        let mut model = AlbumModel::default();
        model.sync(true, QueryRevisions::default(), 2);
        model.select(Some(key("A")));
        let old = model.prepare().unwrap();
        model.select(Some(key("B")));
        model.sync(true, model.revisions, 4);
        assert!(model.prepare().is_none());
        assert_eq!(finish(&mut model, old, 2).published, Published::None);
        let request = model.prepare().unwrap();
        assert!(
            matches!(&request.query, Query::AlbumTracks { key: album, limit: 4, .. } if album == &key("B"))
        );
        finish(&mut model, request, 100);
        assert!(model.demand_tracks(80..84, model.generation()));
        let distant = model.prepare().unwrap();
        assert!(matches!(
            distant.query,
            Query::AlbumTracks { offset: 80, .. }
        ));
        finish(&mut model, distant, 100);
        assert_eq!(model.tracks.len(), 100);
        assert_eq!(model.tracks.get(83).unwrap().id, 83);
    }

    #[test]
    fn failures_survive_hide_and_cancel_until_explicit_retry() {
        let mut model = AlbumModel::default();
        model.sync(true, QueryRevisions::default(), 2);
        model.select(Some(key("A")));
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
        );
        model.retire_worker();
        model.sync(false, model.revisions, 2);
        model.sync(true, model.revisions, 2);
        assert_eq!(model.tracks.error(0), Some("offline"));
        assert!(model.prepare().is_none());
        model.retry();
        let request = model.prepare().unwrap();
        finish(&mut model, request, 1);
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
    fn album_track_search_replaces_pages_and_rejects_previous_completion() {
        let mut projection = QueryProjection::default();
        let mut pages = paged::PagedRows::default();
        let revisions = QueryRevisions::default();
        let mut query = queries().into_iter().nth(1).unwrap();
        sync_pages(&mut projection, &mut pages, &query, revisions);
        pages.begin(0, 2);
        pages.accept(Page {
            total: 2,
            rows: vec![41, 42],
        });
        let previous = query.clone();
        let generation = projection.generation();
        let Query::AlbumTracks { query: search, .. } = &mut query else {
            unreachable!()
        };
        *search = Some("needle".into());
        assert!(sync_pages(&mut projection, &mut pages, &query, revisions));
        assert!(pages.get(0).is_none());
        assert!(!projection.accepts(&previous, generation, revisions));
        pages.begin(0, 2);
        pages.accept(Page {
            total: 1,
            rows: vec![42],
        });
        assert_eq!(pages.get(0), Some(&42));
        let Query::AlbumTracks { query: search, .. } = &mut query else {
            unreachable!()
        };
        *search = None;
        assert!(sync_pages(&mut projection, &mut pages, &query, revisions));
        assert!(pages.get(0).is_none());
    }

    #[test]
    fn hiding_retains_pages_and_failures_but_cancels_pending_submissions() {
        for query in queries() {
            let mut projection = QueryProjection::default();
            let mut pages = paged::PagedRows::default();
            let revisions = QueryRevisions::default();
            assert!(sync_pages(&mut projection, &mut pages, &query, revisions));
            pages.begin(0, 2);
            pages.accept(Page {
                total: 6,
                rows: vec![10, 11],
            });
            pages.begin(2, 2);
            pages.fail("Unavailable".into());
            pages.begin(4, 2);
            let generation = projection.generation();
            cancel_pages(&mut projection, &mut pages);
            assert_eq!(pages.len(), 6);
            assert_eq!(pages.total(), Some(6));
            assert_eq!(pages.get(0), Some(&10));
            assert_eq!(pages.error(2), Some("Unavailable"));
            assert!(!projection.accepts(&query, generation, revisions));
            assert!(!sync_pages(&mut projection, &mut pages, &query, revisions));
            assert_eq!(pages.get(1), Some(&11));
            assert_eq!(pages.demand(4..6, 2), Some(4));
            assert_eq!(pages.demand(2..4, 2), Some(4));
            pages.retry();
            assert_eq!(pages.demand(2..4, 2), Some(2));
        }
    }

    #[test]
    fn data_refresh_preserves_viewport_until_authoritative_total_arrives() {
        for query in queries() {
            let mut projection = QueryProjection::default();
            let mut pages = paged::PagedRows::default();
            let revisions = QueryRevisions::default();
            sync_pages(&mut projection, &mut pages, &query, revisions);
            pages.begin(80, 2);
            pages.accept(Page {
                total: 100,
                rows: vec![80, 81],
            });
            pages.demand(80..84, 2);
            let generation = projection.generation();
            let refreshed = QueryRevisions {
                album: 1,
                ..revisions
            };
            assert!(sync_pages(&mut projection, &mut pages, &query, refreshed));
            assert_eq!(pages.len(), 100);
            assert_eq!(pages.total(), None);
            assert_eq!(pages.visible(), 80..84);
            assert!(pages.get(80).is_none());
            assert!(!projection.accepts(&query, generation, revisions));
            assert_eq!(pages.demand(pages.visible(), 2), Some(80));
            pages.begin(80, 2);
            pages.accept(Page {
                total: 4,
                rows: vec![],
            });
            assert_eq!(pages.len(), 4);
            assert_eq!(pages.demand(pages.visible(), 2), Some(2));
            pages.begin(2, 2);
            pages.accept(Page {
                total: 4,
                rows: vec![2, 3],
            });
            assert_eq!(pages.get(3), Some(&3));
        }
    }

    #[test]
    fn hidden_data_changes_discard_pages_and_bootstrap_previous_empty_results() {
        for query in queries() {
            let revisions = QueryRevisions {
                album: 1,
                ..Default::default()
            };
            for total in [0, 2] {
                let mut projection = QueryProjection::default();
                let mut pages = paged::PagedRows::default();
                sync_pages(
                    &mut projection,
                    &mut pages,
                    &query,
                    QueryRevisions::default(),
                );
                pages.begin(0, 2);
                pages.accept(Page {
                    total,
                    rows: if total == 0 { vec![] } else { vec![1, 2] },
                });
                cancel_pages(&mut projection, &mut pages);
                assert!(sync_pages(&mut projection, &mut pages, &query, revisions));
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
        for mut query in queries() {
            match &mut query {
                Query::Albums { query, .. } | Query::AlbumTracks { query, .. } => {
                    *query = Some("needle".into());
                }
                _ => unreachable!(),
            }
            let mut projection = QueryProjection::default();
            let mut pages = paged::PagedRows::<usize>::default();
            let revisions = QueryRevisions::default();
            sync_pages(&mut projection, &mut pages, &query, revisions);
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
            assert!(!sync_pages(&mut projection, &mut pages, &query, revisions));
            assert!(!projection.accepts(&query, generation, revisions));
            assert_eq!(pages.total(), Some(0));
            assert!(pages.demand(pages.visible(), 4).is_none());

            let refreshed = QueryRevisions {
                album: 1,
                ..revisions
            };
            assert!(sync_pages(&mut projection, &mut pages, &query, refreshed));
            assert_eq!(pages.total(), None);
            assert_eq!(pages.demand(pages.visible(), 4), Some(0));
        }
    }

    #[test]
    fn paging_and_unrelated_revisions_do_not_change_album_data_keys() {
        let mut projection = QueryProjection::default();
        let mut pages = paged::PagedRows::default();
        let query = queries().into_iter().next().unwrap();
        sync_pages(
            &mut projection,
            &mut pages,
            &query,
            QueryRevisions::default(),
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
        assert!(!sync_pages(&mut projection, &mut pages, &paging, revisions));
        assert_eq!(projection.generation(), generation);
        assert_eq!(pages.get(0), Some(&1));
        assert!(projection.accepts(&paging, generation, revisions));
        let changed = Query::Albums {
            query: Some("new search".into()),
            sort: AlbumSort::default(),
            offset: 0,
            limit: 2,
        };
        assert!(sync_pages(&mut projection, &mut pages, &changed, revisions));
        assert!(pages.get(0).is_none());
        assert_eq!(pages.demand(pages.visible(), 2), Some(0));
    }
}
