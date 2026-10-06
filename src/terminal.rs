use crate::{
    ipc,
    model::{
        Command, DirectoryRow, LibraryRow, LibrarySort, PlaybackStatus, PlaylistEntryRow,
        PlaylistSummary, RepeatMode, playback_key_command,
    },
    projection::TuiSnapshot,
    response::{Ack, StateResponse, ViewResponse},
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
    latest: Arc<Mutex<Option<Result<StateResponse, String>>>>,
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

fn spawn_watcher(socket_path: &Path, revision: u64) -> Watcher {
    let socket_path = socket_path.to_owned();
    let (sender, receiver) = bounded(1);
    let latest = Arc::new(Mutex::new(None));
    let latest_worker = Arc::clone(&latest);
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = stopping.clone();
    let cancel_notify = Arc::new(tokio::sync::Notify::new());
    let thread_notify = Arc::clone(&cancel_notify);
    let worker = std::thread::spawn(move || {
        let Ok(mut session) = ipc::watch_session_with_cancel(&socket_path, stop, thread_notify)
        else {
            *latest_worker.lock() = Some(Err("TUI state watcher could not connect".into()));
            let _ = sender.try_send(());
            return;
        };
        let mut revision = revision as u16;
        loop {
            match session.watch_until(revision) {
                Ok(Some(response)) => {
                    revision = response.state.system.revision as u16;
                    let shutting_down = response.state.system.shutting_down;
                    *latest_worker.lock() = Some(Ok(response));
                    let disconnected = sender.try_send(()).is_err_and(|error| {
                        matches!(error, crossbeam_channel::TrySendError::Disconnected(_))
                    });
                    if disconnected || shutting_down {
                        break;
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
    playlist_revision: u64,
}

impl QueryRequest {
    fn command(&self) -> Command {
        let offset = self.offset;
        let limit = self.limit;
        match &self.kind {
            QueryKind::Search(query) => Command::LibraryPage {
                query: Some(query.clone()),
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
                offset,
                limit,
            },
            QueryKind::Directory(path) => Command::DirectoryPage {
                path: path.clone(),
                offset,
                limit,
            },
            QueryKind::Roots(roots) => {
                let local_count = roots.len().saturating_sub(offset).min(limit);
                Command::DirectoryPage {
                    path: PathBuf::new(),
                    offset: offset.saturating_sub(roots.len()),
                    limit: (limit - local_count).max(1),
                }
            }
            QueryKind::Playlists => Command::PlaylistSummaries { offset, limit },
            QueryKind::Playlist(playlist_id) => Command::PlaylistEntries {
                playlist_id: *playlist_id,
                offset,
                limit,
            },
        }
    }
}

struct QueryResult {
    library_revision: u64,
    playlist_revision: u64,
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
            let result = ipc::request_with_cancel(
                &socket_path,
                &request.command(),
                Arc::clone(&worker_stopping),
                Arc::clone(&worker_cancel),
            )
            .map_err(|error| format!("{error:#}"))
            .and_then(|response| {
                if !response.ok {
                    return Err(response
                        .error
                        .unwrap_or_else(|| "View request rejected".into()));
                }
                let view = response
                    .view
                    .ok_or_else(|| "Missing view response".to_owned())?;
                Ok(QueryResult {
                    library_revision: response.state.library.revision,
                    playlist_revision: response.state.library.playlist_revision,
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
    let updates = spawn_watcher(socket_path, snapshot.system.revision);
    let commands = spawn_command_worker(socket_path);
    let queries = spawn_query_worker(socket_path);
    let mut tui = TuiState::new(snapshot);
    let mut redraw = true;
    let mut immediate_redraw = true;
    let mut last_draw = Instant::now() - Duration::from_millis(100);
    loop {
        while updates.receiver.try_recv().is_ok() {}
        if let Some(update) = updates.latest.lock().take() {
            let response = update.map_err(anyhow::Error::msg)?;
            if response.ok && response.state.system.revision != tui.snapshot.system.revision {
                tui.apply_snapshot(TuiSnapshot::from_client(&response.state));
                redraw = true;
            }
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
        Some(QueryRequest {
            kind,
            offset,
            limit,
            view_revision: self.query_revision,
            library_revision: state.library.revision,
            playlist_revision: state.library.playlist_revision,
        })
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
                self.set_message(Some(error));
                return true;
            }
        };
        if result.library_revision != request.library_revision
            || result.playlist_revision != request.playlist_revision
        {
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
    Ok(TuiSnapshot::from_client(
        &ipc::request(socket_path, &Command::Overview)?.state,
    ))
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
            "Type    search library\nTab     query/results focus\n↑/↓     select result\nPgUp/Dn page result\nBackspace edit query\nEnter   focus results\nEsc     return to queue\n\nSearch matches title, artist, album and path."
        }
        View::Tree(_) => {
            "↑/↓     select entry\nPgUp/Dn page entries\nEnter   open directory/add track\nBackspace/← parent directory\nEsc     return to queue\n/       search library\nP       playlists\nSpace   play/pause\nn/p       next/previous\n→       seek forward 5 sec\nHome/End seek start/end\nr       repeat mode\ns       shuffle\n[/]     volume\nq       quit"
        }
        View::PlaylistDetail { .. } => {
            "↑/↓     select track\nPgUp/Dn page tracks\nEnter   play track\na       add track to queue\n←/Esc   back to playlists\nSpace   play/pause\nn/p       next/previous\nHome/End seek start/end\nr       repeat mode\ns       shuffle\n[/]     volume\nq       quit"
        }
        View::Playlists(_) => {
            "↑/↓     select playlist\nPgUp/Dn page playlists\n→       open playlist\nEnter   play playlist\nEsc     return to queue\nP       playlists\n/       search library\nt       browse library tree\nSpace   play/pause\nn/p       next/previous\nHome/End seek start/end\nr       repeat mode\ns       shuffle\n[/]     volume\nq       quit"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        DirectoryPage, LibraryPage, LibraryRow, PlaylistEntryPage, PlaylistSummaryPage, QueueEntry,
        QueueState,
    };
    use crossterm::event::KeyModifiers;

    #[test]
    fn search_debounce_waits_for_the_last_edit_deadline() {
        let now = Instant::now();
        let mut search = LibrarySearch::default();
        search.filter(now);
        assert!(!search.ready(now + Duration::from_millis(149)));
        assert!(search.ready(now + Duration::from_millis(150)));

        search.filter(now + Duration::from_millis(100));
        assert!(!search.ready(now + Duration::from_millis(249)));
        assert!(search.ready(now + Duration::from_millis(250)));
    }

    fn track(id: i64, artist: &str, title: &str) -> LibraryRow {
        LibraryRow {
            id,
            title: title.into(),
            artist: artist.into(),
            album: "album".into(),
            duration: Some(1.0),
            favorite: false,
            missing: false,
            play_count: 0,
        }
    }

    fn tui_snapshot(queue: QueueState) -> TuiSnapshot {
        TuiSnapshot {
            queue,
            ..TuiSnapshot::default()
        }
    }

    fn row(id: i64) -> LibraryRow {
        LibraryRow {
            id,
            title: format!("Title {id}"),
            artist: "Artist".into(),
            album: "Album".into(),
            duration: Some(1.0),
            favorite: false,
            missing: false,
            play_count: 0,
        }
    }

    fn key(ui: &mut UiState, code: KeyCode, state: &TuiSnapshot) -> KeyAction {
        ui.key_action(KeyEvent::new(code, KeyModifiers::NONE), state)
    }

    fn apply_view(ui: &mut UiState, state: &TuiSnapshot, view: ViewResponse) {
        let request = ui.query_request(state).unwrap();
        let result = QueryResult {
            library_revision: state.library.revision,
            playlist_revision: state.library.playlist_revision,
            view,
        };
        assert!(ui.apply_query(state, &request, Ok(result)));
    }

    #[test]
    fn tree_fetches_only_active_directory_and_restores_parent_selection() {
        let state = TuiSnapshot::default();
        let mut ui = UiState {
            view: View::Tree(LibraryTree::default()),
            ..UiState::default()
        };
        apply_view(
            &mut ui,
            &state,
            ViewResponse::DirectoryPage(DirectoryPage {
                total: 2,
                rows: vec![
                    DirectoryRow::Directory {
                        path: PathBuf::from("/music"),
                    },
                    DirectoryRow::Directory {
                        path: PathBuf::from("/podcasts"),
                    },
                ],
            }),
        );
        key(&mut ui, KeyCode::Down, &state);
        key(&mut ui, KeyCode::Enter, &state);
        let request = ui.query_request(&state).unwrap();
        assert_eq!(
            request.kind,
            QueryKind::Directory(PathBuf::from("/podcasts"))
        );
        let View::Tree(tree) = &ui.view else {
            panic!("tree view");
        };
        assert!(tree.entries.pages.is_empty());
        apply_view(
            &mut ui,
            &state,
            ViewResponse::DirectoryPage(DirectoryPage {
                total: 1,
                rows: vec![DirectoryRow::Track(row(300))],
            }),
        );
        assert!(matches!(key(&mut ui, KeyCode::Enter, &state),
            KeyAction::Command(Command::Enqueue { track_ids }) if track_ids == vec![300]));
        key(&mut ui, KeyCode::Left, &state);
        let View::Tree(tree) = &ui.view else {
            panic!("tree view");
        };
        assert!(tree.directory.as_os_str().is_empty());
        assert_eq!(tree.entries.selection.selected(), Some(1));
        assert!(tree.entries.pages.is_empty());
    }

    #[test]
    fn configured_root_pages_keep_database_fallback_and_relative_children() {
        let mut state = TuiSnapshot::default();
        state.system.page_size = 100;
        let roots = (0..130)
            .map(|index| PathBuf::from(format!("/root/{index:03}")))
            .collect::<Vec<_>>();
        let mut ui = UiState {
            view: View::Tree(LibraryTree::new(&roots)),
            page_size: 100,
            ..UiState::default()
        };
        assert!(matches!(
            ui.query_request(&state).unwrap().command(),
            Command::DirectoryPage {
                offset: 0,
                limit: 1,
                ..
            }
        ));
        apply_view(
            &mut ui,
            &state,
            ViewResponse::DirectoryPage(DirectoryPage {
                total: 2,
                rows: vec![DirectoryRow::Directory {
                    path: PathBuf::from("/"),
                }],
            }),
        );
        let View::Tree(tree) = &ui.view else {
            panic!("tree view");
        };
        assert_eq!(tree.entries.items().len(), 100);
        assert_eq!(tree.entries.total, 132);
        key(&mut ui, KeyCode::PageDown, &state);
        assert!(matches!(
            ui.query_request(&state).unwrap().command(),
            Command::DirectoryPage {
                offset: 0,
                limit: 70,
                ..
            }
        ));
        apply_view(
            &mut ui,
            &state,
            ViewResponse::DirectoryPage(DirectoryPage {
                total: 2,
                rows: vec![
                    DirectoryRow::Directory {
                        path: PathBuf::from("/"),
                    },
                    DirectoryRow::Directory {
                        path: PathBuf::from("relative"),
                    },
                ],
            }),
        );
        let View::Tree(tree) = &mut ui.view else {
            panic!("tree view");
        };
        assert_eq!(tree.entries.items().len(), 32);
        tree.entries.selection.select(Some(131));
        key(&mut ui, KeyCode::Enter, &state);
        assert_eq!(
            ui.query_request(&state).unwrap().kind,
            QueryKind::Directory(PathBuf::from("relative"))
        );
    }

    #[test]
    fn directory_track_pages_navigate_beyond_first_page_and_back() {
        let state = TuiSnapshot::default();
        let mut ui = UiState {
            view: View::Tree(LibraryTree {
                directory: PathBuf::from("/music"),
                ..LibraryTree::default()
            }),
            page_size: 64,
            ..UiState::default()
        };
        apply_view(
            &mut ui,
            &state,
            ViewResponse::DirectoryPage(DirectoryPage {
                total: 300,
                rows: (0..64).map(|id| DirectoryRow::Track(row(id))).collect(),
            }),
        );
        key(&mut ui, KeyCode::PageDown, &state);
        assert_eq!(ui.query_request(&state).unwrap().offset, 64);
        apply_view(
            &mut ui,
            &state,
            ViewResponse::DirectoryPage(DirectoryPage {
                total: 300,
                rows: (64..128).map(|id| DirectoryRow::Track(row(id))).collect(),
            }),
        );
        assert!(matches!(key(&mut ui, KeyCode::Enter, &state),
            KeyAction::Command(Command::Enqueue { track_ids }) if track_ids == vec![64]));
        key(&mut ui, KeyCode::PageUp, &state);
        assert!(ui.query_request(&state).is_none());
        assert!(matches!(key(&mut ui, KeyCode::Enter, &state),
            KeyAction::Command(Command::Enqueue { track_ids }) if track_ids == vec![0]));
    }

    #[test]
    fn query_worker_coalesces_pending_requests() {
        let (sender, receiver) = bounded(1);
        let worker = QueryWorker {
            sender: Some(sender),
            pending: Arc::new(Mutex::new(None)),
            latest: Arc::new(Mutex::new(None)),
            stopping: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            worker: None,
        };
        let request = QueryRequest {
            kind: QueryKind::Search("first".into()),
            offset: 0,
            limit: 64,
            view_revision: 0,
            library_revision: 1,
            playlist_revision: 2,
        };
        worker.request(request.clone());
        let latest = QueryRequest {
            kind: QueryKind::Search("latest".into()),
            ..request
        };
        worker.request(latest.clone());
        assert_eq!(worker.pending.lock().as_ref(), Some(&latest));
        assert_eq!(receiver.len(), 1);
    }

    #[test]
    fn revision_unchanged_local_selection_is_visible() {
        let state = tui_snapshot(QueueState {
            entries: Arc::new(vec![
                QueueEntry { id: 1, track_id: 1 },
                QueueEntry { id: 2, track_id: 2 },
            ]),
            current_id: None,
            ..QueueState::default()
        });
        let mut ui = UiState::default();
        ui.sync_queue(&state, &state);
        let revision = ui.local_revision;

        assert!(matches!(
            ui.key_action(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &state),
            KeyAction::Ignored
        ));
        assert_eq!(ui.queue.selected(), Some(1));
        assert_ne!(ui.local_revision, revision);
    }

    #[test]
    fn enter_plays_selected_occurrence_after_queue_reorder() {
        let previous = tui_snapshot(QueueState {
            entries: Arc::new(vec![
                QueueEntry {
                    id: 11,
                    track_id: 1,
                },
                QueueEntry {
                    id: 22,
                    track_id: 1,
                },
                QueueEntry {
                    id: 33,
                    track_id: 2,
                },
            ]),
            current_id: Some(11),
            ..QueueState::default()
        });
        let mut ui = UiState::default();
        ui.sync_queue(&previous, &previous);
        ui.key_action(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &previous);
        let mut next = previous.clone();
        Arc::make_mut(&mut next.queue.entries).rotate_right(1);
        ui.sync_queue(&previous, &next);
        assert!(matches!(
            ui.key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &next),
            KeyAction::Command(Command::PlayQueue { queue_id: 22 })
        ));
        let empty = TuiSnapshot::default();
        ui.sync_queue(&next, &empty);
        assert!(matches!(
            ui.key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &empty),
            KeyAction::Ignored
        ));
    }

    #[test]
    fn queue_cache_follows_order_and_metadata_changes() {
        let mut state = tui_snapshot(QueueState {
            entries: Arc::new(vec![
                QueueEntry {
                    id: 10,
                    track_id: 1,
                },
                QueueEntry {
                    id: 20,
                    track_id: 2,
                },
            ]),
            current_id: None,
            tracks: Arc::new(vec![
                track(1, "Artist A", "Title A"),
                track(2, "Artist B", "Title B"),
            ]),
        });
        let mut cache = QueueViewCache::default();
        cache.sync(&state);
        assert_eq!(
            cache.rows,
            vec![
                "Artist A — Title A".to_owned(),
                "Artist B — Title B".to_owned()
            ]
        );

        state.queue.entries = Arc::new(vec![
            QueueEntry {
                id: 20,
                track_id: 2,
            },
            QueueEntry {
                id: 10,
                track_id: 1,
            },
        ]);
        cache.sync(&state);
        assert_eq!(
            cache.rows,
            vec![
                "Artist B — Title B".to_owned(),
                "Artist A — Title A".to_owned()
            ]
        );

        let mut changed = track(1, "Artist A", "Title A (remastered)");
        changed.album = "new album".into();
        state.queue.tracks = Arc::new(vec![changed, track(2, "Artist B", "Title B")]);
        cache.sync(&state);
        assert_eq!(cache.rows[1], "Artist A — Title A (remastered)");
    }
    #[test]
    fn playlist_detail_plays_or_enqueues_beyond_first_page_and_refetches_on_switch() {
        let mut state = TuiSnapshot::default();
        state.system.page_size = 100;
        let mut ui = UiState {
            view: View::Playlists(PagedList::default()),
            page_size: 100,
            ..UiState::default()
        };
        apply_view(
            &mut ui,
            &state,
            ViewResponse::PlaylistSummaries(PlaylistSummaryPage {
                total: 300,
                rows: (0..100)
                    .map(|id| PlaylistSummary {
                        id,
                        name: format!("Mix {id}"),
                        entry_count: 600,
                    })
                    .collect(),
            }),
        );
        key(&mut ui, KeyCode::PageDown, &state);
        assert_eq!(ui.query_request(&state).unwrap().offset, 100);
        apply_view(
            &mut ui,
            &state,
            ViewResponse::PlaylistSummaries(PlaylistSummaryPage {
                total: 300,
                rows: vec![PlaylistSummary {
                    id: 700,
                    name: "Mix".into(),
                    entry_count: 600,
                }],
            }),
        );
        key(&mut ui, KeyCode::Right, &state);
        assert_eq!(
            ui.query_request(&state).unwrap().kind,
            QueryKind::Playlist(700)
        );
        apply_view(
            &mut ui,
            &state,
            ViewResponse::PlaylistEntries(PlaylistEntryPage {
                total: 600,
                rows: (0..100)
                    .map(|id| PlaylistEntryRow {
                        id,
                        track_id: id,
                        title: format!("Track {id}"),
                        artist: "Artist".into(),
                        album: "Album".into(),
                        missing: false,
                    })
                    .collect(),
            }),
        );
        key(&mut ui, KeyCode::PageDown, &state);
        apply_view(
            &mut ui,
            &state,
            ViewResponse::PlaylistEntries(PlaylistEntryPage {
                total: 600,
                rows: vec![PlaylistEntryRow {
                    id: 900,
                    track_id: 901,
                    title: "Later track".into(),
                    artist: "Artist".into(),
                    album: "Album".into(),
                    missing: false,
                }],
            }),
        );
        assert!(matches!(
            key(&mut ui, KeyCode::Enter, &state),
            KeyAction::Command(Command::Play { track_id: 901 })
        ));
        assert!(matches!(key(&mut ui, KeyCode::Char('a'), &state),
            KeyAction::Command(Command::Enqueue { track_ids }) if track_ids == vec![901]));
        key(&mut ui, KeyCode::PageUp, &state);
        assert!(matches!(
            key(&mut ui, KeyCode::Enter, &state),
            KeyAction::Command(Command::Play { track_id: 0 })
        ));
        key(&mut ui, KeyCode::Esc, &state);
        let View::Playlists(playlists) = &ui.view else {
            panic!("playlist view");
        };
        assert!(playlists.pages.is_empty());
        assert_eq!(playlists.selection.selected(), Some(100));
        key(&mut ui, KeyCode::Char('P'), &state);
        assert_eq!(ui.query_request(&state).unwrap().offset, 0);
    }

    #[test]
    fn notice_expires_without_a_timer_thread() {
        let mut ui = UiState::default();
        ui.set_message(Some("Paused".into()));
        ui.message.as_mut().unwrap().expires_at = Instant::now() - Duration::from_secs(1);
        assert!(ui.expire_message());
        assert!(ui.message.is_none());
    }

    #[test]
    fn search_navigates_and_enqueues_beyond_first_page_and_refetches_evicted_pages() {
        let mut state = TuiSnapshot::default();
        state.system.page_size = 20;
        let mut ui = UiState {
            view: View::Search(LibrarySearch {
                focus: SearchFocus::Results,
                ..LibrarySearch::default()
            }),
            page_size: 20,
            ..UiState::default()
        };
        for offset in [0, 20, 40, 60] {
            let request = ui.query_request(&state).unwrap();
            assert_eq!(request.offset, offset);
            assert!(matches!(
                request.command(),
                Command::LibraryPage { limit: 20, .. }
            ));
            apply_view(
                &mut ui,
                &state,
                ViewResponse::LibraryPage(LibraryPage {
                    total: 1024,
                    rows: (offset..offset + 20).map(|id| row(id as i64)).collect(),
                }),
            );
            let View::Search(search) = &ui.view else {
                panic!("search view");
            };
            assert!(search.results.pages.len() <= 3);
            assert!(
                search
                    .results
                    .pages
                    .iter()
                    .map(|page| page.items.len())
                    .sum::<usize>()
                    <= 60
            );
            assert!(matches!(key(&mut ui, KeyCode::Enter, &state),
                KeyAction::Command(Command::Enqueue { track_ids }) if track_ids == vec![offset as i64]));
            if offset != 60 {
                key(&mut ui, KeyCode::PageDown, &state);
            }
        }
        key(&mut ui, KeyCode::PageUp, &state);
        assert!(ui.query_request(&state).is_none());
        key(&mut ui, KeyCode::PageUp, &state);
        key(&mut ui, KeyCode::PageUp, &state);
        assert_eq!(ui.query_request(&state).unwrap().offset, 0);
        apply_view(
            &mut ui,
            &state,
            ViewResponse::LibraryPage(LibraryPage {
                total: 1024,
                rows: (0..20).map(row).collect(),
            }),
        );
        assert!(matches!(key(&mut ui, KeyCode::Enter, &state),
            KeyAction::Command(Command::Enqueue { track_ids }) if track_ids == vec![0]));
    }

    #[test]
    fn viewport_page_jumps_cross_configured_fetch_boundaries() {
        let mut state = TuiSnapshot::default();
        state.system.page_size = 20;
        let mut ui = UiState {
            view: View::Search(LibrarySearch {
                focus: SearchFocus::Results,
                ..LibrarySearch::default()
            }),
            page_size: 7,
            ..UiState::default()
        };
        apply_view(
            &mut ui,
            &state,
            ViewResponse::LibraryPage(LibraryPage {
                total: 100,
                rows: (0..20).map(row).collect(),
            }),
        );
        for _ in 0..2 {
            key(&mut ui, KeyCode::PageDown, &state);
            assert!(ui.query_request(&state).is_none());
        }
        key(&mut ui, KeyCode::PageDown, &state);
        let request = ui.query_request(&state).unwrap();
        assert_eq!((request.offset, request.limit), (20, 20));
        apply_view(
            &mut ui,
            &state,
            ViewResponse::LibraryPage(LibraryPage {
                total: 100,
                rows: (20..40).map(row).collect(),
            }),
        );
        let View::Search(search) = &ui.view else {
            panic!("search view");
        };
        assert_eq!(search.results.selection.selected(), Some(21));
        assert_eq!(search.results.local_selection().selected(), Some(1));
        assert!(matches!(key(&mut ui, KeyCode::Enter, &state),
            KeyAction::Command(Command::Enqueue { track_ids }) if track_ids == vec![21]));
        key(&mut ui, KeyCode::PageUp, &state);
        assert!(ui.query_request(&state).is_none());
        assert!(matches!(key(&mut ui, KeyCode::Enter, &state),
            KeyAction::Command(Command::Enqueue { track_ids }) if track_ids == vec![14]));
    }

    #[test]
    fn live_page_size_changes_clear_all_view_caches_and_reject_old_requests() {
        fn page(view: &View, offset: usize, limit: usize) -> ViewResponse {
            let rows = offset..offset + limit;
            match view {
                View::Search(_) => ViewResponse::LibraryPage(LibraryPage {
                    total: 1000,
                    rows: rows.map(|id| row(id as i64)).collect(),
                }),
                View::Tree(_) => ViewResponse::DirectoryPage(DirectoryPage {
                    total: 1000,
                    rows: rows.map(|id| DirectoryRow::Track(row(id as i64))).collect(),
                }),
                View::Playlists(_) => ViewResponse::PlaylistSummaries(PlaylistSummaryPage {
                    total: 1000,
                    rows: rows
                        .map(|id| PlaylistSummary {
                            id: id as i64,
                            name: format!("Mix {id}"),
                            entry_count: 1000,
                        })
                        .collect(),
                }),
                View::PlaylistDetail { .. } => ViewResponse::PlaylistEntries(PlaylistEntryPage {
                    total: 1000,
                    rows: rows
                        .map(|id| PlaylistEntryRow {
                            id: id as i64,
                            track_id: id as i64,
                            title: format!("Track {id}"),
                            artist: "Artist".into(),
                            album: "Album".into(),
                            missing: false,
                        })
                        .collect(),
                }),
                View::Queue => unreachable!(),
            }
        }

        for view in [
            View::Search(LibrarySearch::default()),
            View::Tree(LibraryTree::default()),
            View::Playlists(PagedList::default()),
            View::PlaylistDetail {
                playlists: ListState::default(),
                detail: PlaylistDetail {
                    playlist_id: 1,
                    name: "Mix".into(),
                    entries: PagedList::default(),
                },
            },
        ] {
            let mut tui = TuiState::new(TuiSnapshot::default());
            tui.ui.view = view;
            let first = page(&tui.ui.view, 0, 64);
            apply_view(&mut tui.ui, &tui.snapshot, first);
            match &mut tui.ui.view {
                View::Search(search) => search.results.selection.select(Some(65)),
                View::Tree(tree) => tree.entries.selection.select(Some(65)),
                View::Playlists(playlists) => playlists.selection.select(Some(65)),
                View::PlaylistDetail { detail, .. } => detail.entries.selection.select(Some(65)),
                View::Queue => unreachable!(),
            }
            let old_request = tui.ui.query_request(&tui.snapshot).unwrap();
            assert_eq!((old_request.offset, old_request.limit), (64, 64));
            tui.ui.requested = Some(old_request.clone());
            let mut next = tui.snapshot.clone();
            next.system.page_size = 20;
            tui.apply_snapshot(next);
            assert!(tui.ui.requested.is_none());
            assert!(match &tui.ui.view {
                View::Search(search) => search.results.pages.is_empty(),
                View::Tree(tree) => tree.entries.pages.is_empty(),
                View::Playlists(playlists) => playlists.pages.is_empty(),
                View::PlaylistDetail { detail, .. } => detail.entries.pages.is_empty(),
                View::Queue => unreachable!(),
            });
            let current = tui.ui.query_request(&tui.snapshot).unwrap();
            assert_eq!((current.offset, current.limit), (60, 20));
            assert_ne!(current.view_revision, old_request.view_revision);
            assert!(
                !tui.ui
                    .apply_query(&tui.snapshot, &old_request, Err("old size".into()))
            );
            assert!(tui.ui.message.is_none());
            let resized = page(&tui.ui.view, 60, 20);
            apply_view(&mut tui.ui, &tui.snapshot, resized);
            assert!(tui.ui.query_request(&tui.snapshot).is_none());

            let mut next = tui.snapshot.clone();
            next.system.page_size = 64;
            tui.apply_snapshot(next);
            let restored = tui.ui.query_request(&tui.snapshot).unwrap();
            assert_eq!((restored.offset, restored.limit), (64, 64));
            assert_ne!(restored, old_request);
            assert!(
                !tui.ui
                    .apply_query(&tui.snapshot, &old_request, Err("earlier size".into()))
            );
        }
    }

    #[test]
    fn query_limits_follow_configured_sizes_for_every_view_kind() {
        for limit in [20, 64, 256] {
            for kind in [
                QueryKind::Search("track".into()),
                QueryKind::Directory(PathBuf::from("/music")),
                QueryKind::Roots(Arc::new(vec![PathBuf::from("/music")])),
                QueryKind::Playlists,
                QueryKind::Playlist(1),
            ] {
                let request = QueryRequest {
                    kind,
                    offset: limit,
                    limit,
                    view_revision: 0,
                    library_revision: 0,
                    playlist_revision: 0,
                };
                let command_limit = match request.command() {
                    Command::LibraryPage { limit, .. }
                    | Command::DirectoryPage { limit, .. }
                    | Command::PlaylistSummaries { limit, .. }
                    | Command::PlaylistEntries { limit, .. } => limit,
                    _ => unreachable!(),
                };
                assert_eq!(command_limit, limit);
            }
        }
    }

    #[test]
    fn discarded_page_result_does_not_block_revisiting_that_page() {
        let state = TuiSnapshot::default();
        let mut ui = UiState {
            view: View::Search(LibrarySearch {
                focus: SearchFocus::Results,
                ..LibrarySearch::default()
            }),
            page_size: 64,
            ..UiState::default()
        };
        apply_view(
            &mut ui,
            &state,
            ViewResponse::LibraryPage(LibraryPage {
                total: 512,
                rows: (0..64).map(row).collect(),
            }),
        );
        key(&mut ui, KeyCode::PageDown, &state);
        let request = ui.query_request(&state).unwrap();
        ui.requested = Some(request.clone());
        key(&mut ui, KeyCode::PageUp, &state);
        assert!(!ui.apply_query(
            &state,
            &request,
            Ok(QueryResult {
                library_revision: state.library.revision,
                playlist_revision: state.library.playlist_revision,
                view: ViewResponse::LibraryPage(LibraryPage {
                    total: 512,
                    rows: (64..128).map(row).collect(),
                }),
            })
        ));
        assert!(ui.requested.is_none());
        key(&mut ui, KeyCode::PageDown, &state);
        assert_eq!(ui.query_request(&state).as_ref(), Some(&request));
    }

    #[test]
    fn stale_search_and_revision_results_are_discarded() {
        let state = TuiSnapshot::default();
        let mut ui = UiState {
            view: View::Search(LibrarySearch::default()),
            ..UiState::default()
        };
        let request = ui.query_request(&state).unwrap();
        key(&mut ui, KeyCode::Char('x'), &state);
        assert!(!ui.apply_query(&state, &request, Err("old query".into())));
        assert!(ui.message.is_none());
        assert!(ui.query_request(&state).is_none());
        if let View::Search(search) = &mut ui.view {
            search.query_deadline = Some(Instant::now());
        }
        let current = ui.query_request(&state).unwrap();
        assert!(!ui.apply_query(
            &state,
            &current,
            Ok(QueryResult {
                library_revision: 1,
                playlist_revision: 0,
                view: ViewResponse::LibraryPage(LibraryPage {
                    total: 1,
                    rows: vec![row(1)]
                }),
            })
        ));
        apply_view(
            &mut ui,
            &state,
            ViewResponse::LibraryPage(LibraryPage {
                total: 1,
                rows: vec![row(1)],
            }),
        );
        let mut tui = TuiState {
            snapshot: state.clone(),
            ui,
        };
        let mut next = state;
        next.library.revision += 1;
        tui.apply_snapshot(next);
        let View::Search(search) = &tui.ui.view else {
            panic!("search view");
        };
        assert!(search.results.pages.is_empty());
        assert!(tui.ui.query_request(&tui.snapshot).is_some());
    }

    #[test]
    fn playlist_revision_clears_active_entry_pages() {
        let state = TuiSnapshot::default();
        let mut ui = UiState {
            view: View::PlaylistDetail {
                playlists: ListState::default(),
                detail: PlaylistDetail {
                    playlist_id: 7,
                    name: "Mix".into(),
                    entries: PagedList::default(),
                },
            },
            ..UiState::default()
        };
        apply_view(
            &mut ui,
            &state,
            ViewResponse::PlaylistEntries(PlaylistEntryPage {
                total: 1,
                rows: vec![PlaylistEntryRow {
                    id: 1,
                    track_id: 1,
                    title: "Track".into(),
                    artist: "Artist".into(),
                    album: "Album".into(),
                    missing: false,
                }],
            }),
        );
        let mut tui = TuiState {
            snapshot: state.clone(),
            ui,
        };
        let mut next = state;
        next.library.playlist_revision += 1;
        tui.apply_snapshot(next);
        let View::PlaylistDetail { detail, .. } = &tui.ui.view else {
            panic!("detail view");
        };
        assert!(detail.entries.pages.is_empty());
        assert_eq!(
            tui.ui.query_request(&tui.snapshot).unwrap().kind,
            QueryKind::Playlist(7)
        );
    }

    #[test]
    fn dropping_watcher_cancels_an_idle_revision_wait() {
        use std::{io::Read, os::unix::net::UnixListener};

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("watch.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (received_tx, received_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut prefix = [0; 4];
            stream.read_exact(&mut prefix).unwrap();
            let mut request = vec![0; u32::from_le_bytes(prefix) as usize];
            stream.read_exact(&mut request).unwrap();
            received_tx.send(()).unwrap();
            // Keep the connection open without publishing another revision.
            release_rx.recv().unwrap();
        });
        let watcher = spawn_watcher(&socket, 7);
        received_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (finished_tx, finished_rx) = bounded(1);
        let shutdown = std::thread::spawn(move || {
            drop(watcher);
            let _ = finished_tx.send(());
        });
        let stopped = finished_rx.recv_timeout(Duration::from_secs(2));
        // Always release the peer so a regression cannot strand test threads.
        release_tx.send(()).unwrap();
        peer.join().unwrap();
        shutdown.join().unwrap();
        assert!(stopped.is_ok(), "watcher teardown required a new revision");
    }

    #[test]
    fn dropping_query_worker_cancels_an_idle_view_request() {
        use std::{io::Read, os::unix::net::UnixListener};

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("query.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (received_tx, received_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut prefix = [0; 4];
            stream.read_exact(&mut prefix).unwrap();
            let mut request = vec![0; u32::from_le_bytes(prefix) as usize];
            stream.read_exact(&mut request).unwrap();
            received_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        let worker = spawn_query_worker(&socket);
        worker.request(QueryRequest {
            kind: QueryKind::Search("blocked".into()),
            offset: 0,
            limit: 64,
            view_revision: 0,
            library_revision: 0,
            playlist_revision: 0,
        });
        received_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (finished_tx, finished_rx) = bounded(1);
        let shutdown = std::thread::spawn(move || {
            drop(worker);
            let _ = finished_tx.send(());
        });
        let stopped = finished_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        peer.join().unwrap();
        shutdown.join().unwrap();
        assert!(stopped.is_ok(), "query teardown waited for IPC timeout");
    }
}
