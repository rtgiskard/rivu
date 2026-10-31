//! One session-lifetime writer, with one replaceable, explicitly requested save.
//!
//! Edits only update memory. Manual sync captures its own snapshot; later edits
//! cannot change that request or its feedback. Reopening reads session memory.
//! The session owner flushes the latest dirty state during application teardown,
//! outside GPUI's quit observer; dropping it only drains already requested saves.

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

struct SaveRequest {
    id: u64,
    revision: u64,
    snapshot: LayoutSet,
}

struct State {
    latest: LayoutSet,
    load_error: Option<String>,
    revision: u64,
    saved_revision: Option<u64>,
    request_id: u64,
    pending: Option<SaveRequest>,
    saving_revision: Option<u64>,
    completion: Option<Completion>,
    stopping: bool,
    wakeup: Option<Wakeup>,
}

impl State {
    fn request_save(&mut self) -> u64 {
        self.request_id = self.request_id.checked_add(1).expect("layout save request");
        self.pending = Some(SaveRequest {
            id: self.request_id,
            revision: self.revision,
            snapshot: self.latest.clone(),
        });
        self.request_id
    }
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Clone)]
pub(in crate::gui) struct LayoutPersistenceHandle(Arc<Shared>);

/// Startup-only data. Loading it creates no persistence thread or save request.
pub(crate) struct LoadedLayouts {
    path: PathBuf,
    layouts: LayoutSet,
    error: Option<String>,
    saved: bool,
}

impl LoadedLayouts {
    pub(crate) fn load(path: PathBuf) -> Self {
        let (layouts, error, saved) = match LayoutSet::load(&path) {
            Ok(layouts) => (layouts, None, true),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                (LayoutSet::default(), None, false)
            }
            Err(error) => (LayoutSet::default(), Some(format!("{error:#}")), false),
        };
        Self {
            path,
            layouts,
            error,
            saved,
        }
    }
}

pub(in crate::gui) struct LayoutPersistence {
    handle: LayoutPersistenceHandle,
    worker: Option<JoinHandle<()>>,
}

impl LayoutPersistence {
    pub fn new(loaded: LoadedLayouts) -> Self {
        let LoadedLayouts {
            path,
            layouts,
            error,
            saved,
        } = loaded;
        Self::start(layouts, error, saved, move |snapshot| {
            snapshot.save(&path).map_err(|error| format!("{error:#}"))
        })
    }

    fn start(
        layouts: LayoutSet,
        load_error: Option<String>,
        saved: bool,
        mut save: impl FnMut(&LayoutSet) -> SaveResult + Send + 'static,
    ) -> Self {
        let mut saved_snapshot = saved.then(|| layouts.clone());
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                latest: layouts,
                load_error,
                revision: 0,
                saved_revision: saved.then_some(0),
                request_id: 0,
                pending: None,
                saving_revision: None,
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
                    let request = {
                        let mut state = worker_shared.state.lock();
                        while state.pending.is_none() && !state.stopping {
                            worker_shared.changed.wait(&mut state);
                        }
                        let Some(request) = state.pending.take() else {
                            return;
                        };
                        state.saving_revision = Some(request.revision);
                        request
                    };
                    let result = if saved_snapshot
                        .as_ref()
                        .is_some_and(|saved| request.snapshot.persisted_eq(saved))
                    {
                        // Runtime-only changes must not bypass validation of the
                        // request, but require neither encoding nor filesystem I/O.
                        request
                            .snapshot
                            .validate()
                            .map_err(|error| format!("{error:#}"))
                    } else {
                        save(&request.snapshot)
                    };
                    if result.is_ok() {
                        saved_snapshot = Some(request.snapshot);
                    }
                    if let Err(error) = &result {
                        // Even a superseded failure has a diagnostic; it must
                        // not replace the current operation's UI feedback.
                        tracing::warn!(revision = request.id, %error, "layout_save_failed");
                    }
                    let wakeup = {
                        let mut state = worker_shared.state.lock();
                        state.saving_revision = None;
                        if result.is_ok() {
                            state.saved_revision = Some(request.revision);
                            state.load_error = None;
                        }
                        if request.id == state.request_id {
                            state.completion = Some(Completion {
                                revision: request.id,
                                result,
                            });
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
            handle: LayoutPersistenceHandle(shared),
            worker: Some(worker),
        }
    }

    pub(in crate::gui) fn handle(&self) -> LayoutPersistenceHandle {
        self.handle.clone()
    }

    /// Save the latest dirty session state and wait, only at application teardown.
    pub fn flush(&self) -> SaveResult {
        self.handle.flush()
    }
}

impl LayoutPersistenceHandle {
    pub fn snapshot(&self) -> (LayoutSet, Option<String>) {
        let state = self.0.state.lock();
        (state.latest.clone(), state.load_error.clone())
    }

