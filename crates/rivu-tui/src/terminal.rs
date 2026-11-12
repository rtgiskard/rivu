mod paged;
mod render;
mod state;
mod worker;

use render::{draw, draw_loading};
use state::{KeyAction, NoticeKind, TuiState, resolve_key};
use worker::{spawn_command_worker, spawn_query_worker, spawn_watcher};

use crate::projection::TuiSnapshot;
use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use rivu_presentation::{
    i18n::Localizer,
    keyboard::{Action, Context as KeyboardContext},
};
use signal_hook::{
    SigId,
    consts::{SIGINT, SIGTERM},
};
use std::{
    future::Future,
    io::{self, Stdout},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context as TaskContext, Poll, Waker},
    time::{Duration, Instant},
};

/// Attach over IPC after executable-owned readiness completes. Startup is polled
/// after terminal and signal setup; closing the screen drops its waiter, never
/// the independent Core service it may have started.
pub fn run(
    data_dir: &Path,
    language: &str,
    startup: impl Future<Output = Result<()>>,
) -> Result<()> {
    let termination = TerminalTermination::new().context("register terminal shutdown signals")?;
    let i18n = Arc::new(Localizer::for_terminal(language));
    let mut restore = TerminalRestoreGuard::new()?;
    execute!(restore.stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let result = run_loop(
        data_dir,
        &mut terminal,
        i18n,
        &termination.requested,
        startup,
        |timeout| {
            if event::poll(timeout)? {
                event::read().map(Some)
            } else {
                Ok(None)
            }
        },
    );
    drop(terminal);
    let cleanup = restore.restore();
    match result {
        Err(error) => Err(error),
        Ok(()) => cleanup,
    }
}

/// Own terminal modes before any fallible screen initialization can run.
struct TerminalRestoreGuard {
    stdout: Stdout,
    active: bool,
}

impl TerminalRestoreGuard {
    fn new() -> Result<Self> {
        let stdout = io::stdout();
        enable_raw_mode().context("enable terminal raw mode")?;
        Ok(Self {
            stdout,
            active: true,
        })
    }

    fn restore(&mut self) -> Result<()> {
        self.active = false;
        restore_terminal(&mut self.stdout)
    }
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = self.restore();
        }
    }
}

