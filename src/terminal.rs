use crate::{
    ipc,
    model::{AppState, Command, PlaybackStatus, RepeatMode, Response, Track},
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
    collections::BTreeMap,
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
    if let Err(e) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(e.into());
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => {
            restore_terminal_without_screen();
            return Err(e.into());
        }
    };
    let result = run_loop(socket_path, &mut terminal);
    let cleanup = restore_terminal(&mut terminal);
    result.and(cleanup)
}

fn restore_terminal_without_screen() {
    let _ = disable_raw_mode();
}
fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().context("restore terminal raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).context("leave alternate screen")?;
    terminal.show_cursor().context("restore cursor")?;
    Ok(())
}

fn run_loop(socket_path: &Path, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let mut state = request_state(socket_path)?;
    let mut ui = UiState::default();
    ui.sync_queue(&state, &state);
    loop {
        terminal.draw(|frame| draw(frame, &state, &mut ui))?;
        let next = if event::poll(Duration::from_millis(250))? {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match ui.key_action(key, &state) {
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
                    }
                    ui.accept_response(response)
                }
                KeyAction::Command(command) => {
                    ui.accept_response(ipc::request(socket_path, &command)?)
                }
                KeyAction::Ignored => continue,
            }
        } else {
            request_state(socket_path)?
        };
        ui.sync_queue(&state, &next);
        state = next;
    }
    Ok(())
}

#[derive(Default)]
struct UiState {
    queue: ListState,
    search: Option<LibrarySearch>,
    tree: Option<LibraryTree>,
    message: Option<String>,
}

impl UiState {
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
        self.message = if response.ok {
            None
        } else {
            Some(response.error.unwrap_or_else(|| "Command rejected".into()))
        };
        response.state
    }

    fn key_action(&mut self, key: KeyEvent, state: &AppState) -> KeyAction {
        if let Some(search) = self.search.as_mut() {
            match key.code {
                KeyCode::Esc => self.search = None,
                KeyCode::Up => move_selection(&mut search.selection, search.matches.len(), false),
                KeyCode::Down => move_selection(&mut search.selection, search.matches.len(), true),
                KeyCode::Enter => {
                    if let Some(index) = search.selection.selected()
                        && let Some(&track_index) = search.matches.get(index)
                    {
                        return KeyAction::Command(Command::Enqueue {
                            track_ids: vec![search.library[track_index].id],
                        });
                    }
                }
                KeyCode::Backspace => {
                    if search.query.pop().is_some() {
                        search.filter();
                    }
                }
                KeyCode::Char(character) => {
                    search.query.push(character);
                    search.filter();
                }
                _ => {}
            }
            return KeyAction::Ignored;
        }
        if let Some(tree) = self.tree.as_mut() {
            match key.code {
                KeyCode::Esc => self.tree = None,
                KeyCode::Up | KeyCode::Down => {
                    let len = tree.entries().len();
                    move_selection(&mut tree.selection, len, key.code == KeyCode::Down);
                }
                KeyCode::Enter => {
                    if let Some(command) = tree.activate() {
                        return KeyAction::Command(command);
                    }
                }
                KeyCode::Backspace | KeyCode::Left => tree.parent(),
                KeyCode::Char('/') => return KeyAction::Search,
                _ => return key_action(key, state),
            }
            return KeyAction::Ignored;
        }
        match key.code {
            KeyCode::Char('/') => return KeyAction::Search,
            KeyCode::Char('t') => return KeyAction::Tree,
            KeyCode::Up => move_selection(&mut self.queue, state.queue.len(), false),
            KeyCode::Down => move_selection(&mut self.queue, state.queue.len(), true),
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(entry) = self
                    .queue
                    .selected()
                    .and_then(|index| state.queue.get(index))
                {
                    return KeyAction::Command(Command::RemoveQueue { queue_id: entry.id });
                }
            }
            _ => return key_action(key, state),
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

    fn parent(&mut self) {
        if self.directory == self.root {
            return;
        }
        let parent = self.directory.parent().unwrap_or(&self.root).to_path_buf();
        let child = std::mem::replace(&mut self.directory, parent);
        let selected = self
            .entries()
            .iter()
            .position(|entry| matches!(entry, LibraryTreeEntry::Directory(path) if *path == child));
        self.selection = ListState::default().with_selected(selected);
    }
}

