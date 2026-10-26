use super::*;
use crate::store::CountDomain;

struct ReadState {
    config: Arc<Config>,
    revisions: QueryRevisions,
    playlist_entry_revisions: Arc<std::collections::BTreeMap<i64, u64>>,
}

impl ReadState {
    fn capture(state: &CoreState, query: &Query) -> Self {
        Self {
            config: Arc::clone(&state.system.config),
            revisions: state.query_revisions_for(query),
            playlist_entry_revisions: Arc::clone(&state.library.playlist_entry_revisions),
        }
    }
}

struct ReadView<'a> {
    store: &'a Store,
    state: &'a ReadState,
    maintenance: bool,
}
impl ReadView<'_> {
    fn database_maintenance_active(&self) -> bool {
        self.maintenance
    }
    fn view_in_snapshot(
        &self,
        query: &Query,
        current: QueryRevisions,
    ) -> Result<ViewResponse, QueryError> {
        if self.database_maintenance_active()
            && !matches!(
                query,
                Query::ScanTask { .. }
                    | Query::RelocationTask { .. }
                    | Query::ProcessingTask { .. }
            )
        {
            return Err(QueryError::Message(
                "Database maintenance is in progress; retry this read after it finishes".into(),
            ));
        }
        let current_stamp = query.stamp(current);
        let expected = match query {
            Query::TrackBatch { expected, .. }
            | Query::PlaylistSummaryBatch { expected, .. }
            | Query::PlaylistEntryBatch { expected, .. }
            | Query::QueueBatch { expected, .. } => *expected,
            _ => None,
        };
        if let Some(mut expected) = expected {
            if let Query::PlaylistEntryBatch { after, .. } = query
                && expected.playlist_entries.is_none()
                && after.is_none()
            {
                // A non-scoped baseline can begin, never continue, a traversal.
                if expected.library != current.library {
                    return Err(QueryError::RevisionChanged);
                }
                expected.playlist_metadata = current.playlist_metadata;
                expected.playlist_entries = current.playlist_entries;
            }
            if query.stamp(expected) != query.stamp(current) {
                return Err(QueryError::RevisionChanged);
            }
        }
        Ok(match query {
            Query::Albums {
                query,
                sort,
                offset,
                limit,
            } => ViewResponse::Albums(self.store.albums_page(
                query.as_deref(),
                *sort,
                *offset,
                *limit,
            )?),
            Query::AlbumTracks {
                key,
                query,
                offset,
                limit,
            } => ViewResponse::AlbumTracks(self.store.album_tracks_page(
                key,
                query.as_deref(),
                *offset,
                *limit,
            )?),
            Query::DuplicateGroups { offset, limit } => {
                ViewResponse::DuplicateGroups(self.store.duplicate_groups_page(*offset, *limit)?)
            }
            Query::DuplicateMembers {
                fingerprint,
                offset,
                limit,
            } => ViewResponse::DuplicateMembers(self.store.duplicate_members_page(
                fingerprint,
                *offset,
                *limit,
            )?),
            Query::DuplicateMemberSelection {
                fingerprint,
                track_ids,
            } => ViewResponse::DuplicateMembers(
                self.store
                    .duplicate_member_selection(fingerprint, track_ids)?,
            ),
            Query::ProcessingTask { .. } | Query::RelocationTask { .. } => {
                unreachable!("task status stays on Core")
            }
            Query::QueuePage {
                query,
                offset,
                limit,
            } => {
                ViewResponse::QueuePage(self.store.queue_page(query.as_deref(), *offset, *limit)?)
            }
            Query::QueueBatch {
                query,
                after,
                limit,
                ..
            } => ViewResponse::QueueBatch(self.store.queue_batch(
                query.as_deref(),
                *after,
                *limit,
                current_stamp,
            )?),
            Query::QueueEntries { query, queue_ids } => {
                ViewResponse::QueueEntries(self.store.queue_entries(query.as_deref(), queue_ids)?)
            }
            Query::ScanTask { .. } => unreachable!("task status stays on Core"),
            Query::OtherTracksPage {
                query,
                favorite,
                missing,
                sort,
                offset,
                limit,
            } => ViewResponse::LibraryPage(self.store.other_tracks_page(
                &self.state.config.library_roots,
                query.as_deref(),
                *favorite,
                *missing,
                *sort,
                *offset,
                *limit,
            )?),
            Query::OtherTracksCount => ViewResponse::OtherTracksCount(
                self.store
                    .other_tracks_count(&self.state.config.library_roots)?,
            ),
            Query::OtherTracksLocate {
                track_id,
                query,
                favorite,
                missing,
                sort,
            } => ViewResponse::LibraryLocate(self.store.other_tracks_location(
                &self.state.config.library_roots,
                *track_id,
                query.as_deref(),
                *favorite,
                *missing,
                *sort,
            )?),
            Query::LibraryPage {
                query,
                favorite,
                missing,
                sort,
                offset,
                limit,
            } => ViewResponse::LibraryPage(self.store.library_view_page(
                query.as_deref(),
                *favorite,
                *missing,
                *sort,
                *offset,
                *limit,
            )?),
            Query::LibraryRootPage {
                path,
                query,
                favorite,
                missing,
                sort,
                offset,
                limit,
            } => ViewResponse::LibraryPage(self.store.library_root_page(
                path,
                query.as_deref(),
                *favorite,
                *missing,
                *sort,
                *offset,
                *limit,
            )?),
            Query::LibraryRootCounts { roots } => {
                ViewResponse::LibraryRootCounts(self.store.library_root_counts(roots)?)
            }
            Query::LibraryLocate {
                track_id,
                path,
                tree,
                query,
                favorite,
                missing,
                sort,
            } => ViewResponse::LibraryLocate(self.store.library_location(
                *track_id,
                path,
                *tree,
                query.as_deref(),
                *favorite,
                *missing,
                *sort,
            )?),
            Query::TrackPage {
                query,
                favorite,
                missing,
                offset,
                limit,
            } => ViewResponse::TrackPage(self.store.library_page(
                query.as_deref(),
                *favorite,
                *missing,
                *offset,
                *limit,
            )?),
            Query::DirectoryPage {
                path,
                query,
                favorite,
                missing,
                offset,
                limit,
            } => ViewResponse::DirectoryPage(self.store.directory_page(
                path,
                query.as_deref(),
                *favorite,
                *missing,
                *offset,
                *limit,
            )?),
            Query::PlaylistSummaries {
                query,
                offset,
                limit,
            } => ViewResponse::PlaylistSummaries(self.store.playlist_summary_page(
                query.as_deref(),
                *offset,
                *limit,
            )?),
            Query::PlaylistEntries {
                playlist_id,
                query,
                offset,
                limit,
            } => ViewResponse::PlaylistEntries(self.store.playlist_entries_page(
                *playlist_id,
                query.as_deref(),
                *offset,
                *limit,
            )?),
            Query::Track { track_id } => ViewResponse::Track(self.store.track(*track_id)?),
            Query::PlaylistEntry { entry_id } => {
                ViewResponse::PlaylistEntry(self.store.playlist_entry(*entry_id)?)
            }
            Query::PlaylistEntryLocations {
                playlist_id,
                query,
                entry_ids,
            } => ViewResponse::PlaylistEntryLocations(self.store.playlist_entry_locations(
                *playlist_id,
                query.as_deref(),
                entry_ids,
            )?),
            Query::LibraryStats => ViewResponse::LibraryStats(self.store.library_stats()?),
            Query::TrackBatch {
                query,
                favorite,
                missing,
                after,
                limit,
                ..
            } => ViewResponse::TrackBatch(self.store.track_batch(
                query.as_deref(),
                *favorite,
                *missing,
                *after,
                *limit,
            )?),
            Query::PlaylistSummaryBatch {
                query,
                after,
                limit,
                ..
            } => ViewResponse::PlaylistSummaryBatch(self.store.playlist_summary_batch(
                query.as_deref(),
                *after,
                *limit,
            )?),
            Query::PlaylistEntryBatch {
                playlist_id,
                query,
                after,
                limit,
                ..
            } => ViewResponse::PlaylistEntryBatch(self.store.playlist_entry_batch(
                *playlist_id,
                query.as_deref(),
                *after,
                *limit,
                current_stamp,
            )?),
        })
    }
}

