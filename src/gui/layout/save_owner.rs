//! One session-lifetime writer, with one replaceable pending snapshot.
//!
//! Window closure never joins this worker. Reopening reads the latest in-memory
//! snapshot rather than racing its pending disk replacement. The session owner
//! flushes/joins only during application teardown, outside GPUI's quit observer.

use super::LayoutSet;
use parking_lot::{Condvar, Mutex};
use std::{path::PathBuf, sync::Arc, thread::JoinHandle};

type SaveResult = Result<(), String>;
type Wakeup = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub(in crate::gui) struct Completion {
    pub revision: u64,
    pub result: SaveResult,
}

struct State {
    latest: LayoutSet,
    load_error: Option<String>,
    revision: u64,
    pending: bool,
    completion: Option<Completion>,
    stopping: bool,
    wakeup: Option<Wakeup>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Clone)]
pub(in crate::gui) struct LayoutSaveHandle(Arc<Shared>);

pub(in crate::gui) struct LayoutSaveOwner {
    handle: LayoutSaveHandle,
    worker: Option<JoinHandle<()>>,
}

impl LayoutSaveOwner {
    pub fn new(path: PathBuf) -> Self {
        // This is the session's only startup read, not an interactive save or a
        // window-reopen read. All runtime validation/encoding/writes are below.
        let (layouts, error) = if path.exists() {
            match LayoutSet::load(&path) {
                Ok(layouts) => (layouts, None),
                Err(error) => (LayoutSet::default(), Some(format!("{error:#}"))),
            }
        } else {
            (LayoutSet::default(), None)
        };
        Self::start(layouts, error, move |snapshot| {
            snapshot.save(&path).map_err(|error| format!("{error:#}"))
        })
    }

    fn start(
        layouts: LayoutSet,
        load_error: Option<String>,
        mut save: impl FnMut(&LayoutSet) -> SaveResult + Send + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                latest: layouts,
                load_error,
                revision: 0,
                pending: false,
                completion: None,
                stopping: false,
                wakeup: None,
            }),
            changed: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("layout-save".into())
            .spawn(move || {
                loop {
                    let (revision, snapshot) = {
                        let mut state = worker_shared.state.lock();
                        while !state.pending && !state.stopping {
                            worker_shared.changed.wait(&mut state);
                        }
                        if !state.pending {
                            return;
                        }
                        state.pending = false;
                        (state.revision, state.latest.clone())
                    };
                    let result = save(&snapshot);
                    if let Err(error) = &result {
                        // Even a superseded failure has a diagnostic; it must
                        // not replace the current operation's UI feedback.
                        tracing::warn!(revision, %error, "layout_save_failed");
                    }
                    let wakeup = {
                        let mut state = worker_shared.state.lock();
                        if revision == state.revision {
                            if result.is_ok() {
                                state.load_error = None;
                            }
                            state.completion = Some(Completion { revision, result });
                            state.wakeup.clone()
                        } else {
                            None
                        }
                    };
                    worker_shared.changed.notify_all();
                    if let Some(wakeup) = wakeup {
                        wakeup();
                    }
                }
            })
            .expect("starting layout save worker");
        Self {
            handle: LayoutSaveHandle(shared),
            worker: Some(worker),
        }
    }

    pub fn handle(&self) -> LayoutSaveHandle {
        self.handle.clone()
    }

    /// Application teardown only; never call from a window lifecycle callback.
    pub fn flush(&self) -> SaveResult {
        self.handle.flush()
    }
}

impl LayoutSaveHandle {
    pub fn snapshot(&self) -> (LayoutSet, Option<String>) {
        let state = self.0.state.lock();
        (state.latest.clone(), state.load_error.clone())
    }

    /// Shared snapshot handoff, replacing rather than queueing pending saves.
    pub fn submit(&self, layouts: &LayoutSet) -> u64 {
        let revision = {
            let mut state = self.0.state.lock();
            assert!(!state.stopping, "layout save owner has stopped");
            state.revision = state.revision.checked_add(1).expect("layout save revision");
            state.latest = layouts.clone();
            state.pending = true;
            state.revision
        };
        self.0.changed.notify_one();
        revision
    }

