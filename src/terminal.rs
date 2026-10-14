use crate::{
    ipc,
    model::{
        ClientSnapshot, Command, DirectoryRow, LibraryRow, LibrarySort, PlaybackStatus,
        PlaylistEntryRow, PlaylistSummary, Query, QueueRow, RepeatMode, playback_key_command,
    },
    projection::TuiSnapshot,
    response::{Ack, PlaylistEntryRevision, StateSections, ViewResponse},
};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use parking_lot::Mutex;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    collections::HashMap,
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// Connect to an existing core over IPC. This never creates an audio engine and
/// quitting the screen only restores the terminal; the server keeps playing.
pub fn run(data_dir: &Path) -> Result<()> {
    enable_raw_mode().context("enable terminal raw mode")?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        restore_terminal_without_screen(&mut stdout);
        return Err(error.into());
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            let mut stdout = io::stdout();
            restore_terminal_without_screen(&mut stdout);
            return Err(error.into());
        }
    };
    let result = run_loop(data_dir, &mut terminal);
    let cleanup = restore_terminal(&mut terminal);
    match result {
        Err(error) => Err(error),
        Ok(()) => cleanup,
    }
}

fn restore_terminal_without_screen(stdout: &mut Stdout) {
    let _ = disable_raw_mode();
    let _ = execute!(stdout, LeaveAlternateScreen, crossterm::cursor::Show);
}
fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let mut first = None;
    if let Err(error) = disable_raw_mode().context("restore terminal raw mode") {
        first = Some(error);
    }
    if let Err(error) =
        execute!(terminal.backend_mut(), LeaveAlternateScreen).context("leave alternate screen")
        && first.is_none()
    {
        first = Some(error);
    }
    if let Err(error) = terminal.show_cursor().context("restore cursor")
        && first.is_none()
    {
        first = Some(error);
    }
    first.map_or(Ok(()), Err)
}

type WatchedSnapshot = Result<(u32, ClientSnapshot), String>;