pub(super) fn read_snapshot(
    store: &Store,
    state: &CoreState,
    query: &Query,
    maintenance: bool,
) -> QueryResponse {
    ReadState::capture(state, query).read(store, query, maintenance)
}

impl ReadState {
    fn read(&self, store: &Store, query: &Query, maintenance: bool) -> QueryResponse {
        let mut revisions = self.revisions;
        let result = ReadView {
            store,
            state: self,
            maintenance,
        }
        .view_in_snapshot(query, revisions);
        if let Ok(ViewResponse::PlaylistEntry(Some(entry))) = &result {
            revisions.playlist_entries =
                self.playlist_entry_revisions
                    .get(&entry.playlist_id)
                    .map(|&revision| rivu_protocol::response::PlaylistEntryRevision {
                        playlist_id: entry.playlist_id,
                        revision,
                    });
        }
        QueryResponse { revisions, result }
    }
}

#[derive(Default)]
struct GateState {
    owner_busy: bool,
    maintenance: bool,
    active: bool,
}

#[derive(Default)]
pub(super) struct ReadFence {
    state: Mutex<GateState>,
    owner_ready: parking_lot::Condvar,
    retired: Event,
}

impl ReadFence {
    pub(super) fn transition(self: &Arc<Self>) -> OwnerTransition {
        self.state.lock().owner_busy = true;
        OwnerTransition {
            fence: Arc::clone(self),
            finished: false,
        }
    }

    pub(super) async fn idle(&self) {
        loop {
            let retired = self.retired.listen();
            if !self.state.lock().active {
                return;
            }
            retired.await;
        }
    }
}

