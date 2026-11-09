use super::*;
use std::{
    io::{Read, Write},
    panic::AssertUnwindSafe,
    process::Command,
};

const CHILD_MODE: &str = "RIVU_TUI_TERMINAL_RESTORE_CHILD";
const PANIC_MESSAGE: &str = "terminal restoration regression panic";

fn confirm_raw_mode_with_pty_parent() {
    io::stdout().write_all(b"RIVU_PTY_RAW_READY").unwrap();
    io::stdout().flush().unwrap();
    let mut confirmation = [0];
    io::stdin().read_exact(&mut confirmation).unwrap();
    assert_eq!(confirmation, [b'x']);
}

struct PanicAfterLoadingScreen(bool);

impl Future for PanicAfterLoadingScreen {
    type Output = Result<()>;

    fn poll(mut self: std::pin::Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Self::Output> {
        if self.0 {
            confirm_raw_mode_with_pty_parent();
            panic!("{PANIC_MESSAGE}");
        }
        self.0 = true;
        Poll::Pending
    }
}

// Run only in a fresh session under the parent test's private PTY. In particular,
// never change the test runner's terminal or crossterm's process-global raw state.
#[test]
fn isolated_terminal_restore_child() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    match mode.as_str() {
        "unwind" => {
            let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
                run(directory.path(), "en", PanicAfterLoadingScreen(false))
            }))
            .expect_err("terminal cleanup must not swallow the startup panic");
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied());
            assert_eq!(message, Some(PANIC_MESSAGE));
        }
        "error" => {
            let error = run(directory.path(), "en", async {
                confirm_raw_mode_with_pty_parent();
                anyhow::bail!("terminal startup regression error")
            })
            .unwrap_err();
            assert_eq!(error.to_string(), "terminal startup regression error");
        }
        "partial" => {
            let initialize = || -> Result<()> {
                let mut restore = TerminalRestoreGuard::new()?;
                execute!(
                    restore.stdout,
                    EnterAlternateScreen,
                    crossterm::cursor::Hide
                )?;
                confirm_raw_mode_with_pty_parent();
                // Fail after acquiring terminal modes but before constructing a
                // ratatui Terminal: no Terminal destructor can do this cleanup.
                anyhow::bail!("terminal initialization regression error")
            };
            assert_eq!(
                initialize().unwrap_err().to_string(),
                "terminal initialization regression error"
            );
        }
        _ => panic!("unknown isolated terminal test mode: {mode}"),
    }
}

#[test]
fn real_pty_restores_modes_screen_and_cursor_after_unwind_and_errors() {
    // Python 3's standard-library PTY and termios support is the explicit Linux
    // test prerequisite; no shell, user terminal, Core service or timing input is
    // involved. Each case execs a new test process with its own controlling TTY.
    let output = Command::new("python3")
        .arg("-c")
        .arg(
            r#"
import fcntl
import os
import pty
import selectors
import struct
import subprocess
import sys
import termios
import time

executable, test_name, mode_variable = sys.argv[1:]
for mode in ('unwind', 'error', 'partial'):
    master, slave = pty.openpty()
    process = None
    try:
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))
        baseline = termios.tcgetattr(slave)
        assert baseline[3] & termios.ICANON, 'PTY must begin in canonical mode'
        assert baseline[3] & termios.ECHO, 'PTY must begin with echo enabled'

        def take_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        environment = os.environ.copy()
        environment[mode_variable] = mode
        process = subprocess.Popen(
            [executable, '--exact', test_name, '--nocapture', '--test-threads=1'],
            stdin=slave, stdout=slave, stderr=slave, env=environment,
            preexec_fn=take_terminal,
        )
        os.set_blocking(master, False)
        captured = bytearray()
        confirmed_raw = False
        deadline = time.monotonic() + 10
        with selectors.DefaultSelector() as ready:
            ready.register(master, selectors.EVENT_READ)
            while process.poll() is None:
                assert time.monotonic() < deadline, f'{mode}: child timed out'
                for key, _ in ready.select(timeout=0.05):
                    captured.extend(os.read(key.fd, 65536))
                if not confirmed_raw and b'RIVU_PTY_RAW_READY' in captured:
                    active = termios.tcgetattr(slave)
                    assert not active[3] & termios.ICANON, f'{mode}: raw mode not entered'
                    assert not active[3] & termios.ECHO, f'{mode}: echo not disabled'
                    os.write(master, b'x')
                    confirmed_raw = True
        while True:
            try:
                captured.extend(os.read(master, 65536))
            except BlockingIOError:
                break
        transcript = captured.decode(errors='replace')
        assert process.returncode == 0, f'{mode}: child failed:\n{transcript}'
        assert termios.tcgetattr(slave) == baseline, f'{mode}: termios not restored'
        entered = captured.rfind(b'\x1b[?1049h')
        left = captured.rfind(b'\x1b[?1049l')
        shown = captured.rfind(b'\x1b[?25h')
        assert entered >= 0, f'{mode}: alternate screen was not entered'
        assert left > entered, f'{mode}: alternate screen was not left'
        assert shown > left, f'{mode}: cursor was not shown after leaving screen'
        assert confirmed_raw, f'{mode}: child did not reach raw-mode handshake'
        if mode in ('unwind', 'partial'):
            hidden = captured.rfind(b'\x1b[?25l')
            assert entered < hidden < shown, f'{mode}: hidden cursor not restored'
    finally:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)
        os.close(slave)
"#,
        )
        .arg(std::env::current_exe().unwrap())
        .arg("terminal::restore_tests::isolated_terminal_restore_child")
        .arg(CHILD_MODE)
        .output()
        .expect("the Linux PTY regression test requires python3 with its standard library");
    assert!(
        output.status.success(),
        "isolated PTY regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
