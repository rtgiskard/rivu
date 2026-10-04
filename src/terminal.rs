use crate::{
    ipc,
    model::{AppState, Command, PlaybackStatus, RepeatMode, Response, Track, playback_key_command},
};
use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    collections::{BTreeMap, HashMap},
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
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

fn run_loop(socket_path: &Path, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let mut state = request_state(socket_path)?;
    let mut ui = UiState::default();
    ui.sync_queue(&state, &state);
    let mut redraw = true;
    loop {
        if redraw {
            terminal.draw(|frame| draw(frame, &state, &mut ui))?;
            redraw = false;
        }
        if event::poll(Duration::from_millis(250))? {
            let event = event::read()?;
            match event {
                Event::Resize(_, _) => {
                    redraw = true;
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let local_revision = ui.local_revision;
                    let next = match ui.key_action(key, &state) {
                        KeyAction::Quit => break,
                        action @ (KeyAction::Search | KeyAction::Tree) => {
                            let response = ipc::request(socket_path, &Command::Status)?;
                            if response.ok {
                                let library = Arc::clone(&response.state.library);
                                if matches!(action, KeyAction::Tree) {
                                    ui.tree = Some(LibraryTree::new(library));
                                    ui.search = None;
                                } else {
                                    ui.search = Some(LibrarySearch::new(library));
                                    ui.tree = None;
                                }
                                ui.mark_local_change();
                            }
                            ui.accept_response(response)
                        }
                        KeyAction::Command(command) => {
                            ui.accept_response(ipc::request(socket_path, &command)?)
                        }
                        KeyAction::Ignored => {
                            if ui.local_revision != local_revision {
                                redraw = true;
                            }
                            continue;
                        }
                    };
                    if ui.local_revision != local_revision || next.revision != state.revision {
                        redraw = true;
                    }
                    ui.sync_queue(&state, &next);
                    state = next;
                }
                _ => {}
            }
        } else {
            let next = request_state(socket_path)?;
            if next.revision != state.revision {
                ui.sync_queue(&state, &next);
                state = next;
                redraw = true;
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct UiState {
    queue: ListState,
    search: Option<LibrarySearch>,
    tree: Option<LibraryTree>,
    message: Option<String>,
    queue_cache: QueueViewCache,
    local_revision: u64,
}

impl UiState {
    fn mark_local_change(&mut self) {
        self.local_revision = self.local_revision.wrapping_add(1);
    }

    fn sync_queue(&mut self, previous: &AppState, next: &AppState) {
        let selected = self.queue.selected().unwrap_or(0);
        let queue_id = previous.queue.get(selected).map(|entry| entry.id);
        let index = next
            .queue
            .iter()
            .position(|entry| Some(entry.id) == queue_id);
        self.queue.select(
            (!next.queue.is_empty()).then(|| index.unwrap_or(selected.min(next.queue.len() - 1))),
        );
    }

    fn accept_response(&mut self, response: Response) -> AppState {
        let message = if response.ok {
            None
        } else {
            Some(response.error.unwrap_or_else(|| "Command rejected".into()))
        };
        if self.message != message {
            self.message = message;
            self.mark_local_change();
        }
        response.state
    }

    fn key_action(&mut self, key: KeyEvent, state: &AppState) -> KeyAction {
        if self.search.is_some() {
            let mut changed = false;
            let mut command = None;
            let mut close = false;
            {
                let search = self.search.as_mut().expect("search checked above");
                match key.code {
                    KeyCode::Esc => close = true,
                    KeyCode::Up => {
                        changed =
                            move_selection(&mut search.selection, search.matches.len(), false);
                    }
                    KeyCode::Down => {
                        changed = move_selection(&mut search.selection, search.matches.len(), true);
                    }
                    KeyCode::Enter => {
                        if let Some(index) = search.selection.selected()
                            && let Some(&track_index) = search.matches.get(index)
                        {
                            command = Some(Command::Enqueue {
                                track_ids: vec![search.library[track_index].id],
                            });
                        }
                    }
                    KeyCode::Backspace => {
                        if search.query.pop().is_some() {
                            search.filter();
                            changed = true;
                        }
                    }
                    KeyCode::Char(character) => {
                        search.query.push(character);
                        search.filter();
                        changed = true;
                    }
                    _ => {}
                }
            }
            if close {
                self.search = None;
                changed = true;
            }
            if changed {
                self.mark_local_change();
            }
            return command.map_or(KeyAction::Ignored, KeyAction::Command);
        }
        if self.tree.is_some() {
            let mut changed = false;
            let mut command = None;
            let mut close = false;
            let mut delegate = false;
            {
                let tree = self.tree.as_mut().expect("tree checked above");
                match key.code {
                    KeyCode::Esc => close = true,
                    KeyCode::Up | KeyCode::Down => {
                        let len = tree.entries().len();
                        changed =
                            move_selection(&mut tree.selection, len, key.code == KeyCode::Down);
                    }
                    KeyCode::Enter => {
                        command = tree.activate();
                        changed = command.is_none();
                    }
                    KeyCode::Backspace | KeyCode::Left => changed = tree.parent(),
                    KeyCode::Char('/') => delegate = true,
                    _ => delegate = true,
                }
            }
            if close {
                self.tree = None;
                changed = true;
            }
            if changed {
                self.mark_local_change();
            }
            if delegate {
                return if key.code == KeyCode::Char('/') {
                    KeyAction::Search
                } else {
                    key_action(key, state)
                };
            }
            return command.map_or(KeyAction::Ignored, KeyAction::Command);
        }
        let changed = match key.code {
            KeyCode::Char('/') => return KeyAction::Search,
            KeyCode::Char('t') => return KeyAction::Tree,
            KeyCode::Up => move_selection(&mut self.queue, state.queue.len(), false),
            KeyCode::Down => move_selection(&mut self.queue, state.queue.len(), true),
            KeyCode::Enter => {
                return self
                    .queue
                    .selected()
                    .and_then(|index| state.queue.get(index))
                    .map_or(KeyAction::Ignored, |entry| {
                        KeyAction::Command(Command::PlayQueue { queue_id: entry.id })
                    });
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(entry) = self
                    .queue
                    .selected()
                    .and_then(|index| state.queue.get(index))
                {
                    return KeyAction::Command(Command::RemoveQueue { queue_id: entry.id });
                }
                false
            }
            _ => return key_action(key, state),
        };
        if changed {
            self.mark_local_change();
        }
        KeyAction::Ignored
    }
}
struct LibrarySearch {
    library: Arc<Vec<Track>>,
    searchable: Vec<String>,
    query: String,
    matches: Vec<usize>,
    selection: ListState,
}

impl LibrarySearch {
    fn new(library: Arc<Vec<Track>>) -> Self {
        let searchable = library
            .iter()
            .map(|track| {
                format!(
                    "{} {} {} {}",
                    track.title,
                    track.artist,
                    track.album,
                    track.path.display()
                )
                .to_lowercase()
            })
            .collect();
        let matches = (0..library.len()).collect();
        let selection = ListState::default().with_selected((!library.is_empty()).then_some(0));
        Self {
            library,
            searchable,
            query: String::new(),
            matches,
            selection,
        }
    }

    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        self.matches.clear();
        self.matches.extend(
            self.searchable
                .iter()
                .enumerate()
                .filter_map(|(index, text)| text.contains(&query).then_some(index)),
        );
        self.selection
            .select((!self.matches.is_empty()).then_some(0));
    }
}

/// A library snapshot indexed into flat, directory-first lists of immediate children.
/// `t` opens the common library directory; parent navigation stops at that root.
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
    fn new(library: Arc<Vec<Track>>) -> Self {
        let mut root = library
            .first()
            .and_then(|track| track.path.parent())
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf();
        for track in library.iter().skip(1) {
            while !track.path.starts_with(&root) {
                if !root.pop() {
                    root.clear();
                    break;
                }
            }
        }
        let mut directories = BTreeMap::new();
        directories.insert(root.clone(), Vec::new());
        let mut ancestors = Vec::new();
        for (index, track) in library.iter().enumerate() {
            let directory = track.path.parent().unwrap_or_else(|| Path::new(""));
            let mut path = directory;
            while !directories.contains_key(path) {
                ancestors.push(path);
                path = path.parent().unwrap_or(&root);
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
                .get_mut(directory)
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

enum KeyAction {
    Quit,
    Search,
    Tree,
    Command(Command),
    Ignored,
}

fn key_action(key: KeyEvent, state: &AppState) -> KeyAction {
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
    playback_key_command(key_name, state).map_or(KeyAction::Ignored, KeyAction::Command)
}

fn request_state(socket_path: &Path) -> Result<AppState> {
    Ok(ipc::request(socket_path, &Command::Overview)?.state)
}

fn draw(frame: &mut ratatui::Frame<'_>, state: &AppState, ui: &mut UiState) {
    let area = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(area);
    let now = state
        .current_track()
        .map(|t| format!("{} — {}", t.artist, t.title))
        .unwrap_or_else(|| "Nothing playing".into());
    let status = match state.status {
        PlaybackStatus::Playing => "Playing",
        PlaybackStatus::Paused => "Paused",
        PlaybackStatus::Stopped => "Stopped",
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " RIVU ",
                Style::default()
                    .fg(Color::Rgb(122, 162, 247))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{status}  {now}")),
        ]))
        .block(Block::default().borders(Borders::BOTTOM)),
        outer[0],
    );
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(outer[1]);
    let position = state.position.max(0.0);
    let duration = state.duration.unwrap_or(0.0).max(position + 0.001);
    let ratio = (position / duration).clamp(0.0, 1.0);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(2)])
        .split(columns[0]);
    frame.render_widget(
        Gauge::default()
            .block(
                Block::default()
                    .title(format!(
                        " {} / {} ",
                        fmt_time(position),
                        fmt_time(state.duration.unwrap_or(0.0))
                    ))
                    .borders(Borders::ALL),
            )
            .gauge_style(Style::default().fg(Color::Rgb(122, 162, 247)))
            .ratio(ratio),
        left[0],
    );
    if let Some(search) = ui.search.as_mut() {
        draw_search(frame, left[1], search);
    } else if let Some(tree) = ui.tree.as_mut() {
        draw_tree(frame, left[1], tree, state.config.nerd_symbols);
    } else {
        draw_queue(frame, left[1], state, &mut ui.queue, &mut ui.queue_cache);
    }
    let keys = if ui.search.is_some() {
        "Type    search library\nBackspace edit query\n↑/↓     select result\nEnter   add to queue\nEsc     return to queue\n\nSearch matches title, artist, album and path."
    } else if ui.tree.is_some() {
        "↑/↓     select entry\nEnter   open directory/add track\nBackspace/← parent directory\nEsc     return to queue\n/       search library\nSpace   play/pause\nn/p     next/previous\n→       seek forward 5 sec\nHome/End seek start/end\nr       repeat mode\ns       shuffle\n[/]     volume\nq       quit"
    } else {
        "↑/↓     select queue\nEnter   play selected\nd/Del   remove selected\n/       search library\nt       browse library tree\nSpace   play/pause\nn/p     next/previous\n←/→     seek 5 sec\nHome/End seek start/end\nr       repeat mode\ns       shuffle\n[/]     volume\nq       quit"
    };
    let help = Paragraph::new(keys)
        .wrap(Wrap { trim: true })
        .block(Block::default().title("Keys").borders(Borders::ALL));
    frame.render_widget(help, columns[1]);
    let status = if state.config.nerd_symbols {
        format!(
            "󰕾 {:>3}% 󰒝 {} 󰑖 {} 󰘦 {}",
            (state.volume * 100.0).round() as u8,
            if state.shuffle { "on" } else { "off" },
            repeat_label(state.repeat),
            state.queue.len()
        )
    } else {
        format!(
            "vol {:>3}%  shuffle {}  repeat {}  queue {}",
            (state.volume * 100.0).round() as u8,
            if state.shuffle { "on" } else { "off" },
            repeat_label(state.repeat),
            state.queue.len()
        )
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(status),
            Line::styled(
                ui.message.as_deref().unwrap_or(""),
                Style::default().fg(Color::Yellow),
            ),
        ])
        .style(Style::default().fg(Color::Rgb(86, 95, 137))),
        outer[2],
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
    fn matches(&self, state: &AppState) -> bool {
        self.queue.len() == state.queue.len()
            && self
                .queue
                .iter()
                .zip(state.queue.iter())
                .all(|(&(id, track_id), entry)| id == entry.id && track_id == entry.track_id)
            && self.tracks.len() == state.library.len()
            && self
                .tracks
                .iter()
                .zip(state.library.iter())
                .all(|(cached, track)| {
                    cached.id == track.id
                        && cached.title == track.title
                        && cached.artist == track.artist
                })
    }

    fn sync(&mut self, state: &AppState) {
        if self.matches(state) {
            return;
        }
        self.queue = state
            .queue
            .iter()
            .map(|entry| (entry.id, entry.track_id))
            .collect();
        self.tracks = state
            .library
            .iter()
            .map(|track| QueueTrackKey {
                id: track.id,
                title: track.title.clone(),
                artist: track.artist.clone(),
            })
            .collect();
        let mut track_index = HashMap::with_capacity(state.library.len());
        for (index, track) in state.library.iter().enumerate() {
            track_index.insert(track.id, index);
        }
        self.rows = state
            .queue
            .iter()
            .map(|entry| {
                track_index
                    .get(&entry.track_id)
                    .and_then(|&index| state.library.get(index))
                    .map(|track| format!("{} — {}", track.artist, track.title))
                    .unwrap_or_else(|| "Missing track".into())
            })
            .collect();
    }
}

