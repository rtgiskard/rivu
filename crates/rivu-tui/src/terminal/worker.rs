use crate::projection::TuiSnapshot;
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};
use parking_lot::Mutex;
use rivu_protocol::{
    AppCommand,
    model::{ClientSnapshot, LibrarySort, Query, QueryIdentity, QueryStamp},
    query_tracking::QuerySubmission,
    response::{Ack, ObservedState, QueryError, StateSections, StateWatch, ViewResponse},
};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    thread::JoinHandle,
};

type WatchedSnapshot = Result<(u32, ClientSnapshot), String>;

const TERMINAL_SECTIONS: StateSections = StateSections {
    processing: false,
    ..StateSections::ALL
};

pub(super) struct Watcher {
    pub(super) receiver: Receiver<()>,
    pub(super) latest: Arc<Mutex<Option<WatchedSnapshot>>>,
    stopping: rivu_client::SessionCancellation,
    worker: Option<JoinHandle<()>>,
}

impl Watcher {
    pub(super) fn take_update(&self) -> Result<Option<(u32, ClientSnapshot)>> {
        // The slot coalesces state, while channel closure is a terminal event:
        // a dead watcher must never leave its owner polling an unchanged screen.
        let disconnected = matches!(self.receiver.try_recv(), Err(TryRecvError::Disconnected));
        let update = self
            .latest
            .lock()
            .take()
            .transpose()
            .map_err(anyhow::Error::msg)?;
        if disconnected
            && !update
                .as_ref()
                .is_some_and(|(_, state)| state.system.shutting_down)
        {
            anyhow::bail!("TUI state watcher disconnected without a shutdown snapshot");
        }
        Ok(update)
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stopping.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(super) fn spawn_watcher(data_dir: &Path) -> Watcher {
    let data_dir = data_dir.to_owned();
    let (sender, receiver) = bounded(1);
    let latest = Arc::new(Mutex::new(None));
    let latest_worker = Arc::clone(&latest);
    let stopping = rivu_client::SessionCancellation::default();
    let stop = stopping.clone();
    let worker = std::thread::spawn(move || {
        let mut session = rivu_client::client_session_with_cancel(&data_dir, stop);
        let initial = session.get_state(TERMINAL_SECTIONS).and_then(|response| {
            let instance_id = session
                .instance_id()
                .context("Missing IPC Hello identity")?;
            session.bind_instance(instance_id)?;
            Ok((instance_id, response))
        });
        let mut state = match initial {
            Ok((instance_id, response)) => {
                let mut state = ObservedState::new(instance_id);
                state.apply(instance_id, response);
                state
            }
            Err(error) => {
                *latest_worker.lock() = Some(Err(format!("TUI state handshake failed: {error:#}")));
                let _ = sender.try_send(());
                return;
            }
        };
        *latest_worker.lock() = Some(Ok((state.instance_id(), state.snapshot().clone())));
        let _ = sender.try_send(());
        if state.snapshot().system.shutting_down {
            return;
        }
        loop {
            match session.watch_until(StateWatch {
                revisions: state.applied(),
                sections: TERMINAL_SECTIONS,
            }) {
                Ok(Some(revisions)) => {
                    let changed = revisions
                        .changed_since(state.applied())
                        .intersection(TERMINAL_SECTIONS);
                    if !changed.is_empty() {
                        match session.get_state(changed) {
                            Ok(response) => {
                                state.apply(state.instance_id(), response);
                                let shutting_down = state.snapshot().system.shutting_down;
                                *latest_worker.lock() =
                                    Some(Ok((state.instance_id(), state.snapshot().clone())));
                                let disconnected = sender.try_send(()).is_err_and(|error| {
                                    matches!(
                                        error,
                                        crossbeam_channel::TrySendError::Disconnected(_)
                                    )
                                });
                                if disconnected || shutting_down {
                                    break;
                                }
                            }
                            Err(error) => {
                                *latest_worker.lock() =
                                    Some(Err(format!("TUI state read failed: {error:#}")));
                                let _ = sender.try_send(());
                                break;
                            }
                        }
                    }
                }
                Ok(None) => break, // Only local owner cancellation ends a watch this way.
                Err(error) => {
                    *latest_worker.lock() =
                        Some(Err(format!("TUI state watcher stopped: {error:#}")));
                    let _ = sender.try_send(());
                    break;
                }
            }
        }
    });
    Watcher {
        receiver,
        latest,
        stopping,
        worker: Some(worker),
    }
}

#[derive(Clone, Debug)]
pub(super) struct WorkerFailure {
    error: QueryError,
    instance_changed: bool,
}

impl From<anyhow::Error> for WorkerFailure {
    fn from(error: anyhow::Error) -> Self {
        Self {
            instance_changed: error.is::<rivu_client::InstanceChanged>(),
            error: QueryError::Message(format!("{error:#}")),
        }
    }
}

impl From<QueryError> for WorkerFailure {
    fn from(error: QueryError) -> Self {
        Self {
            error,
            instance_changed: false,
        }
    }
}

impl WorkerFailure {
    pub(super) fn into_message(self) -> Result<String> {
        Ok(match self.into_query_error()? {
            QueryError::Message(message) => message,
            error => error.to_string(),
        })
    }

    pub(super) fn into_query_error(self) -> Result<QueryError> {
        if self.instance_changed {
            return Err(anyhow::Error::new(self.error));
        }
        Ok(self.error)
    }
}

pub(super) struct CommandWorker {
    pub(super) sender: Option<Sender<AppCommand>>,
    stopping: Option<Sender<()>>,
    shutdown: rivu_client::SessionCancellation,
    pub(super) results: Receiver<Result<Ack, WorkerFailure>>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for CommandWorker {
    fn drop(&mut self) {
        self.sender.take();
        self.shutdown.cancel();
        if let Some(stopping) = self.stopping.take() {
            let _ = stopping.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(super) fn spawn_command_worker(data_dir: &Path, instance_id: u32) -> CommandWorker {
    let data_dir = data_dir.to_owned();
    let (sender, receiver) = bounded::<AppCommand>(8);
    let (stopping, stop_receiver) = bounded::<()>(1);
    let (result_sender, results) = bounded::<Result<Ack, WorkerFailure>>(8);
    let shutdown = rivu_client::SessionCancellation::default();
    let stop = shutdown.clone();
    let worker = std::thread::spawn(move || {
        let mut session = rivu_client::client_session_with_cancel(&data_dir, stop.clone());
        let mut session = session
            .bind_instance(instance_id)
            .map(|()| session)
            .map_err(WorkerFailure::from);
        while let Ok(command) = receiver.recv() {
            if stop.is_cancelled() {
                break;
            }
            let result = match &mut session {
                Ok(session) => session.request_ack(command).map_err(WorkerFailure::from),
                Err(error) => Err(error.clone()),
            };
            let instance_changed = result.as_ref().is_err_and(|error| error.instance_changed);
            crossbeam_channel::select! {
                send(result_sender, result) -> _ => {}
                recv(stop_receiver) -> _ => break,
            }
            if instance_changed {
                break;
            }
        }
    });
    CommandWorker {
        sender: Some(sender),
        stopping: Some(stopping),
        shutdown,
        results,
        worker: Some(worker),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadKind<'a> {
    Queue,
    QueueSelection(u64),
    Search(&'a str),
    LibrarySelection(i64),
    Directory(&'a Path),
    DirectorySelection {
        track_id: i64,
        path: &'a Path,
    },
    OtherTracksCount,
    OtherTracks {
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: LibrarySort,
        anchor: Option<i64>,
    },
    Playlists,
    Playlist(i64),
    PlaylistSummary(i64),
    PlaylistSelection {
        playlist_id: i64,
        entry_id: i64,
    },
    Track(i64),
}

#[derive(Clone, Copy, Default)]
pub(super) struct ReadChanges {
    pub(super) rows_changed: bool,
    pub(super) coordinates_changed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ReadDemand<'a> {
    pub(super) kind: ReadKind<'a>,
    pub(super) filter: Option<&'a str>,
    pub(super) offset: usize,
    pub(super) limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReadSpec {
    // Complete terminal intent exports both protocol work and borrowed demand.
    // Retain its actual protocol query for allocation-free polling and stamping.
    query: Query,
    // OtherTracks queries carry an intrinsic empty query even without a TUI filter.
    has_filter: bool,
    pub(super) offset: usize,
    pub(super) limit: usize,
    pub(super) stamp: QueryStamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct QueryRequest {
    pub(super) demand: Arc<ReadSpec>,
    pub(super) submission: QuerySubmission,
}

impl ReadKind<'_> {
    pub(super) fn filtered_query(
        self,
        offset: usize,
        limit: usize,
        filter: Option<String>,
    ) -> Query {
        let mut query = self.into_query(offset, limit);
        match &mut query {
            Query::QueuePage { query, .. }
            | Query::QueueEntries { query, .. }
            | Query::LibraryPage { query, .. }
            | Query::DirectoryPage { query, .. }
            | Query::LibraryLocate { query, .. }
            | Query::OtherTracksPage { query, .. }
            | Query::OtherTracksLocate { query, .. }
            | Query::PlaylistSummaries { query, .. }
            | Query::PlaylistEntries { query, .. }
            | Query::PlaylistEntryLocations { query, .. }
                if filter.is_some() =>
            {
                *query = filter;
            }
            _ => {}
        }
        query
    }

    fn into_query(self, offset: usize, limit: usize) -> Query {
        match self {
            ReadKind::OtherTracksCount => Query::OtherTracksCount,
            ReadKind::LibrarySelection(track_id) => Query::LibraryLocate {
                track_id,
                path: PathBuf::new(),
                tree: false,
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
            },
            ReadKind::DirectorySelection { track_id, path } => Query::LibraryLocate {
                track_id,
                path: path.to_owned(),
                tree: true,
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
            },
            ReadKind::Track(track_id) => Query::Track { track_id },
            ReadKind::PlaylistSummary(playlist_id) => Query::PlaylistSummary { playlist_id },
            ReadKind::Queue => Query::QueuePage {
                query: None,
                offset,
                limit,
            },
            ReadKind::QueueSelection(queue_id) => Query::QueueEntries {
                query: None,
                queue_ids: vec![queue_id],
            },
            ReadKind::Search(query) => Query::LibraryPage {
                query: Some(query.to_owned()),
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
                offset,
                limit,
            },
            ReadKind::OtherTracks {
                favorite,
                missing,
                sort,
                anchor,
            } => match anchor {
                Some(track_id) => Query::OtherTracksLocate {
                    track_id,
                    query: Some(String::new()),
                    favorite,
                    missing,
                    sort,
                },
                None => Query::OtherTracksPage {
                    query: Some(String::new()),
                    favorite,
                    missing,
                    sort,
                    offset,
                    limit,
                },
            },
            ReadKind::Directory(path) => Query::DirectoryPage {
                path: path.to_owned(),
                query: None,
                favorite: None,
                missing: None,
                offset,
                limit,
            },
            ReadKind::Playlists => Query::PlaylistSummaries {
                query: None,
                offset,
                limit,
            },
            ReadKind::Playlist(playlist_id) => Query::PlaylistEntries {
                query: None,
                playlist_id,
                offset,
                limit,
            },
            ReadKind::PlaylistSelection {
                playlist_id,
                entry_id,
            } => Query::PlaylistEntryLocations {
                query: None,
                playlist_id,
                entry_ids: vec![entry_id],
            },
        }
    }

    pub(super) fn playlist_id(&self) -> Option<i64> {
        match self {
            Self::Playlist(id)
            | Self::PlaylistSelection {
                playlist_id: id, ..
            } => Some(*id),
            _ => None,
        }
    }

    pub(super) fn changes(
        self,
        previous: &TuiSnapshot,
        next: &TuiSnapshot,
        filter: &str,
    ) -> ReadChanges {
        let playlist_id = self.playlist_id();
        let previous = previous.query_revisions(playlist_id);
        let next = next.query_revisions(playlist_id);
        if previous == next {
            return ReadChanges::default();
        }
        let query = self.filtered_query(0, 0, (!filter.is_empty()).then(|| filter.to_owned()));
        let previous = query.stamp(previous).revisions();
        let next = query.stamp(next).revisions();
        ReadChanges {
            rows_changed: previous != next,
            coordinates_changed: previous.with_bitrate_revision(0) != next.with_bitrate_revision(0),
        }
    }
}

impl ReadDemand<'_> {
    pub(super) fn own(self, state: &TuiSnapshot) -> ReadSpec {
        let query =
            self.kind
                .filtered_query(self.offset, self.limit, self.filter.map(str::to_owned));
        let mut demand = ReadSpec {
            has_filter: self.filter.is_some(),
            query,
            offset: self.offset,
            limit: self.limit,
            stamp: Default::default(),
        };
        demand.stamp = demand.query.stamp(demand.revisions(state));
        demand
    }

    pub(super) fn matches(self, demand: &ReadSpec, state: &TuiSnapshot) -> bool {
        self == demand.borrowed() && demand.stamp == demand.query.stamp(demand.revisions(state))
    }
}

impl ReadSpec {
    pub(super) fn protocol_query(&self) -> &Query {
        &self.query
    }

    pub(super) fn kind(&self) -> ReadKind<'_> {
        self.borrowed().kind
    }

    pub(super) fn borrowed(&self) -> ReadDemand<'_> {
        let (kind, filter) = match self.query.identity() {
            QueryIdentity::QueuePage(filter) => (ReadKind::Queue, filter.as_deref()),
            QueryIdentity::QueueEntries([id], filter) => {
                (ReadKind::QueueSelection(*id), filter.as_deref())
            }
            QueryIdentity::LibraryPage(query, ..) => (
                ReadKind::Search(
                    query
                        .as_deref()
                        .expect("Terminal search queries carry search text"),
                ),
                query.as_deref(),
            ),
            QueryIdentity::LibraryLocate(track_id, path, tree, filter, ..) => (
                if tree {
                    ReadKind::DirectorySelection { track_id, path }
                } else {
                    ReadKind::LibrarySelection(track_id)
                },
                filter.as_deref(),
            ),
            QueryIdentity::DirectoryPage(path, filter, ..) => {
                (ReadKind::Directory(path), filter.as_deref())
            }
            QueryIdentity::OtherTracksCount => (ReadKind::OtherTracksCount, None),
            QueryIdentity::OtherTracksPage(filter, favorite, missing, sort) => (
                ReadKind::OtherTracks {
                    favorite,
                    missing,
                    sort,
                    anchor: None,
                },
                filter.as_deref(),
            ),
            QueryIdentity::OtherTracksLocate(track_id, filter, favorite, missing, sort) => (
                ReadKind::OtherTracks {
                    favorite,
                    missing,
                    sort,
                    anchor: Some(track_id),
                },
                filter.as_deref(),
            ),
            QueryIdentity::PlaylistSummaries(filter) => (ReadKind::Playlists, filter.as_deref()),
            QueryIdentity::PlaylistEntries(id, filter) => {
                (ReadKind::Playlist(id), filter.as_deref())
            }
            QueryIdentity::PlaylistEntryLocations(playlist_id, [entry_id], filter) => (
                ReadKind::PlaylistSelection {
                    playlist_id,
                    entry_id: *entry_id,
                },
                filter.as_deref(),
            ),
            QueryIdentity::PlaylistSummary(id) => (ReadKind::PlaylistSummary(id), None),
            QueryIdentity::Track(id) => (ReadKind::Track(id), None),
            _ => unreachable!("ReadSpec only contains terminal queries"),
        };
        ReadDemand {
            kind,
            filter: self.has_filter.then_some(filter).flatten(),
            offset: self.offset,
            limit: self.limit,
        }
    }

    pub(super) fn revisions(&self, state: &TuiSnapshot) -> rivu_protocol::response::QueryRevisions {
        let revisions = state.query_revisions(self.kind().playlist_id());
        if matches!(self.query, Query::Track { .. }) {
            revisions.for_query(
                &self.query,
                &state.library.playlist_entry_revisions,
                &state.library.metadata_revisions,
            )
        } else {
            revisions
        }
    }

    pub(super) fn accepts_view(&self, view: &ViewResponse) -> bool {
        match (self.kind(), view) {
            (ReadKind::Queue, ViewResponse::QueuePage(_))
            | (ReadKind::QueueSelection(_), ViewResponse::QueueEntries(_))
            | (
                ReadKind::Search(_) | ReadKind::OtherTracks { anchor: None, .. },
                ViewResponse::LibraryPage(_),
            )
            | (
                ReadKind::LibrarySelection(_)
                | ReadKind::DirectorySelection { .. }
                | ReadKind::OtherTracks {
                    anchor: Some(_), ..
                },
                ViewResponse::LibraryLocate(_),
            )
            | (ReadKind::Directory(_), ViewResponse::DirectoryPage(_))
            | (ReadKind::OtherTracksCount, ViewResponse::OtherTracksCount(_))
            | (ReadKind::Playlists, ViewResponse::PlaylistSummaries(_))
            | (ReadKind::Playlist(_), ViewResponse::PlaylistEntries(_))
            | (ReadKind::PlaylistSelection { .. }, ViewResponse::PlaylistEntryLocations(_)) => true,
            (ReadKind::Track(id), ViewResponse::Track(track)) => {
                track.as_ref().is_none_or(|track| track.id == id)
            }
            (ReadKind::PlaylistSummary(id), ViewResponse::PlaylistSummary(summary)) => {
                summary.as_ref().is_none_or(|summary| summary.id == id)
            }
            _ => false,
        }
    }

    pub(super) fn query(&self) -> Query {
        self.query.clone()
    }
}

pub(super) struct QueryResult {
    pub(super) revisions: rivu_protocol::response::QueryRevisions,
    pub(super) view: ViewResponse,
}

struct QueryJob {
    request: QueryRequest,
    ticket: rivu_protocol::query_tracking::QueryTicket,
}

type QueryCompletion = (QueryRequest, Result<QueryResult, WorkerFailure>);

pub(super) struct QueryWorker {
    sender: Option<Sender<()>>,
    pending: Arc<Mutex<Option<QueryJob>>>,
    pub(super) latest: Arc<Mutex<Option<QueryCompletion>>>,
    cancellation: rivu_protocol::query_tracking::QueryEpoch,
    // Logical ownership only: withdrawing demand does not retire the worker's
    // retained physical exchange, which still drains before the next job.
    demanded: bool,
    shutdown: rivu_client::SessionCancellation,
    worker: Option<JoinHandle<()>>,
}

impl QueryWorker {
    pub(super) fn request(&mut self, request: QueryRequest) {
        self.cancel();
        let ticket = self.cancellation.ticket();
        *self.pending.lock() = Some(QueryJob { request, ticket });
        self.demanded = true;
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(());
        }
    }

    pub(super) fn cancel(&mut self) {
        if self.demanded {
            self.demanded = false;
            self.cancellation.cancel();
            self.pending.lock().take();
        }
    }
}

impl Drop for QueryWorker {
    fn drop(&mut self) {
        self.cancel();
        // Replacements cancel the active read but retain its physical wait.
        // Shutdown closes the socket, dropping Core's reply receiver instead.
        self.shutdown.cancel();
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(super) fn spawn_query_worker(data_dir: &Path, instance_id: u32) -> QueryWorker {
    let data_dir = data_dir.to_owned();
    let (sender, receiver) = bounded(1);
    let pending = Arc::new(Mutex::new(None::<QueryJob>));
    let requests = Arc::clone(&pending);
    let latest = Arc::new(Mutex::new(None));
    let results = Arc::clone(&latest);
    let shutdown = rivu_client::SessionCancellation::default();
    let stopping = shutdown.clone();
    let worker = std::thread::spawn(move || {
        let mut session = rivu_client::client_session_with_cancel(&data_dir, stopping.clone());
        let mut session = session
            .bind_instance(instance_id)
            .map(|()| session)
            .map_err(WorkerFailure::from);
        while receiver.recv().is_ok() {
            if stopping.is_cancelled() {
                break;
            }
            let Some(job) = requests.lock().take() else {
                continue;
            };
            if !job.ticket.is_current() {
                continue;
            }
            let result = match &mut session {
                Ok(session) => session
                    .query_retained_until(job.request.demand.query(), job.ticket.cancelled())
                    .map_err(WorkerFailure::from)
                    .and_then(|response| {
                        let view = response.result.map_err(WorkerFailure::from)?;
                        Ok(QueryResult {
                            revisions: response.revisions,
                            view,
                        })
                    }),
                Err(error) => Err(error.clone()),
            };
            let instance_changed = result.as_ref().is_err_and(|error| error.instance_changed);
            if instance_changed || job.ticket.is_current() {
                *results.lock() = Some((job.request, result));
            }
            if instance_changed {
                break;
            }
        }
    });
    QueryWorker {
        sender: Some(sender),
        pending,
        latest,
        cancellation: rivu_protocol::query_tracking::QueryEpoch::default(),
        demanded: false,
        shutdown,
        worker: Some(worker),
    }
}

#[cfg(test)]
mod tests;