fn move_selection(selection: &mut ListState, len: usize, down: bool) {
    selection.select(if len == 0 {
        None
    } else {
        let index = selection.selected().unwrap_or(0);
        Some(if down {
            index.saturating_add(1).min(len - 1)
        } else {
            index.saturating_sub(1)
        })
    });
}

enum KeyAction {
    Quit,
    Search,
    Tree,
    Command(Command),
    Ignored,
}

fn key_action(key: KeyEvent, state: &AppState) -> KeyAction {
    let command = match key.code {
        KeyCode::Char('q') => return KeyAction::Quit,
        KeyCode::Char(' ') => Some(Command::Toggle),
        KeyCode::Char('n') => Some(Command::Next),
        KeyCode::Char('p') => Some(Command::Previous),
        KeyCode::Left => seek_command(state, -5.0),
        KeyCode::Right => seek_command(state, 5.0),
        KeyCode::Home => seek_to(state, 0.0),
        KeyCode::End => state
            .duration
            .filter(|duration| duration.is_finite())
            .and_then(|duration| seek_to(state, duration)),
        KeyCode::Char('r') => Some(Command::Repeat {
            mode: match state.repeat {
                RepeatMode::Off => RepeatMode::All,
                RepeatMode::All => RepeatMode::One,
                RepeatMode::One => RepeatMode::Off,
            },
        }),
        KeyCode::Char('s') => Some(Command::Shuffle {
            enabled: !state.shuffle,
        }),
        KeyCode::Char(']') => Some(Command::Volume {
            value: (state.volume + 0.05).min(1.0),
        }),
        KeyCode::Char('[') => Some(Command::Volume {
            value: (state.volume - 0.05).max(0.0),
        }),
        _ => None,
    };
    command.map_or(KeyAction::Ignored, KeyAction::Command)
}

fn seek_to(state: &AppState, seconds: f64) -> Option<Command> {
    (state.status != PlaybackStatus::Stopped).then_some(Command::Seek { seconds })
}

fn seek_command(state: &AppState, delta: f64) -> Option<Command> {
    seek_to(state, (state.position + delta).max(0.0))
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
        draw_queue(frame, left[1], state, &mut ui.queue);
    }
    let keys = if ui.search.is_some() {
        "Type    search library\nBackspace edit query\n↑/↓     select result\nEnter   add to queue\nEsc     return to queue\n\nSearch matches title, artist, album and path."
    } else if ui.tree.is_some() {
        "↑/↓     select entry\nEnter   open directory/add track\nBackspace/← parent directory\nEsc     return to queue\n/       search library\nSpace   play/pause\nn/p     next/previous\n→       seek forward 5 sec\nHome/End seek start/end\nr       repeat mode\ns       shuffle\n[/]     volume\nq       quit"
    } else {
        "↑/↓     select queue\nd/Del   remove selected\n/       search library\nt       browse library tree\nSpace   play/pause\nn/p     next/previous\n←/→     seek 5 sec\nHome/End seek start/end\nr       repeat mode\ns       shuffle\n[/]     volume\nq       quit"
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

fn draw_queue(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &AppState,
    selection: &mut ListState,
) {
    let items = state.queue.iter().map(|entry| {
        let track = state
            .library
            .iter()
            .find(|track| track.id == entry.track_id);
        let title = track
            .map(|track| format!("{} — {}", track.artist, track.title))
            .unwrap_or_else(|| "Missing track".into());
        let marker = if state.current_queue_id == Some(entry.id) {
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