fn draw_queue(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &AppState,
    selection: &mut ListState,
    cache: &mut QueueViewCache,
) {
    cache.sync(state);
    let items = cache.rows.iter().enumerate().map(|(index, title)| {
        let marker = if state.current_queue_id == Some(state.queue[index].id) {
            "▶ "
        } else {
            "  "
        };
        ListItem::new(format!("{marker}{title}"))
    });
    let title = if state.queue.is_empty() {
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

fn draw_search(frame: &mut ratatui::Frame<'_>, area: Rect, search: &mut LibrarySearch) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);
    frame.render_widget(
        Paragraph::new(search.query.as_str()).block(
            Block::default()
                .title("Library search — type to filter")
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
            .block(Block::default().title(title).borders(Borders::ALL))
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
    use super::*;
    use crate::model::QueueEntry;
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

    #[test]
    fn revision_unchanged_local_selection_is_visible() {
        let state = AppState {
            queue: Arc::new(vec![
                QueueEntry { id: 1, track_id: 1 },
                QueueEntry { id: 2, track_id: 2 },
            ]),
            ..AppState::default()
        };
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
        let previous = AppState {
            queue: Arc::new(vec![
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
            current_queue_id: Some(11),
            ..AppState::default()
        };
        let mut ui = UiState::default();
        ui.sync_queue(&previous, &previous);
        ui.key_action(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &previous);
        let mut next = previous.clone();
        Arc::make_mut(&mut next.queue).rotate_right(1);
        ui.sync_queue(&previous, &next);
        assert!(matches!(
            ui.key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &next),
            KeyAction::Command(Command::PlayQueue { queue_id: 22 })
        ));
        let empty = AppState::default();
        ui.sync_queue(&next, &empty);
        assert!(matches!(
            ui.key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &empty),
            KeyAction::Ignored
        ));
    }

    #[test]
    fn queue_cache_follows_order_and_metadata_changes() {
        let mut state = AppState {
            library: Arc::new(vec![
                track(1, "Artist A", "Title A"),
                track(2, "Artist B", "Title B"),
            ]),
            queue: Arc::new(vec![
                QueueEntry {
                    id: 10,
                    track_id: 1,
                },
                QueueEntry {
                    id: 20,
                    track_id: 2,
                },
            ]),
            ..AppState::default()
        };
        let mut cache = QueueViewCache::default();
        cache.sync(&state);
        assert_eq!(
            cache.rows,
            vec![
                "Artist A — Title A".to_owned(),
                "Artist B — Title B".to_owned()
            ]
        );

        state.queue = Arc::new(vec![
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
        state.library = Arc::new(vec![changed, track(2, "Artist B", "Title B")]);
        cache.sync(&state);
        assert_eq!(cache.rows[1], "Artist A — Title A (remastered)");
    }
}
