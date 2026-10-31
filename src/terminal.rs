mod paged;
mod render;
mod state;
mod worker;

use render::{draw, draw_loading};
use state::{KeyAction, NoticeKind, TuiState, resolve_key};
use worker::{Watcher, spawn_command_worker, spawn_query_worker, spawn_watcher};

use crate::{
    i18n::Localizer,
    keyboard::{Action, Context as KeyboardContext},
    projection::TuiSnapshot,
};
use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use signal_hook::{
    SigId,
    consts::{SIGINT, SIGTERM},
};
use std::{
    io::{self, Stdout},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Connect to an existing core over IPC. This never creates an audio engine and
/// quitting the screen only restores the terminal; the server keeps playing.
pub fn run(data_dir: &Path, language: &str) -> Result<()> {
    let termination = TerminalTermination::new().context("register terminal shutdown signals")?;
    let i18n = Arc::new(Localizer::for_terminal(language));
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
                Ok(ack) if !ack.is_success() => {
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
