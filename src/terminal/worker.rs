use super::{Command, Path, PathBuf, Query, TuiSnapshot};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};
use parking_lot::Mutex;
use rivu_protocol::{
    model::{ClientSnapshot, LibrarySort, QueryStamp},
    response::{Ack, StateSections, ViewResponse},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

type WatchedSnapshot = Result<(u32, ClientSnapshot), String>;

pub(super) struct Watcher {
    pub(super) receiver: Receiver<()>,
    pub(super) latest: Arc<Mutex<Option<WatchedSnapshot>>>,
    stopping: rivu_client::SessionStop,
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
    let stopping = rivu_client::SessionStop::default();
    let stop = stopping.clone();
    let worker = std::thread::spawn(move || {
        let Ok(mut session) = rivu_client::client_session_with_cancel(&data_dir, stop) else {
            *latest_worker.lock() = Some(Err("TUI state watcher could not initialize".into()));
            let _ = sender.try_send(());
            return;
        };
        let initial = session.get_state(StateSections::ALL).and_then(|response| {
            let instance_id = session
                .instance_id()
                .context("Missing IPC Hello identity")?;
            session.bind_instance(instance_id)?;
            Ok((instance_id, response))
        });
        let mut state = match initial {
            Ok((instance_id, response)) => {
                let mut state = ClientSnapshot::default();
                let revisions = response.revisions;
                response.apply_to(&mut state);
                (state, revisions, instance_id)
            }
            Err(error) => {
                *latest_worker.lock() = Some(Err(format!("TUI state handshake failed: {error:#}")));
                let _ = sender.try_send(());
                return;
            }
        };
        *latest_worker.lock() = Some(Ok((state.2, state.0.clone())));
        let _ = sender.try_send(());
        if state.0.system.shutting_down {
            return;
        }
        loop {
            match session.watch_until(state.1) {
                Ok(Some(revisions)) => {
                    let changed = revisions.changed_since(state.1);
                    if !changed.is_empty() {
                        match session.get_state(changed) {
                            Ok(response) => {
                                state
                                    .1
                                    .apply_sections(response.revisions, response.sections());
                                response.apply_to(&mut state.0);
                                let shutting_down = state.0.system.shutting_down;
                                *latest_worker.lock() = Some(Ok((state.2, state.0.clone())));
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
                    } else {
                        state.1 = revisions;
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
    message: String,
    instance_changed: bool,
}

impl From<anyhow::Error> for WorkerFailure {
    fn from(error: anyhow::Error) -> Self {
        Self {
            instance_changed: error.is::<rivu_client::InstanceChanged>(),
            message: format!("{error:#}"),
        }
    }
}

impl WorkerFailure {
    pub(super) fn into_message(self) -> Result<String> {
        if self.instance_changed {
            return Err(anyhow::Error::msg(self.message));
        }
        Ok(self.message)
    }
}

pub(super) struct CommandWorker {
    pub(super) sender: Option<Sender<Command>>,
    stopping: Option<Sender<()>>,
    shutdown: rivu_client::SessionStop,
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
    let (sender, receiver) = bounded::<Command>(8);
    let (stopping, stop_receiver) = bounded::<()>(1);
    let (result_sender, results) = bounded::<Result<Ack, WorkerFailure>>(8);
    let shutdown = rivu_client::SessionStop::default();
    let stop = shutdown.clone();
    let worker = std::thread::spawn(move || {
        let mut session = rivu_client::client_session_with_cancel(&data_dir, stop.clone())
            .and_then(|mut session| {
                session.bind_instance(instance_id)?;
                Ok(session)
            })
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum QueryKind {
    Queue,
    QueueSelection(u64),
    Search(String),
    Directory(PathBuf),
    OtherTracks {
        query: String,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: LibrarySort,
        anchor: Option<i64>,
    },
    OtherTracksCount,
    Playlists,
    Playlist(i64),
    PlaylistSelection {
        playlist_id: i64,
        entry_id: i64,
    },
    Track(i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct QueryRequest {
    pub(super) kind: QueryKind,
    pub(super) offset: usize,
    pub(super) limit: usize,
    pub(super) generation: u64,
    pub(super) stamp: QueryStamp,
}

impl QueryKind {
    pub(super) fn query(&self, offset: usize, limit: usize) -> Query {
        self.clone().into_query(offset, limit)
    }

    fn into_query(self, offset: usize, limit: usize) -> Query {
        match self {
            QueryKind::OtherTracksCount => Query::OtherTracksCount,
            QueryKind::Track(track_id) => Query::Track { track_id },
            QueryKind::Queue => Query::QueuePage {
                query: None,
                offset,
                limit,
            },
            QueryKind::QueueSelection(queue_id) => Query::QueueEntries {
                query: None,
                queue_ids: vec![queue_id],
            },
            QueryKind::Search(query) => Query::LibraryPage {
                query: Some(query),
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
                offset,
                limit,
            },
            QueryKind::OtherTracks {
                query,
                favorite,
                missing,
                sort,
                anchor,
            } => match anchor {
                Some(track_id) => Query::OtherTracksLocate {
                    track_id,
                    query: Some(query),
                    favorite,
                    missing,
                    sort,
                },
                None => Query::OtherTracksPage {
                    query: Some(query),
                    favorite,
                    missing,
                    sort,
                    offset,
                    limit,
                },
            },
            QueryKind::Directory(path) => Query::DirectoryPage {
                path,
                query: None,
                favorite: None,
                missing: None,
                offset,
                limit,
            },
            QueryKind::Playlists => Query::PlaylistSummaries {
                query: None,
                offset,
                limit,
            },
            QueryKind::Playlist(playlist_id) => Query::PlaylistEntries {
                query: None,
                playlist_id,
                offset,
                limit,
            },
            QueryKind::PlaylistSelection {
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

    pub(super) fn changed(self, previous: &TuiSnapshot, next: &TuiSnapshot) -> bool {
        let playlist_id = self.playlist_id();
        let query = self.into_query(0, 0);
        query.stamp(previous.query_revisions(playlist_id))
            != query.stamp(next.query_revisions(playlist_id))
    }
}

impl QueryRequest {
    pub(super) fn query(&self) -> Query {
        self.kind.query(self.offset, self.limit)
    }
}

pub(super) struct QueryResult {
    pub(super) revisions: rivu_protocol::response::QueryRevisions,
    pub(super) view: ViewResponse,
}

struct QueryJob {
    request: QueryRequest,
    cancelled: Arc<AtomicBool>,
}

type QueryCompletion = (QueryRequest, Result<QueryResult, WorkerFailure>);

pub(super) struct QueryWorker {
    sender: Option<Sender<()>>,
    pending: Arc<Mutex<Option<QueryJob>>>,
    pub(super) latest: Arc<Mutex<Option<QueryCompletion>>>,
    cancellation: Option<Arc<AtomicBool>>,
    shutdown: rivu_client::SessionStop,
    worker: Option<JoinHandle<()>>,
}

impl QueryWorker {
    pub(super) fn request(&mut self, request: QueryRequest) {
        self.cancel();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.cancellation = Some(Arc::clone(&cancelled));
        *self.pending.lock() = Some(QueryJob { request, cancelled });
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(());
        }
    }

    pub(super) fn cancel(&mut self) {
        if let Some(cancelled) = self.cancellation.take() {
            cancelled.store(true, Ordering::Release);
        }
        self.pending.lock().take();
    }
}

impl Drop for QueryWorker {
    fn drop(&mut self) {
        self.cancel();
        // Replacement invalidates results, not the in-flight server query. Only
        // shutdown aborts its socket wait; otherwise one active query drains
        // before the most recent pending request is sent.
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
    let shutdown = rivu_client::SessionStop::default();
    let stopping = shutdown.clone();
    let worker = std::thread::spawn(move || {
        let mut session = rivu_client::client_session_with_cancel(&data_dir, stopping.clone())
            .and_then(|mut session| {
                session.bind_instance(instance_id)?;
                Ok(session)
            })
            .map_err(WorkerFailure::from);
        while receiver.recv().is_ok() {
            if stopping.is_cancelled() {
                break;
            }
            let Some(job) = requests.lock().take() else {
                continue;
            };
            let result = match &mut session {
                Ok(session) => session
                    .query(job.request.query())
                    .map_err(WorkerFailure::from)
                    .and_then(|response| {
                        let view = response.result.map_err(|error| WorkerFailure {
                            message: error.to_string(),
                            instance_changed: false,
                        })?;
                        Ok(QueryResult {
                            revisions: response.revisions,
                            view,
                        })
                    }),
                Err(error) => Err(error.clone()),
            };
            let instance_changed = result.as_ref().is_err_and(|error| error.instance_changed);
            if instance_changed || !job.cancelled.load(Ordering::Acquire) {
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
        cancellation: None,
        shutdown,
        worker: Some(worker),
    }
}

#[cfg(test)]
mod tests;