    /// Update session memory without requesting I/O or waking the save worker.
    pub fn submit(&self, layouts: &LayoutSet) -> u64 {
        let mut state = self.0.state.lock();
        assert!(!state.stopping, "layout save owner has stopped");
        state.revision = state.revision.checked_add(1).expect("layout save revision");
        state.latest = layouts.clone();
        state.revision
    }

    /// Capture the current session state for asynchronous persistence.
    /// The returned request ID is independent of memory-edit revisions.
    pub fn sync(&self) -> u64 {
        let request_id = {
            let mut state = self.0.state.lock();
            assert!(!state.stopping, "layout save owner has stopped");
            state.request_save()
        };
        self.0.changed.notify_one();
        request_id
    }

    pub fn set_wakeup(&self, wakeup: impl Fn() + Send + Sync + 'static) {
        self.0.state.lock().wakeup = Some(Arc::new(wakeup));
    }

    /// Only a newer save request, not a memory edit, supersedes visible feedback.
    pub fn completion(&self) -> Option<Completion> {
        let state = self.0.state.lock();
        state
            .completion
            .as_ref()
            .filter(|completion| completion.revision == state.request_id)
            .cloned()
    }

    fn flush(&self) -> SaveResult {
        let mut state = self.0.state.lock();
        // A fallback is not consent to replace a damaged input. Only explicit
        // sync may authorize replacement; ordinary edits and teardown preserve it.
        if state.load_error.is_some() && state.request_id == 0 {
            return Ok(());
        }
        let mut attempted_revision = None;
        loop {
            let revision = state.revision;
            let requested_latest = state.saving_revision == Some(revision)
                || state
                    .pending
                    .as_ref()
                    .is_some_and(|request| request.revision == revision);
            if requested_latest {
                attempted_revision = Some(revision);
            } else if state.saved_revision != Some(revision) && attempted_revision != Some(revision)
            {
                // Retry a prior failure once per flush, but never loop on a
                // failure from this flush. Recheck memory after every write.
                state.request_save();
                attempted_revision = Some(revision);
                self.0.changed.notify_one();
            }
            if state.pending.is_none() && state.saving_revision.is_none() {
                return if attempted_revision == Some(revision) {
                    state
                        .completion
                        .as_ref()
                        .expect("completed layout save")
                        .result
                        .clone()
                } else {
                    Ok(())
                };
            }
            self.0.changed.wait(&mut state);
        }
    }
}

impl Drop for LayoutPersistence {
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
        let others: Vec<_> = set
            .names()
            .filter(|other| *other != name)
            .map(str::to_owned)
            .collect();
        for other in others {
            set.remove(&other).unwrap();
        }
        set
    }

    fn wait_for_completion(handle: &LayoutPersistenceHandle, request_id: u64) -> Completion {
        let mut state = handle.0.state.lock();
        loop {
            if let Some(completion) = &state.completion
                && completion.revision == request_id
            {
                return completion.clone();
            }
            handle.0.changed.wait(&mut state);
        }
    }