fn restore_terminal(stdout: &mut Stdout) -> Result<()> {
    let mut first = None;
    if let Err(error) = disable_raw_mode().context("restore terminal raw mode") {
        first = Some(error);
    }
    if let Err(error) = execute!(stdout, LeaveAlternateScreen).context("leave alternate screen")
        && first.is_none()
    {
        first = Some(error);
    }
    if let Err(error) = execute!(stdout, crossterm::cursor::Show).context("restore cursor")
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

fn run_loop<B: ratatui::backend::Backend>(
    data_dir: &Path,
    terminal: &mut Terminal<B>,
    i18n: Arc<Localizer>,
    terminating: &AtomicBool,
    startup: impl Future<Output = Result<()>>,
    next_event: impl FnMut(Duration) -> io::Result<Option<Event>>,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let mut startup = std::pin::pin!(startup);
    let mut updates = None;
    run_loop_with_events(
        data_dir,
        terminal,
        i18n,
        terminating,
        || {
            if updates.is_none() {
                // Sample completion once per existing input iteration (at most
                // 50ms idle). I/O stays asynchronous without a waiter thread.
                match startup
                    .as_mut()
                    .poll(&mut TaskContext::from_waker(Waker::noop()))
                {
                    Poll::Pending => return Ok(None),
                    Poll::Ready(result) => result?,
                }
                updates = Some(spawn_watcher(data_dir));
            }
            updates
                .as_ref()
                .expect("Ready startup starts the watcher")
                .take_update()
        },
        next_event,
    )
}

fn run_loop_with_events<B: ratatui::backend::Backend>(
    data_dir: &Path,
    terminal: &mut Terminal<B>,
    i18n: Arc<Localizer>,
    terminating: &AtomicBool,
    take_update: impl FnMut() -> Result<Option<(u32, rivu_protocol::model::ClientSnapshot)>>,
    next_event: impl FnMut(Duration) -> io::Result<Option<Event>>,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    run_loop_with_workers(
        data_dir,
        terminal,
        i18n,
        terminating,
        take_update,
        next_event,
        |data_dir, instance_id| {
            (
                spawn_command_worker(data_dir, instance_id),
                spawn_query_worker(data_dir, instance_id),
            )
        },
    )
}

fn run_loop_with_workers<B: ratatui::backend::Backend>(
    data_dir: &Path,
    terminal: &mut Terminal<B>,
    i18n: Arc<Localizer>,
    terminating: &AtomicBool,
    mut take_update: impl FnMut() -> Result<Option<(u32, rivu_protocol::model::ClientSnapshot)>>,
    mut next_event: impl FnMut(Duration) -> io::Result<Option<Event>>,
    mut spawn_workers: impl FnMut(&Path, u32) -> (worker::CommandWorker, worker::QueryWorker),
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let mut workers = None;
    let mut tui = None::<TuiState>;
    let mut redraw = true;
    let mut immediate_redraw = true;
    let mut last_draw = Instant::now() - Duration::from_millis(100);
    let mut size = terminal.size()?;
    // Reuse the input loop's existing bounded wait; signals need no extra worker
    // or periodic wakeup and are observed even before the IPC handshake finishes.
    while !terminating.load(Ordering::Relaxed) {
        if let Some((instance_id, state)) = take_update()? {
            // Shutdown belongs to the connection owner, not the projection or
            // query workers. Exit before creating workers or dispatching work,
            // including when shutdown is the loading screen's first snapshot.
            if state.system.shutting_down {
                break;
            }
            if workers.is_none() {
                workers = Some(spawn_workers(data_dir, instance_id));
            }
            let next = TuiSnapshot::from_client(state, tui.as_ref().map(|tui| &tui.snapshot));
            match &mut tui {
                Some(tui) => {
                    redraw |= !next.same_presentation(&tui.snapshot);
                    tui.apply_snapshot(next);
                }
                None => {
                    tui = Some(TuiState::new(next, Arc::clone(&i18n)));
                    redraw = true;
                }
            }
        }
        let Some(tui) = &mut tui else {
            if redraw {
                terminal.draw(|frame| {
                    let area = frame.area();
                    size.width = area.width;
                    size.height = area.height;
                    draw_loading(frame, &i18n);
                })?;
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
                    Event::Resize(width, height) => {
                        size.width = width;
                        size.height = height;
                        redraw = true;
                    }
                    _ => {}
                }
            }
            continue;
        };
        let (commands, queries) = workers.as_mut().expect("Initial state starts TUI workers");
        while let Some(result) = commands.take_result()? {
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
        render::commit_layout(
            ratatui::layout::Rect::new(0, 0, size.width, size.height),
            &mut tui.ui,
        );
        if tui.ui.prepare_local_view(&tui.snapshot) {
            redraw = true;
            immediate_redraw = true;
        }
        let local_revision = tui.ui.local_revision;
        let latest = queries.take_result()?;
        if let Some((request, result)) = latest {
            // Instance validity is independent of view/state revision validity:
            // even a superseded query must exit on a replacement server.
            let result = match result {
                Ok(result) => Ok(result),
                Err(error) => Err(error.into_query_error()?),
            };
            if tui.ui.apply_query(&tui.snapshot, &request, result) {
                redraw = true;
                immediate_redraw = true;
            }
        }
        if let Some((request, ticket)) = tui.ui.prepare_query_request(&tui.snapshot) {
            queries.request(request, ticket)?;
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
            terminal.draw(|frame| {
                let area = frame.area();
                size.width = area.width;
                size.height = area.height;
                render::commit_layout(area, &mut tui.ui);
                draw(frame, &tui.snapshot, &mut tui.ui);
            })?;
            last_draw = Instant::now();
            redraw = false;
            immediate_redraw = false;
        }
        if let Some(event) = next_event(Duration::from_millis(50))? {
            match event {
                Event::Resize(width, height) => {
                    size.width = width;
                    size.height = height;
                    redraw = true;
                    immediate_redraw = true;
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let local_revision = tui.ui.local_revision;
                    match tui.ui.key_action(key, &tui.snapshot) {
                        KeyAction::Quit => break,
                        KeyAction::Search => {
                            tui.ui.open_search();
                        }
                        action @ (KeyAction::Command(_) | KeyAction::Shutdown) => {
                            let command = if let KeyAction::Command(command) = action {
                                command.into()
                            } else {
                                rivu_protocol::AppCommand::Shutdown
                            };
                            if !commands.request(command)? {
                                tui.ui.set_notice(
                                    i18n.text("terminal-command-busy").into(),
                                    NoticeKind::Warning,
                                );
                                redraw = true;
                                immediate_redraw = true;
                            }
                            if tui.ui.local_revision != local_revision {
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
                    }
                    redraw |= tui.ui.local_revision != local_revision;
                    immediate_redraw = true;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod restore_tests;

#[cfg(test)]
mod startup_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use std::pin::Pin;

    struct PendingStartup {
        polled: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }

    impl Future for PendingStartup {
        type Output = Result<()>;

        fn poll(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Self::Output> {
            self.polled.store(true, Ordering::Relaxed);
            Poll::Pending
        }
    }

    impl Drop for PendingStartup {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn loading_owner_keeps_close_input_responsive_and_drops_pending_startup() {
        for close in [
            Some(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            None,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            let polled = Arc::new(AtomicBool::new(false));
            let dropped = Arc::new(AtomicBool::new(false));
            let terminating = AtomicBool::new(false);
            let mut waits = 0;
            run_loop(
                directory.path(),
                &mut terminal,
                Arc::new(Localizer::for_terminal("en")),
                &terminating,
                PendingStartup {
                    polled: Arc::clone(&polled),
                    dropped: Arc::clone(&dropped),
                },
                |_| {
                    assert!(polled.load(Ordering::Relaxed));
                    waits += 1;
                    if waits <= 2 {
                        let code = if waits == 1 {
                            KeyCode::Char('q')
                        } else {
                            KeyCode::Esc
                        };
                        return Ok(Some(Event::Key(KeyEvent::from(code))));
                    }
                    assert_eq!(waits, 3, "pending startup must not consume close input");
                    if let Some(key) = close {
                        Ok(Some(Event::Key(key)))
                    } else {
                        terminating.store(true, Ordering::Relaxed);
                        Ok(None)
                    }
                },
            )
            .unwrap();
            assert_eq!(waits, 3);
            assert!(dropped.load(Ordering::Relaxed));
        }
    }
}