struct Watcher {
    receiver: Receiver<()>,
    latest: Arc<Mutex<Option<WatchedSnapshot>>>,
    stopping: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.cancel_notify.notify_waiters();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn spawn_watcher(data_dir: &Path) -> Watcher {
    let data_dir = data_dir.to_owned();
    let (sender, receiver) = bounded(1);
    let latest = Arc::new(Mutex::new(None));
    let latest_worker = Arc::clone(&latest);
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stopping);
    let cancel_notify = Arc::new(tokio::sync::Notify::new());
    let thread_notify = Arc::clone(&cancel_notify);
    let worker = std::thread::spawn(move || {
        let Ok(mut session) = ipc::client_session_with_cancel(&data_dir, stop, thread_notify)
        else {
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
                Ok(None) => break,
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
        cancel_notify,
        worker: Some(worker),
    }
}

#[derive(Clone, Debug)]
struct WorkerFailure {
    message: String,
    instance_changed: bool,
}

impl From<anyhow::Error> for WorkerFailure {
    fn from(error: anyhow::Error) -> Self {
        Self {
            instance_changed: error.is::<ipc::InstanceChanged>(),
            message: format!("{error:#}"),
        }
    }
}

impl WorkerFailure {
    fn into_message(self) -> Result<String> {
        if self.instance_changed {
            return Err(anyhow::Error::msg(self.message));
        }
        Ok(self.message)
    }
}

struct CommandWorker {
    sender: Option<Sender<Command>>,
    stopping: Option<Sender<()>>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
    results: Receiver<Result<Ack, WorkerFailure>>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for CommandWorker {
    fn drop(&mut self) {
        self.sender.take();
        self.shutdown.store(true, Ordering::Release);
        self.shutdown_notify.notify_waiters();
        if let Some(stopping) = self.stopping.take() {
            let _ = stopping.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn spawn_command_worker(data_dir: &Path, instance_id: u32) -> CommandWorker {
    let data_dir = data_dir.to_owned();
    let (sender, receiver) = bounded::<Command>(8);
    let (stopping, stop_receiver) = bounded::<()>(1);
    let (result_sender, results) = bounded::<Result<Ack, WorkerFailure>>(8);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_notify = Arc::new(tokio::sync::Notify::new());
    let stop = Arc::clone(&shutdown);
    let stop_notify = Arc::clone(&shutdown_notify);
    let worker = std::thread::spawn(move || {
        let mut session =
            ipc::client_session_with_cancel(&data_dir, Arc::clone(&stop), stop_notify)
                .and_then(|mut session| {
                    session.bind_instance(instance_id)?;
                    Ok(session)
                })
                .map_err(WorkerFailure::from);
        while let Ok(command) = receiver.recv() {
            if stop.load(Ordering::Acquire) {
                break;
            }
            let result = match &mut session {
                Ok(session) => session.request_ack(&command).map_err(WorkerFailure::from),
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
        shutdown_notify,
        results,
        worker: Some(worker),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum QueryKind {
    Queue,
    QueueSelection(u64),
    Search(String),
    Directory(PathBuf),
    Playlists,
    Playlist(i64),
    PlaylistSelection { playlist_id: i64, entry_id: i64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct QueryRequest {
    kind: QueryKind,
    offset: usize,
    limit: usize,
    view_revision: u64,
    library_revision: u64,
    structure_revision: u64,
    playlist_revision: u64,
    playlist_entry_revision: Option<PlaylistEntryRevision>,
    queue_revision: u64,
}

impl QueryRequest {
    fn query(&self) -> Query {
        let offset = self.offset;
        let limit = self.limit;
        match &self.kind {
            QueryKind::Queue => Query::QueuePage { offset, limit },
            QueryKind::QueueSelection(queue_id) => Query::QueueEntries {
                queue_ids: vec![*queue_id],
            },
            QueryKind::Search(query) => Query::LibraryPage {
                query: Some(query.clone()),
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
                offset,
                limit,
            },
            QueryKind::Directory(path) => Query::DirectoryPage {
                path: path.clone(),
                query: None,
                favorite: None,
                missing: None,
                offset,
                limit,
            },
            QueryKind::Playlists => Query::PlaylistSummaries { offset, limit },
            QueryKind::Playlist(playlist_id) => Query::PlaylistEntries {
                playlist_id: *playlist_id,
                offset,
                limit,
            },
            QueryKind::PlaylistSelection { entry_id, .. } => Query::PlaylistEntry {
                entry_id: *entry_id,
            },
        }
    }
}

struct QueryResult {
    revisions: crate::response::QueryRevisions,
    view: ViewResponse,
}

struct QueryJob {
    request: QueryRequest,
    cancelled: Arc<AtomicBool>,
}

type QueryCompletion = (QueryRequest, Result<QueryResult, WorkerFailure>);

struct QueryWorker {
    sender: Option<Sender<()>>,
    pending: Arc<Mutex<Option<QueryJob>>>,
    latest: Arc<Mutex<Option<QueryCompletion>>>,
    cancellation: Option<Arc<AtomicBool>>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
    worker: Option<JoinHandle<()>>,
}

impl QueryWorker {
    fn request(&mut self, request: QueryRequest) {
        self.cancel();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.cancellation = Some(Arc::clone(&cancelled));
        *self.pending.lock() = Some(QueryJob { request, cancelled });
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(());
        }
    }

    fn cancel(&mut self) {
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
        self.shutdown.store(true, Ordering::Release);
        self.shutdown_notify.notify_waiters();
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn spawn_query_worker(data_dir: &Path, instance_id: u32) -> QueryWorker {
    let data_dir = data_dir.to_owned();
    let (sender, receiver) = bounded(1);
    let pending = Arc::new(Mutex::new(None::<QueryJob>));
    let requests = Arc::clone(&pending);
    let latest = Arc::new(Mutex::new(None));
    let results = Arc::clone(&latest);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_notify = Arc::new(tokio::sync::Notify::new());
    let stopping = Arc::clone(&shutdown);
    let stop_notify = Arc::clone(&shutdown_notify);
    let worker = std::thread::spawn(move || {
        let mut session =
            ipc::client_session_with_cancel(&data_dir, Arc::clone(&stopping), stop_notify)
                .and_then(|mut session| {
                    session.bind_instance(instance_id)?;
                    Ok(session)
                })
                .map_err(WorkerFailure::from);
        while receiver.recv().is_ok() {
            if stopping.load(Ordering::Acquire) {
                break;
            }
            let Some(job) = requests.lock().take() else {
                continue;
            };
            let result = match &mut session {
                Ok(session) => session
                    .query(&job.request.query())
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
        shutdown_notify,
        worker: Some(worker),
    }
}

fn run_loop(data_dir: &Path, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let updates = spawn_watcher(data_dir);
    let mut workers = None;
    let mut tui = None::<TuiState>;
    let mut redraw = true;
    let mut immediate_redraw = true;
    let mut last_draw = Instant::now() - Duration::from_millis(100);
    loop {
        while updates.receiver.try_recv().is_ok() {}
        if let Some(update) = updates.latest.lock().take() {
            let (instance_id, state) = update.map_err(anyhow::Error::msg)?;
            if workers.is_none() {
                workers = Some((
                    spawn_command_worker(data_dir, instance_id),
                    spawn_query_worker(data_dir, instance_id),
                ));
            }
            let next = TuiSnapshot::from_client(&state, tui.as_ref().map(|tui| &tui.snapshot));
            match &mut tui {
                Some(tui) => tui.apply_snapshot(next),
                None => tui = Some(TuiState::new(next)),
            }
            redraw = true;
        }
        let Some(tui) = &mut tui else {
            if redraw {
                terminal.draw(draw_loading)?;
                redraw = false;
            }
            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key)
                        if key.kind != KeyEventKind::Release && key.code == KeyCode::Char('q') =>
                    {
                        break;
                    }
                    Event::Resize(_, _) => redraw = true,
                    _ => {}
                }
            }
            continue;
        };
        let (commands, queries) = workers.as_mut().expect("Initial state starts TUI workers");
        while let Ok(result) = commands.results.try_recv() {
            match result {
                Ok(ack) if !ack.ok => {
                    tui.ui
                        .set_message(Some(ack.error.unwrap_or_else(|| "Command rejected".into())));
                }
                Ok(_) => {}
                Err(error) => tui.ui.set_message(Some(error.into_message()?)),
            }
            redraw = true;
            immediate_redraw = true;
        }
        if tui.ui.prepare_local_view(&tui.snapshot) {
            redraw = true;
            immediate_redraw = true;
        }
        let latest = queries.latest.lock().take();
        if let Some((request, result)) = latest {
            // Instance validity is independent of view/state revision validity:
            // even a superseded query must exit on a replacement server.
            let result = match result {
                Ok(result) => Ok(result),
                Err(error) => Err(error.into_message()?),
            };
            if tui.ui.apply_query(&tui.snapshot, &request, result) {
                redraw = true;
                immediate_redraw = true;
            }
        }
        match tui.ui.query_request(&tui.snapshot) {
            Some(request) if tui.ui.requested.as_ref() != Some(&request) => {
                queries.request(request.clone());
                tui.ui.requested = Some(request);
            }
            None => {
                queries.cancel();
                tui.ui.requested = None;
            }
            _ => {}
        }
        if tui.ui.expire_message() {
            redraw = true;
            immediate_redraw = true;
        }
        if redraw && (immediate_redraw || last_draw.elapsed() >= Duration::from_millis(100)) {
            terminal.draw(|frame| draw(frame, &tui.snapshot, &mut tui.ui))?;
            last_draw = Instant::now();
            redraw = false;
            immediate_redraw = false;
        }
        if event::poll(Duration::from_millis(50))? {
            let event = event::read()?;
            match event {
                Event::Resize(_, _) => {
                    redraw = true;
                    immediate_redraw = true;
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let local_revision = tui.ui.local_revision;
                    let previous_revision = tui.snapshot.system.revision;
                    let next = match tui.ui.key_action(key, &tui.snapshot) {
                        KeyAction::Quit => break,
                        action @ (KeyAction::Search | KeyAction::Tree) => {
                            tui.ui.view = if matches!(action, KeyAction::Tree) {
                                View::Tree(LibraryTree::new(&tui.snapshot.system.library_roots))
                            } else {
                                View::Search(LibrarySearch::default())
                            };
                            tui.ui.requested = None;
                            tui.ui.mark_local_change();
                            tui.snapshot.clone()
                        }
                        KeyAction::Command(command) => {
                            let queued = commands
                                .sender
                                .as_ref()
                                .is_some_and(|sender| sender.try_send(command).is_ok());
                            if !queued {
                                tui.ui.set_message(Some("Command queue busy".into()));
                                redraw = true;
                                immediate_redraw = true;
                            }
                            continue;
                        }
                        KeyAction::Ignored => {
                            if tui.ui.local_revision != local_revision {
                                redraw = true;
                                immediate_redraw = true;
                            }
                            continue;
                        }
                    };
                    tui.apply_snapshot(next);
                    redraw |= tui.ui.local_revision != local_revision
                        || tui.snapshot.system.revision != previous_revision;
                    immediate_redraw = true;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

struct TuiState {
    snapshot: TuiSnapshot,
    ui: UiState,
}

impl TuiState {
    fn new(snapshot: TuiSnapshot) -> Self {
        let mut ui = UiState::default();
        ui.queue.total = snapshot.queue.total;
        ui.queue.loaded = true;
        ui.queue
            .selection
            .select((snapshot.queue.total != 0).then_some(0));
        Self { snapshot, ui }
    }

    fn apply_snapshot(&mut self, next: TuiSnapshot) {
        let reordered = if let View::PlaylistDetail { detail, .. } = &mut self.ui.view {
            let changed = self
                .snapshot
                .library
                .playlist_entry_revisions
                .get(&detail.playlist_id)
                != next
                    .library
                    .playlist_entry_revisions
                    .get(&detail.playlist_id);
            if changed {
                detail.entry_anchor = detail
                    .entry_anchor
                    .or_else(|| detail.entries.selected().map(|entry| entry.id));
            }
            changed
        } else {
            false
        };
        if self.snapshot.library.revision != next.library.revision
            || self.snapshot.library.playlist_revision != next.library.playlist_revision
            || self.snapshot.system.page_size != next.system.page_size
            || reordered
        {
            self.ui.invalidate_views();
        }
        if self.snapshot.system.library_roots != next.system.library_roots
            && let View::Tree(tree) = &mut self.ui.view
        {
            tree.roots = LibraryTree::roots(&next.system.library_roots);
            tree.entries.clear();
            self.ui.requested = None;
        }
        self.ui.sync_queue(&self.snapshot, &next);
        if let Some(message) = snapshot_message(&self.snapshot, &next) {
            self.ui.set_message(Some(message));
        }
        self.snapshot = next;
    }
}

fn snapshot_message(previous: &TuiSnapshot, next: &TuiSnapshot) -> Option<String> {
    if previous.playback.status != next.playback.status {
        return Some(match next.playback.status {
            PlaybackStatus::Playing => "Playing".into(),
            PlaybackStatus::Paused => "Paused".into(),
            PlaybackStatus::Stopped => "Stopped".into(),
        });
    }
    if previous.queue.current_id != next.queue.current_id {
        return next
            .current_track()
            .map(|track| format!("Now playing: {} — {}", track.artist, track.title));
    }
    match next.queue.total.cmp(&previous.queue.total) {
        std::cmp::Ordering::Greater => Some("Added to queue".into()),
        std::cmp::Ordering::Less => Some("Removed from queue".into()),
        std::cmp::Ordering::Equal => None,
    }
}

struct Notice {
    text: String,
    expires_at: Instant,
}

#[derive(Default)]
enum View {
    #[default]
    Queue,
    Search(LibrarySearch),
    Tree(LibraryTree),
    Playlists(PagedList<PlaylistSummary>),
    PlaylistDetail {
        playlists: (ListState, usize),
        detail: PlaylistDetail,
    },
}

#[derive(Default)]
struct UiState {
    view: View,
    queue: PagedList<QueueRow>,
    // Resolve the selected occurrence after edits before requesting its new viewport.
    queue_anchor: Option<u64>,
    show_help: bool,
    message: Option<Notice>,
    requested: Option<QueryRequest>,
    query_revision: u64,
    failed_request: Option<QueryRequest>,
    local_revision: u64,
    // Keyboard PageUp/PageDown distance follows the terminal viewport, not query limits.
    page_size: usize,
}
impl UiState {
    fn prepare_local_view(&mut self, state: &TuiSnapshot) -> bool {
        let View::Tree(tree) = &mut self.view else {
            return false;
        };
        if !tree.directory.as_os_str().is_empty() {
            return false;
        }
        let limit = state.system.page_size as usize;
        let mut changed = false;
        while let Some(offset) = tree.entries.next_offset(limit) {
            let rows = tree
                .roots
                .iter()
                .skip(offset)
                .take(limit)
                .cloned()
                .map(|path| DirectoryRow::Directory { path })
                .collect();
            tree.entries.insert(offset, limit, tree.roots.len(), rows);
            changed = true;
        }
        changed
    }

    fn query_request(&self, state: &TuiSnapshot) -> Option<QueryRequest> {
        let limit = state.system.page_size as usize;
        let (kind, offset) = match &self.view {
            View::Queue if state.queue.total != 0 => match self.queue_anchor {
                Some(queue_id) => (QueryKind::QueueSelection(queue_id), 0),
                None => (QueryKind::Queue, self.queue.next_offset(limit)?),
            },
            View::Search(search) if search.ready(Instant::now()) => {
                let offset = search.results.next_offset(limit)?;
                (QueryKind::Search(search.query.clone()), offset)
            }
            View::Tree(tree) if !tree.directory.as_os_str().is_empty() => (
                QueryKind::Directory(tree.directory.clone()),
                tree.entries.next_offset(limit)?,
            ),
            View::Playlists(playlists) => (QueryKind::Playlists, playlists.next_offset(limit)?),
            View::PlaylistDetail { detail, .. } => match detail.entry_anchor {
                Some(entry_id) => (
                    QueryKind::PlaylistSelection {
                        playlist_id: detail.playlist_id,
                        entry_id,
                    },
                    0,
                ),
                None => (
                    QueryKind::Playlist(detail.playlist_id),
                    detail.entries.next_offset(limit)?,
                ),
            },
            _ => return None,
        };
        let playlist_entry_revision = match &kind {
            QueryKind::Playlist(playlist_id) | QueryKind::PlaylistSelection { playlist_id, .. } => {
                Some(PlaylistEntryRevision {
                    playlist_id: *playlist_id,
                    revision: state
                        .library
                        .playlist_entry_revisions
                        .get(playlist_id)
                        .copied()
                        .unwrap_or(state.library.playlist_revision),
                })
            }
            _ => None,
        };
        let request = QueryRequest {
            kind,
            offset,
            limit,
            view_revision: self.query_revision,
            library_revision: state.library.revision,
            structure_revision: state.library.structure_revision,
            playlist_revision: state.library.playlist_revision,
            playlist_entry_revision,
            queue_revision: state.queue.revision,
        };
        if self.failed_request.as_ref() == Some(&request) {
            None
        } else {
            Some(request)
        }
    }

    fn apply_query(
        &mut self,
        state: &TuiSnapshot,
        request: &QueryRequest,
        result: Result<QueryResult, String>,
    ) -> bool {
        if self.query_request(state).as_ref() != Some(request) {
            if self.requested.as_ref() == Some(request) {
                self.requested = None;
            }
            return false;
        }
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.requested = None;
                self.failed_request = Some(request.clone());
                self.set_message(Some(error));
                return true;
            }
        };
        let revision_matches = match &request.kind {
            QueryKind::Queue | QueryKind::QueueSelection(_) => {
                result.revisions.queue == request.queue_revision
            }
            QueryKind::Directory(_) => result.revisions.structure == request.structure_revision,
            QueryKind::Playlists => {
                result.revisions.library == request.library_revision
                    && result.revisions.playlist == request.playlist_revision
            }
            QueryKind::Playlist(_) | QueryKind::PlaylistSelection { .. } => {
                result.revisions.library == request.library_revision
                    && result.revisions.playlist == request.playlist_revision
                    && (result.revisions.playlist_entries == request.playlist_entry_revision
                        || matches!(&result.view, ViewResponse::PlaylistEntry(None)))
            }
            QueryKind::Search(_) => result.revisions.library == request.library_revision,
        };
        if !revision_matches {
            self.requested = None;
            return false;
        }
        match (&mut self.view, result.view) {
            (View::Queue, ViewResponse::QueuePage(page)) if request.kind == QueryKind::Queue => {
                self.queue
                    .insert(request.offset, request.limit, page.total, page.rows);
            }
            (View::Queue, ViewResponse::QueueEntries(rows)) => {
                if let QueryKind::QueueSelection(queue_id) = request.kind {
                    if let Some(row) = rows.iter().find(|row| row.id == queue_id) {
                        self.queue.selection.select(Some(row.position));
                        self.queue.set_viewport(self.queue.viewport_rows);
                    }
                    self.queue_anchor = None;
                } else {
                    self.set_message(Some("Unexpected view response".into()));
                }
            }
            (View::Search(search), ViewResponse::LibraryPage(page)) => {
                search
                    .results
                    .insert(request.offset, request.limit, page.total, page.rows);
            }
            (View::Tree(tree), ViewResponse::DirectoryPage(page)) => {
                tree.entries
                    .insert(request.offset, request.limit, page.total, page.rows);
            }
            (View::Playlists(playlists), ViewResponse::PlaylistSummaries(page)) => {
                playlists.insert(request.offset, request.limit, page.total, page.rows);
            }
            (View::PlaylistDetail { detail, .. }, ViewResponse::PlaylistEntries(page)) => {
                detail
                    .entries
                    .insert(request.offset, request.limit, page.total, page.rows);
            }
            (View::PlaylistDetail { detail, .. }, ViewResponse::PlaylistEntry(entry)) => {
                if let QueryKind::PlaylistSelection {
                    playlist_id,
                    entry_id,
                } = request.kind
                {
                    let entry = entry
                        .filter(|entry| entry.id == entry_id && entry.playlist_id == playlist_id);
                    if let Some(entry) = entry {
                        detail.entries.total = entry.total;
                        detail.entries.loaded = true;
                        detail.entries.selection.select(Some(entry.position));
                        detail.entries.set_viewport(detail.entries.viewport_rows);
                    }
                    detail.entry_anchor = None;
                } else {
                    self.set_message(Some("Unexpected view response".into()));
                }
            }
            _ => {
                self.set_message(Some("Unexpected view response".into()));
                return true;
            }
        }
        self.mark_local_change();
        true
    }

    fn mark_local_change(&mut self) {
        self.local_revision = self.local_revision.wrapping_add(1);
    }

    fn set_message(&mut self, message: Option<String>) {
        let current = self.message.as_ref().map(|notice| notice.text.as_str());
        if current != message.as_deref() {
            self.message = message.map(|text| Notice {
                text,
                expires_at: Instant::now() + Duration::from_secs(10),
            });
            self.mark_local_change();
        }
    }

    fn expire_message(&mut self) -> bool {
        if self
            .message
            .as_ref()
            .is_some_and(|notice| Instant::now() >= notice.expires_at)
        {
            self.message = None;
            return true;
        }
        false
    }

    fn sync_queue(&mut self, previous: &TuiSnapshot, next: &TuiSnapshot) {
        if previous.queue.revision == next.queue.revision {
            return;
        }
        self.queue_anchor = self
            .queue_anchor
            .or_else(|| self.queue.selected().map(|row| row.id));
        let selected = self.queue.selection.selected().unwrap_or(0);
        self.queue.clear();
        self.queue.total = next.queue.total;
        self.queue.loaded = true;
        self.queue
            .selection
            .select((next.queue.total != 0).then(|| selected.min(next.queue.total - 1)));
        if next.queue.total == 0 {
            self.queue_anchor = None;
        }
        self.queue.set_viewport(self.queue.viewport_rows);
        self.requested = None;
        self.failed_request = None;
    }

    fn invalidate_views(&mut self) {
        self.requested = None;
        self.failed_request = None;
        self.query_revision = self.query_revision.wrapping_add(1);
        match &mut self.view {
            View::Search(search) => search.results.clear(),
            View::Tree(tree) => tree.entries.clear(),
            View::Playlists(playlists) => playlists.clear(),
            View::PlaylistDetail { detail, .. } => detail.entries.clear(),
            View::Queue => {}
        }
    }

    fn key_action(&mut self, key: KeyEvent, state: &TuiSnapshot) -> KeyAction {
        if key.code == KeyCode::Char('R') {
            self.requested = None;
            self.failed_request = None;
            self.mark_local_change();
            return KeyAction::Ignored;
        }
        if self.failed_request.is_some() {
            self.failed_request = None;
        }
        if key.code == KeyCode::Char('?') {
            self.show_help = !self.show_help;
            self.mark_local_change();
            return KeyAction::Ignored;
        }
        if !matches!(self.view, View::Search(_)) {
            match key.code {
                KeyCode::Char('/') => return KeyAction::Search,
                KeyCode::Char('t') => return KeyAction::Tree,
                KeyCode::Char('P') => {
                    self.view = View::Playlists(PagedList::default());
                    self.requested = None;
                    self.mark_local_change();
                    return KeyAction::Ignored;
                }
                _ => {}
            }
        }
        let page_size = self.page_size;
        let view = std::mem::take(&mut self.view);
        let mut changed = false;
        let mut command = None;
        match view {
            View::Search(mut search) => {
                let close = key.code == KeyCode::Esc;
                if !close {
                    match key.code {
                        KeyCode::Tab => {
                            search.focus = match search.focus {
                                SearchFocus::Query => SearchFocus::Results,
                                SearchFocus::Results => SearchFocus::Query,
                            };
                            changed = true;
                        }
                        KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
                            if search.focus == SearchFocus::Results =>
                        {
                            changed = search.results.navigate(key.code, page_size);
                        }
                        KeyCode::Enter if search.focus == SearchFocus::Results => {
                            command = search.results.selected().map(|track| Command::Enqueue {
                                track_ids: vec![track.id],
                            });
                        }
                        KeyCode::Enter => {
                            search.focus = SearchFocus::Results;
                            changed = true;
                        }
                        KeyCode::Backspace if search.focus == SearchFocus::Query => {
                            if search.query.pop().is_some() {
                                search.filter(Instant::now());
                                self.requested = None;
                                changed = true;
                            }
                        }
                        KeyCode::Char(character) if search.focus == SearchFocus::Query => {
                            search.query.push(character);
                            search.filter(Instant::now());
                            self.requested = None;
                            changed = true;
                        }
                        _ => {}
                    }
                }
                self.view = if close {
                    View::Queue
                } else {
                    View::Search(search)
                };
                changed |= close;
            }
            View::Tree(mut tree) => {
                let close = key.code == KeyCode::Esc;
                if !close {
                    match key.code {
                        KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                            changed = tree.entries.navigate(key.code, page_size);
                        }
                        KeyCode::Enter => {
                            let directory = tree.directory.clone();
                            command = tree.activate();
                            changed = tree.directory != directory;
                            if changed {
                                self.requested = None;
                            }
                        }
                        KeyCode::Backspace | KeyCode::Left => {
                            changed = tree.parent();
                            if changed {
                                self.requested = None;
                            }
                        }
                        _ => {
                            self.view = View::Tree(tree);
                            return key_action(key, state);
                        }
                    }
                }
                self.view = if close { View::Queue } else { View::Tree(tree) };
                changed |= close;
            }
            View::PlaylistDetail {
                playlists,
                mut detail,
            } => {
                let close = matches!(key.code, KeyCode::Esc | KeyCode::Left);
                if !close {
                    match key.code {
                        KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                            changed = detail.entries.navigate(key.code, page_size);
                            if changed {
                                detail.entry_anchor = None;
                            }
                        }
                        KeyCode::Enter => {
                            command = detail.entries.selected().map(|entry| Command::Play {
                                track_id: entry.track_id,
                            });
                        }
                        KeyCode::Char('a') => {
                            command = detail.entries.selected().map(|entry| Command::Enqueue {
                                track_ids: vec![entry.track_id],
                            });
                        }
                        _ => {
                            self.view = View::PlaylistDetail { playlists, detail };
                            return key_action(key, state);
                        }
                    }
                }
                self.view = if close {
                    self.requested = None;
                    View::Playlists(PagedList {
                        selection: playlists.0,
                        viewport_rows: playlists.1,
                        ..PagedList::default()
                    })
                } else {
                    View::PlaylistDetail { playlists, detail }
                };
                changed |= close;
            }
            View::Playlists(mut playlists) => {
                let close = key.code == KeyCode::Esc;
                if !close {
                    match key.code {
                        KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                            changed = playlists.navigate(key.code, page_size);
                        }
                        KeyCode::Right => {
                            if let Some(playlist) = playlists.selected() {
                                let detail = PlaylistDetail {
                                    playlist_id: playlist.id,
                                    name: playlist.name.clone(),
                                    entries: PagedList::default(),
                                    entry_anchor: None,
                                };
                                self.view = View::PlaylistDetail {
                                    playlists: (playlists.selection, playlists.viewport_rows),
                                    detail,
                                };
                                self.requested = None;
                                self.mark_local_change();
                                return KeyAction::Ignored;
                            }
                        }
                        KeyCode::Enter => {
                            command = playlists.selected().map(|playlist| Command::PlayPlaylist {
                                playlist_id: playlist.id,
                            });
                        }
                        _ => {
                            self.view = View::Playlists(playlists);
                            return key_action(key, state);
                        }
                    }
                }
                self.view = if close {
                    View::Queue
                } else {
                    View::Playlists(playlists)
                };
                changed |= close;
            }
            View::Queue => {
                changed = match key.code {
                    KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                        let changed = self.queue.navigate(key.code, page_size);
                        if changed {
                            self.queue_anchor = None;
                        }
                        changed
                    }
                    KeyCode::Enter => {
                        command = self
                            .queue
                            .selected()
                            .map(|entry| Command::PlayQueue { queue_id: entry.id });
                        false
                    }
                    KeyCode::Char('d') | KeyCode::Delete => {
                        command = self
                            .queue
                            .selected()
                            .map(|entry| Command::RemoveQueue { queue_id: entry.id });
                        false
                    }
                    _ => return key_action(key, state),
                };
            }
        }
        if changed {
            self.mark_local_change();
        }
        command.map_or(KeyAction::Ignored, KeyAction::Command)
    }
}
struct PagedList<T> {
    pages: HashMap<usize, Vec<T>>,
    total: usize,
    loaded: bool,
    fetch_size: usize,
    selection: ListState,
    viewport_rows: usize,
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
        }
    }
}

impl<T> PagedList<T> {
    fn clear(&mut self) {
        self.pages.clear();
        self.total = 0;
        self.loaded = false;
        self.fetch_size = 0;
    }

    fn next_offset(&self, fetch_size: usize) -> Option<usize> {
        let fetch_size = fetch_size.max(1);
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
        // One neighboring batch on either side hides boundaries during ordinary scrolling.
        let next = last.saturating_add(fetch_size);
        if next < self.total && missing(next) {
            return Some(next);
        }
        first
            .checked_sub(fetch_size)
            .filter(|offset| missing(*offset))
    }

    fn visible_end(&self) -> usize {
        self.selection
            .offset()
            .saturating_add(self.viewport_rows)
            .min(self.total)
    }

    fn set_viewport(&mut self, rows: usize) {
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
        self.trim_pages();
    }

    fn trim_pages(&mut self) {
        let fetch_size = self.fetch_size.max(1);
        let first = (self.selection.offset() / fetch_size * fetch_size).saturating_sub(fetch_size);
        let last = self.visible_end().saturating_sub(1) / fetch_size * fetch_size;
        let last = last.saturating_add(fetch_size);
        // At most ceil(viewport_rows / fetch_size) + 3 batches, independent of list length.
        self.pages
            .retain(|offset, _| (first..=last).contains(offset));
    }

    fn navigate(&mut self, key: KeyCode, page_size: usize) -> bool {
        if !self.loaded {
            return false;
        }
        let changed = match key {
            KeyCode::Up | KeyCode::Down => {
                move_selection(&mut self.selection, self.total, key == KeyCode::Down)
            }
            KeyCode::PageUp | KeyCode::PageDown => move_selection_page(
                &mut self.selection,
                self.total,
                page_size,
                key == KeyCode::PageDown,
            ),
            _ => false,
        };
        self.set_viewport(self.viewport_rows);
        changed
    }

    fn insert(&mut self, offset: usize, fetch_size: usize, total: usize, items: Vec<T>) {
        if self.fetch_size != fetch_size {
            self.clear();
        }
        self.fetch_size = fetch_size;
        self.pages.insert(offset, items);
        self.total = total;
        self.loaded = true;
        let selected = self.selection.selected().unwrap_or(0);
        self.selection
            .select((total != 0).then(|| selected.min(total - 1)));
        self.set_viewport(self.viewport_rows);
    }

    fn get(&self, index: usize) -> Option<&T> {
        let fetch_size = self.fetch_size.max(1);
        let offset = index / fetch_size * fetch_size;
        self.pages.get(&offset)?.get(index - offset)
    }

    fn selected(&self) -> Option<&T> {
        self.get(self.selection.selected()?)
    }
}

struct PlaylistDetail {
    playlist_id: i64,
    name: String,
    entries: PagedList<PlaylistEntryRow>,
    entry_anchor: Option<i64>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum SearchFocus {
    #[default]
    Query,
    Results,
}

#[derive(Default)]
struct LibrarySearch {
    query: String,
    results: PagedList<LibraryRow>,
    focus: SearchFocus,
    query_deadline: Option<Instant>,
}

impl LibrarySearch {
    fn filter(&mut self, now: Instant) {
        self.results = PagedList::default();
        self.query_deadline = Some(now + crate::model::SEARCH_DEBOUNCE);
    }

    fn ready(&self, now: Instant) -> bool {
        self.query_deadline.is_none_or(|deadline| now >= deadline)
    }
}

/// Only the active directory's bounded pages are retained. Ancestors store
/// navigation coordinates, not directory contents.
#[derive(Default)]
struct LibraryTree {
    directory: PathBuf,
    roots: Vec<PathBuf>,
    ancestors: Vec<(PathBuf, ListState, usize)>,
    entries: PagedList<DirectoryRow>,
}

impl LibraryTree {
    fn roots(configured: &[PathBuf]) -> Vec<PathBuf> {
        let mut roots: Vec<_> = configured
            .iter()
            .filter(|path| !path.as_os_str().is_empty())
            .cloned()
            .collect();
        roots.sort();
        roots.dedup();
        roots
    }

    fn new(configured: &[PathBuf]) -> Self {
        Self {
            roots: Self::roots(configured),
            ..Self::default()
        }
    }

    fn activate(&mut self) -> Option<Command> {
        match self.entries.selected()? {
            DirectoryRow::Directory { path } => {
                let path = path.clone();
                self.ancestors.push((
                    std::mem::replace(&mut self.directory, path),
                    std::mem::take(&mut self.entries.selection),
                    self.entries.viewport_rows,
                ));
                self.entries = PagedList::default();
                None
            }
            DirectoryRow::Track(track) => Some(Command::Enqueue {
                track_ids: vec![track.id],
            }),
        }
    }

    fn parent(&mut self) -> bool {
        let Some((directory, selection, viewport_rows)) = self.ancestors.pop() else {
            return false;
        };
        self.directory = directory;
        self.entries = PagedList {
            selection,
            viewport_rows,
            ..PagedList::default()
        };
        true
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

#[expect(
    clippy::large_enum_variant,
    reason = "Key actions are consumed immediately; boxing commands would allocate per command key"
)]
enum KeyAction {
    Quit,
    Search,
    Tree,
    Command(Command),
    Ignored,
}

fn key_action(key: KeyEvent, state: &TuiSnapshot) -> KeyAction {
    if key.code == KeyCode::Char('q') {
        return KeyAction::Quit;
    }

    let mut encoded = [0_u8; 4];
    let key_name = match key.code {
        KeyCode::Char(' ') => "space",
        KeyCode::Char(character) => {
            let bytes = character.encode_utf8(&mut encoded).as_bytes();
            std::str::from_utf8(bytes).expect("char encoding is valid UTF-8")
        }
        KeyCode::Left => "left",
        KeyCode::Right => "right",
        KeyCode::Home => "home",
        KeyCode::End => "end",
        _ => return KeyAction::Ignored,
    };
    playback_key_command(key_name, &state.playback).map_or(KeyAction::Ignored, KeyAction::Command)
}

fn draw_loading(frame: &mut ratatui::Frame<'_>) {
    frame.render_widget(
        Paragraph::new("Connecting to core…\nq to quit")
            .block(Block::default().title("Rivu").borders(Borders::ALL)),
        frame.area(),
    );
}

fn draw(frame: &mut ratatui::Frame<'_>, state: &TuiSnapshot, ui: &mut UiState) {
    let area = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(area);
    let now = state
        .current_track()
        .map(|t| format!("{} — {}", t.artist, t.title))
        .unwrap_or_else(|| "Nothing playing".into());
    let (status, status_icon) = match state.playback.status {
        PlaybackStatus::Playing => ("Playing", "󰐊"),
        PlaybackStatus::Paused => ("Paused", "󰏤"),
        PlaybackStatus::Stopped => ("Stopped", "󰓛"),
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " RIVU ",
                Style::default()
                    .fg(Color::Rgb(122, 162, 247))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{status_icon} {status}  {now}")),
        ]))
        .block(Block::default().borders(Borders::BOTTOM)),
        outer[0],
    );
    let columns = if ui.show_help {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
            .split(outer[1])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100)])
            .split(outer[1])
    };
    if matches!(&ui.view, View::Search(_)) {
        ui.page_size = columns[0].height.saturating_sub(5).max(1) as usize;
    } else {
        ui.page_size = columns[0].height.saturating_sub(2).max(1) as usize;
    }
    match &mut ui.view {
        View::Search(search) => draw_search(frame, columns[0], search),
        View::Tree(tree) => draw_tree(frame, columns[0], tree),
        View::PlaylistDetail { detail, .. } => draw_playlist_detail(frame, columns[0], detail),
        View::Playlists(playlists) => draw_playlists(frame, columns[0], playlists),
        View::Queue => draw_queue(frame, columns[0], state, &mut ui.queue),
    }
    let position = state.playback.position.max(0.0);
    let duration = state.playback.duration.unwrap_or(0.0).max(position + 0.001);
    let ratio = (position / duration).clamp(0.0, 1.0);
    frame.render_widget(
        Gauge::default()
            .block(
                Block::default()
                    .title(format!(
                        " {} / {} ",
                        fmt_time(position),
                        fmt_time(state.playback.duration.unwrap_or(0.0))
                    ))
                    .borders(Borders::ALL),
            )
            .gauge_style(Style::default().fg(Color::Rgb(122, 162, 247)))
            .ratio(ratio),
        outer[2],
    );
    let keys = match &ui.view {
        View::Search(_) => {
            "Type    search library\nTab     query/results focus\n↑/↓     select result\nPgUp/Dn scroll screen\nR       retry loading\nBackspace edit query\nEnter   focus results\nEsc     return to queue\n\nSearch matches title, artist, album and path."
        }
        View::Tree(_) => {
            "↑/↓     select entry\nPgUp/Dn scroll screen\nR       retry loading\nEnter   open directory/add track\nBackspace/← parent directory\nEsc     return to queue\n/       search library\nP       playlists\nSpace   play/pause\nn/p       next/previous\n→       seek forward 5 sec\nHome/End seek start/end\nr       repeat mode\ns       shuffle\n[/]     volume\nq       quit"
        }
        View::PlaylistDetail { .. } => {
            "↑/↓     select track\nPgUp/Dn scroll screen\nR       retry loading\nEnter   play track\na       add track to queue\n←/Esc   back to playlists\nSpace   play/pause\nn/p       next/previous\nHome/End seek start/end\nr       repeat mode\ns       shuffle\n[/]     volume\nq       quit"
        }
        View::Playlists(_) => {
            "↑/↓     select playlist\nPgUp/Dn scroll screen\nR       retry loading\n→       open playlist\nEnter   play playlist\nEsc     return to queue\nP       playlists\n/       search library\nt       browse library tree\nSpace   play/pause\nn/p       next/previous\nHome/End seek start/end\nr       repeat mode\ns       shuffle\n[/]     volume\nq       quit"
        }
        View::Queue => {
            "↑/↓     select queue\nPgUp/Dn scroll screen\nR       retry loading\nEnter   play selected\nd/Del   remove selected\n/       search library\nt       browse library tree\nP       playlists\nSpace   play/pause\n←/→     seek 5 sec\nHome/End seek start/end\nr       repeat mode\ns       shuffle\n[/]     volume\nq       quit"
        }
    };
    if ui.show_help {
        let help = Paragraph::new(keys)
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Keys").borders(Borders::ALL));
        frame.render_widget(help, columns[1]);
    }
    let status = format!(
        "󰕾 {:>3}% 󰒝 {} 󰑖 {} 󰘦 {}",
        (state.playback.volume * 100.0).round() as u8,
        if state.playback.shuffle { "on" } else { "off" },
        repeat_label(state.playback.repeat),
        state.queue.total
    );
    let status_columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(outer[3]);
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::Rgb(86, 95, 137))),
        status_columns[0],
    );
    let prompt = ui.message.as_ref().map_or(
        if ui.show_help {
            "?  close help"
        } else {
            "?  help"
        },
        |notice| notice.text.as_str(),
    );
    frame.render_widget(
        Paragraph::new(prompt)
            .alignment(Alignment::Right)
            .style(Style::default().fg(Color::Yellow)),
        status_columns[1],
    );
}

fn draw_queue(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &TuiSnapshot,
    queue: &mut PagedList<QueueRow>,
) {
    queue.set_viewport(area.height.saturating_sub(2) as usize);
    let loading =
        (queue.selection.offset()..queue.visible_end()).any(|index| queue.get(index).is_none());
    let title = if state.queue.total == 0 {
        "Queue — empty; / to search, t to browse".into()
    } else {
        format!(
            "Queue — {}/{}{}",
            queue.selection.selected().unwrap_or(0) + 1,
            state.queue.total,
            if loading { " — loading" } else { "" },
        )
    };
    let items = (queue.selection.offset()..queue.visible_end()).map(|index| {
        ListItem::new(queue.get(index).map_or_else(
            || "…".into(),
            |row| {
                let marker = if state.queue.current_id == Some(row.id) {
                    "▶ "
                } else {
                    "  "
                };
                format!("{marker}{} — {}", row.track.artist, row.track.title)
            },
        ))
    });
    let mut selection = ListState::default().with_selected(
        queue
            .selection
            .selected()
            .map(|index| index - queue.selection.offset()),
    );
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().title(title).borders(Borders::ALL))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> "),
        area,
        &mut selection,
    );
}
fn draw_paged_list<T>(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: String,
    page: &mut PagedList<T>,
    row: impl Fn(&T) -> String,
) {
    page.set_viewport(area.height.saturating_sub(2) as usize);
    let loading = !page.loaded
        || (page.selection.offset()..page.visible_end()).any(|index| page.get(index).is_none());
    let title = if loading {
        format!("{title} — loading")
    } else if page.total == 0 {
        format!("{title} — empty")
    } else {
        format!(
            "{title} — {}/{}",
            page.selection.selected().unwrap_or(0) + 1,
            page.total
        )
    };
    let items = (page.selection.offset()..page.visible_end())
        .map(|index| ListItem::new(page.get(index).map_or_else(|| "…".into(), &row)));
    let mut selection = ListState::default().with_selected(
        page.selection
            .selected()
            .map(|index| index - page.selection.offset()),
    );
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().title(title).borders(Borders::ALL))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> "),
        area,
        &mut selection,
    );
}

