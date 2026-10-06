use crate::{
    ipc,
    model::{Command, PlaybackStatus, RepeatMode, Track, playback_key_command},
    projection::TuiSnapshot,
    response::{Ack, StateResponse},
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
    collections::{BTreeMap, HashMap},
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
    worker: Option<JoinHandle<()>>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
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
    let worker = std::thread::spawn(move || {
        let Ok(mut session) = ipc::watch_session(&socket_path) else {
            *latest_worker.lock() = Some(Err("TUI state watcher could not connect".into()));
            let _ = sender.try_send(());
            return;
        };
        let mut revision = revision as u16;
        while !stop.load(Ordering::Acquire) {
            match session.watch_until(revision, || stop.load(Ordering::Acquire)) {
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

fn run_loop(socket_path: &Path, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let snapshot = request_state(socket_path)?;
    let updates = spawn_watcher(socket_path, snapshot.system.revision);
    let commands = spawn_command_worker(socket_path);
    let mut tui = TuiState::new(snapshot);
    tui.ui.sync_playlists(&tui.snapshot, &tui.snapshot);
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
                            let library = Arc::clone(&tui.snapshot.library.tracks);
                            if matches!(action, KeyAction::Tree) {
                                tui.ui.view = View::Tree(LibraryTree::new(
                                    library,
                                    Arc::clone(&tui.snapshot.system.library_roots),
                                ));
                            } else {
                                tui.ui.view = View::Search(LibrarySearch::new(library));
                            }
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
        Self {
            snapshot,
            ui: UiState::default(),
        }
    }

    fn apply_snapshot(&mut self, mut next: TuiSnapshot) {
        if self.snapshot.library.revision == next.library.revision {
            next.library.tracks = Arc::clone(&self.snapshot.library.tracks);
        }
        self.ui.sync_queue(&self.snapshot, &next);
        self.ui.sync_playlists(&self.snapshot, &next);
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
    Playlists(ListState),
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
    playlist_cache: PlaylistDetailCache,
    local_revision: u64,
    page_size: usize,
}
impl UiState {
    fn query_request(&self, state: &TuiSnapshot) -> Option<QueryRequest> {
        let limit = state.system.page_size as usize;
        let (kind, offset) = match &self.view {
            View::Search(search) if search.results.needs_page(limit) && search.ready(Instant::now()) => (
                QueryKind::Search(search.query.clone()),
                search.results.offset(limit),
            ),
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

    fn sync_playlists(&mut self, previous: &TuiSnapshot, next: &TuiSnapshot) {
        match &mut self.view {
            View::Playlists(selection) => {
                Self::sync_playlist_selection(selection, previous, next);
            }
            View::PlaylistDetail { playlists, detail } => {
                Self::sync_playlist_selection(playlists, previous, next);
                let Some(playlist) = next
                    .library
                    .playlists
                    .iter()
                    .find(|playlist| playlist.id == detail.playlist_id)
                else {
                    let playlists = std::mem::take(playlists);
                    self.view = View::Playlists(playlists);
                    return;
                };
                let selected = detail.selection.selected().unwrap_or(0);
                detail.selection.select(
                    (!playlist.entries.is_empty())
                        .then(|| selected.min(playlist.entries.len() - 1)),
                );
            }
            _ => {}
        }
    }
    fn sync_playlist_selection(
        selection: &mut ListState,
        previous: &TuiSnapshot,
        next: &TuiSnapshot,
    ) {
        let selected = selection.selected().unwrap_or(0);
        let playlist_id = previous
            .library
            .playlists
            .get(selected)
            .map(|playlist| playlist.id);
        let index = next
            .library
            .playlists
            .iter()
            .position(|playlist| Some(playlist.id) == playlist_id);
        selection.select(
            (!next.library.playlists.is_empty())
                .then(|| index.unwrap_or(selected.min(next.library.playlists.len() - 1))),
        );
    }

    fn key_action(&mut self, key: KeyEvent, state: &TuiSnapshot) -> KeyAction {
        if key.code == KeyCode::Char('?') {
            self.show_help = !self.show_help;
            self.mark_local_change();
            return KeyAction::Ignored;
        }
        let page_size = self.page_size;
        let view = std::mem::take(&mut self.view);
        match view {
            View::Search(mut search) => {
                let mut changed = false;
                let mut command = None;
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
                        KeyCode::Up | KeyCode::Down if search.focus == SearchFocus::Results => {
                            changed = move_selection(
                                &mut search.selection,
                                search.matches.len(),
                                key.code == KeyCode::Down,
                            );
                        }
                        KeyCode::PageUp | KeyCode::PageDown
                            if search.focus == SearchFocus::Results =>
                        {
                            changed = move_selection_page(
                                &mut search.selection,
                                search.matches.len(),
                                page_size,
                                key.code == KeyCode::PageDown,
                            );
                        }
                        KeyCode::Enter if search.focus == SearchFocus::Results => {
                            if let Some(index) = search.selection.selected()
                                && let Some(&track_index) = search.matches.get(index)
                            {
                                command = Some(Command::Enqueue {
                                    track_ids: vec![search.library[track_index].id],
                                });
                            }
                        }
                        KeyCode::Enter => {
                            search.focus = SearchFocus::Results;
                            changed = true;
                        }
                        KeyCode::Backspace if search.focus == SearchFocus::Query => {
                            if search.query.pop().is_some() {
                                search.filter(Instant::now());
                                changed = true;
                            }
                        }
                        KeyCode::Char(character) if search.focus == SearchFocus::Query => {
                            search.query.push(character);
                            search.filter(Instant::now());
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
                if changed || close {
                    self.mark_local_change();
                }
                command.map_or(KeyAction::Ignored, KeyAction::Command)
            }
            View::Tree(mut tree) => {
                let mut changed = false;
                let mut command = None;
                let close = key.code == KeyCode::Esc;
                if !close {
                    match key.code {
                        KeyCode::Up | KeyCode::Down => {
                            let len = tree.entries().len();
                            changed =
                                move_selection(&mut tree.selection, len, key.code == KeyCode::Down);
                        }
                        KeyCode::PageUp | KeyCode::PageDown => {
                            let len = tree.entries().len();
                            changed = move_selection_page(
                                &mut tree.selection,
                                len,
                                page_size,
                                key.code == KeyCode::PageDown,
                            );
                        }
                        KeyCode::Enter => {
                            command = tree.activate();
                            changed = command.is_none();
                        }
                        KeyCode::Backspace | KeyCode::Left => changed = tree.parent(),
                        _ => {
                            self.view = View::Tree(tree);
                            return key_action(key, state);
                        }
                    }
                }
                self.view = if close { View::Queue } else { View::Tree(tree) };
                if changed || close {
                    self.mark_local_change();
                }
                command.map_or(KeyAction::Ignored, KeyAction::Command)
            }
            View::PlaylistDetail {
                playlists,
                mut detail,
            } => {
                let Some(playlist) = state
                    .library
                    .playlists
                    .iter()
                    .find(|playlist| playlist.id == detail.playlist_id)
                else {
                    self.view = View::Playlists(playlists);
                    return KeyAction::Ignored;
                };
                let mut changed = false;
                let mut command = None;
                let close = matches!(key.code, KeyCode::Esc | KeyCode::Left);
                if !close {
                    match key.code {
                        KeyCode::Up | KeyCode::Down => {
                            changed = move_selection(
                                &mut detail.selection,
                                playlist.entries.len(),
                                key.code == KeyCode::Down,
                            );
                        }
                        KeyCode::PageUp | KeyCode::PageDown => {
                            changed = move_selection_page(
                                &mut detail.selection,
                                playlist.entries.len(),
                                page_size,
                                key.code == KeyCode::PageDown,
                            );
                        }
                        KeyCode::Enter => {
                            if let Some(index) = detail.selection.selected()
                                && let Some(entry) = playlist.entries.get(index)
                            {
                                command = Some(Command::Play {
                                    track_id: entry.track_id,
                                });
                            }
                        }
                        KeyCode::Char('a') => {
                            if let Some(index) = detail.selection.selected()
                                && let Some(entry) = playlist.entries.get(index)
                            {
                                command = Some(Command::Enqueue {
                                    track_ids: vec![entry.track_id],
                                });
                            }
                        }
                        _ => {
                            self.view = View::PlaylistDetail { playlists, detail };
                            return key_action(key, state);
                        }
                    }
                }
                self.view = if close {
                    View::Playlists(playlists)
                } else {
                    View::PlaylistDetail { playlists, detail }
                };
                if changed || close {
                    self.mark_local_change();
                }
                command.map_or(KeyAction::Ignored, KeyAction::Command)
            }
            View::Playlists(mut selection) => {
                let mut changed = false;
                let mut command = None;
                let close = key.code == KeyCode::Esc;
                if !close {
                    match key.code {
                        KeyCode::Up | KeyCode::Down => {
                            changed = move_selection(
                                &mut selection,
                                state.library.playlists.len(),
                                key.code == KeyCode::Down,
                            );
                        }
                        KeyCode::PageUp | KeyCode::PageDown => {
                            changed = move_selection_page(
                                &mut selection,
                                state.library.playlists.len(),
                                page_size,
                                key.code == KeyCode::PageDown,
                            );
                        }
                        KeyCode::Right => {
                            if let Some(index) = selection.selected()
                                && let Some(playlist) = state.library.playlists.get(index)
                            {
                                self.view = View::PlaylistDetail {
                                    playlists: selection,
                                    detail: PlaylistDetail {
                                        playlist_id: playlist.id,
                                        selection: ListState::default().with_selected(
                                            (!playlist.entries.is_empty()).then_some(0),
                                        ),
                                    },
                                };
                                self.mark_local_change();
                                return KeyAction::Ignored;
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(index) = selection.selected()
                                && let Some(playlist) = state.library.playlists.get(index)
                            {
                                command = Some(Command::PlayPlaylist {
                                    playlist_id: playlist.id,
                                });
                            }
                        }
                        _ => {
                            self.view = View::Playlists(selection);
                            return key_action(key, state);
                        }
                    }
                }
                self.view = if close {
                    View::Queue
                } else {
                    View::Playlists(selection)
                };
                if changed || close {
                    self.mark_local_change();
                }
                command.map_or(KeyAction::Ignored, KeyAction::Command)
            }
            View::Queue => {
                let changed = match key.code {
                    KeyCode::Char('/') => return KeyAction::Search,
                    KeyCode::Char('t') => return KeyAction::Tree,
                    KeyCode::Char('P') => {
                        self.view = View::Playlists(ListState::default());
                        self.mark_local_change();
                        return KeyAction::Ignored;
                    }
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
                        return self
                            .queue
                            .selected()
                            .and_then(|index| state.queue.entries.get(index))
                            .map_or(KeyAction::Ignored, |entry| {
                                KeyAction::Command(Command::PlayQueue { queue_id: entry.id })
                            });
                    }
                    KeyCode::Char('d') | KeyCode::Delete => {
                        return self
                            .queue
                            .selected()
                            .and_then(|index| state.queue.entries.get(index))
                            .map_or(KeyAction::Ignored, |entry| {
                                KeyAction::Command(Command::RemoveQueue { queue_id: entry.id })
                            });
                    }
                    _ => return key_action(key, state),
                };
                if changed {
                    self.mark_local_change();
                }
                KeyAction::Ignored
            }
        }
    }
}
struct PlaylistDetail {
    playlist_id: i64,
    selection: ListState,
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

/// A library snapshot indexed into a flat directory-first tree.
/// Each configured library root is a top-level node; unconfigured tracks use a fallback tree.
struct LibraryTree {
    library: Arc<Vec<Track>>,
    directories: BTreeMap<PathBuf, Vec<LibraryTreeEntry>>,
    root: PathBuf,
    directory: PathBuf,
    selection: ListState,
}

enum LibraryTreeEntry {
    Directory(PathBuf),
    Track(usize),
}

impl LibraryTree {
    fn new(library: Arc<Vec<Track>>, library_roots: Arc<Vec<PathBuf>>) -> Self {
        let root = PathBuf::new();
        let mut directories = BTreeMap::from([(root.clone(), Vec::new())]);

        for configured_root in library_roots
            .iter()
            .filter(|path| !path.as_os_str().is_empty())
        {
            if directories.contains_key(configured_root) {
                continue;
            }
            directories.insert(configured_root.clone(), Vec::new());
            directories
                .get_mut(&root)
                .expect("tree root is indexed")
                .push(LibraryTreeEntry::Directory(configured_root.clone()));
        }

        for (index, track) in library.iter().enumerate() {
            let track_directory = track.path.parent().unwrap_or_else(|| Path::new(""));
            let configured_root = library_roots
                .iter()
                .filter(|root| !root.as_os_str().is_empty() && track.path.starts_with(root))
                .max_by_key(|root| root.components().count());
            let base = configured_root.unwrap_or(&root);
            let mut path = track_directory;
            let mut ancestors = Vec::new();
            while path != base && !directories.contains_key(path) {
                ancestors.push(path);
                path = path.parent().unwrap_or(base);
            }
            if !directories.contains_key(path) {
                path = base;
            }
            for child in ancestors.drain(..).rev() {
                directories
                    .get_mut(path)
                    .expect("parent directory is indexed")
                    .push(LibraryTreeEntry::Directory(child.to_path_buf()));
                directories.insert(child.to_path_buf(), Vec::new());
                path = child;
            }
            directories
                .get_mut(track_directory)
                .expect("track directory is indexed")
                .push(LibraryTreeEntry::Track(index));
        }
        for entries in directories.values_mut() {
            entries.sort_by(|left, right| match (left, right) {
                (LibraryTreeEntry::Directory(left), LibraryTreeEntry::Directory(right)) => {
                    left.cmp(right)
                }
                (LibraryTreeEntry::Directory(_), LibraryTreeEntry::Track(_)) => {
                    std::cmp::Ordering::Less
                }
                (LibraryTreeEntry::Track(_), LibraryTreeEntry::Directory(_)) => {
                    std::cmp::Ordering::Greater
                }
                (LibraryTreeEntry::Track(left), LibraryTreeEntry::Track(right)) => {
                    library[*left].path.cmp(&library[*right].path)
                }
            });
        }
        let selection =
            ListState::default().with_selected((!directories[&root].is_empty()).then_some(0));
        Self {
            library,
            directories,
            directory: root.clone(),
            root,
            selection,
        }
    }

    fn entries(&self) -> &[LibraryTreeEntry] {
        &self.directories[&self.directory]
    }

    fn activate(&mut self) -> Option<Command> {
        let entry = self.entries().get(self.selection.selected()?)?;
        match entry {
            LibraryTreeEntry::Directory(path) => {
                self.directory = path.clone();
                self.selection =
                    ListState::default().with_selected((!self.entries().is_empty()).then_some(0));
                None
            }
            LibraryTreeEntry::Track(index) => Some(Command::Enqueue {
                track_ids: vec![self.library[*index].id],
            }),
        }
    }

    fn parent(&mut self) -> bool {
        if self.directory == self.root {
            return false;
        }
        let parent = self.directory.parent().unwrap_or(&self.root).to_path_buf();
        let child = std::mem::replace(&mut self.directory, parent);
        let selected = self
            .entries()
            .iter()
            .position(|entry| matches!(entry, LibraryTreeEntry::Directory(path) if *path == child));
        self.selection = ListState::default().with_selected(selected);
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
        View::PlaylistDetail { detail, .. } => draw_playlist_detail(
            frame,
            columns[0],
            state,
            detail,
            &mut ui.playlist_cache,
            state.system.nerd_symbols,
        ),
        View::Playlists(selection) => draw_playlists(
            frame,
            columns[0],
            state,
            selection,
            state.system.nerd_symbols,
        ),
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
#[derive(Default)]
struct PlaylistDetailCache {
    playlist_id: Option<i64>,
    library_revision: u64,
    entry_track_ids: Vec<i64>,
    nerd_symbols: bool,
    rows: Vec<String>,
}

impl PlaylistDetailCache {
    fn sync(&mut self, state: &TuiSnapshot, playlist: &crate::model::Playlist, nerd_symbols: bool) {
        let entry_track_ids = playlist.entries.iter().map(|entry| entry.track_id);
        if self.playlist_id == Some(playlist.id)
            && self.library_revision == state.library.revision
            && self.nerd_symbols == nerd_symbols
            && self.entry_track_ids.len() == playlist.entries.len()
            && self
                .entry_track_ids
                .iter()
                .copied()
                .eq(entry_track_ids.clone())
        {
            return;
        }
        let marker = if nerd_symbols { "󰎆 " } else { "♪ " };
        let tracks = state
            .library
            .tracks
            .iter()
            .map(|track| (track.id, track))
            .collect::<HashMap<_, _>>();
        self.rows = playlist
            .entries
            .iter()
            .map(|entry| {
                tracks
                    .get(&entry.track_id)
                    .map(|track| format!("{marker}{} — {}", track.artist, track.title))
                    .unwrap_or_else(|| format!("{marker}Missing track"))
            })
            .collect();
        self.playlist_id = Some(playlist.id);
        self.library_revision = state.library.revision;
        self.entry_track_ids = playlist
            .entries
            .iter()
            .map(|entry| entry.track_id)
            .collect();
        self.nerd_symbols = nerd_symbols;
    }
}

impl QueueViewCache {
    fn matches(&self, state: &TuiSnapshot) -> bool {
        self.queue.len() == state.queue.entries.len()
            && self
                .queue
                .iter()
                .zip(state.queue.entries.iter())
                .all(|(&(id, track_id), entry)| id == entry.id && track_id == entry.track_id)
            && self.tracks.len() == state.library.tracks.len()
            && self
                .tracks
                .iter()
                .zip(state.library.tracks.iter())
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
            .library
            .tracks
            .iter()
            .map(|track| QueueTrackKey {
                id: track.id,
                title: track.title.clone(),
                artist: track.artist.clone(),
            })
            .collect();
        let mut track_index = HashMap::with_capacity(state.library.tracks.len());
        for (index, track) in state.library.tracks.iter().enumerate() {
            track_index.insert(track.id, index);
        }
        self.rows = state
            .queue
            .entries
            .iter()
            .map(|entry| {
                track_index
                    .get(&entry.track_id)
                    .and_then(|&index| state.library.tracks.get(index))
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
fn draw_playlist_detail(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &TuiSnapshot,
    detail: &mut PlaylistDetail,
    cache: &mut PlaylistDetailCache,
    nerd_symbols: bool,
) {
    let Some(playlist) = state
        .library
        .playlists
        .iter()
        .find(|playlist| playlist.id == detail.playlist_id)
    else {
        frame.render_widget(
            Block::default()
                .title("Playlist not found")
                .borders(Borders::ALL),
            area,
        );
        return;
    };
    cache.sync(state, playlist, nerd_symbols);
    let items = cache.rows.iter().map(|row| ListItem::new(row.as_str()));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(format!(
                        "Playlist — {} ({} tracks)",
                        playlist.name,
                        playlist.entries.len()
                    ))
                    .borders(Borders::ALL),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> "),
        area,
        &mut detail.selection,
    );
}

fn draw_playlists(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &TuiSnapshot,
    selection: &mut ListState,
    nerd_symbols: bool,
) {
    let items = state.library.playlists.iter().map(|playlist| {
        let marker = if nerd_symbols { "󰲋 " } else { "[P] " };
        ListItem::new(format!(
            "{marker}{}  ({} tracks)",
            playlist.name,
            playlist.entries.len()
        ))
    });
    let title = if state.library.playlists.is_empty() {
        "Playlists — empty"
    } else {
        "Playlists"
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
    let items = search.matches.iter().map(|&index| {
        let track = &search.library[index];
        ListItem::new(format!(
            "{} — {} [{}]{}",
            track.artist,
            track.title,
            track.album,
            if track.missing { " [missing]" } else { "" }
        ))
    });
    let title = if search.matches.is_empty() {
        "Library — no matches".into()
    } else {
        format!("Library — {} matches", search.matches.len())
    };
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(if search.focus == SearchFocus::Results {
                        title
                    } else {
                        format!("{title} — Tab to focus")
                    })
                    .borders(Borders::ALL),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> "),
        rows[1],
        &mut search.selection,
    );
}

fn draw_tree(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    tree: &mut LibraryTree,
    nerd_symbols: bool,
) {
    let entries = &tree.directories[&tree.directory];
    let items = entries.iter().map(|entry| match entry {
        LibraryTreeEntry::Directory(path) => {
            let marker = if nerd_symbols { "\u{f07b} " } else { "[dir] " };
            let name = path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy();
            ListItem::new(format!("{marker}{name}/"))
        }
        LibraryTreeEntry::Track(index) => {
            let track = &tree.library[*index];
            let marker = if nerd_symbols { "\u{f001} " } else { "      " };
            let name = track
                .path
                .file_name()
                .unwrap_or(track.path.as_os_str())
                .to_string_lossy();
            ListItem::new(format!(
                "{marker}{name} — {} — {}{}",
                track.artist,
                track.title,
                if track.missing { " [missing]" } else { "" }
            ))
        }
    });
    let directory = if tree.directory.as_os_str().is_empty() {
        "Library tree".into()
    } else {
        format!("Library tree — {}", tree.directory.display())
    };
    let title = if entries.is_empty() {
        format!("{directory} — empty")
    } else {
        directory
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
        &mut tree.selection,
    );
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
    use super::*;
    use crate::model::PlaybackState;
    use crate::model::{LibrarySnapshot, Playlist, PlaylistEntry, QueueEntry, QueueState};
    use crate::projection::{TuiLibrarySnapshot, TuiSystemSnapshot};
    use crossterm::event::KeyModifiers;

    fn track(id: i64, artist: &str, title: &str) -> Track {
        Track {
            id,
            path: PathBuf::from(format!("/music/{id}.flac")),
            fingerprint: None,
            cue: None,
            title: title.into(),
            artist: artist.into(),
            album: "album".into(),
            duration: Some(1.0),
            codec: "flac".into(),
            channels: 2,
            sample_rate: 44_100,
            bitrate_bps: None,
            track_number: None,
            disc_number: None,
            bits_per_sample: None,
            release_date: None,
            favorite: false,
            missing: false,
            play_count: 0,
            last_played: None,
        }
    }

    fn tui_snapshot(library: LibrarySnapshot, queue: QueueState) -> TuiSnapshot {
        TuiSnapshot {
            library: TuiLibrarySnapshot {
                revision: library.revision,
                tracks: library.tracks,
                playlists: library.playlists,
            },
            queue,
            playback: PlaybackState::default(),
            system: TuiSystemSnapshot::default(),
        }
    }

    #[test]
    fn tree_has_each_configured_root_at_top_level() {
        let mut first = track(1, "Artist", "First");
        first.path = PathBuf::from("/music/first.flac");
        let mut second = track(2, "Artist", "Second");
        second.path = PathBuf::from("/podcasts/second.flac");
        let tree = LibraryTree::new(
            Arc::new(vec![first, second]),
            Arc::new(vec![PathBuf::from("/music"), PathBuf::from("/podcasts")]),
        );

        let roots: Vec<_> = tree
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                LibraryTreeEntry::Directory(path) => Some(path.clone()),
                LibraryTreeEntry::Track(_) => None,
            })
            .collect();
        assert_eq!(
            roots,
            vec![PathBuf::from("/music"), PathBuf::from("/podcasts")]
        );
    }

    #[test]
    fn revision_unchanged_local_selection_is_visible() {
        let state = tui_snapshot(
            LibrarySnapshot::default(),
            QueueState {
                entries: Arc::new(vec![
                    QueueEntry { id: 1, track_id: 1 },
                    QueueEntry { id: 2, track_id: 2 },
                ]),
                current_id: None,
            },
        );
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
        let previous = tui_snapshot(
            LibrarySnapshot::default(),
            QueueState {
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
            },
        );
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
        let mut state = tui_snapshot(
            LibrarySnapshot {
                tracks: Arc::new(vec![
                    track(1, "Artist A", "Title A"),
                    track(2, "Artist B", "Title B"),
                ]),
                ..LibrarySnapshot::default()
            },
            QueueState {
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
            },
        );
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
        state.library.tracks = Arc::new(vec![changed, track(2, "Artist B", "Title B")]);
        cache.sync(&state);
        assert_eq!(cache.rows[1], "Artist A — Title A (remastered)");
    }
    #[test]
    fn playlist_detail_plays_or_enqueues_selected_track() {
        let library = LibrarySnapshot {
            tracks: Arc::new(vec![track(1, "Artist", "Title")]),
            playlists: Arc::new(vec![Playlist {
                id: 7,
                name: "Mix".into(),
                entries: vec![PlaylistEntry { id: 9, track_id: 1 }],
            }]),
            ..LibrarySnapshot::default()
        };
        let state = tui_snapshot(library, QueueState::default());
        let mut ui = UiState::default();
        ui.view = View::Playlists(ListState::default().with_selected(Some(0)));
        assert!(matches!(
            ui.key_action(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &state),
            KeyAction::Ignored
        ));
        assert!(matches!(&ui.view, View::PlaylistDetail { .. }));
        assert!(matches!(
            ui.key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &state),
            KeyAction::Command(Command::Play { track_id: 1 })
        ));
        assert!(matches!(
            ui.key_action(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &state),
            KeyAction::Command(Command::Enqueue { track_ids }) if track_ids == vec![1]
        ));
        assert!(matches!(
            ui.key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &state),
            KeyAction::Ignored
        ));
        assert!(matches!(&ui.view, View::Playlists(_)));
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
    fn tui_state_reuses_tracks_when_library_revision_is_unchanged() {
        let mut first = tui_snapshot(LibrarySnapshot::default(), QueueState::default());
        first.library.revision = 9;
        let old_tracks = Arc::clone(&first.library.tracks);
        let mut next = first.clone();
        next.library.tracks = Arc::new(Vec::new());

        let mut tui = TuiState::new(first);
        tui.apply_snapshot(next);

        assert!(Arc::ptr_eq(&tui.snapshot.library.tracks, &old_tracks));
    }
}
