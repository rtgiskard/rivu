//! Test-only process isolation shared by workspace test suites.

use std::process::{Child, Command};
use std::time::{Duration, Instant};

const CHILD_TEST: &str = "RIVU_BOUNDED_TEST";
const CHILD_ENTERED: &str = "RIVU_BOUNDED_TEST_ENTERED";

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
    struct Runner {
        child: Child,
        reaped: bool,
    }
    impl Drop for Runner {
        fn drop(&mut self) {
            if !self.reaped {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let entered = directory.path().join("entered");
    let mut runner = Runner {
        child: Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD_TEST, test)
            .env(CHILD_ENTERED, &entered)
            .spawn()
            .unwrap(),
        reaped: false,
    };
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = runner.child.try_wait().unwrap() {
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