fn draw_playlist_detail(frame: &mut ratatui::Frame<'_>, area: Rect, detail: &mut PlaylistDetail) {
    let marker = "󰎆 ";
    draw_paged_list(
        frame,
        area,
        format!("Playlist — {}", detail.name),
        &mut detail.entries,
        |entry| {
            format!(
                "{marker}{} — {}{}",
                entry.artist,
                entry.title,
                if entry.missing { " [missing]" } else { "" }
            )
        },
    );
}

fn draw_playlists(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    playlists: &mut PagedList<PlaylistSummary>,
) {
    let marker = "󰲋 ";
    draw_paged_list(frame, area, "Playlists".into(), playlists, |playlist| {
        format!(
            "{marker}{}  ({} tracks)",
            playlist.name, playlist.entry_count
        )
    });
}

fn draw_search(frame: &mut ratatui::Frame<'_>, area: Rect, search: &mut LibrarySearch) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);
    frame.render_widget(
        Paragraph::new(search.query.as_str()).block(
            Block::default()
                .title(if search.focus == SearchFocus::Query {
                    "Search query — Tab to results"
                } else {
                    "Search query"
                })
                .borders(Borders::ALL),
        ),
        rows[0],
    );
    let title = if search.focus == SearchFocus::Results {
        "Library".into()
    } else {
        "Library — Tab to focus".into()
    };
    draw_paged_list(frame, rows[1], title, &mut search.results, |track| {
        format!(
            "{} — {} [{}]{}",
            track.artist,
            track.title,
            track.album,
            if track.missing { " [missing]" } else { "" }
        )
    });
}