pub(super) struct OwnerTransition {
    fence: Arc<ReadFence>,
    finished: bool,
}
impl OwnerTransition {
    pub(super) fn finish(mut self, maintenance: bool) {
        let mut state = self.fence.state.lock();
        state.maintenance = maintenance;
        state.owner_busy = false;
        self.finished = true;
        self.fence.owner_ready.notify_all();
    }
}
impl Drop for OwnerTransition {
    fn drop(&mut self) {
        if !self.finished {
            let mut state = self.fence.state.lock();
            state.maintenance = true;
            state.owner_busy = false;
            self.fence.owner_ready.notify_all();
        }
    }
}

struct ReadLease(Arc<ReadFence>);
impl Drop for ReadLease {
    fn drop(&mut self) {
        self.0.state.lock().active = false;
        self.0.retired.notify(usize::MAX);
    }
}

struct ReadRequest {
    query: Query,
    ticket: Option<QueryTicket>,
    state: ReadState,
    reply: oneshot::Sender<CoreResponse>,
}

impl ReadRequest {
    fn is_cancelled(&self) -> bool {
        self.reply.is_canceled()
            || self
                .ticket
                .as_ref()
                .is_some_and(|ticket| !ticket.is_current())
    }
}

struct ReadConnection {
    store: Store,
    library_revision: u64,
    duplicate_groups_revision: u64,
    // Structure, album, queue; library already owns the filtered count above.
    scalar_revisions: (u64, u64, u64),
    scalar_playlist_revisions: Arc<std::collections::BTreeMap<i64, u64>>,
}

/// One independent SQLite owner, with bounded admission and one live snapshot.
/// It never occupies a long-job blocking permit or the control owner.
pub(super) struct ReadQueries {
    requests: mpsc::Sender<ReadRequest>,
    worker: Option<JoinHandle<()>>,
    pub(super) fence: Arc<ReadFence>,
}

