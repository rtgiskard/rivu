mod render;
mod worker;

use render::{draw, draw_loading, track_label};
use worker::{
    QueryKind, QueryRequest, QueryResult, Watcher, spawn_command_worker, spawn_query_worker,
    spawn_watcher,
};

use crate::{
    i18n::Localizer,
    keyboard::{self, Action, Context as KeyboardContext, Modifiers},
    model::{
        Command, DirectoryRow, LibraryRow, PlaybackStatus, PlaylistEntryRow, PlaylistSummary,
        Query, QueueRow,
    },
    projection::{QueryProjection, TuiSnapshot},
    response::ViewResponse,
};
use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend, widgets::ListState};
use signal_hook::{
    SigId,
    consts::{SIGINT, SIGTERM},
};
use std::{
    borrow::Cow,
    collections::HashMap,
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;

/// Connect to an existing core over IPC. This never creates an audio engine and
/// quitting the screen only restores the terminal; the server keeps playing.
pub fn run(data_dir: &Path, language: &str) -> Result<()> {
    let termination = TerminalTermination::new().context("register terminal shutdown signals")?;
    let i18n = Arc::new(Localizer::new(language));
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
    let result = run_loop(data_dir, &mut terminal, i18n, &termination.requested);
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

/// Signal handlers only request exit; terminal and IPC cleanup stays on the owner.
struct TerminalTermination {
    requested: Arc<AtomicBool>,
    registrations: [SigId; 2],
}

impl TerminalTermination {
    fn new() -> io::Result<Self> {
        let requested = Arc::new(AtomicBool::new(false));
        let interrupt = signal_hook::flag::register(SIGINT, Arc::clone(&requested))?;
        let terminate = match signal_hook::flag::register(SIGTERM, Arc::clone(&requested)) {
            Ok(registration) => registration,
            Err(error) => {
                signal_hook::low_level::unregister(interrupt);
                return Err(error);
            }
        };
        Ok(Self {
            requested,
            registrations: [interrupt, terminate],
        })
    }
}

impl Drop for TerminalTermination {
    fn drop(&mut self) {
        for registration in self.registrations {
            signal_hook::low_level::unregister(registration);
        }
    }
}

fn run_loop(
    data_dir: &Path,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    i18n: Arc<Localizer>,
    terminating: &AtomicBool,
) -> Result<()> {
    run_loop_with_events(
        data_dir,
        terminal,
        i18n,
        terminating,
        spawn_watcher(data_dir),
        |timeout| {
            if event::poll(timeout)? {
                event::read().map(Some)
            } else {
                Ok(None)
            }
        },
    )
}

fn run_loop_with_events<B: ratatui::backend::Backend>(
    data_dir: &Path,
    terminal: &mut Terminal<B>,
    i18n: Arc<Localizer>,
    terminating: &AtomicBool,
    updates: Watcher,
    mut next_event: impl FnMut(Duration) -> io::Result<Option<Event>>,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let mut workers = None;
    let mut tui = None::<TuiState>;
    let mut redraw = true;
    let mut immediate_redraw = true;
    let mut last_draw = Instant::now() - Duration::from_millis(100);
    // Reuse the input loop's existing bounded wait; signals need no extra worker
    // or periodic wakeup and are observed even before the IPC handshake finishes.
    while !terminating.load(Ordering::Relaxed) {
        if let Some((instance_id, state)) = updates.take_update()? {
            // Shutdown belongs to the connection owner, not the projection or
            // query workers. Exit before creating workers or dispatching work,
            // including when shutdown is the loading screen's first snapshot.
            if state.system.shutting_down {
                break;
            }
            if workers.is_none() {
                workers = Some((
                    spawn_command_worker(data_dir, instance_id),
                    spawn_query_worker(data_dir, instance_id),
                ));
            }
            let next = TuiSnapshot::from_client(&state, tui.as_ref().map(|tui| &tui.snapshot));
            match &mut tui {
                Some(tui) => tui.apply_snapshot(next),
                None => tui = Some(TuiState::new(next, Arc::clone(&i18n))),
            }
            redraw = true;
        }
        let Some(tui) = &mut tui else {
            if redraw {
                terminal.draw(|frame| draw_loading(frame, &i18n))?;
                redraw = false;
            }
            if let Some(event) = next_event(Duration::from_millis(50))? {
                match event {
                    Event::Key(key)
                        if key.kind != KeyEventKind::Release
                            && resolve_key(key, KeyboardContext::TuiQueue)
                                == Some(Action::Close) =>
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
                    tui.ui.set_notice(
                        ack.error
                            .unwrap_or_else(|| i18n.text("terminal-command-rejected").into()),
                        NoticeKind::Error,
                    );
                }
                Ok(_) => {}
                Err(error) => tui.ui.set_notice(error.into_message()?, NoticeKind::Error),
            }
            redraw = true;
            immediate_redraw = true;
        }
        if tui.ui.prepare_local_view(&tui.snapshot) {
            redraw = true;
            immediate_redraw = true;
        }
        let local_revision = tui.ui.local_revision;
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
        if tui.ui.local_revision != local_revision {
            redraw = true;
            immediate_redraw = true;
        }
        if tui.ui.expire_message() | tui.ui.expire_startup_hint(Instant::now()) {
            redraw = true;
            immediate_redraw = true;
        }
        if redraw && (immediate_redraw || last_draw.elapsed() >= Duration::from_millis(100)) {
            terminal.draw(|frame| draw(frame, &tui.snapshot, &mut tui.ui))?;
            last_draw = Instant::now();
            redraw = false;
            immediate_redraw = false;
        }
        if let Some(event) = next_event(Duration::from_millis(50))? {
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
                        KeyAction::Search => {
                            tui.ui.open_search();
                            tui.snapshot.clone()
                        }
                        KeyAction::Command(command) => {
                            let queued = commands
                                .sender
                                .as_ref()
                                .is_some_and(|sender| sender.try_send(command).is_ok());
                            if !queued {
                                tui.ui.set_notice(
                                    i18n.text("terminal-command-busy").into(),
                                    NoticeKind::Warning,
                                );
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
    fn new(snapshot: TuiSnapshot, i18n: Arc<Localizer>) -> Self {
        let mut ui = UiState::new(i18n);
        ui.queue.total = snapshot.queue.total;
        ui.queue.loaded = true;
        ui.queue
            .selection
            .select((snapshot.queue.total != 0).then_some(0));
        Self { snapshot, ui }
    }

    fn apply_snapshot(&mut self, next: TuiSnapshot) {
        let page_size_changed = self.snapshot.system.page_size != next.system.page_size;
        self.ui.sync_queue(&self.snapshot, &next);
        if page_size_changed {
            self.ui.queue.pages.clear();
            self.ui.queue.fetch_size = 0;
        }
        let active_changed = self
            .ui
            .view
            .sync_snapshot(&self.snapshot, &next, page_size_changed);
        if let Some(tree) = &mut self.ui.cached_library {
            tree.sync_snapshot(&self.snapshot, &next, page_size_changed);
        }
        if let Some(playlists) = &mut self.ui.cached_playlists {
            playlists.sync_snapshot(&self.snapshot, &next, page_size_changed);
        }
        if active_changed || page_size_changed {
            self.ui.cancel_query();
        }
        if let Some(message) = snapshot_message(&self.snapshot, &next, &self.ui.i18n) {
            self.ui.set_notice(message, NoticeKind::Information);
        }
        self.snapshot = next;
    }
}

fn snapshot_message(
    previous: &TuiSnapshot,
    next: &TuiSnapshot,
    i18n: &Localizer,
) -> Option<String> {
    if previous.playback.status != next.playback.status {
        return Some(match next.playback.status {
            PlaybackStatus::Playing => i18n.text("terminal-playing").into(),
            PlaybackStatus::Paused => i18n.text("terminal-paused").into(),
            PlaybackStatus::Stopped => i18n.text("terminal-stopped").into(),
        });
    }
    if previous.queue.current_id != next.queue.current_id {
        return next.current_track().map(|track| {
            let label = track_label(&track.title, &track.artist);
            i18n.format("terminal-now-playing", &[("track", label.as_ref().into())])
        });
    }
    match next.queue.total.cmp(&previous.queue.total) {
        std::cmp::Ordering::Greater => Some(i18n.text("terminal-queue-added").into()),
        std::cmp::Ordering::Less => Some(i18n.text("terminal-queue-removed").into()),
        std::cmp::Ordering::Equal => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NoticeKind {
    Information,
    Warning,
    Error,
}

struct Notice {
    text: String,
    kind: NoticeKind,
    expires_at: Instant,
}

#[derive(Default)]
struct Details {
    target: Option<i64>,
    source_label: &'static str,
    state: DetailState,
    stamp: Option<crate::model::QueryStamp>,
    scroll: usize,
    max_scroll: usize,
}

#[derive(Default)]
enum DetailState {
    #[default]
    Loading,
    Ready(Box<crate::model::Track>),
    Removed,
    Error(String),
    Container(String),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TopTab {
    Queue,
    Playlists,
    Library,
}

impl View {
    fn tab(&self) -> TopTab {
        match self {
            Self::Queue => TopTab::Queue,
            Self::Playlists(_) | Self::PlaylistDetail { .. } => TopTab::Playlists,
            Self::Tree(_) | Self::Search(_) => TopTab::Library,
        }
    }

    fn query_kind(&self, selection: bool) -> Option<QueryKind> {
        match self {
            Self::Queue => Some(QueryKind::Queue),
            Self::Search(search) => Some(QueryKind::Search(search.applied_query.clone())),
            Self::Tree(tree) if !tree.directory.as_os_str().is_empty() => {
                Some(QueryKind::Directory(tree.directory.clone()))
            }
            Self::Playlists(_) => Some(QueryKind::Playlists),
            Self::PlaylistDetail { detail, .. } => Some(match detail.entry_anchor {
                Some(entry_id) if selection => QueryKind::PlaylistSelection {
                    playlist_id: detail.playlist_id,
                    entry_id,
                },
                _ => QueryKind::Playlist(detail.playlist_id),
            }),
            _ => None,
        }
    }

    // Hidden pages follow the same revision rules as the visible page, without
    // fetching data or disturbing the visible page's query generation.
    fn sync_snapshot(
        &mut self,
        previous: &TuiSnapshot,
        next: &TuiSnapshot,
        page_size_changed: bool,
    ) -> bool {
        let rows_changed = self
            .query_kind(false)
            .is_some_and(|kind| kind.changed(previous, next));
        let query_changed = if matches!(self, Self::PlaylistDetail { detail, .. } if detail.entry_anchor.is_some())
        {
            self.query_kind(true)
                .is_some_and(|kind| kind.changed(previous, next))
        } else {
            rows_changed
        };
        if let Self::PlaylistDetail { detail, .. } = self {
            let compact = Query::PlaylistEntryLocations {
                query: None,
                playlist_id: detail.playlist_id,
                entry_ids: Vec::new(),
            };
            let scope = Some(detail.playlist_id);
            if compact.stamp(previous.query_revisions(scope))
                != compact.stamp(next.query_revisions(scope))
            {
                detail.entry_anchor = detail
                    .entry_anchor
                    .or(detail.selected_entry_id)
                    .or_else(|| detail.entries.selected().map(|entry| entry.id));
            }
            if !next
                .library
                .playlist_entry_revisions
                .contains_key(&detail.playlist_id)
            {
                let Self::PlaylistDetail { playlists, .. } = std::mem::take(self) else {
                    unreachable!();
                };
                *self = Self::Playlists(PagedList {
                    selection: playlists.0,
                    viewport_rows: playlists.1,
                    ..PagedList::default()
                });
                return true;
            }
        }
        if rows_changed || page_size_changed {
            match self {
                Self::Search(search) => search.results.clear(),
                Self::Tree(tree) => tree.entries.clear(),
                Self::Playlists(playlists) => playlists.clear(),
                Self::PlaylistDetail { detail, .. } => detail.entries.clear(),
                Self::Queue => {}
            }
        }
        if previous.system.library_roots != next.system.library_roots
            && let Self::Tree(tree) = self
        {
            tree.roots = LibraryTree::roots(&next.system.library_roots);
            if tree.directory.as_os_str().is_empty() {
                tree.entries.clear();
            }
        }
        query_changed && !matches!(self, Self::Queue)
    }
}

struct UiState {
    i18n: Arc<Localizer>,
    view: View,
    // Each slot owns only its tab's view; switching moves bounded page caches.
    cached_library: Option<View>,
    cached_playlists: Option<View>,
    queue: PagedList<QueueRow>,
    // Resolve the selected occurrence after edits before requesting its new viewport.
    queue_anchor: Option<u64>,
    queue_selected_id: Option<u64>,
    show_help: bool,
    help_max_scroll: usize,
    help_scroll: usize,
    message: Option<Notice>,
    // Started only after the initial IPC snapshot; None is permanently dismissed.
    startup_hint_deadline: Option<Instant>,
    details: Details,
    details_active: bool,
    requested: Option<QueryRequest>,
    projection: QueryProjection,
    failed_request: Option<QueryRequest>,
    local_revision: u64,
    // Keyboard PageUp/PageDown distance follows the terminal viewport, not query limits.
    page_size: usize,
}
impl UiState {
    fn new(i18n: Arc<Localizer>) -> Self {
        Self {
            i18n,
            view: View::default(),
            cached_library: None,
            cached_playlists: None,
            queue: PagedList::default(),
            queue_anchor: None,
            queue_selected_id: None,
            show_help: false,
            help_max_scroll: 0,
            help_scroll: 0,
            message: None,
            startup_hint_deadline: Some(Instant::now() + Duration::from_secs(5)),
            details: Details::default(),
            details_active: false,
            requested: None,
            projection: QueryProjection::default(),
            failed_request: None,
            local_revision: 0,
            page_size: 0,
        }
    }

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

    fn query_request(&mut self, state: &TuiSnapshot) -> Option<QueryRequest> {
        if self.details_active {
            let kind = QueryKind::Track(self.details.target?);
            let query = kind.query(0, 1);
            let revisions = state.query_revisions(None).for_query(
                &query,
                &state.library.playlist_entry_revisions,
                &state.library.metadata_revisions,
            );
            let stamp = query.stamp(revisions);
            if self.details.stamp == Some(stamp) {
                return None;
            }
            self.details.state = DetailState::Loading;
            self.projection.sync(&query, revisions);
            return Some(QueryRequest {
                kind,
                offset: 0,
                limit: 1,
                generation: self.projection.generation(),
                stamp,
            });
        }
        if let View::Search(search) = &mut self.view
            && search.apply_pending(Instant::now())
        {
            self.cancel_query();
            self.mark_local_change();
        }
        let limit = state.system.page_size as usize;
        let (kind, offset) = match &self.view {
            View::Queue if state.queue.total != 0 => match self.queue_anchor {
                Some(queue_id) => (QueryKind::QueueSelection(queue_id), 0),
                None => (QueryKind::Queue, self.queue.next_offset(limit)?),
            },
            View::Search(search) => {
                let offset = search.results.next_offset(limit)?;
                (QueryKind::Search(search.applied_query.clone()), offset)
            }
            View::Tree(tree) if !tree.directory.as_os_str().is_empty() => (
                QueryKind::Directory(tree.directory.clone()),
                tree.entries.next_offset(limit)?,
            ),
            View::Playlists(playlists) => (QueryKind::Playlists, playlists.next_offset(limit)?),
            View::PlaylistDetail { detail, .. }
                if state
                    .library
                    .playlist_entry_revisions
                    .contains_key(&detail.playlist_id) =>
            {
                match detail.entry_anchor {
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
                }
            }
            _ => return None,
        };
        let query = kind.query(offset, limit);
        let revisions = state.query_revisions(kind.playlist_id());
        self.projection.sync(&query, revisions);
        let request = QueryRequest {
            kind,
            offset,
            limit,
            generation: self.projection.generation(),
            stamp: query.stamp(revisions),
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
                if self.details_active {
                    self.details.stamp = Some(request.stamp);
                    self.details.state = DetailState::Error(error);
                    self.mark_local_change();
                    return true;
                }
                self.set_notice(error, NoticeKind::Error);
                return true;
            }
        };
        if !self
            .projection
            .accepts(&request.query(), request.generation, result.revisions)
        {
            self.requested = None;
            return false;
        }
        if self.details_active {
            self.details.state = match result.view {
                ViewResponse::Track(Some(track)) if Some(track.id) == self.details.target => {
                    DetailState::Ready(Box::new(track))
                }
                ViewResponse::Track(None) => DetailState::Removed,
                _ => DetailState::Error(self.i18n.text("terminal-unexpected-response").into()),
            };
            self.details.stamp = Some(request.stamp);
            self.requested = None;
            self.mark_local_change();
            return true;
        }
        match (&mut self.view, result.view) {
            (View::Queue, ViewResponse::QueuePage(page)) if request.kind == QueryKind::Queue => {
                self.queue
                    .insert(request.offset, request.limit, page.total, page.rows);
                self.queue_selected_id = self.queue.selected().map(|row| row.id);
            }
            (View::Queue, ViewResponse::QueueEntries(rows)) => {
                if let QueryKind::QueueSelection(queue_id) = request.kind {
                    if let Some(row) = rows.iter().find(|row| row.id == queue_id) {
                        self.queue.selection.select(Some(row.position));
                        self.queue.set_viewport(self.queue.viewport_rows);
                    }
                    self.queue_selected_id =
                        rows.iter().find(|row| row.id == queue_id).map(|row| row.id);
                    self.queue_anchor = None;
                } else {
                    self.set_notice(
                        self.i18n.text("terminal-unexpected-response").into(),
                        NoticeKind::Error,
                    );
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
                detail.selected_entry_id = detail.entries.selected().map(|entry| entry.id);
            }
            (
                View::PlaylistDetail { detail, .. },
                ViewResponse::PlaylistEntryLocations(entries),
            ) => {
                if let QueryKind::PlaylistSelection {
                    playlist_id,
                    entry_id,
                } = request.kind
                {
                    let entry = entries
                        .into_iter()
                        .find(|entry| entry.id == entry_id && entry.playlist_id == playlist_id);
                    detail.selected_entry_id = entry.as_ref().map(|entry| entry.id);
                    if let Some(entry) = entry {
                        detail.entries.total = entry.total;
                        detail.entries.loaded = true;
                        detail.entries.selection.select(Some(entry.position));
                        detail.entries.set_viewport(detail.entries.viewport_rows);
                    }
                    detail.entry_anchor = None;
                } else {
                    self.set_notice(
                        self.i18n.text("terminal-unexpected-response").into(),
                        NoticeKind::Error,
                    );
                }
            }
            _ => {
                self.set_notice(
                    self.i18n.text("terminal-unexpected-response").into(),
                    NoticeKind::Error,
                );
                return true;
            }
        }
        self.mark_local_change();
        true
    }

    fn mark_local_change(&mut self) {
        self.local_revision = self.local_revision.wrapping_add(1);
    }

    fn open_details(&mut self) {
        let (target, source_label, container) = match &self.view {
            View::Queue => (
                self.queue.selected().map(|row| row.track.id),
                "terminal-queue",
                None,
            ),
            View::Search(search) => (
                search.results.selected().map(|track| track.id),
                "terminal-search",
                None,
            ),
            View::PlaylistDetail { detail, .. } => (
                detail.entries.selected().map(|entry| entry.track_id),
                "terminal-playlists",
                None,
            ),
            View::Tree(tree) => match tree.entries.selected() {
                Some(DirectoryRow::Track(track)) => (Some(track.id), "terminal-library", None),
                Some(DirectoryRow::Directory { path }) => (
                    None,
                    "terminal-library",
                    Some(path.to_string_lossy().into_owned()),
                ),
                None => (None, "terminal-library", None),
            },
            View::Playlists(playlists) => (
                None,
                "terminal-playlists",
                playlists.selected().map(|playlist| playlist.name.clone()),
            ),
        };
        self.details = Details {
            target,
            source_label,
            state: container.map_or(DetailState::Loading, DetailState::Container),
            ..Details::default()
        };
        self.details_active = true;
        self.startup_hint_deadline = None;
        self.cancel_query();
        self.mark_local_change();
    }

    fn set_notice(&mut self, text: String, kind: NoticeKind) {
        self.startup_hint_deadline = None;
        // Keep actionable failures visible when an unrelated playback update arrives.
        if kind == NoticeKind::Information
            && self.message.as_ref().is_some_and(|notice| {
                notice.kind != NoticeKind::Information && Instant::now() < notice.expires_at
            })
        {
            return;
        }
        self.message = Some(Notice {
            text,
            kind,
            expires_at: Instant::now() + Duration::from_secs(10),
        });
        self.mark_local_change();
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

    fn expire_startup_hint(&mut self, now: Instant) -> bool {
        if self
            .startup_hint_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.startup_hint_deadline = None;
            return true;
        }
        false
    }

    fn sync_queue(&mut self, previous: &TuiSnapshot, next: &TuiSnapshot) {
        if !QueryKind::Queue.changed(previous, next) {
            return;
        }
        self.queue_anchor = self
            .queue_anchor
            .or_else(|| self.queue.selected().map(|row| row.id))
            .or(self.queue_selected_id);
        let selected = self.queue.selection.selected().unwrap_or(0);
        self.queue.clear();
        self.queue.total = next.queue.total;
        self.queue.loaded = true;
        self.queue
            .selection
            .select((next.queue.total != 0).then(|| selected.min(next.queue.total - 1)));
        if next.queue.total == 0 {
            self.queue_anchor = None;
            self.queue_selected_id = None;
        }
        self.queue.set_viewport(self.queue.viewport_rows);
        if matches!(self.view, View::Queue) {
            self.cancel_query();
        }
    }

    fn cancel_query(&mut self) {
        self.requested = None;
        self.failed_request = None;
        self.projection.cancel();
    }

    fn open_search(&mut self) {
        self.store_page();
        self.view = View::Search(LibrarySearch::default());
        self.cancel_query();
        self.mark_local_change();
    }

    fn store_page(&mut self) {
        match std::mem::take(&mut self.view) {
            view @ View::Tree(_) => self.cached_library = Some(view),
            view @ (View::Playlists(_) | View::PlaylistDetail { .. }) => {
                self.cached_playlists = Some(view);
            }
            View::Queue | View::Search(_) => {}
        }
    }

    fn switch_tab(&mut self, backward: bool, state: &TuiSnapshot) {
        let target = match (self.view.tab(), backward) {
            (TopTab::Queue, false) | (TopTab::Library, true) => TopTab::Playlists,
            (TopTab::Playlists, false) | (TopTab::Queue, true) => TopTab::Library,
            (TopTab::Library, false) | (TopTab::Playlists, true) => TopTab::Queue,
        };
        self.store_page();
        self.view = match target {
            TopTab::Queue => View::Queue,
            TopTab::Playlists => self
                .cached_playlists
                .take()
                .unwrap_or_else(|| View::Playlists(PagedList::default())),
            TopTab::Library => self
                .cached_library
                .take()
                .unwrap_or_else(|| View::Tree(LibraryTree::new(&state.system.library_roots))),
        };
        self.startup_hint_deadline = None;
        self.cancel_query();
        self.mark_local_change();
    }

    fn keyboard_context(&self) -> KeyboardContext {
        match &self.view {
            View::Queue => KeyboardContext::TuiQueue,
            View::Search(search) if search.focus == SearchFocus::Query => {
                KeyboardContext::TuiSearchQuery
            }
            View::Search(_) => KeyboardContext::TuiSearchResults,
            View::Tree(_) => KeyboardContext::TuiTree,
            View::Playlists(_) => KeyboardContext::TuiPlaylists,
            View::PlaylistDetail { .. } => KeyboardContext::TuiPlaylistTracks,
        }
    }

    fn key_action(&mut self, key: KeyEvent, state: &TuiSnapshot) -> KeyAction {
        // Global terminal exit wins over editors and modal input ownership.
        if resolve_key(key, KeyboardContext::TuiHelp) == Some(Action::Close) {
            return KeyAction::Quit;
        }
        if self.details_active {
            let details = &mut self.details;
            match resolve_key(key, KeyboardContext::TuiHelp) {
                Some(Action::Cancel) => {
                    self.details_active = false;
                    self.cancel_query();
                }
                Some(Action::Up) => details.scroll = details.scroll.saturating_sub(1),
                Some(Action::Down) => {
                    details.scroll = details.scroll.saturating_add(1).min(details.max_scroll)
                }
                Some(Action::PageUp) => {
                    details.scroll = details.scroll.saturating_sub(self.page_size)
                }
                Some(Action::PageDown) => {
                    details.scroll = details
                        .scroll
                        .saturating_add(self.page_size)
                        .min(details.max_scroll)
                }
                Some(Action::First) => details.scroll = 0,
                Some(Action::Last) => details.scroll = details.max_scroll,
                _ => return KeyAction::Ignored,
            }
            self.mark_local_change();
            return KeyAction::Ignored;
        }
        let context = self.keyboard_context();
        if self.show_help {
            let action = resolve_key(key, KeyboardContext::TuiHelp);
            match action {
                Some(Action::Help | Action::Cancel) => self.show_help = false,
                Some(Action::Up) => self.help_scroll = self.help_scroll.saturating_sub(1),
                Some(Action::Down) => {
                    self.help_scroll = self.help_scroll.saturating_add(1).min(self.help_max_scroll)
                }
                Some(Action::PageUp) => {
                    self.help_scroll = self.help_scroll.saturating_sub(self.page_size)
                }
                Some(Action::PageDown) => {
                    self.help_scroll = self
                        .help_scroll
                        .saturating_add(self.page_size)
                        .min(self.help_max_scroll)
                }
                Some(Action::First) => self.help_scroll = 0,
                Some(Action::Last) => self.help_scroll = self.help_max_scroll,
                _ => return KeyAction::Ignored,
            }
            self.mark_local_change();
            return KeyAction::Ignored;
        }
        // The query editor owns all printable characters, including keys that
        // control playback elsewhere. Control/Alt/Super chords are never text.
        if context == KeyboardContext::TuiSearchQuery
            && !key.modifiers.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER
                    | KeyModifiers::HYPER
                    | KeyModifiers::META,
            )
            && let KeyCode::Char(character) = key.code
        {
            if let View::Search(search) = &mut self.view {
                search.query.push(character);
                if search.filter(Instant::now()) {
                    self.cancel_query();
                }
            }
            self.mark_local_change();
            return KeyAction::Ignored;
        }
        let Some(action) = resolve_key(key, context) else {
            return KeyAction::Ignored;
        };
        if matches!(action, Action::Help | Action::Search | Action::Traverse) {
            self.startup_hint_deadline = None;
        }
        match action {
            Action::Help => {
                self.show_help = true;
                self.help_scroll = 0;
                self.mark_local_change();
                return KeyAction::Ignored;
            }
            Action::Close => return KeyAction::Quit,
            Action::Details => {
                self.open_details();
                return KeyAction::Ignored;
            }
            Action::Favorite => {
                let track_id = match &self.view {
                    View::Queue => self.queue.selected().map(|row| row.track.id),
                    View::Search(search) => search.results.selected().map(|track| track.id),
                    View::PlaylistDetail { detail, .. } => {
                        detail.entries.selected().map(|entry| entry.track_id)
                    }
                    View::Tree(tree) => tree.entries.selected().and_then(|entry| match entry {
                        DirectoryRow::Track(track) => Some(track.id),
                        DirectoryRow::Directory { .. } => None,
                    }),
                    View::Playlists(_) => None,
                };
                return track_id.map_or(KeyAction::Ignored, |track_id| {
                    KeyAction::Command(Command::ToggleFavorite {
                        track_ids: vec![track_id],
                    })
                });
            }
            Action::Search => return KeyAction::Search,
            Action::Traverse if !matches!(self.view, View::Search(_)) => {
                self.switch_tab(
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT),
                    state,
                );
                return KeyAction::Ignored;
            }
            _ => {}
        }
        if let Some(command) =
            keyboard::playback_command(action, &state.playback, state.queue.current_id)
        {
            return KeyAction::Command(command);
        }
        let page_size = self.page_size;
        let view = std::mem::take(&mut self.view);
        let mut changed = false;
        let mut command = None;
        match view {
            View::Search(mut search) => {
                let close = action == Action::Cancel && search.focus == SearchFocus::Query;
                match action {
                    Action::Traverse => {
                        search.focus = match search.focus {
                            SearchFocus::Query => SearchFocus::Results,
                            SearchFocus::Results => SearchFocus::Query,
                        };
                        changed = true;
                    }
                    Action::Cancel if !close => {
                        search.focus = SearchFocus::Query;
                        changed = true;
                    }
                    Action::Activate if search.focus == SearchFocus::Results => {
                        command = search
                            .results
                            .selected()
                            .map(|track| Command::Play { track_id: track.id });
                    }
                    Action::Activate => {
                        search.focus = SearchFocus::Results;
                        changed = true;
                    }
                    Action::Backspace => {
                        if search.query.pop().is_some() {
                            if search.filter(Instant::now()) {
                                self.cancel_query();
                            }
                            changed = true;
                        }
                    }
                    _ => changed = search.results.navigate_action(action, page_size),
                }
                self.view = if close {
                    self.cancel_query();
                    View::Queue
                } else {
                    View::Search(search)
                };
                changed |= close;
            }
            View::Tree(mut tree) => {
                let mut close = false;
                match action {
                    Action::Cancel | Action::Parent => {
                        changed = tree.parent();
                        close = action == Action::Cancel && !changed;
                        if changed || close {
                            self.cancel_query();
                        }
                    }
                    Action::Activate | Action::Expand => {
                        let directory = tree.directory.clone();
                        if action == Action::Activate
                            || matches!(
                                tree.entries.selected(),
                                Some(DirectoryRow::Directory { .. })
                            )
                        {
                            command = tree.activate();
                        }
                        changed = tree.directory != directory;
                        if changed {
                            self.cancel_query();
                        }
                    }
                    _ => changed = tree.entries.navigate_action(action, page_size),
                }
                self.view = if close {
                    self.cached_library = Some(View::Tree(tree));
                    View::Queue
                } else {
                    View::Tree(tree)
                };
                changed |= close;
            }
            View::PlaylistDetail {
                playlists,
                mut detail,
            } => {
                let close = matches!(action, Action::Cancel | Action::Parent);
                match action {
                    Action::Activate => {
                        command = detail.entries.selected().map(|entry| Command::Play {
                            track_id: entry.track_id,
                        })
                    }
                    Action::Enqueue => {
                        command = detail.entries.selected().map(|entry| Command::Enqueue {
                            track_ids: vec![entry.track_id],
                        })
                    }
                    Action::Remove => {
                        command = detail
                            .entries
                            .selected()
                            .map(|entry| Command::RemovePlaylistEntry { entry_id: entry.id })
                    }
                    _ => {
                        changed = detail.entries.navigate_action(action, page_size);
                        if changed {
                            detail.entry_anchor = None;
                            detail.selected_entry_id =
                                detail.entries.selected().map(|entry| entry.id);
                        }
                    }
                }
                self.view = if close {
                    self.cancel_query();
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
                let close = action == Action::Cancel;
                match action {
                    Action::Expand => {
                        if let Some(playlist) = playlists.selected() {
                            let detail = PlaylistDetail {
                                playlist_id: playlist.id,
                                name: playlist.name.clone(),
                                entries: PagedList::default(),
                                entry_anchor: None,
                                selected_entry_id: None,
                            };
                            self.view = View::PlaylistDetail {
                                playlists: (playlists.selection, playlists.viewport_rows),
                                detail,
                            };
                            self.cancel_query();
                            self.mark_local_change();
                            return KeyAction::Ignored;
                        }
                    }
                    Action::Activate => {
                        command = playlists.selected().map(|playlist| Command::PlayPlaylist {
                            playlist_id: playlist.id,
                        })
                    }
                    _ => changed = playlists.navigate_action(action, page_size),
                }
                self.view = if close {
                    self.cancel_query();
                    self.cached_playlists = Some(View::Playlists(playlists));
                    View::Queue
                } else {
                    View::Playlists(playlists)
                };
                changed |= close;
            }
            View::Queue => match action {
                Action::Activate => {
                    command = self
                        .queue
                        .selected()
                        .map(|entry| Command::PlayQueue { queue_id: entry.id })
                }
                Action::Remove => {
                    command = self
                        .queue
                        .selected()
                        .map(|entry| Command::RemoveQueue { queue_id: entry.id })
                }
                _ => {
                    changed = self.queue.navigate_action(action, page_size);
                    if changed {
                        self.queue_anchor = None;
                        self.queue_selected_id = self.queue.selected().map(|row| row.id);
                    }
                }
            },
        }
        if changed {
            if matches!(action, Action::First | Action::Last) {
                self.cancel_query();
            }
            self.mark_local_change();
        }
        command.map_or(KeyAction::Ignored, KeyAction::Command)
    }
}

#[cfg(test)]
impl Default for UiState {
    fn default() -> Self {
        Self::new(Arc::new(Localizer::new("en-US")))
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

    fn navigate_action(&mut self, action: Action, page_size: usize) -> bool {
        if !self.loaded {
            return false;
        }
        let changed = match action {
            Action::Up | Action::Down => {
                move_selection(&mut self.selection, self.total, action == Action::Down)
            }
            Action::PageUp | Action::PageDown => move_selection_page(
                &mut self.selection,
                self.total,
                page_size,
                action == Action::PageDown,
            ),
            Action::First | Action::Last => {
                let next = if action == Action::First {
                    (self.total != 0).then_some(0)
                } else {
                    self.total.checked_sub(1)
                };
                let changed = next != self.selection.selected();
                self.selection.select(next);
                changed
            }
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
    selected_entry_id: Option<i64>,
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
    applied_query: String,
    results: PagedList<LibraryRow>,
    focus: SearchFocus,
    query_deadline: Option<Instant>,
}

impl LibrarySearch {
    fn filter(&mut self, now: Instant) -> bool {
        let query = self.query.trim();
        if query == self.applied_query {
            self.query_deadline = None;
            return false;
        }
        if query.is_empty() {
            self.query_deadline = None;
            self.applied_query.clear();
            self.results = PagedList::default();
            return true;
        }
        self.query_deadline = Some(now + crate::model::SEARCH_DEBOUNCE);
        false
    }

    fn apply_pending(&mut self, now: Instant) -> bool {
        if !self.query_deadline.is_some_and(|deadline| now >= deadline) {
            return false;
        }
        self.query_deadline = None;
        let query = self.query.trim();
        if query == self.applied_query {
            return false;
        }
        self.applied_query.clear();
        self.applied_query.push_str(query);
        self.results = PagedList::default();
        true
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
            DirectoryRow::Track(track) => Some(Command::Play { track_id: track.id }),
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

enum KeyAction {
    Quit,
    Search,
    Command(Command),
    Ignored,
}

fn resolve_key(key: KeyEvent, context: KeyboardContext) -> Option<Action> {
    if key
        .modifiers
        .intersects(KeyModifiers::HYPER | KeyModifiers::META)
    {
        return None;
    }
    let modifiers = Modifiers {
        control: key.modifiers.contains(KeyModifiers::CONTROL),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        shift: key.modifiers.contains(KeyModifiers::SHIFT) || key.code == KeyCode::BackTab,
        platform: key.modifiers.contains(KeyModifiers::SUPER),
        function: false,
    };
    let mut encoded = [0_u8; 4];
    let name = match key.code {
        KeyCode::Char(' ') => "space",
        KeyCode::Char(character) => character.encode_utf8(&mut encoded),
        KeyCode::Left => "left",
        KeyCode::Right => "right",
        KeyCode::Up => "up",
        KeyCode::Down => "down",
        KeyCode::Home => "home",
        KeyCode::End => "end",
        KeyCode::PageUp => "pageup",
        KeyCode::PageDown => "pagedown",
        KeyCode::Enter => "enter",
        KeyCode::Esc => "escape",
        KeyCode::Tab | KeyCode::BackTab => "tab",
        KeyCode::Backspace => "backspace",
        KeyCode::Delete => "delete",
        KeyCode::F(1) => "f1",
        _ => return None,
    };
    keyboard::resolve(name, modifiers, context)
}

fn help_bindings(context: KeyboardContext) -> impl Iterator<Item = keyboard::HelpRow> {
    keyboard::help_rows(&[context, KeyboardContext::TuiHelp])
}

#[cfg(test)]
mod search_tests;

#[cfg(test)]
mod tests;