fn draw_tree(frame: &mut ratatui::Frame<'_>, area: Rect, tree: &mut LibraryTree) {
    let title = if tree.directory.as_os_str().is_empty() {
        "Library tree".into()
    } else {
        format!("Library tree — {}", tree.directory.display())
    };
    draw_paged_list(frame, area, title, &mut tree.entries, |entry| match entry {
        DirectoryRow::Directory { path } => {
            let marker = "\u{f07b} ";
            let name = path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy();
            format!(
                "{marker}{name}{}",
                if name.ends_with('/') { "" } else { "/" }
            )
        }
        DirectoryRow::Track(track) => {
            let marker = "\u{f001} ";
            format!(
                "{marker}{} — {}{}",
                track.artist,
                track.title,
                if track.missing { " [missing]" } else { "" }
            )
        }
    });
}

fn repeat_label(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::Off => "off",
        RepeatMode::All => "all",
        RepeatMode::One => "one",
    }
}
fn fmt_time(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "0:00".into();
    }
    format!("{}:{:02}", (seconds as u64) / 60, (seconds as u64) % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::Page, response::QueryRevisions};
    use ratatui::backend::TestBackend;

    #[test]
    fn watcher_shutdown_closes_stalled_initial_handshake() {
        use std::{io::Read, os::unix::net::UnixListener};

        let directory = tempfile::tempdir().unwrap();
        let listener =
            UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
        let watcher = spawn_watcher(directory.path());
        let mut stream = listener.accept().unwrap().0;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut prefix = [0; 4];
        stream.read_exact(&mut prefix).unwrap();
        let length = u32::from_le_bytes(prefix) as usize;
        assert!(length <= 4096);
        stream.read_exact(&mut vec![0; length]).unwrap();
        // Hello has reached the peer, which deliberately never replies. The
        // loading screen's exit must join the watcher without its 120s timeout.
        let (finished, completion) = bounded(1);
        let shutdown = std::thread::spawn(move || {
            drop(watcher);
            finished.send(()).unwrap();
        });
        completion.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        shutdown.join().unwrap();
    }

    #[test]
    fn watcher_reports_initial_connection_failure_to_loading_consumer() {
        let directory = tempfile::tempdir().unwrap();
        let watcher = spawn_watcher(directory.path());
        watcher
            .receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(matches!(watcher.latest.lock().take(), Some(Err(_))));
    }

    #[test]
    fn queue_pages_keep_global_navigation_and_duplicate_occurrence_actions() {
        let mut state = TuiSnapshot::default();
        state.queue.total = 200_000;
        state.system.page_size = 3;
        let mut tui = TuiState::new(state.clone());
        tui.ui.page_size = 4;
        tui.ui.queue.selection.select(Some(50_000));
        *tui.ui.queue.selection.offset_mut() = 49_999;
        tui.ui.queue.set_viewport(4);
        load_queue(&mut tui.ui, &state);
        assert_eq!(tui.ui.queue.selection.selected(), Some(50_000));
        assert_eq!(tui.ui.queue.selection.offset(), 49_999);
        assert!(matches!(
            tui.ui.key_action(KeyEvent::from(KeyCode::Enter), &state),
            KeyAction::Command(Command::PlayQueue { queue_id: 51_000 })
        ));
        assert!(matches!(
            tui.ui.key_action(KeyEvent::from(KeyCode::Delete), &state),
            KeyAction::Command(Command::RemoveQueue { queue_id: 51_000 })
        ));
        // Every row refers to track 7, but the playing marker follows its occurrence.
        state.queue.current_id = Some(51_000);
        state.playback.position = 42.;
        tui.apply_snapshot(state.clone());
        assert!(tui.ui.query_request(&state).is_none());
        let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
        terminal
            .draw(|frame| draw_queue(frame, frame.area(), &state, &mut tui.ui.queue))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert_eq!(text.matches("▶ artist — row-7").count(), 1);
        tui.ui.key_action(KeyEvent::from(KeyCode::PageDown), &state);
        assert_eq!(tui.ui.queue.selection.selected(), Some(50_004));
        tui.ui.key_action(KeyEvent::from(KeyCode::PageUp), &state);
        tui.ui.key_action(KeyEvent::from(KeyCode::Up), &state);
        tui.ui.key_action(KeyEvent::from(KeyCode::Down), &state);
        assert_eq!(tui.ui.queue.selection.selected(), Some(50_000));
        tui.ui.queue.selection.select(Some(199_999));
        tui.ui.queue.set_viewport(4);
        load_queue(&mut tui.ui, &state);
        assert_eq!(tui.ui.queue.selected().unwrap().position, 199_999);
        assert!(tui.ui.queue.pages.len() <= 4usize.div_ceil(3) + 3);
        assert!(tui.ui.queue.pages.values().map(Vec::len).sum::<usize>() <= 15);
    }

    #[test]
    fn queue_mutations_relocate_selected_occurrence_and_clamp_deleted_selection() {
        let mut state = TuiSnapshot::default();
        state.queue.total = 12;
        state.system.page_size = 3;
        let mut tui = TuiState::new(state.clone());
        tui.ui.queue.selection.select(Some(8));
        load_queue(&mut tui.ui, &state);
        state.queue.total = 11;
        state.queue.revision = 1;
        tui.apply_snapshot(state.clone());
        assert!(tui.ui.queue.pages.is_empty());
        let request = tui.ui.query_request(&state).unwrap();
        assert_eq!(request.kind, QueryKind::QueueSelection(1008));
        assert!(
            matches!(request.query(), Query::QueueEntries { queue_ids } if queue_ids == vec![1008])
        );
        let relocated = QueryResult {
            revisions: QueryRevisions {
                queue: 1,
                ..QueryRevisions::default()
            },
            view: ViewResponse::QueueEntries(vec![queue_row(2, 1008)]),
        };
        assert!(tui.ui.apply_query(&state, &request, Ok(relocated)));
        assert_eq!(tui.ui.queue.selection.selected(), Some(2));
        let request = tui.ui.query_request(&state).unwrap();
        assert_eq!(request.offset, 0);
        let mut page = response(&request, state.queue.total);
        let ViewResponse::QueuePage(rows) = &mut page.view else {
            unreachable!()
        };
        rows.rows[2] = queue_row(2, 1008);
        tui.ui.apply_query(&state, &request, Ok(page));
        state.queue.total = 2;
        state.queue.revision = 2;
        tui.apply_snapshot(state.clone());
        let request = tui.ui.query_request(&state).unwrap();
        assert_eq!(request.kind, QueryKind::QueueSelection(1008));
        tui.ui.apply_query(
            &state,
            &request,
            Ok(QueryResult {
                revisions: QueryRevisions {
                    queue: 2,
                    ..QueryRevisions::default()
                },
                view: ViewResponse::QueueEntries(Vec::new()),
            }),
        );
        assert_eq!(tui.ui.queue.selection.selected(), Some(1));
        load_queue(&mut tui.ui, &state);
        state.queue.total = 0;
        state.queue.revision = 3;
        tui.apply_snapshot(state.clone());
        assert_eq!(tui.ui.queue.selection.selected(), None);
        assert!(tui.ui.query_request(&state).is_none());
        assert!(matches!(
            tui.ui.key_action(KeyEvent::from(KeyCode::Enter), &state),
            KeyAction::Ignored
        ));
        let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
        terminal
            .draw(|frame| draw_queue(frame, frame.area(), &state, &mut tui.ui.queue))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Queue — empty; / to search, t to browse"));
    }

    #[test]
    fn queue_rejects_delayed_pages_and_selection_lookups_after_navigation() {
        let mut state = TuiSnapshot::default();
        state.queue.total = 30;
        state.system.page_size = 3;
        let mut tui = TuiState::new(state.clone());
        let delayed = tui.ui.query_request(&state).unwrap();
        assert!(matches!(
            delayed.query(),
            Query::QueuePage {
                offset: 0,
                limit: 3
            }
        ));
        tui.ui.page_size = 8;
        tui.ui.key_action(KeyEvent::from(KeyCode::PageDown), &state);
        assert!(
            !tui.ui
                .apply_query(&state, &delayed, Ok(response(&delayed, 30)))
        );
        load_queue(&mut tui.ui, &state);
        state.queue.revision = 1;
        tui.apply_snapshot(state.clone());
        let relocation = tui.ui.query_request(&state).unwrap();
        tui.ui.key_action(KeyEvent::from(KeyCode::Down), &state);
        assert!(!tui.ui.apply_query(
            &state,
            &relocation,
            Ok(QueryResult {
                revisions: QueryRevisions {
                    queue: 1,
                    ..QueryRevisions::default()
                },
                view: ViewResponse::QueueEntries(vec![queue_row(0, 1008)]),
            })
        ));
        assert_eq!(tui.ui.queue.selection.selected(), Some(9));
        let request = tui.ui.query_request(&state).unwrap();
        let mut stale = response(&request, 30);
        stale.revisions.queue = 0;
        tui.ui.requested = Some(request.clone());
        assert!(!tui.ui.apply_query(&state, &request, Ok(stale)));
        assert!(tui.ui.queue.pages.is_empty());
        assert!(tui.ui.requested.is_none());
        assert!(
            tui.ui
                .apply_query(&state, &request, Err("queue unavailable".into()))
        );
        assert!(tui.ui.query_request(&state).is_none());
        tui.ui
            .key_action(KeyEvent::from(KeyCode::Char('R')), &state);
        assert_eq!(tui.ui.query_request(&state), Some(request));
        load_queue(&mut tui.ui, &state);
    }

    fn queue_row(position: usize, id: u64) -> QueueRow {
        QueueRow {
            id,
            track_id: 7,
            position,
            track: track(7),
        }
    }

    fn load_queue(ui: &mut UiState, state: &TuiSnapshot) {
        for _ in 0..16 {
            let Some(request) = ui.query_request(state) else {
                return;
            };
            assert!(ui.apply_query(state, &request, Ok(response(&request, state.queue.total))));
        }
        panic!("queue viewport requests did not converge");
    }

    #[test]
    fn empty_queue_addition_updates_count_message_and_waits_for_occurrence_rows() {
        let mut state = TuiSnapshot::default();
        let mut tui = TuiState::new(state.clone());
        assert!(tui.ui.query_request(&state).is_none());
        state.queue.total = 2;
        state.queue.revision = 1;
        tui.apply_snapshot(state.clone());
        assert_eq!(tui.ui.queue.selection.selected(), Some(0));
        assert_eq!(tui.ui.message.as_ref().unwrap().text, "Added to queue");
        assert!(matches!(
            tui.ui.key_action(KeyEvent::from(KeyCode::Delete), &state),
            KeyAction::Ignored
        ));
        load_queue(&mut tui.ui, &state);
        assert!(matches!(
            tui.ui.key_action(KeyEvent::from(KeyCode::Delete), &state),
            KeyAction::Command(Command::RemoveQueue { queue_id: 1000 })
        ));
        state.queue.total = 0;
        state.queue.revision = 2;
        tui.apply_snapshot(state);
        assert_eq!(tui.ui.message.as_ref().unwrap().text, "Removed from queue");
        assert!(tui.ui.queue.pages.is_empty());
    }

    fn track(index: usize) -> LibraryRow {
        LibraryRow {
            id: index as i64,
            title: format!("row-{index}"),
            artist: "artist".into(),
            album: "album".into(),
            duration: None,
            codec: "flac".into(),
            bitrate_bps: None,
            favorite: false,
            missing: false,
            play_count: 0,
        }
    }

    fn response(request: &QueryRequest, total: usize) -> QueryResult {
        let indices = request.offset..(request.offset + request.limit).min(total);
        let view = match &request.kind {
            QueryKind::Queue => ViewResponse::QueuePage(Page {
                total,
                rows: indices
                    .map(|position| queue_row(position, position as u64 + 1000))
                    .collect(),
            }),
            QueryKind::QueueSelection(_) => {
                panic!("selection lookups require an explicit position")
            }
            QueryKind::Search(_) => ViewResponse::LibraryPage(Page {
                total,
                rows: indices.map(track).collect(),
            }),
            QueryKind::Directory(_) => ViewResponse::DirectoryPage(Page {
                total,
                rows: indices
                    .map(|index| DirectoryRow::Track(track(index)))
                    .collect(),
            }),
            QueryKind::Playlists => ViewResponse::PlaylistSummaries(Page {
                total,
                rows: indices
                    .map(|index| PlaylistSummary {
                        id: index as i64,
                        name: format!("row-{index}"),
                        entry_count: 12,
                    })
                    .collect(),
            }),
            QueryKind::Playlist(_) => ViewResponse::PlaylistEntries(Page {
                total,
                rows: indices
                    .map(|index| PlaylistEntryRow {
                        id: index as i64,
                        track_id: index as i64,
                        title: format!("row-{index}"),
                        artist: "artist".into(),
                        album: "album".into(),
                        favorite: false,
                        missing: false,
                    })
                    .collect(),
            }),
            QueryKind::PlaylistSelection {
                playlist_id,
                entry_id,
            } => ViewResponse::PlaylistEntry(Some(crate::model::PlaylistEntryLocation {
                id: *entry_id,
                playlist_id: *playlist_id,
                position: *entry_id as usize,
                track_id: *entry_id,
                total,
            })),
        };
        QueryResult {
            revisions: QueryRevisions {
                library: request.library_revision,
                structure: request.structure_revision,
                playlist: request.playlist_revision,
                playlist_entries: request.playlist_entry_revision,
                queue: request.queue_revision,
            },
            view,
        }
    }

    fn load_visible(ui: &mut UiState, state: &TuiSnapshot) {
        ui.prepare_local_view(state);
        for _ in 0..16 {
            let Some(request) = ui.query_request(state) else {
                return;
            };
            assert!(ui.apply_query(state, &request, Ok(response(&request, 12))));
        }
        panic!("visible requests did not converge");
    }

    fn render_view(ui: &mut UiState) -> String {
        let height = if matches!(ui.view, View::Search(_)) {
            9
        } else {
            6
        };
        let mut terminal = Terminal::new(TestBackend::new(60, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                match &mut ui.view {
                    View::Search(search) => draw_search(frame, area, search),
                    View::Tree(tree) => draw_tree(frame, area, tree),
                    View::Playlists(playlists) => draw_playlists(frame, area, playlists),
                    View::PlaylistDetail { detail, .. } => {
                        draw_playlist_detail(frame, area, detail)
                    }
                    View::Queue => panic!("expected a queried view"),
                }
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn playlist_reorder_preserves_unrelated_terminal_views() {
        let mut snapshot = TuiSnapshot::default();
        snapshot.system.page_size = 3;
        for view in [
            View::Search(LibrarySearch::default()),
            View::Tree(LibraryTree {
                directory: "/music".into(),
                ..LibraryTree::default()
            }),
            View::Playlists(PagedList::default()),
            View::PlaylistDetail {
                playlists: (ListState::default(), 1),
                detail: PlaylistDetail {
                    playlist_id: 2,
                    name: "other".into(),
                    entries: PagedList::default(),
                    entry_anchor: None,
                },
            },
        ] {
            let mut tui = TuiState::new(snapshot.clone());
            tui.ui.view = view;
            load_visible(&mut tui.ui, &snapshot);
            let revision = tui.ui.query_revision;
            let mut next = snapshot.clone();
            Arc::make_mut(&mut next.library.playlist_entry_revisions).insert(1, 1);
            tui.apply_snapshot(next);
            assert_eq!(tui.ui.query_revision, revision);
            assert!(tui.ui.query_request(&tui.snapshot).is_none());
        }
    }

    #[test]
    fn terminal_playlist_reorder_relocates_identity_and_rejects_stale_results() {
        let mut snapshot = TuiSnapshot::default();
        snapshot.system.page_size = 1;
        let mut tui = TuiState::new(snapshot);
        tui.ui.view = View::PlaylistDetail {
            playlists: (ListState::default(), 1),
            detail: PlaylistDetail {
                playlist_id: 1,
                name: "one".into(),
                entries: PagedList::default(),
                entry_anchor: None,
            },
        };
        let first = tui.ui.query_request(&tui.snapshot).unwrap();
        assert!(
            tui.ui
                .apply_query(&tui.snapshot, &first, Ok(response(&first, 12)))
        );
        let pending = tui.ui.query_request(&tui.snapshot).unwrap();
        let old_page = response(&pending, 12);
        let mut next = tui.snapshot.clone();
        Arc::make_mut(&mut next.library.playlist_entry_revisions).insert(1, 1);
        tui.apply_snapshot(next);
        assert!(!tui.ui.apply_query(&tui.snapshot, &pending, Ok(old_page)));
        let lookup = tui.ui.query_request(&tui.snapshot).unwrap();
        assert_eq!(
            lookup.kind,
            QueryKind::PlaylistSelection {
                playlist_id: 1,
                entry_id: 0
            }
        );
        let old_lookup = response(&lookup, 12);
        let mut next = tui.snapshot.clone();
        Arc::make_mut(&mut next.library.playlist_entry_revisions).insert(1, 2);
        tui.apply_snapshot(next);
        assert!(!tui.ui.apply_query(&tui.snapshot, &lookup, Ok(old_lookup)));
        let lookup = tui.ui.query_request(&tui.snapshot).unwrap();
        let mut located = response(&lookup, 12);
        let ViewResponse::PlaylistEntry(Some(entry)) = &mut located.view else {
            panic!("expected entry location");
        };
        entry.position = 9;
        assert!(tui.ui.apply_query(&tui.snapshot, &lookup, Ok(located)));
        let page = tui.ui.query_request(&tui.snapshot).unwrap();
        assert_eq!(page.kind, QueryKind::Playlist(1));
        assert_eq!(page.offset, 9);
        let mut relocated = response(&page, 12);
        let ViewResponse::PlaylistEntries(entries) = &mut relocated.view else {
            panic!("expected entries");
        };
        entries.rows[0].id = 0;
        entries.rows[0].track_id = 0;
        assert!(tui.ui.apply_query(&tui.snapshot, &page, Ok(relocated)));
        assert!(matches!(
            tui.ui
                .key_action(KeyEvent::from(KeyCode::Enter), &tui.snapshot),
            KeyAction::Command(Command::Play { track_id: 0 })
        ));
    }

    #[test]
    fn every_queried_view_renders_across_batches_without_resetting_scroll() {
        let mut state = TuiSnapshot::default();
        state.system.page_size = 3;
        let views = [
            View::Search(LibrarySearch {
                focus: SearchFocus::Results,
                ..LibrarySearch::default()
            }),
            View::Tree(LibraryTree {
                directory: "/music".into(),
                ..LibraryTree::default()
            }),
            View::Playlists(PagedList::default()),
            View::PlaylistDetail {
                playlists: (ListState::default(), 1),
                detail: PlaylistDetail {
                    playlist_id: 7,
                    name: "collection".into(),
                    entries: PagedList::default(),
                    entry_anchor: None,
                },
            },
        ];
        for view in views {
            let mut ui = UiState {
                view,
                page_size: 4,
                ..UiState::default()
            };
            render_view(&mut ui);
            load_visible(&mut ui, &state);
            for _ in 0..3 {
                ui.key_action(KeyEvent::from(KeyCode::Down), &state);
            }
            let rendered = render_view(&mut ui);
            assert!(rendered.contains("row-0"), "{rendered}");
            assert!(rendered.contains("row-3"), "{rendered}");
            let selected = ui.key_action(KeyEvent::from(KeyCode::Enter), &state);
            match selected {
                KeyAction::Command(Command::Enqueue { track_ids }) => {
                    assert_eq!(track_ids, vec![3])
                }
                KeyAction::Command(Command::PlayPlaylist { playlist_id: 3 })
                | KeyAction::Command(Command::Play { track_id: 3 }) => {}
                _ => panic!("selection did not activate the global row"),
            }
            ui.key_action(KeyEvent::from(KeyCode::Down), &state);
            load_visible(&mut ui, &state);
            let rendered = render_view(&mut ui);
            assert!(!rendered.contains("row-0"), "{rendered}");
            assert!(rendered.contains("row-1"), "{rendered}");
            assert!(rendered.contains("row-4"), "{rendered}");
        }
    }

    #[test]
    fn viewport_navigation_and_retention_do_not_depend_on_batch_size() {
        for fetch_size in [1, 3, 32] {
            let mut list = PagedList::default();
            list.set_viewport(10);
            for down in [true, false] {
                for _ in 0..200 {
                    let mut requests = 0;
                    while let Some(offset) = list.next_offset(fetch_size) {
                        let rows = (offset..(offset + fetch_size).min(10_000)).collect();
                        list.insert(offset, fetch_size, 10_000, rows);
                        requests += 1;
                        assert!(requests <= 10usize.div_ceil(fetch_size) + 4);
                    }
                    assert_eq!(list.total, 10_000);
                    assert!(list.pages.len() <= 10usize.div_ceil(fetch_size) + 3);
                    assert!(
                        (list.selection.offset()..list.visible_end())
                            .all(|index| list.get(index) == Some(&index))
                    );
                    let before = list.selection.selected().unwrap();
                    list.navigate(
                        if down {
                            KeyCode::PageDown
                        } else {
                            KeyCode::PageUp
                        },
                        10,
                    );
                    assert_eq!(
                        list.selection.selected(),
                        Some(if down {
                            before + 10
                        } else {
                            before.saturating_sub(10)
                        })
                    );
                }
            }
        }
    }

    #[test]
    fn tree_top_level_contains_only_configured_roots_and_restores_parent() {
        let mut state = TuiSnapshot::default();
        state.system.page_size = 1;
        for roots in [
            vec![],
            vec![PathBuf::from("/music")],
            vec![
                "/first".into(),
                "/second".into(),
                "/third".into(),
                "/fourth".into(),
                "/fifth".into(),
            ],
            vec!["/".into()],
        ] {
            let mut ui = UiState {
                view: View::Tree(LibraryTree::new(&roots)),
                page_size: 4,
                ..UiState::default()
            };
            render_view(&mut ui);
            load_visible(&mut ui, &state);
            let View::Tree(tree) = &ui.view else {
                unreachable!()
            };
            assert_eq!(tree.entries.total, roots.len());
            assert!(tree.entries.loaded);
            assert!(ui.query_request(&state).is_none());
            if roots.is_empty() {
                assert!(tree.entries.selection.selected().is_none());
                continue;
            }
            let mut expected = roots.clone();
            expected.sort();
            for path in expected {
                load_visible(&mut ui, &state);
                let View::Tree(tree) = &ui.view else {
                    unreachable!()
                };
                assert_eq!(
                    tree.entries.selected(),
                    Some(&DirectoryRow::Directory { path: path.clone() })
                );
                ui.key_action(KeyEvent::from(KeyCode::Enter), &state);
                let request = ui.query_request(&state).unwrap();
                assert_eq!(request.kind, QueryKind::Directory(path));
                ui.key_action(KeyEvent::from(KeyCode::Left), &state);
                load_visible(&mut ui, &state);
                ui.key_action(KeyEvent::from(KeyCode::Down), &state);
            }
        }
    }

    #[test]
    fn failed_load_retries_and_stale_revisions_do_not_stall_navigation() {
        let mut state = TuiSnapshot::default();
        state.system.page_size = 3;
        let mut ui = UiState {
            view: View::Playlists(PagedList::default()),
            ..UiState::default()
        };
        let request = ui.query_request(&state).unwrap();
        ui.requested = Some(request.clone());
        assert!(ui.apply_query(&state, &request, Err("connection failed".into())));
        assert!(ui.query_request(&state).is_none());
        ui.key_action(KeyEvent::from(KeyCode::Char('R')), &state);
        assert_eq!(ui.query_request(&state), Some(request.clone()));
        let mut stale = response(&request, 12);
        stale.revisions.playlist = 1;
        ui.requested = Some(request.clone());
        assert!(!ui.apply_query(&state, &request, Ok(stale)));
        assert!(ui.requested.is_none());
        load_visible(&mut ui, &state);
    }

    #[test]
    fn replacing_queries_keeps_one_stalled_socket_until_worker_shutdown() {
        use std::{io::Read, os::unix::net::UnixListener};

        let directory = tempfile::tempdir().unwrap();
        let listener =
            UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
        let (connected, accepted) = bounded(1);
        let server = std::thread::spawn(move || {
            connected.send(listener.accept().unwrap().0).unwrap();
        });
        let mut worker = spawn_query_worker(directory.path(), 1);
        let state = TuiSnapshot::default();
        let ui = UiState {
            view: View::Playlists(PagedList::default()),
            ..UiState::default()
        };
        let request = ui.query_request(&state).unwrap();
        worker.request(request.clone());
        let mut stream = accepted.recv_timeout(Duration::from_secs(5)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        for offset in 1..100 {
            worker.request(QueryRequest {
                offset,
                ..request.clone()
            });
        }
        // The peer deliberately stalls Hello. Replacements must not abandon
        // the active connection and start more server work behind it.
        let mut buffer = [0; 4096];
        let retained = loop {
            match stream.read(&mut buffer) {
                Ok(0) => break false,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break true;
                }
                Err(error) => panic!("{error}"),
            }
        };
        drop(worker);
        let closed = stream.read(&mut buffer).unwrap();
        server.join().unwrap();
        assert!(
            retained,
            "replacement abandoned an active server connection"
        );
        assert_eq!(closed, 0, "worker shutdown must close a stalled connection");
    }

    #[test]
    fn replaced_queries_drain_then_send_only_latest_on_the_same_session() {
        use ipc::tests::{complete_test_handshake, read_test_query, reply_test_query};
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let listener =
            UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
        let (started, active) = bounded(1);
        let (release, resume) = bounded(1);
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake(&mut stream);
            assert!(matches!(
                read_test_query(&mut stream),
                Query::QueuePage { offset: 0, .. }
            ));
            started.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(5)).unwrap();
            for offset in [0, 99] {
                if offset != 0 {
                    assert!(matches!(
                        read_test_query(&mut stream),
                        Query::QueuePage { offset: 99, .. }
                    ));
                }
                reply_test_query(
                    &mut stream,
                    crate::response::QueryResponse {
                        revisions: QueryRevisions::default(),
                        result: Ok(ViewResponse::QueuePage(crate::model::Page {
                            total: 100,
                            rows: Vec::new(),
                        })),
                    },
                );
            }
        });
        let mut worker = spawn_query_worker(directory.path(), 1);
        let request = QueryRequest {
            kind: QueryKind::Queue,
            offset: 0,
            limit: 20,
            view_revision: 0,
            library_revision: 0,
            structure_revision: 0,
            playlist_revision: 0,
            playlist_entry_revision: None,
            queue_revision: 0,
        };
        worker.request(request.clone());
        active.recv_timeout(Duration::from_secs(5)).unwrap();
        for offset in 1..100 {
            worker.request(QueryRequest {
                offset,
                ..request.clone()
            });
        }
        assert!(worker.latest.lock().is_none());
        release.send(()).unwrap();
        // Finish the accepted jobs without shutdown cancellation so the final
        // consumer result can be inspected without polling or timing races.
        worker.sender.take();
        worker.worker.take().unwrap().join().unwrap();
        let (request, result) = worker.latest.lock().take().unwrap();
        assert_eq!(request.offset, 99);
        assert!(matches!(result.unwrap().view, ViewResponse::QueuePage(page) if page.total == 100));
        server.join().unwrap();
    }

    #[test]
    fn command_worker_reports_ambiguous_failure_without_replaying_the_mutation() {
        use ipc::tests::{complete_test_handshake, read_test_command, reply_test_ack};
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let listener =
            UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake(&mut stream);
            assert!(
                matches!(read_test_command(&mut stream), Command::CreatePlaylist { name } if name == "first")
            );
            // Mutation execution is ambiguous to the client: no Ack arrives.
            drop(stream);
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake(&mut stream);
            assert!(
                matches!(read_test_command(&mut stream), Command::CreatePlaylist { name } if name == "second")
            );
            reply_test_ack(
                &mut stream,
                Ack {
                    ok: true,
                    error: None,
                    revision: 2,
                    scan_id: None,
                    relocation_id: None,
                },
            );
        });
        let worker = spawn_command_worker(directory.path(), 1);
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(Command::CreatePlaylist {
                name: "first".into(),
            })
            .unwrap();
        assert!(
            worker
                .results
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .is_err()
        );
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(Command::CreatePlaylist {
                name: "second".into(),
            })
            .unwrap();
        let ack = worker
            .results
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(ack.ok);
        assert_eq!(ack.revision, 2);
        drop(worker);
        server.join().unwrap();
    }

    #[test]
    fn command_worker_shutdown_closes_pending_exchange_and_drops_queued_commands() {
        use ipc::tests::{complete_test_handshake, read_test_command};
        use std::{io::Read, os::unix::net::UnixListener};

        let directory = tempfile::tempdir().unwrap();
        let listener =
            UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
        let (started, active) = bounded(1);
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake(&mut stream);
            assert!(
                matches!(read_test_command(&mut stream), Command::CreatePlaylist { name } if name == "first")
            );
            started.send(stream).unwrap();
        });
        let worker = spawn_command_worker(directory.path(), 1);
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(Command::CreatePlaylist {
                name: "first".into(),
            })
            .unwrap();
        let mut stream = active.recv_timeout(Duration::from_secs(5)).unwrap();
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(Command::CreatePlaylist {
                name: "second".into(),
            })
            .unwrap();
        drop(worker);
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        server.join().unwrap();
    }

    #[test]
    fn watcher_identity_binds_delayed_command_and_query_workers() {
        use ipc::tests::{complete_test_handshake, complete_test_handshake_for_instance};
        use std::{io::Read, os::unix::net::UnixListener};

        let directory = tempfile::tempdir().unwrap();
        let listener =
            UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
        let server = std::thread::spawn(move || {
            let mut watch = listener.accept().unwrap().0;
            complete_test_handshake(&mut watch);
            // Keep A's watch alive while lazy workers encounter B. Identical
            // state revision tokens must not authorize this different instance.
            for _ in 0..2 {
                let mut stream = listener.accept().unwrap().0;
                complete_test_handshake_for_instance(&mut stream, 2);
                assert_eq!(stream.read(&mut [0]).unwrap(), 0);
            }
        });
        let watcher = spawn_watcher(directory.path());
        watcher
            .receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        let (instance_id, _) = watcher.latest.lock().take().unwrap().unwrap();
        assert_eq!(instance_id, 1);
        let commands = spawn_command_worker(directory.path(), instance_id);
        commands
            .sender
            .as_ref()
            .unwrap()
            .send(Command::CreatePlaylist {
                name: "A-only".into(),
            })
            .unwrap();
        let failure = commands
            .results
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        assert!(failure.instance_changed);
        assert!(
            failure.into_message().is_err(),
            "instance failure must leave the TUI loop"
        );
        let mut queries = spawn_query_worker(directory.path(), instance_id);
        let request = QueryRequest {
            kind: QueryKind::Queue,
            offset: 0,
            limit: 20,
            view_revision: 0,
            library_revision: 0,
            structure_revision: 0,
            playlist_revision: 0,
            playlist_entry_revision: None,
            queue_revision: 0,
        };
        queries.request(request);
        queries.sender.take();
        queries.worker.take().unwrap().join().unwrap();
        let (_, result) = queries.latest.lock().take().unwrap();
        let failure = result.err().unwrap();
        assert!(failure.instance_changed);
        assert!(failure.into_message().is_err());
        drop(commands);
        drop(queries);
        drop(watcher);
        server.join().unwrap();
    }

    #[test]
    fn command_worker_reconnection_rejects_replacement_instance() {
        use ipc::tests::{
            complete_test_handshake, complete_test_handshake_for_instance, read_test_command,
        };
        use std::{io::Read, os::unix::net::UnixListener};

        let directory = tempfile::tempdir().unwrap();
        let listener =
            UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake(&mut stream);
            read_test_command(&mut stream);
            drop(stream);
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake_for_instance(&mut stream, 2);
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        });
        let worker = spawn_command_worker(directory.path(), 1);
        for name in ["ambiguous", "old-baseline"] {
            worker
                .sender
                .as_ref()
                .unwrap()
                .send(Command::CreatePlaylist { name: name.into() })
                .unwrap();
            let failure = worker
                .results
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap_err();
            assert_eq!(failure.instance_changed, name == "old-baseline");
        }
        drop(worker);
        server.join().unwrap();
    }

    #[test]
    fn superseded_query_still_reports_replacement_instance_as_fatal() {
        use ipc::tests::complete_test_handshake_for_instance;
        use std::{io::Read, os::unix::net::UnixListener};

        let directory = tempfile::tempdir().unwrap();
        let listener =
            UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
        let (accepted, connected) = bounded(1);
        let (release, resume) = bounded(1);
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            accepted.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(5)).unwrap();
            complete_test_handshake_for_instance(&mut stream, 2);
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        });
        let mut worker = spawn_query_worker(directory.path(), 1);
        worker.request(QueryRequest {
            kind: QueryKind::Queue,
            offset: 0,
            limit: 20,
            view_revision: 0,
            library_revision: 0,
            structure_revision: 0,
            playlist_revision: 0,
            playlist_entry_revision: None,
            queue_revision: 0,
        });
        connected.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.cancel();
        release.send(()).unwrap();
        worker.sender.take();
        worker.worker.take().unwrap().join().unwrap();
        let (_, result) = worker.latest.lock().take().unwrap();
        let failure = result.err().unwrap();
        assert!(failure.instance_changed);
        assert!(failure.into_message().is_err());
        server.join().unwrap();
    }

    #[test]
    fn query_worker_reconnection_rejects_replacement_before_accepting_results() {
        use ipc::tests::{
            complete_test_handshake, complete_test_handshake_for_instance, read_test_query,
        };
        use std::{io::Read, os::unix::net::UnixListener};

        let directory = tempfile::tempdir().unwrap();
        let listener =
            UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
        let (started, active) = bounded(1);
        let (release, resume) = bounded(1);
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake(&mut stream);
            assert!(matches!(
                read_test_query(&mut stream),
                Query::QueuePage { offset: 0, .. }
            ));
            started.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(stream);
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake_for_instance(&mut stream, 2);
            assert_eq!(
                stream.read(&mut [0]).unwrap(),
                0,
                "B received a query from A's baseline"
            );
        });
        let mut worker = spawn_query_worker(directory.path(), 1);
        let request = QueryRequest {
            kind: QueryKind::Queue,
            offset: 0,
            limit: 20,
            view_revision: 0,
            library_revision: 0,
            structure_revision: 0,
            playlist_revision: 0,
            playlist_entry_revision: None,
            queue_revision: 0,
        };
        worker.request(request.clone());
        active.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.request(QueryRequest {
            offset: 20,
            ..request
        });
        release.send(()).unwrap();
        worker.sender.take();
        worker.worker.take().unwrap().join().unwrap();
        let (request, result) = worker.latest.lock().take().unwrap();
        assert_eq!(request.offset, 20);
        let failure = result.err().unwrap();
        assert!(failure.instance_changed);
        assert!(failure.into_message().is_err());
        server.join().unwrap();
    }

    #[test]
    fn returning_to_parent_restores_selection_and_middle_of_viewport() {
        let state = TuiSnapshot::default();
        let mut tree = LibraryTree::default();
        tree.entries.selection = ListState::default().with_selected(Some(34)).with_offset(30);
        tree.entries.set_viewport(10);
        tree.entries.insert(
            0,
            100,
            100,
            (0..100)
                .map(|index| DirectoryRow::Directory {
                    path: format!("/music/{index}").into(),
                })
                .collect(),
        );
        let mut ui = UiState {
            view: View::Tree(tree),
            ..UiState::default()
        };
        ui.key_action(KeyEvent::from(KeyCode::Enter), &state);
        ui.key_action(KeyEvent::from(KeyCode::Backspace), &state);
        let View::Tree(tree) = &mut ui.view else {
            panic!("expected parent directory")
        };
        tree.entries.insert(
            20,
            20,
            100,
            (20..40)
                .map(|index| DirectoryRow::Track(track(index)))
                .collect(),
        );
        tree.entries.set_viewport(10);
        assert_eq!(tree.entries.selection.selected(), Some(34));
        assert_eq!(tree.entries.selection.offset(), 30);

        let mut playlists = PagedList {
            selection: ListState::default().with_selected(Some(34)).with_offset(30),
            ..PagedList::default()
        };
        playlists.set_viewport(10);
        playlists.insert(
            0,
            100,
            100,
            (0..100)
                .map(|index| PlaylistSummary {
                    id: index,
                    name: format!("collection-{index}"),
                    entry_count: 1,
                })
                .collect(),
        );
        ui.view = View::Playlists(playlists);
        ui.key_action(KeyEvent::from(KeyCode::Right), &state);
        ui.key_action(KeyEvent::from(KeyCode::Left), &state);
        let View::Playlists(playlists) = &mut ui.view else {
            panic!("expected playlist catalog")
        };
        playlists.insert(
            20,
            20,
            100,
            (20..40)
                .map(|index| PlaylistSummary {
                    id: index,
                    name: format!("collection-{index}"),
                    entry_count: 1,
                })
                .collect(),
        );
        playlists.set_viewport(10);
        assert_eq!(playlists.selection.selected(), Some(34));
        assert_eq!(playlists.selection.offset(), 30);
    }
}