impl ReadQueries {
    pub(super) fn start(path: PathBuf, shared: Arc<RwLock<CoreState>>) -> Result<Self> {
        let (requests, receiver) = mpsc::bounded::<ReadRequest>(64);
        let fence = Arc::new(ReadFence::default());
        let worker_fence = Arc::clone(&fence);
        let worker = thread::Builder::new()
            .name("rivu-queries".into())
            .spawn(move || {
                let mut reader = None;
                while let Ok(request) = receiver.recv_blocking() {
                    let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        Self::execute(&path, &mut reader, &shared, &worker_fence, &request)
                    }))
                    .unwrap_or_else(|_| QueryResponse {
                        revisions: request.state.revisions,
                        result: Err(QueryError::Message("Read worker panicked".into())),
                    });
                    let _ = request.reply.send(CoreResponse::Query(response));
                }
            })?;
        Ok(Self {
            requests,
            worker: Some(worker),
            fence,
        })
    }

    fn execute(
        path: &Path,
        reader: &mut Option<ReadConnection>,
        shared: &RwLock<CoreState>,
        fence: &Arc<ReadFence>,
        request: &ReadRequest,
    ) -> QueryResponse {
        let revisions = request.state.revisions;
        let failed = |error| QueryResponse {
            revisions,
            result: Err(error),
        };
        if request.is_cancelled() {
            return failed(QueryError::Cancelled);
        }
        {
            let mut gate = fence.state.lock();
            while gate.owner_busy {
                fence.owner_ready.wait(&mut gate);
            }
            if request.is_cancelled() {
                return failed(QueryError::Cancelled);
            }
            if gate.maintenance {
                return failed(QueryError::Message(
                    "Database maintenance is in progress; retry this read after it finishes".into(),
                ));
            }
            gate.active = true;
        }
        // A failed begin/rollback discards the connection before this physical
        // lease can retire. Successful requests reuse the reader/statements.
        let _lease = ReadLease(Arc::clone(fence));
        if reader.is_none() {
            match Store::open_reader(path) {
                Ok(store) => {
                    *reader = Some(ReadConnection {
                        store,
                        library_revision: request.state.revisions.library,
                        duplicate_groups_revision: request.state.revisions.duplicate_groups,
                        scalar_revisions: (
                            request.state.revisions.structure,
                            request.state.revisions.album,
                            request.state.revisions.queue,
                        ),
                        scalar_playlist_revisions: Arc::clone(
                            &request.state.playlist_entry_revisions,
                        ),
                    })
                }
                Err(error) => {
                    return failed(QueryError::Message(format!(
                        "Opening read connection: {error:#}"
                    )));
                }
            }
        }
        let connection = reader.as_mut().expect("reader opened");
        let result = Self::read_pinned(connection, shared, fence, request);
        match result {
            Ok(response) => response,
            Err(error) => {
                drop(reader.take());
                failed(QueryError::Message(format!(
                    "Read snapshot failed: {error:#}"
                )))
            }
        }
    }

    fn read_pinned(
        connection: &mut ReadConnection,
        shared: &RwLock<CoreState>,
        fence: &Arc<ReadFence>,
        request: &ReadRequest,
    ) -> Result<QueryResponse> {
        let snapshot = connection.store.read_snapshot()?;
        let revisions = request.state.revisions;
        let failed = |error| QueryResponse {
            revisions,
            result: Err(error),
        };
        let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            {
                let mut gate = fence.state.lock();
                while gate.owner_busy {
                    fence.owner_ready.wait(&mut gate);
                }
                if request.is_cancelled() {
                    return failed(QueryError::Cancelled);
                }
                // SQL is pinned before this version check. No I/O holds the gate,
                // and the owner cannot begin a commit/publication transition here.
                let published = shared.read();
                // An entry-ID lookup has no owning-playlist stamp until resolved.
                // Copy-on-write map identity covers movement and deletion in O(1).
                let entry_map_changed = matches!(&request.query, Query::PlaylistEntry { .. })
                    && !Arc::ptr_eq(
                        &published.library.playlist_entry_revisions,
                        &request.state.playlist_entry_revisions,
                    );
                if gate.maintenance
                    || entry_map_changed
                    || request
                        .query
                        .stamp(published.query_revisions_for(&request.query))
                        != request.query.stamp(revisions)
                {
                    return failed(QueryError::RevisionChanged);
                }
            }
            // Only accepted snapshots can change cache ownership. A rejected
            // version cannot tag old cached totals as belonging to a newer one.
            if connection.library_revision != revisions.library {
                snapshot.invalidate_filtered_track_count();
                snapshot.invalidate_scalar_count_domains(&[
                    CountDomain::Other,
                    CountDomain::Directory,
                    CountDomain::Queue,
                    CountDomain::PlaylistEntries,
                ]);
                connection.library_revision = revisions.library;
            }
            if connection.duplicate_groups_revision != revisions.duplicate_groups {
                snapshot.invalidate_duplicate_group_count();
                connection.duplicate_groups_revision = revisions.duplicate_groups;
            }
            let scalar_revisions = (revisions.structure, revisions.album, revisions.queue);
            if connection.scalar_revisions.0 != scalar_revisions.0 {
                snapshot
                    .invalidate_scalar_count_domains(&[CountDomain::Other, CountDomain::Directory]);
            }
            if connection.scalar_revisions.1 != scalar_revisions.1 {
                snapshot.invalidate_scalar_count_domains(&[
                    CountDomain::Albums,
                    CountDomain::AlbumTracks,
                ]);
            }
            if connection.scalar_revisions.2 != scalar_revisions.2 {
                snapshot.invalidate_scalar_count_domains(&[CountDomain::Queue]);
            }
            connection.scalar_revisions = scalar_revisions;
            if !Arc::ptr_eq(
                &connection.scalar_playlist_revisions,
                &request.state.playlist_entry_revisions,
            ) {
                snapshot.invalidate_scalar_count_domains(&[CountDomain::PlaylistEntries]);
                connection.scalar_playlist_revisions =
                    Arc::clone(&request.state.playlist_entry_revisions);
            }
            // Later writes cannot change this accepted WAL snapshot/version.
            request.state.read(&snapshot, &request.query, false)
        }))
        .unwrap_or_else(|_| failed(QueryError::Message("Read worker panicked".into())));
        // Logical cancellation/error/panic still physically retires the snapshot.
        snapshot.finish()?;
        Ok(response)
    }

    pub(super) fn submit(
        &self,
        query: Query,
        ticket: Option<QueryTicket>,
        state: &CoreState,
        reply: oneshot::Sender<CoreResponse>,
    ) {
        let request = ReadRequest {
            state: ReadState::capture(state, &query),
            query,
            ticket,
            reply,
        };
        if let Err(error) = self.requests.try_send(request) {
            let request = error.into_inner();
            let response = QueryResponse {
                revisions: request.state.revisions,
                result: Err(QueryError::Message("Read queue is full or stopped".into())),
            };
            let _ = request.reply.send(CoreResponse::Query(response));
        }
    }
}

