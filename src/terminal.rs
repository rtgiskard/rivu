use crate::{
    ipc,
    model::{AppState, Command, PlaybackStatus, Response},
};
use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Wrap},
};
use std::{
    io::{self, Stdout},
    path::Path,
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
    loop {
        terminal.draw(|frame| draw(frame, &state))?;
        if event::poll(Duration::from_millis(250))? {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            let command = match key {
                KeyEvent {
                    code: KeyCode::Char('q') | KeyCode::Esc,
                    ..
                } => break,
                KeyEvent {
                    code: KeyCode::Char(' '),
                    ..
                } => Some(Command::Toggle),
                KeyEvent {
                    code: KeyCode::Char('n'),
                    ..
                } => Some(Command::Next),
                KeyEvent {
                    code: KeyCode::Char('p'),
                    ..
                } => Some(Command::Previous),
                KeyEvent {
                    code: KeyCode::Char('+') | KeyCode::Char('='),
                    ..
                } => Some(Command::Seek {
                    seconds: state.position + 5.0,
                }),
                KeyEvent {
                    code: KeyCode::Char('-'),
                    ..
                } => Some(Command::Seek {
                    seconds: (state.position - 5.0).max(0.0),
                }),
                KeyEvent {
                    code: KeyCode::Char(']'),
                    ..
                } => Some(Command::Volume {
                    value: (state.volume + 0.05).min(1.0),
                }),
                KeyEvent {
                    code: KeyCode::Char('['),
                    ..
                } => Some(Command::Volume {
                    value: (state.volume - 0.05).max(0.0),
                }),
                _ => None,
            };
            if let Some(command) = command {
                state = send(socket_path, &command)?;
            }
        } else {
            state = request_state(socket_path)?;
        }
    }
    Ok(())
}
fn request_state(socket_path: &Path) -> Result<AppState> {
    Ok(ipc::request(socket_path, &Command::Overview)?.state)
}
fn send(socket_path: &Path, command: &Command) -> Result<AppState> {
    let response: Response = ipc::request(socket_path, command)?;
    if !response.ok {
        anyhow::bail!(
            response
                .error
                .unwrap_or_else(|| "core rejected command".into())
        );
    }
    Ok(response.state)
}

fn draw(frame: &mut ratatui::Frame<'_>, state: &AppState) {
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
                    .fg(Color::Cyan)
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
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(ratio),
        left[0],
    );
    let items: Vec<ListItem> = state
        .queue
        .iter()
        .take(12)
        .map(|entry| {
            let track = state.library.iter().find(|t| t.id == entry.track_id);
            let title = track
                .map(|t| format!("{} — {}", t.artist, t.title))
                .unwrap_or_else(|| "Missing track".into());
            let marker = if state.current_queue_id == Some(entry.id) {
                "▶ "
            } else {
                "  "
            };
            ListItem::new(format!("{marker}{title}"))
        })
        .collect();
    frame.render_widget(
        List::new(items).block(Block::default().title("Queue").borders(Borders::ALL)),
        left[1],
    );
    let help = Paragraph::new(
        "space play/pause   n next   p previous\n+/- seek 5 sec   [/] volume   q quit",
    )
    .wrap(Wrap { trim: true })
    .block(Block::default().title("Keys").borders(Borders::ALL));
    frame.render_widget(help, columns[1]);
    frame.render_widget(
        Paragraph::new(format!(
            "vol {:>3}%  queue {}",
            (state.volume * 100.0).round() as u8,
            state.queue.len()
        ))
        .style(Style::default().fg(Color::DarkGray)),
        outer[2],
    );
}
fn fmt_time(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "0:00".into();
    }
    format!("{}:{:02}", (seconds as u64) / 60, (seconds as u64) % 60)
}
