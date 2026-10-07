use crate::{
    ipc,
    model::{
        Command, DirectoryRow, LibraryRow, LibrarySort, PlaybackStatus, PlaylistEntryRow,
        PlaylistSummary, Query, RepeatMode, playback_key_command,
    },
    projection::{ClientSnapshot, TuiSnapshot},
    response::{Ack, StateSections, ViewResponse},
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
    collections::{HashMap, VecDeque},
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
pub fn run(socket_path: &Path) -> Result<()> {
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
    let result = run_loop(socket_path, &mut terminal);
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

struct Watcher {
    receiver: Receiver<()>,
    latest: Arc<Mutex<Option<Result<ClientSnapshot, String>>>>,
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

fn spawn_watcher(socket_path: &Path) -> Watcher {
    let socket_path = socket_path.to_owned();
    let (sender, receiver) = bounded(1);
    let latest = Arc::new(Mutex::new(None));
    let latest_worker = Arc::clone(&latest);
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stopping);
    let cancel_notify = Arc::new(tokio::sync::Notify::new());
    let thread_notify = Arc::clone(&cancel_notify);
    let worker = std::thread::spawn(move || {
        let Ok(mut session) = ipc::watch_session_with_cancel(&socket_path, stop, thread_notify)
        else {
            *latest_worker.lock() = Some(Err("TUI state watcher could not connect".into()));
            let _ = sender.try_send(());
            return;
        };
        let mut state = match session.get_state(StateSections::ALL) {
            Ok(response) => {
                let mut state = ClientSnapshot::default();
                let revisions = response.revisions;
                response.apply_to(&mut state);
                (state, revisions)
            }
            Err(error) => {
                *latest_worker.lock() = Some(Err(format!("TUI state handshake failed: {error:#}")));
                let _ = sender.try_send(());
                return;
            }
        };
        *latest_worker.lock() = Some(Ok(state.0.clone()));
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
                                *latest_worker.lock() = Some(Ok(state.0.clone()));
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

struct CommandWorker {
    sender: Option<Sender<Command>>,
    stopping: Option<Sender<()>>,
    results: Receiver<Result<Ack, String>>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for CommandWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(stopping) = self.stopping.take() {
            let _ = stopping.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn spawn_command_worker(socket_path: &Path) -> CommandWorker {
    let socket_path = socket_path.to_owned();
    let (sender, receiver) = bounded::<Command>(8);
    let (stopping, stop_receiver) = bounded::<()>(1);
    let (result_sender, results) = bounded::<Result<Ack, String>>(8);
    let worker = std::thread::spawn(move || {
        while let Ok(command) = receiver.recv() {
            let result =
                ipc::request_ack(&socket_path, &command).map_err(|error| format!("{error:#}"));
            crossbeam_channel::select! {
                send(result_sender, result) -> _ => {}
                recv(stop_receiver) -> _ => break,
            }
        }
    });
    CommandWorker {
        sender: Some(sender),
        stopping: Some(stopping),
        results,
        worker: Some(worker),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum QueryKind {
    Search(String),
    Directory(PathBuf),
    Roots(Arc<Vec<PathBuf>>),
    Playlists,
    Playlist(i64),
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
}

impl QueryRequest {
    fn query(&self) -> Query {
        let offset = self.offset;
        let limit = self.limit;
        match &self.kind {
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
                offset,
                limit,
            },
            QueryKind::Roots(roots) => {
                let local_count = roots.len().saturating_sub(offset).min(limit);
                Query::DirectoryPage {
                    path: PathBuf::new(),
                    offset: offset.saturating_sub(roots.len()),
                    limit: (limit - local_count).max(1),
                }
            }
            QueryKind::Playlists => Query::PlaylistSummaries { offset, limit },
            QueryKind::Playlist(playlist_id) => Query::PlaylistEntries {
                playlist_id: *playlist_id,
                offset,
                limit,
            },
        }
    }
}

struct QueryResult {
    revisions: crate::response::QueryRevisions,
    view: ViewResponse,
}

struct QueryWorker {
    sender: Option<Sender<()>>,
    pending: Arc<Mutex<Option<QueryRequest>>>,
    latest: Arc<Mutex<Option<(QueryRequest, Result<QueryResult, String>)>>>,
    stopping: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
    worker: Option<JoinHandle<()>>,
}

impl QueryWorker {
    fn request(&self, request: QueryRequest) {
        *self.pending.lock() = Some(request);
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(());
        }
    }
}

impl Drop for QueryWorker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.cancel_notify.notify_waiters();
        self.pending.lock().take();
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn spawn_query_worker(socket_path: &Path) -> QueryWorker {
    let socket_path = socket_path.to_owned();
    let (sender, receiver) = bounded(1);
    let pending = Arc::new(Mutex::new(None::<QueryRequest>));
    let requests = Arc::clone(&pending);
    let latest = Arc::new(Mutex::new(None));
    let results = Arc::clone(&latest);
    let stopping = Arc::new(AtomicBool::new(false));
    let cancel_notify = Arc::new(tokio::sync::Notify::new());
    let worker_stopping = Arc::clone(&stopping);
    let worker_cancel = Arc::clone(&cancel_notify);
    let worker = std::thread::spawn(move || {
        while receiver.recv().is_ok() {
            let Some(request) = requests.lock().take() else {
                continue;
            };
            let result = ipc::query_with_cancel(
                &socket_path,
                &request.query(),
                Arc::clone(&worker_stopping),
                Arc::clone(&worker_cancel),
            )
            .map_err(|error| format!("{error:#}"))
            .and_then(|response| {
                let view = response.result?;
                Ok(QueryResult {
                    revisions: response.revisions,
                    view,
                })
            });
            *results.lock() = Some((request, result));
        }
    });
    QueryWorker {
        sender: Some(sender),
        pending,
        latest,
        stopping,
        cancel_notify,
        worker: Some(worker),
    }
}

fn run_loop(socket_path: &Path, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let snapshot = request_state(socket_path)?;
    let updates = spawn_watcher(socket_path);
    let commands = spawn_command_worker(socket_path);
    let queries = spawn_query_worker(socket_path);
    let mut tui = TuiState::new(snapshot);
    let mut redraw = true;
    let mut immediate_redraw = true;
    let mut last_draw = Instant::now() - Duration::from_millis(100);
    loop {
        while updates.receiver.try_recv().is_ok() {}
        if let Some(update) = updates.latest.lock().take() {
            let state = update.map_err(anyhow::Error::msg)?;
            let next = TuiSnapshot::from_client(&state);
            tui.apply_snapshot(next);
            redraw = true;
        }
        while let Ok(result) = commands.results.try_recv() {
            match result {
                Ok(ack) if !ack.ok => {
                    tui.ui
                        .set_message(Some(ack.error.unwrap_or_else(|| "Command rejected".into())));
                }
                Ok(_) => {}
                Err(error) => tui.ui.set_message(Some(error)),
            }
            redraw = true;
            immediate_redraw = true;
        }
        if let Some((request, result)) = queries.latest.lock().take() {
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
            None => tui.ui.requested = None,
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
        ui.sync_queue(&snapshot, &snapshot);
        Self { snapshot, ui }
    }

    fn apply_snapshot(&mut self, next: TuiSnapshot) {
        if self.snapshot.library.revision != next.library.revision
            || self.snapshot.library.playlist_revision != next.library.playlist_revision
            || self.snapshot.system.page_size != next.system.page_size
        {
            self.ui.invalidate_views();
        }
        if self.snapshot.system.library_roots != next.system.library_roots {
            if let View::Tree(tree) = &mut self.ui.view {
                tree.roots = LibraryTree::roots(&next.system.library_roots);
                tree.entries.clear();
                self.ui.requested = None;
            }
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
    match next.queue.entries.len().cmp(&previous.queue.entries.len()) {
        std::cmp::Ordering::Greater => Some("Added to queue".into()),
        std::cmp::Ordering::Less => Some("Removed from queue".into()),
        std::cmp::Ordering::Equal => None,
    }
}

struct Notice {
    text: String,
    expires_at: Instant,
}

enum View {
    Queue,
    Search(LibrarySearch),
    Tree(LibraryTree),
    Playlists(PagedList<PlaylistSummary>),
    PlaylistDetail {
        playlists: ListState,
        detail: PlaylistDetail,
    },
}

impl Default for View {
    fn default() -> Self {
        Self::Queue
    }
}

#[derive(Default)]
struct UiState {
    view: View,
    queue: ListState,
    show_help: bool,
    message: Option<Notice>,
    queue_cache: QueueViewCache,
    requested: Option<QueryRequest>,
    query_revision: u64,
    failed_request: Option<QueryRequest>,
    local_revision: u64,
    // Keyboard PageUp/PageDown distance follows the terminal viewport, not query limits.
    page_size: usize,
}
impl UiState {
    fn query_request(&self, state: &TuiSnapshot) -> Option<QueryRequest> {
        let limit = state.system.page_size as usize;
        let (kind, offset) = match &self.view {
            View::Search(search)
                if search.results.needs_page(limit) && search.ready(Instant::now()) =>
            {
                (
                    QueryKind::Search(search.query.clone()),
                    search.results.offset(limit),
                )
            }
            View::Tree(tree) if tree.entries.needs_page(limit) => (
                if tree.directory.as_os_str().is_empty() {
                    QueryKind::Roots(Arc::clone(&tree.roots))
                } else {
                    QueryKind::Directory(tree.directory.clone())
                },
                tree.entries.offset(limit),
            ),
            View::Playlists(playlists) if playlists.needs_page(limit) => {
                (QueryKind::Playlists, playlists.offset(limit))
            }
            View::PlaylistDetail { detail, .. } if detail.entries.needs_page(limit) => (
                QueryKind::Playlist(detail.playlist_id),
                detail.entries.offset(limit),
            ),
            _ => return None,
        };
        let request = QueryRequest {
            kind,
            offset,
            limit,
            view_revision: self.query_revision,
            library_revision: state.library.revision,
            structure_revision: state.library.structure_revision,
            playlist_revision: state.library.playlist_revision,
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
            QueryKind::Directory(_) | QueryKind::Roots(_) => {
                result.revisions.structure == request.structure_revision
            }
            QueryKind::Playlists | QueryKind::Playlist(_) => {
                result.revisions.playlist == request.playlist_revision
            }
            QueryKind::Search(_) => result.revisions.library == request.library_revision,
        };
        if !revision_matches {
            return false;
        }
        match (&mut self.view, result.view) {
            (View::Search(search), ViewResponse::LibraryPage(page)) => {
                search
                    .results
                    .insert(request.offset, request.limit, page.total, page.rows);
            }
            (View::Tree(tree), ViewResponse::DirectoryPage(mut page)) => {
                if let QueryKind::Roots(roots) = &request.kind {
                    let local_count = roots
                        .len()
                        .saturating_sub(request.offset)
                        .min(request.limit);
                    page.rows.truncate(request.limit - local_count);
                    if local_count != 0 {
                        drop(
                            page.rows.splice(
                                0..0,
                                roots
                                    .iter()
                                    .skip(request.offset)
                                    .take(local_count)
                                    .cloned()
                                    .map(|path| DirectoryRow::Directory { path }),
                            ),
                        );
                    }
                    page.total += roots.len();
                }
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
        let selected = self.queue.selected().unwrap_or(0);
        let queue_id = previous.queue.entries.get(selected).map(|entry| entry.id);
        let index = next
            .queue
            .entries
            .iter()
            .position(|entry| Some(entry.id) == queue_id);
        self.queue.select(
            (!next.queue.entries.is_empty())
                .then(|| index.unwrap_or(selected.min(next.queue.entries.len() - 1))),
        );
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
                        selection: playlists,
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
                                };
                                self.view = View::PlaylistDetail {
                                    playlists: playlists.selection,
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
                    KeyCode::Up | KeyCode::Down => move_selection(
                        &mut self.queue,
                        state.queue.entries.len(),
                        key.code == KeyCode::Down,
                    ),
                    KeyCode::PageUp | KeyCode::PageDown => move_selection_page(
                        &mut self.queue,
                        state.queue.entries.len(),
                        page_size,
                        key.code == KeyCode::PageDown,
                    ),
                    KeyCode::Enter => {
                        command = self
                            .queue
                            .selected()
                            .and_then(|index| state.queue.entries.get(index))
                            .map(|entry| Command::PlayQueue { queue_id: entry.id });
                        false
                    }
                    KeyCode::Char('d') | KeyCode::Delete => {
                        command = self
                            .queue
                            .selected()
                            .and_then(|index| state.queue.entries.get(index))
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
const RETAINED_VIEW_PAGES: usize = 3;

struct ViewPage<T> {
    offset: usize,
    items: Vec<T>,
}

struct PagedList<T> {
    pages: VecDeque<ViewPage<T>>,
    total: usize,
    loaded: bool,
    fetch_size: usize,
    selection: ListState,
}

impl<T> Default for PagedList<T> {
    fn default() -> Self {
        Self {
            pages: VecDeque::new(),
            total: 0,
            loaded: false,
            fetch_size: 0,
            selection: ListState::default(),
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

    fn offset(&self, fetch_size: usize) -> usize {
        self.selection.selected().unwrap_or(0) / fetch_size * fetch_size
    }

    fn needs_page(&self, fetch_size: usize) -> bool {
        !self.loaded
            || self.fetch_size != fetch_size
            || !self
                .pages
                .iter()
                .any(|page| page.offset == self.offset(fetch_size))
    }

    fn navigate(&mut self, key: KeyCode, page_size: usize) -> bool {
        if !self.loaded {
            return false;
        }
        match key {
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
        }
    }

    fn insert(&mut self, offset: usize, fetch_size: usize, total: usize, items: Vec<T>) {
        if self.fetch_size != fetch_size {
            self.clear();
        }
        self.fetch_size = fetch_size;
        self.pages.retain(|page| page.offset != offset);
        if self.pages.len() == RETAINED_VIEW_PAGES {
            self.pages.pop_front();
        }
        self.pages.push_back(ViewPage { offset, items });
        self.total = total;
        self.loaded = true;
        let selected = self.selection.selected().unwrap_or(0);
        self.selection
            .select((total != 0).then(|| selected.min(total - 1)));
    }

    fn selected(&self) -> Option<&T> {
        let selected = self.selection.selected()?;
        self.pages.iter().find_map(|page| {
            selected
                .checked_sub(page.offset)
                .and_then(|index| page.items.get(index))
        })
    }

    fn items(&self) -> &[T] {
        self.pages
            .iter()
            .find(|page| page.offset == self.offset(self.fetch_size.max(1)))
            .map_or(&[], |page| page.items.as_slice())
    }

    fn local_selection(&self) -> ListState {
        ListState::default().with_selected(
            self.selection
                .selected()
                .map(|index| index - self.offset(self.fetch_size.max(1))),
        )
    }
}

struct PlaylistDetail {
    playlist_id: i64,
    name: String,
    entries: PagedList<PlaylistEntryRow>,
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
    roots: Arc<Vec<PathBuf>>,
    ancestors: Vec<(PathBuf, ListState)>,
    entries: PagedList<DirectoryRow>,
}

impl LibraryTree {
    fn roots(configured: &[PathBuf]) -> Arc<Vec<PathBuf>> {
        let mut roots: Vec<_> = configured
            .iter()
            .filter(|path| !path.as_os_str().is_empty() && path.as_path() != Path::new("/"))
            .cloned()
            .collect();
        roots.sort();
        roots.dedup();
        Arc::new(roots)
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
        let Some((directory, selection)) = self.ancestors.pop() else {
            return false;
        };
        self.directory = directory;
        self.entries = PagedList {
            selection,
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

fn request_state(socket_path: &Path) -> Result<TuiSnapshot> {
    let response = ipc::get_state(socket_path, StateSections::ALL)?;
    let mut state = ClientSnapshot::default();
    response.apply_to(&mut state);
    Ok(TuiSnapshot::from_client(&state))
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
        PlaybackStatus::Playing => (
            "Playing",
            if state.system.nerd_symbols {
                "󰐊"
            } else {
                ">"
            },
        ),
        PlaybackStatus::Paused => (
            "Paused",
            if state.system.nerd_symbols {
                "󰏤"
            } else {
                "||"
            },
        ),
        PlaybackStatus::Stopped => (
            "Stopped",
            if state.system.nerd_symbols {
                "󰓛"
            } else {
                "[]"
            },
        ),
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
        View::Tree(tree) => draw_tree(frame, columns[0], tree, state.system.nerd_symbols),
        View::PlaylistDetail { detail, .. } => {
            draw_playlist_detail(frame, columns[0], detail, state.system.nerd_symbols)
        }
        View::Playlists(playlists) => {
            draw_playlists(frame, columns[0], playlists, state.system.nerd_symbols)
        }
        View::Queue => draw_queue(frame, columns[0], state, &mut ui.queue, &mut ui.queue_cache),
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
            "Type    search library\nTab     query/results focus\n↑/↓     select result\nPgUp/Dn page result\nR       retry failed page\nBackspace edit query\nEnter   focus results\nEsc     return to queue\n\nSearch matches title, artist, album and path."
        }
        View::Tree(_) => {
            "↑/↓     select entry\nPgUp/Dn page entries\nR       retry failed page\nEnter   open directory/add track\nBackspace/← parent directory\nEsc     return to queue\n/       search library\nP       playlists\nSpace   play/pause\nn/p       next/previous\n→       seek forward 5 sec\nHome/End seek start/end\nr       repeat mode\ns       shuffle\n[/]     volume\nq       quit"
        }
        View::PlaylistDetail { .. } => {
            "↑/↓     select track\nPgUp/Dn page tracks\nR       retry failed page\nEnter   play track\na       add track to queue\n←/Esc   back to playlists\nSpace   play/pause\nn/p       next/previous\nHome/End seek start/end\nr       repeat mode\ns       shuffle\n[/]     volume\nq       quit"
        }
        View::Playlists(_) => {
            "↑/↓     select playlist\nPgUp/Dn page playlists\nR       retry failed page\n→       open playlist\nEnter   play playlist\nEsc     return to queue\nP       playlists\n/       search library\nt       browse library tree\nSpace   play/pause\nn/p       next/previous\nHome/End seek start/end\nr       repeat mode\ns       shuffle\n[/]     volume\nq       quit"
        }
        View::Queue => {
            "↑/↓     select queue\nPgUp/Dn page queue\nEnter   play selected\nd/Del   remove selected\n/       search library\nt       browse library tree\nP       playlists\nSpace   play/pause\n←/→     seek 5 sec\nHome/End seek start/end\nr       repeat mode\ns       shuffle\n[/]     volume\nq       quit"
        }
    };
    if ui.show_help {
        let help = Paragraph::new(keys)
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Keys").borders(Borders::ALL));
        frame.render_widget(help, columns[1]);
    }
    let status = if state.system.nerd_symbols {
        format!(
            "󰕾 {:>3}% 󰒝 {} 󰑖 {} 󰘦 {}",
            (state.playback.volume * 100.0).round() as u8,
            if state.playback.shuffle { "on" } else { "off" },
            repeat_label(state.playback.repeat),
            state.queue.entries.len()
        )
    } else {
        format!(
            "vol {:>3}%  shuffle {}  repeat {}  queue {}",
            (state.playback.volume * 100.0).round() as u8,
            if state.playback.shuffle { "on" } else { "off" },
            repeat_label(state.playback.repeat),
            state.queue.entries.len()
        )
    };
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

#[derive(Default)]
struct QueueViewCache {
    queue: Vec<(u64, i64)>,
    tracks: Vec<QueueTrackKey>,
    rows: Vec<String>,
}

struct QueueTrackKey {
    id: i64,
    title: String,
    artist: String,
}

impl QueueViewCache {
    fn matches(&self, state: &TuiSnapshot) -> bool {
        self.queue.len() == state.queue.entries.len()
            && self
                .queue
                .iter()
                .zip(state.queue.entries.iter())
                .all(|(&(id, track_id), entry)| id == entry.id && track_id == entry.track_id)
            && self.tracks.len() == state.queue.tracks.len()
            && self
                .tracks
                .iter()
                .zip(state.queue.tracks.iter())
                .all(|(cached, track)| {
                    cached.id == track.id
                        && cached.title == track.title
                        && cached.artist == track.artist
                })
    }

    fn sync(&mut self, state: &TuiSnapshot) {
        if self.matches(state) {
            return;
        }
        self.queue = state
            .queue
            .entries
            .iter()
            .map(|entry| (entry.id, entry.track_id))
            .collect();
        self.tracks = state
            .queue
            .tracks
            .iter()
            .map(|track| QueueTrackKey {
                id: track.id,
                title: track.title.clone(),
                artist: track.artist.clone(),
            })
            .collect();
        let mut track_index = HashMap::with_capacity(state.queue.tracks.len());
        for (index, track) in state.queue.tracks.iter().enumerate() {
            track_index.insert(track.id, index);
        }
        self.rows = state
            .queue
            .entries
            .iter()
            .map(|entry| {
                track_index
                    .get(&entry.track_id)
                    .and_then(|&index| state.queue.tracks.get(index))
                    .map(|track| format!("{} — {}", track.artist, track.title))
                    .unwrap_or_else(|| "Missing track".into())
            })
            .collect();
    }
}

fn draw_queue(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &TuiSnapshot,
    selection: &mut ListState,
    cache: &mut QueueViewCache,
) {
    cache.sync(state);
    let items = cache.rows.iter().enumerate().map(|(index, title)| {
        let marker = if state.queue.current_id == Some(state.queue.entries[index].id) {
            "▶ "
        } else {
            "  "
        };
        ListItem::new(format!("{marker}{title}"))
    });
    let title = if state.queue.entries.is_empty() {
        "Queue — empty; / to search, t to browse"
    } else {
        "Queue"
    };
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
        selection,
    );
}
fn draw_paged_list<T>(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: String,
    page: &PagedList<T>,
    row: impl Fn(&T) -> String,
) {
    let title = if page.needs_page(page.fetch_size.max(1)) {
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
    let items = page.items().iter().map(|item| ListItem::new(row(item)));
    let mut selection = page.local_selection();
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

fn draw_playlist_detail(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    detail: &PlaylistDetail,
    nerd_symbols: bool,
) {
    let marker = if nerd_symbols { "󰎆 " } else { "♪ " };
    draw_paged_list(
        frame,
        area,
        format!("Playlist — {}", detail.name),
        &detail.entries,
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
    playlists: &PagedList<PlaylistSummary>,
    nerd_symbols: bool,
) {
    let marker = if nerd_symbols { "󰲋 " } else { "[P] " };
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
    draw_paged_list(frame, rows[1], title, &search.results, |track| {
        format!(
            "{} — {} [{}]{}",
            track.artist,
            track.title,
            track.album,
            if track.missing { " [missing]" } else { "" }
        )
    });
}

fn draw_tree(frame: &mut ratatui::Frame<'_>, area: Rect, tree: &LibraryTree, nerd_symbols: bool) {
    let title = if tree.directory.as_os_str().is_empty() {
        "Library tree".into()
    } else {
        format!("Library tree — {}", tree.directory.display())
    };
    draw_paged_list(frame, area, title, &tree.entries, |entry| match entry {
        DirectoryRow::Directory { path } => {
            let marker = if nerd_symbols { "\u{f07b} " } else { "[dir] " };
            let name = path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy();
            format!("{marker}{name}/")
        }
        DirectoryRow::Track(track) => {
            let marker = if nerd_symbols { "\u{f001} " } else { "      " };
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