impl Drop for ReadQueries {
    fn drop(&mut self) {
        self.requests.close();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(receiver: oneshot::Receiver<CoreResponse>) -> QueryResponse {
        let response = futures::executor::block_on(rivu_protocol::transport::timeout(
            Duration::from_secs(5),
            receiver,
        ))
        .unwrap()
        .unwrap();
        let CoreResponse::Query(response) = response else {
            panic!("expected query response")
        };
        response
    }

    fn execute_read(
        core: &Core,
        path: &Path,
        reader: &mut Option<ReadConnection>,
        fence: &Arc<ReadFence>,
        query: Query,
    ) -> QueryResponse {
        let (reply, _receiver) = oneshot::channel();
        let request = ReadRequest {
            state: ReadState::capture(&core.shared.read(), &query),
            query,
            ticket: None,
            reply,
        };
        ReadQueries::execute(path, reader, &core.shared, fence, &request)
    }

    #[test]
    fn bitrate_commits_refresh_payload_stamps_without_changing_query_shape() {
        let (directory, mut core) = super::super::tests::fixture_with_database(Some(10.0));
        core.publish();
        let path = directory.path().join("library.db");
        let query = Query::Track { track_id: 1 };
        let before = core.shared.read().clone();
        let store = Store::open_reader(&path).unwrap();
        let mut reader = Some(ReadConnection {
            store,
            library_revision: before.library.revision,
            duplicate_groups_revision: before.library.duplicate_groups_revision,
            scalar_revisions: (
                before.library.structure_revision,
                before.library.album_revision,
                before.queue.revision,
            ),
            scalar_playlist_revisions: Arc::clone(&before.library.playlist_entry_revisions),
        });
        let fence = Arc::new(ReadFence::default());
        let old_response = execute_read(&core, &path, &mut reader, &fence, query.clone());
        assert!(
            matches!(&old_response.result, Ok(ViewResponse::Track(Some(track))) if track.bitrate_bps.is_none())
        );
        let source = core
            .store
            .playback_source(&core.store.track(1).unwrap().unwrap())
            .unwrap()
            .unwrap();
        for (bps, exact) in [(96_000, false), (192_000, true)] {
            assert!(core.store.upgrade_bitrate(&source, bps, exact).unwrap());
            core.state.library.bitrate_revision =
                core.state.library.bitrate_revision.wrapping_add(1);
            core.publish();
        }
        let (reply, _receiver) = oneshot::channel();
        let stale = ReadRequest {
            state: ReadState::capture(&before, &query),
            query: query.clone(),
            ticket: None,
            reply,
        };
        assert!(matches!(
            ReadQueries::execute(&path, &mut reader, &core.shared, &fence, &stale).result,
            Err(QueryError::RevisionChanged)
        ));
        let statistics = Query::LibraryStats;
        let (reply, _receiver) = oneshot::channel();
        let scalar = ReadRequest {
            state: ReadState::capture(&before, &statistics),
            query: statistics,
            ticket: None,
            reply,
        };
        assert!(matches!(
            ReadQueries::execute(&path, &mut reader, &core.shared, &fence, &scalar).result,
            Ok(ViewResponse::LibraryStats(_))
        ));
        // Background readers retain no Track payloads between accepted snapshots.
        let current = execute_read(&core, &path, &mut reader, &fence, query.clone());
        assert_ne!(
            query.stamp(old_response.revisions),
            query.stamp(current.revisions)
        );
        assert_eq!(
            current.revisions.bitrate,
            old_response.revisions.bitrate.wrapping_add(2)
        );
        assert!(
            matches!(current.result, Ok(ViewResponse::Track(Some(track))) if (track.bitrate_bps, track.bitrate_exact) == (Some(192_000), true))
        );
        let state = core.shared.read();
        assert_eq!(state.library.revision, before.library.revision);
        assert_eq!(
            state.library.structure_revision,
            before.library.structure_revision
        );
        assert_eq!(state.library.album_revision, before.library.album_revision);
        assert_eq!(state.queue, before.queue);
        assert_ne!(
            query.stamp(old_response.revisions),
            query.stamp(state.query_revisions_for(&query))
        );
    }

    fn favorite_query() -> Query {
        Query::LibraryPage {
            query: None,
            favorite: Some(true),
            missing: None,
            sort: Default::default(),
            offset: 0,
            limit: 0,
        }
    }

    fn assert_counts(
        core: &Core,
        path: &Path,
        reader: &mut Option<ReadConnection>,
        fence: &Arc<ReadFence>,
        favorites: usize,
        duplicates: usize,
    ) {
        assert!(matches!(
            execute_read(core, path, reader, fence, favorite_query()).result,
            Ok(ViewResponse::LibraryPage(page)) if page.total == favorites
        ));
        assert!(matches!(
            execute_read(core, path, reader, fence, Query::DuplicateGroups { offset: 0, limit: 0 }).result,
            Ok(ViewResponse::DuplicateGroups(page)) if page.total == duplicates
        ));
    }

    fn set_fingerprints(core: &mut Core, track_ids: &[i64], fingerprint: Option<&str>) {
        let records = track_ids
            .iter()
            .map(|&id| {
                let track = core.track(id).unwrap();
                let known = core.store.known_source(&track.path).unwrap().unwrap();
                library::ScanRecord {
                    path: track.path,
                    cue: known.cue,
                    size: known.size,
                    modified_ns: known.modified_ns,
                    fingerprint: fingerprint.map(str::to_owned),
                    media: known.media.unwrap(),
                }
            })
            .collect();
        let impacts = core
            .store
            .apply_scan(&ScanResult {
                records,
                ..Default::default()
            })
            .unwrap();
        core.apply_scan_impacts(impacts).unwrap();
        core.publish();
    }

    #[test]
    fn abandoned_queued_read_is_cancelled_before_database_admission() {
        let (directory, core) = super::super::tests::fixture_with_database(Some(10.0));
        let query = Query::Track { track_id: 1 };
        let (reply, receiver) = oneshot::channel();
        let request = ReadRequest {
            state: ReadState::capture(&core.shared.read(), &query),
            query,
            ticket: None,
            reply,
        };
        drop(receiver);
        let mut reader = None;
        let fence = Arc::new(ReadFence::default());
        // Opening this path would fail, so Cancelled proves the read was pruned
        // before opening a connection, not merely before decoding the row.
        let rejected = ReadQueries::execute(
            &directory.path().join("does-not-exist.db"),
            &mut reader,
            &core.shared,
            &fence,
            &request,
        );
        assert!(matches!(rejected.result, Err(QueryError::Cancelled)));
        let accepted = execute_read(
            &core,
            &directory.path().join("library.db"),
            &mut reader,
            &fence,
            Query::Track { track_id: 2 },
        );
        assert!(matches!(accepted.result, Ok(ViewResponse::Track(Some(track))) if track.id == 2));
    }

    #[test]
    fn queued_read_rechecks_receiver_and_ticket_after_owner_transition() {
        for cancel_ticket in [false, true] {
            let (directory, core) = super::super::tests::fixture_with_database(Some(10.0));
            let fence = Arc::new(ReadFence::default());
            let transition = fence.transition();
            let mut epoch = QueryEpoch::default();
            let (reply, receiver) = oneshot::channel();
            let query = Query::Track { track_id: 1 };
            let request = ReadRequest {
                state: ReadState::capture(&core.shared.read(), &query),
                query,
                ticket: cancel_ticket.then(|| epoch.ticket()),
                reply,
            };
            let shared = Arc::clone(&core.shared);
            let worker_fence = Arc::clone(&fence);
            let missing_path = directory.path().join("does-not-exist.db");
            let worker = thread::spawn(move || {
                let mut reader = None;
                let rejected = ReadQueries::execute(
                    &missing_path,
                    &mut reader,
                    &shared,
                    &worker_fence,
                    &request,
                );
                (rejected, reader)
            });
            // Observe the real condvar waiter while retaining the gate, so
            // cancellation necessarily occurs after the first admission check.
            let deadline = Instant::now() + Duration::from_secs(5);
            let gate = loop {
                let gate = fence.state.lock();
                if fence.owner_ready.notify_one() {
                    break gate;
                }
                drop(gate);
                assert!(Instant::now() < deadline, "read did not wait for owner");
                thread::yield_now();
            };
            if cancel_ticket {
                epoch.cancel();
            } else {
                drop(receiver);
            }
            drop(gate);
            transition.finish(false);
            let (rejected, mut reader) = worker.join().unwrap();
            assert!(matches!(rejected.result, Err(QueryError::Cancelled)));
            let accepted = execute_read(
                &core,
                &directory.path().join("library.db"),
                &mut reader,
                &fence,
                Query::Track { track_id: 2 },
            );
            assert!(
                matches!(accepted.result, Ok(ViewResponse::Track(Some(track))) if track.id == 2)
            );
        }
    }

    #[test]
    fn old_snapshot_counts_are_not_reused_after_domain_changes_or_rejected_reads() {
        let (directory, mut core) = super::super::tests::fixture_with_database(Some(10.0));
        let path = directory.path().join("library.db");
        let before = core.shared.read().clone();
        let connection = ReadConnection {
            store: Store::open_reader(&path).unwrap(),
            library_revision: before.library.revision,
            duplicate_groups_revision: before.library.duplicate_groups_revision,
            scalar_revisions: (
                before.library.structure_revision,
                before.library.album_revision,
                before.queue.revision,
            ),
            scalar_playlist_revisions: Arc::clone(&before.library.playlist_entry_revisions),
        };
        let snapshot = connection.store.read_snapshot().unwrap();
        core.command(Command::SetFavorite {
            track_ids: vec![1],
            favorite: true,
        })
        .unwrap();
        let fingerprint = blake3::hash(b"duplicate count domain").to_hex().to_string();
        set_fingerprints(&mut core, &[1, 2], Some(&fingerprint));
        // Fill both slots from the older SQL snapshot after the new revisions
        // are already published. The next accepted read must replace them.
        assert_eq!(
            snapshot
                .library_page(None, Some(true), None, 0, 0)
                .unwrap()
                .total,
            0
        );
        assert_eq!(snapshot.duplicate_groups_page(0, 0).unwrap().total, 0);
        snapshot.finish().unwrap();
        let fence = Arc::new(ReadFence::default());
        let mut reader = Some(connection);
        let query = favorite_query();
        let (reply, _receiver) = oneshot::channel();
        let rejected = ReadRequest {
            state: ReadState::capture(&before, &query),
            query,
            ticket: None,
            reply,
        };
        assert!(matches!(
            ReadQueries::execute(&path, &mut reader, &core.shared, &fence, &rejected).result,
            Err(QueryError::RevisionChanged)
        ));
        assert_counts(&core, &path, &mut reader, &fence, 1, 1);
        core.command(Command::CreatePlaylist {
            name: "Unrelated count domain".into(),
        })
        .unwrap();
        core.publish();
        assert_counts(&core, &path, &mut reader, &fence, 1, 1);
        core.command(Command::SetFavorite {
            track_ids: vec![1],
            favorite: false,
        })
        .unwrap();
        core.publish();
        assert_counts(&core, &path, &mut reader, &fence, 0, 1);
        set_fingerprints(&mut core, &[2], None);
        assert_counts(&core, &path, &mut reader, &fence, 0, 0);
    }

    #[test]
    fn scalar_totals_follow_metadata_membership_and_playlist_lifetime_revisions() {
        let (directory, mut core) = super::super::tests::fixture_with_database(Some(10.0));
        let path = directory.path().join("library.db");
        core.command(Command::CreatePlaylistWithTracks {
            name: "Scalar totals".into(),
            track_ids: vec![1],
        })
        .unwrap();
        core.publish();
        let fence = Arc::new(ReadFence::default());
        let mut reader = None;
        let queries = [
            Query::QueuePage {
                query: Some("scalar needle".into()),
                offset: 0,
                limit: 0,
            },
            Query::PlaylistEntries {
                playlist_id: 1,
                query: Some("scalar needle".into()),
                offset: 0,
                limit: 0,
            },
        ];
        let total = |response: QueryResponse| match response.result.unwrap() {
            ViewResponse::QueuePage(page) => page.total,
            ViewResponse::PlaylistEntries(page) => page.total,
            other => panic!("unexpected response: {other:?}"),
        };
        for query in &queries {
            assert_eq!(
                total(execute_read(
                    &core,
                    &path,
                    &mut reader,
                    &fence,
                    query.clone()
                )),
                0
            );
        }
        let old = core.shared.read().clone();
        core.command(Command::EditTrack {
            track_id: 1,
            title: Some("scalar needle".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .unwrap();
        core.publish();
        // Rejection must not move count ownership to an unaccepted request.
        let (reply, _receiver) = oneshot::channel();
        let stale = ReadRequest {
            state: ReadState::capture(&old, &queries[1]),
            query: queries[1].clone(),
            ticket: None,
            reply,
        };
        assert!(matches!(
            ReadQueries::execute(&path, &mut reader, &core.shared, &fence, &stale).result,
            Err(QueryError::RevisionChanged)
        ));
        for query in &queries {
            assert_eq!(
                total(execute_read(
                    &core,
                    &path,
                    &mut reader,
                    &fence,
                    query.clone()
                )),
                1
            );
        }
        core.command(Command::EditTrack {
            track_id: 2,
            title: Some("scalar needle".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .unwrap();
        core.command(Command::AddPlaylist {
            playlist_id: 1,
            track_ids: vec![2],
        })
        .unwrap();
        core.publish();
        assert_eq!(
            total(execute_read(
                &core,
                &path,
                &mut reader,
                &fence,
                queries[0].clone()
            )),
            3
        );
        assert_eq!(
            total(execute_read(
                &core,
                &path,
                &mut reader,
                &fence,
                queries[1].clone()
            )),
            2
        );
        core.command(Command::DeletePlaylist { playlist_id: 1 })
            .unwrap();
        core.publish();
        assert!(
            execute_read(&core, &path, &mut reader, &fence, queries[1].clone())
                .result
                .is_err()
        );
    }

    #[test]
    fn queued_read_during_commit_publication_gap_rejects_old_revision() {
        let (directory, mut core) = super::super::tests::fixture_with_database(Some(10.0));
        core.publish();
        let before = core.shared.read().clone();
        let reads =
            ReadQueries::start(directory.path().join("library.db"), core.shared.clone()).unwrap();
        let transition = reads.fence.transition();
        core.command(Command::EditTrack {
            track_id: 1,
            title: Some("Committed after capture".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .unwrap();
        assert_eq!(
            core.store.track(1).unwrap().unwrap().title,
            "Committed after capture"
        );
        let (reply, pending) = oneshot::channel();
        reads.submit(Query::Track { track_id: 1 }, None, &before, reply);
        let mut pending = pending;
        assert!(matches!(pending.try_recv(), Ok(None)));
        core.publish();
        transition.finish(false);
        let rejected = response(pending);
        assert_eq!(
            rejected.revisions,
            before.query_revisions_for(&Query::Track { track_id: 1 })
        );
        assert!(matches!(rejected.result, Err(QueryError::RevisionChanged)));

        let current = core.shared.read().clone();
        let (reply, pending) = oneshot::channel();
        reads.submit(Query::Track { track_id: 1 }, None, &current, reply);
        let accepted = response(pending);
        assert_eq!(
            accepted.revisions,
            current.query_revisions_for(&Query::Track { track_id: 1 })
        );
        assert!(
            matches!(accepted.result, Ok(ViewResponse::Track(Some(track))) if track.title == "Committed after capture")
        );
    }

    #[test]
    fn pinned_sql_snapshot_remains_consistent_across_committed_write() {
        let (directory, core) = super::super::tests::fixture_with_database(Some(10.0));
        let path = directory.path().join("library.db");
        let snapshot = Store::open_read_snapshot(&path).unwrap();
        let state = core.shared.read().clone();
        let query = Query::Track { track_id: 1 };
        let before = read_snapshot(&snapshot, &state, &query, false);
        assert!(
            matches!(&before.result, Ok(ViewResponse::Track(Some(track))) if track.title == "Track 1")
        );
        core.store
            .edit_track(1, Some("New committed title"), None, None, None)
            .unwrap();
        assert_eq!(
            core.store.track(1).unwrap().unwrap().title,
            "New committed title"
        );
        let pinned = read_snapshot(&snapshot, &state, &query, false);
        assert_eq!(pinned.revisions, before.revisions);
        assert!(
            matches!(pinned.result, Ok(ViewResponse::Track(Some(track))) if track.title == "Track 1")
        );
        drop(snapshot);
        let fresh = Store::open_read_snapshot(&path).unwrap();
        assert!(
            matches!(read_snapshot(&fresh, &state, &query, false).result,
            Ok(ViewResponse::Track(Some(track))) if track.title == "New committed title")
        );
    }

    #[test]
    fn maintenance_waits_for_physical_snapshot_retirement() {
        let (directory, core) = super::super::tests::fixture_with_database(Some(10.0));
        let path = directory.path().join("library.db");
        let reader = Store::open_reader(&path).unwrap();
        let fence = Arc::new(ReadFence::default());
        fence.state.lock().active = true;
        let lease = ReadLease(fence.clone());
        let snapshot = reader.read_snapshot().unwrap();
        assert!(snapshot.track(1).unwrap().is_some());
        let transition = fence.transition();
        let mut idle = std::pin::pin!(fence.idle());
        let waker = futures::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        assert!(idle.as_mut().poll(&mut context).is_pending());
        // Logical maintenance admission cannot release a live SQLite snapshot.
        assert!(snapshot.track(1).unwrap().is_some());
        snapshot.finish().unwrap();
        assert!(idle.as_mut().poll(&mut context).is_pending());
        drop(lease);
        assert!(idle.as_mut().poll(&mut context).is_ready());
        transition.finish(true);
        let (reply, _receiver) = oneshot::channel();
        let request = ReadRequest {
            query: Query::Track { track_id: 1 },
            ticket: None,
            state: ReadState::capture(&core.shared.read(), &Query::Track { track_id: 1 }),
            reply,
        };
        let mut reader = Some(ReadConnection {
            store: reader,
            library_revision: core.state.library.revision,
            duplicate_groups_revision: core.state.library.duplicate_groups_revision,
            scalar_revisions: (
                core.state.library.structure_revision,
                core.state.library.album_revision,
                core.state.queue.revision,
            ),
            scalar_playlist_revisions: Arc::clone(&core.state.library.playlist_entry_revisions),
        });
        let rejected = ReadQueries::execute(&path, &mut reader, &core.shared, &fence, &request);
        assert!(
            matches!(rejected.result, Err(QueryError::Message(error)) if error.contains("maintenance"))
        );
    }

    #[test]
    fn unscoped_entry_lookup_rejects_captured_revision_after_move_or_removal() {
        let (directory, mut core) = super::super::tests::fixture_with_database(Some(10.0));
        core.command(Command::CreatePlaylistWithTracks {
            name: "Entries".into(),
            track_ids: vec![1, 2, 3],
        })
        .unwrap();
        core.publish();
        let entry_id = core
            .store
            .playlist_entries_page(1, None, 0, 1)
            .unwrap()
            .rows[0]
            .id;
        let query = Query::PlaylistEntry { entry_id };
        let reads =
            ReadQueries::start(directory.path().join("library.db"), core.shared.clone()).unwrap();

        for command in [
            Command::MovePlaylistEntry { entry_id, index: 2 },
            Command::RemovePlaylistEntry { entry_id },
        ] {
            let captured = core.shared.read().clone();
            assert!(
                captured
                    .query_revisions_for(&query)
                    .playlist_entries
                    .is_none()
            );
            let transition = reads.fence.transition();
            let (reply, pending) = oneshot::channel();
            reads.submit(query.clone(), None, &captured, reply);
            core.command(command).unwrap();
            core.publish();
            transition.finish(false);
            let rejected = response(pending);
            assert_eq!(rejected.revisions, captured.query_revisions_for(&query));
            assert!(matches!(rejected.result, Err(QueryError::RevisionChanged)));
        }

        let current = core.shared.read().clone();
        let (reply, pending) = oneshot::channel();
        reads.submit(query.clone(), None, &current, reply);
        let missing = response(pending);
        assert_eq!(missing.revisions, current.query_revisions_for(&query));
        assert!(matches!(
            missing.result,
            Ok(ViewResponse::PlaylistEntry(None))
        ));
    }
}
