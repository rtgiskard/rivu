//! Test-only process isolation shared by workspace test suites.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const CHILD_TEST: &str = "RIVU_BOUNDED_TEST";
const CHILD_ENTERED: &str = "RIVU_BOUNDED_TEST_ENTERED";

struct TestProcess {
    child: Child,
    reaped: bool,
}

impl TestProcess {
    fn exited(&self) -> io::Result<bool> {
        // Keep the leader's PID reserved until the group has been signalled,
        // even if the leader exits while a worker is still running.
        let mut status = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        loop {
            // SAFETY: valid output storage and the PID of our unreaped child.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id() as libc::id_t,
                    status.as_mut_ptr(),
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                // SAFETY: waitid initialized the status (pid 0 for no event).
                return Ok(unsafe { status.assume_init().si_pid() } != 0);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn kill_group(&self) -> io::Result<()> {
        // SAFETY: the unreaped child is the leader of this private group.
        if unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

impl Drop for TestProcess {
    fn drop(&mut self) {
        if !self.reaped {
            if self.kill_group().is_err() {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
        }
    }
}

/// Re-run the calling test in a killable process, not a thread whose
/// destructor could join the same stuck worker during unwinding.
///
/// Returns `true` in the parent after the child passed; the caller then returns
/// without running the body again. Returns `false` inside the child. The test
/// name comes from the harness thread; the marker distinguishes a successful
/// test from a successful harness invocation that ran no tests.
pub fn run_in_subprocess(timeout: Duration) -> bool {
    let current = std::thread::current();
    let test = current
        .name()
        .filter(|name| *name != "main")
        .expect("bounded tests must run on the harness-named test thread");
    if std::env::var(CHILD_TEST).as_deref() == Ok(test) {
        std::fs::File::create_new(std::env::var_os(CHILD_ENTERED).unwrap()).unwrap();
        return false;
    }
    let directory = tempfile::tempdir().unwrap();
    let entered = directory.path().join("entered");
    let mut runner = TestProcess {
        child: Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .process_group(0)
            .env(CHILD_TEST, test)
            .env(CHILD_ENTERED, &entered)
            .spawn()
            .unwrap(),
        reaped: false,
    };
    let deadline = Instant::now() + timeout;
    loop {
        if runner.exited().unwrap() {
            runner.kill_group().unwrap();
            let status = runner.child.wait().unwrap();
            runner.reaped = true;
            assert!(status.success(), "{test} failed: {status}");
            assert!(entered.is_file(), "child did not run {test}");
            return true;
        }
        if Instant::now() >= deadline {
            panic!("{test} exceeded {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