    #[test]
    fn unchanged_sync_and_reverted_edits_do_not_write() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let writes = Arc::clone(&written);
        let original = named("disk");
        let owner = LayoutPersistence::start(original.clone(), None, true, move |snapshot| {
            writes.lock().push(snapshot.active_name().to_owned());
            Ok(())
        });
        let handle = owner.handle();
        for _ in 0..2 {
            handle.submit(&original);
            let request = handle.sync();
            assert!(wait_for_completion(&handle, request).result.is_ok());
        }
        handle.submit(&named("changed"));
        handle.submit(&original);
        owner.flush().unwrap();
        assert!(written.lock().is_empty());
        handle.submit(&named("saved"));
        let request = handle.sync();
        assert!(wait_for_completion(&handle, request).result.is_ok());
        let request = handle.sync();
        assert!(wait_for_completion(&handle, request).result.is_ok());
        owner.flush().unwrap();
        assert_eq!(&*written.lock(), &["saved"]);
    }

    #[test]
    fn successful_running_snapshot_not_newer_memory_becomes_baseline() {
        let (started, starts) = mpsc::channel();
        let (release, releases) = mpsc::channel();
        let original = named("disk");
        let owner = LayoutPersistence::start(original.clone(), None, true, move |snapshot| {
            started.send(snapshot.active_name().to_owned()).unwrap();
            releases.recv().unwrap();
            Ok(())
        });
        let handle = owner.handle();
        handle.submit(&named("captured"));
        let request = handle.sync();
        assert_eq!(starts.recv().unwrap(), "captured");
        handle.submit(&original);
        release.send(()).unwrap();
        assert!(wait_for_completion(&handle, request).result.is_ok());
        let request = handle.sync();
        assert_eq!(starts.recv().unwrap(), "disk");
        release.send(()).unwrap();
        assert!(wait_for_completion(&handle, request).result.is_ok());
        owner.flush().unwrap();
    }

    #[test]
    fn missing_file_is_initialized_on_flush() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("layout.json");
        let owner = LayoutPersistence::new(LoadedLayouts::load(path.clone()));
        let expected = owner.handle().snapshot().0;
        owner.flush().unwrap();
        assert!(LayoutSet::load(&path).unwrap().persisted_eq(&expected));
    }

    #[test]
    fn invalid_file_survives_edits_and_flush_until_explicit_sync() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("layout.json");
        let invalid = b"{\"layouts\": damaged but recoverable";
        std::fs::write(&path, invalid).unwrap();
        let owner = LayoutPersistence::new(LoadedLayouts::load(path.clone()));
        let handle = owner.handle();
        assert!(handle.snapshot().1.is_some());
        handle.submit(&named("replacement"));
        owner.flush().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), invalid);
        let request = handle.sync();
        assert!(wait_for_completion(&handle, request).result.is_ok());
        owner.flush().unwrap();
        assert_eq!(LayoutSet::load(&path).unwrap().active_name(), "replacement");
        assert!(handle.snapshot().1.is_none());
    }

    #[test]
    fn nonregular_layout_is_rejected_and_preserved_on_flush() {
        use std::{ffi::CString, os::unix::fs::symlink};

        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("fifo");
        let name = CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is NUL terminated and mkfifo retains no pointer.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let path = directory.path().join("layout.json");
        symlink(&fifo, &path).unwrap();
        let owner = LayoutPersistence::new(LoadedLayouts::load(path.clone()));
        assert!(owner.handle().snapshot().1.is_some());
        owner.flush().unwrap();
        assert!(std::fs::symlink_metadata(path).unwrap().is_symlink());
        assert!(!std::fs::metadata(fifo).unwrap().is_file());
    }

    #[test]
    fn equal_content_still_rejects_invalid_runtime_state() {
        let original = named("disk");
        let owner = LayoutPersistence::start(original.clone(), None, true, |_| {
            panic!("equal persisted content must not reach the file writer")
        });
        let mut invalid = original;
        invalid.current_mut().next_id = 0;
        let handle = owner.handle();
        handle.submit(&invalid);
        let request = handle.sync();
        assert!(wait_for_completion(&handle, request).result.is_err());
    }

    #[test]
    fn edits_stay_in_memory_across_window_reopen_and_owner_drop() {
        let (write, writes) = mpsc::channel();
        let owner = LayoutPersistence::start(named("disk"), None, true, move |snapshot| {
            write.send(snapshot.active_name().to_owned()).unwrap();
            Ok(())
        });
        let window = owner.handle();
        assert_eq!(window.submit(&named("first")), 1);
        assert_eq!(window.submit(&named("session-only")), 2);
        assert!(window.completion().is_none());
        drop(window);
        let reopened = owner.handle();
        assert_eq!(reopened.snapshot().0.active_name(), "session-only");
        drop(owner);
        assert!(matches!(
            writes.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn flush_saves_latest_dirty_memory_once_and_skips_clean_state() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let writes = Arc::clone(&written);
        let owner = LayoutPersistence::start(named("disk"), None, true, move |snapshot| {
            writes.lock().push(snapshot.active_name().to_owned());
            Ok(())
        });
        owner.flush().unwrap();
        assert!(written.lock().is_empty());
        let handle = owner.handle();
        handle.submit(&named("replaced"));
        handle.submit(&named("latest"));
        owner.flush().unwrap();
        assert_eq!(&*written.lock(), &["latest"]);
        owner.flush().unwrap();
        assert_eq!(&*written.lock(), &["latest"]);
        handle.submit(&named("newest"));
        owner.flush().unwrap();
        assert_eq!(&*written.lock(), &["latest", "newest"]);
    }

    #[test]
    fn sync_coalesces_captured_requests_and_feedback_survives_later_edits() {
        let (started, starts) = mpsc::channel();
        let (release, releases) = mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::new()));
        let writes = Arc::clone(&written);
        let gui_thread = std::thread::current().id();
        let owner = LayoutPersistence::start(LayoutSet::default(), None, true, move |snapshot| {
            assert_ne!(std::thread::current().id(), gui_thread);
            started.send(snapshot.active_name().to_owned()).unwrap();
            releases.recv().unwrap();
            writes.lock().push(snapshot.active_name().to_owned());
            Ok(())
        });
        let handle = owner.handle();
        handle.submit(&named("first"));
        handle.sync();
        assert_eq!(starts.recv().unwrap(), "first");
        handle.submit(&named("replaced"));
        handle.sync();
        handle.submit(&named("manual"));
        let requested = handle.sync();
        handle.submit(&named("session-only"));
        release.send(()).unwrap();
        assert_eq!(starts.recv().unwrap(), "manual");
        assert!(handle.completion().is_none());
        release.send(()).unwrap();
        assert!(wait_for_completion(&handle, requested).result.is_ok());
        handle.submit(&named("later-still"));
        assert_eq!(handle.completion().unwrap().revision, requested);
        assert_eq!(handle.snapshot().0.active_name(), "later-still");
        drop(owner);
        assert_eq!(&*written.lock(), &["first", "manual"]);
    }

    #[test]
    fn flush_waits_for_latest_memory_including_edits_during_a_write() {
        let (started, starts) = mpsc::channel();
        let (release, releases) = mpsc::channel();
        let owner = LayoutPersistence::start(LayoutSet::default(), None, true, move |snapshot| {
            started.send(snapshot.active_name().to_owned()).unwrap();
            releases.recv().unwrap();
            Ok(())
        });
        let handle = owner.handle();
        handle.submit(&named("running"));
        handle.sync();
        assert_eq!(starts.recv().unwrap(), "running");
        handle.submit(&named("latest"));
        let flushing = handle.clone();
        let (finished, finishes) = mpsc::channel();
        let flush = std::thread::spawn(move || {
            finished.send(flushing.flush()).unwrap();
        });
        release.send(()).unwrap();
        assert_eq!(starts.recv().unwrap(), "latest");
        handle.submit(&named("newest"));
        assert!(matches!(
            finishes.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        assert_eq!(starts.recv().unwrap(), "newest");
        assert!(matches!(
            finishes.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        finishes.recv().unwrap().unwrap();
        flush.join().unwrap();
        owner.flush().unwrap();
        assert!(handle.completion().unwrap().result.is_ok());
    }

    #[test]
    fn failure_feedback_survives_edits_and_manual_retry_needs_no_new_edit() {
        let (started, starts) = mpsc::channel();
        let (release, releases) = mpsc::channel();
        let mut attempts = 0;
        let owner = LayoutPersistence::start(
            LayoutSet::default(),
            Some("load failure".into()),
            false,
            move |snapshot| {
                attempts += 1;
                started.send(snapshot.active_name().to_owned()).unwrap();
                releases.recv().unwrap();
                if attempts == 1 {
                    Err("save failure".into())
                } else {
                    Ok(())
                }
            },
        );
        let handle = owner.handle();
        handle.submit(&named("requested"));
        let failed = handle.sync();
        assert_eq!(starts.recv().unwrap(), "requested");
        handle.submit(&named("later-edit"));
        release.send(()).unwrap();
        assert_eq!(
            wait_for_completion(&handle, failed).result,
            Err("save failure".into())
        );
        assert_eq!(handle.completion().unwrap().revision, failed);
        assert_eq!(handle.snapshot().1.as_deref(), Some("load failure"));
        let retry = handle.sync();
        assert!(retry > failed);
        assert!(handle.completion().is_none());
        assert_eq!(starts.recv().unwrap(), "later-edit");
        release.send(()).unwrap();
        assert!(wait_for_completion(&handle, retry).result.is_ok());
        assert!(handle.snapshot().1.is_none());
        owner.flush().unwrap();
    }

    #[test]
    fn owner_drop_drains_requests_without_persisting_later_window_edits() {
        let (started, starts) = mpsc::channel();
        let (release, releases) = mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::new()));
        let writes = Arc::clone(&written);
        let owner = LayoutPersistence::start(LayoutSet::default(), None, true, move |snapshot| {
            started.send(snapshot.active_name().to_owned()).unwrap();
            releases.recv().unwrap();
            writes.lock().push(snapshot.active_name().to_owned());
            Ok(())
        });
        let window = owner.handle();
        window.submit(&named("old"));
        window.sync();
        assert_eq!(starts.recv().unwrap(), "old");
        window.submit(&named("manual"));
        window.sync();
        window.submit(&named("reopened"));
        drop(window);
        let reopened = owner.handle();
        assert_eq!(reopened.snapshot().0.active_name(), "reopened");
        let (stopped, stops) = mpsc::channel();
        let teardown = std::thread::spawn(move || {
            drop(owner);
            stopped.send(()).unwrap();
        });
        assert!(matches!(stops.try_recv(), Err(mpsc::TryRecvError::Empty)));
        release.send(()).unwrap();
        assert_eq!(starts.recv().unwrap(), "manual");
        release.send(()).unwrap();
        stops.recv().unwrap();
        teardown.join().unwrap();
        assert_eq!(&*written.lock(), &["old", "manual"]);
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
        let owner = LayoutPersistence::new(LoadedLayouts::load(path.clone()));
        let handle = owner.handle();
        let mut invalid = named("invalid");
        invalid.current_mut().next_id = 0;
        handle.submit(&invalid);
        let invalid_request = handle.sync();
        assert!(
            wait_for_completion(&handle, invalid_request)
                .result
                .is_err()
        );
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
        let failed_request = handle.sync();
        assert!(wait_for_completion(&handle, failed_request).result.is_err());
        assert_eq!(fs::read(path.join("keep")).unwrap(), b"unchanged");
        assert_eq!(fs::read(&backup).unwrap(), previous);
        assert_eq!(handle.snapshot().0.active_name(), "latest");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        fs::remove_dir_all(&path).unwrap();
        fs::rename(&backup, &path).unwrap();
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