    pub fn set_wakeup(&self, wakeup: impl Fn() + Send + Sync + 'static) {
        self.0.state.lock().wakeup = Some(Arc::new(wakeup));
    }

    /// Only the latest submitted revision may publish visible feedback.
    pub fn completion(&self) -> Option<Completion> {
        let state = self.0.state.lock();
        state
            .completion
            .as_ref()
            .filter(|completion| completion.revision == state.revision)
            .cloned()
    }

    fn flush(&self) -> SaveResult {
        let mut state = self.0.state.lock();
        loop {
            if state.revision == 0 {
                return Ok(());
            }
            if let Some(completion) = &state.completion
                && completion.revision == state.revision
            {
                return completion.result.clone();
            }
            self.0.changed.wait(&mut state);
        }
    }
}

impl Drop for LayoutSaveOwner {
    fn drop(&mut self) {
        self.handle.0.state.lock().stopping = true;
        self.handle.0.changed.notify_one();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("layout save worker panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn named(name: &str) -> LayoutSet {
        let mut set = LayoutSet::default();
        set.rename(name).unwrap();
        set
    }

    #[test]
    fn slow_save_coalesces_to_one_pending_snapshot_and_flushes_latest() {
        let (started, starts) = mpsc::channel();
        let (release, releases) = mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::new()));
        let writes = Arc::clone(&written);
        let gui_thread = std::thread::current().id();
        let owner = LayoutSaveOwner::start(LayoutSet::default(), None, move |snapshot| {
            assert_ne!(std::thread::current().id(), gui_thread);
            started.send(snapshot.active_name().to_owned()).unwrap();
            releases.recv().unwrap();
            writes.lock().push(snapshot.active_name().to_owned());
            Ok(())
        });
        let handle = owner.handle();
        handle.submit(&named("first"));
        assert_eq!(starts.recv().unwrap(), "first");
        for name in ["second", "third", "latest"] {
            handle.submit(&named(name));
        }
        {
            let state = handle.0.state.lock();
            assert!(state.pending);
            assert_eq!(state.latest.active_name(), "latest");
            assert_eq!(state.revision, 4);
        }
        release.send(()).unwrap();
        assert_eq!(starts.recv().unwrap(), "latest");
        assert!(handle.completion().is_none());
        release.send(()).unwrap();
        owner.flush().unwrap();
        assert_eq!(&*written.lock(), &["first", "latest"]);
        assert_eq!(handle.completion().unwrap().revision, 4);
    }

    #[test]
    fn flush_waits_for_the_latest_revision_not_just_the_running_write() {
        let (started, starts) = mpsc::channel();
        let (release, releases) = mpsc::channel();
        let owner = LayoutSaveOwner::start(LayoutSet::default(), None, move |snapshot| {
            started.send(snapshot.active_name().to_owned()).unwrap();
            releases.recv().unwrap();
            Ok(())
        });
        let handle = owner.handle();
        handle.submit(&named("running"));
        starts.recv().unwrap();
        handle.submit(&named("replaced"));
        let flushing = handle.clone();
        let (entered, enters) = mpsc::channel();
        let (finished, finishes) = mpsc::channel();
        let flush = std::thread::spawn(move || {
            entered.send(()).unwrap();
            finished.send(flushing.flush()).unwrap();
        });
        enters.recv().unwrap();
        handle.submit(&named("latest"));
        assert!(matches!(
            finishes.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        assert_eq!(starts.recv().unwrap(), "latest");
        assert!(matches!(
            finishes.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        finishes.recv().unwrap().unwrap();
        flush.join().unwrap();
        assert_eq!(handle.completion().unwrap().revision, 3);
    }

    #[test]
    fn latest_failure_is_visible_and_next_edit_recovers_without_stale_feedback() {
        let (started, starts) = mpsc::channel();
        let (release, releases) = mpsc::channel();
        let owner = LayoutSaveOwner::start(LayoutSet::default(), None, move |snapshot| {
            let name = snapshot.active_name().to_owned();
            started.send(name.clone()).unwrap();
            releases.recv().unwrap();
            if name.contains("fail") {
                Err(name)
            } else {
                Ok(())
            }
        });
        let handle = owner.handle();
        handle.submit(&named("first-failure"));
        starts.recv().unwrap();
        release.send(()).unwrap();
        assert_eq!(owner.flush(), Err("first-failure".into()));
        assert_eq!(
            handle.completion().unwrap().result,
            Err("first-failure".into())
        );
        handle.submit(&named("old-failure"));
        starts.recv().unwrap();
        handle.submit(&named("recovered"));
        release.send(()).unwrap();
        assert_eq!(starts.recv().unwrap(), "recovered");
        assert!(handle.completion().is_none());
        release.send(()).unwrap();
        owner.flush().unwrap();
        assert!(handle.completion().unwrap().result.is_ok());
    }

    #[test]
    fn window_handles_do_not_own_or_flush_the_writer_and_teardown_drains_pending() {
        let (started, starts) = mpsc::channel();
        let (release, releases) = mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::new()));
        let writes = Arc::clone(&written);
        let owner = LayoutSaveOwner::start(LayoutSet::default(), None, move |snapshot| {
            started.send(()).unwrap();
            releases.recv().unwrap();
            writes.lock().push(snapshot.active_name().to_owned());
            Ok(())
        });
        let window = owner.handle();
        window.submit(&named("old"));
        starts.recv().unwrap();
        window.submit(&named("reopened"));
        drop(window);
        let reopened = owner.handle();
        assert_eq!(reopened.snapshot().0.active_name(), "reopened");
        assert!(reopened.0.state.lock().pending);
        let (dropping, drops) = mpsc::channel();
        let (stopped, stops) = mpsc::channel();
        let teardown = std::thread::spawn(move || {
            dropping.send(()).unwrap();
            drop(owner);
            stopped.send(()).unwrap();
        });
        drops.recv().unwrap();
        assert!(matches!(stops.try_recv(), Err(mpsc::TryRecvError::Empty)));
        release.send(()).unwrap();
        starts.recv().unwrap();
        release.send(()).unwrap();
        stops.recv().unwrap();
        teardown.join().unwrap();
        assert_eq!(&*written.lock(), &["old", "reopened"]);
    }

    #[test]
    fn failed_atomic_replacement_keeps_disk_and_latest_named_state_can_retry() {
        use std::{
            fs,
            sync::atomic::{AtomicU64, Ordering},
        };
        static ID: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "rivu-layout-owner-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("layout.json");
        let original = named("original");
        original.save(&path).unwrap();
        let previous = fs::read(&path).unwrap();
        let owner = LayoutSaveOwner::new(path.clone());
        let handle = owner.handle();
        let mut invalid = named("invalid");
        invalid.current_mut().next_id = 0;
        handle.submit(&invalid);
        assert!(owner.flush().is_err());
        assert_eq!(fs::read(&path).unwrap(), previous);
        let mut latest = named("latest");
        latest.duplicate("extra").unwrap();
        latest.select("latest").unwrap();
        latest.remove("extra").unwrap();
        // Force an actual rename failure, then repair the destination. No
        // retry loop or resubmission of an older snapshot may overwrite latest.
        let backup = directory.join("previous.json");
        fs::rename(&path, &backup).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), b"unchanged").unwrap();
        handle.submit(&latest);
        assert!(owner.flush().is_err());
        assert_eq!(fs::read(path.join("keep")).unwrap(), b"unchanged");
        assert_eq!(fs::read(&backup).unwrap(), previous);
        assert_eq!(handle.snapshot().0.active_name(), "latest");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        fs::remove_dir_all(&path).unwrap();
        fs::rename(&backup, &path).unwrap();
        handle.submit(&latest);
        owner.flush().unwrap();
        assert_eq!(LayoutSet::load(&path).unwrap().active_name(), "latest");
        assert_eq!(
            LayoutSet::load(&path).unwrap().names().collect::<Vec<_>>(),
            ["latest"]
        );
        drop(owner);
        fs::remove_dir_all(directory).unwrap();
    }
}
