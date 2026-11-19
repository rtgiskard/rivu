//! Bounded, cancellable playback file intake before a decoder is created.

use super::FileInput;
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, TryRecvError};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

// A second intake can bypass one stalled read. There is deliberately no queue:
// a regular-file syscall can block indefinitely, so saturating the limit must
// fail promptly rather than leave the current opening behind cancelled work.
const MAX_FILE_LOADS: usize = 2;

pub(crate) struct FileLoader(Arc<AtomicUsize>);

impl Default for FileLoader {
    fn default() -> Self {
        // A detached load can outlive its Worker. Share the bound even across
        // workers, but let isolated tests control their own intake capacity.
        #[cfg(not(test))]
        let active = {
            static ACTIVE: std::sync::LazyLock<Arc<AtomicUsize>> =
                std::sync::LazyLock::new(|| Arc::new(AtomicUsize::new(0)));
            Arc::clone(&ACTIVE)
        };
        #[cfg(test)]
        let active = Arc::new(AtomicUsize::new(0));
        Self(active)
    }
}

struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl FileLoader {
    pub(crate) fn start(
        &self,
        read: impl FnOnce(&AtomicBool) -> Result<FileInput> + Send + 'static,
    ) -> Result<FileLoad> {
        self.0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |active| {
                (active < MAX_FILE_LOADS).then_some(active + 1)
            })
            .ok()
            .context("Audio file intake is busy; retry the load once an earlier read finishes")?;
        let slot = Slot(Arc::clone(&self.0));
        let state = Arc::new(Completion::default());
        let load = Arc::clone(&state);
        let (done, ready) = crossbeam_channel::bounded(0);
        std::thread::Builder::new()
            .name("rivu-audio-file".into())
            .spawn(move || {
                let result = read(&load.cancelled);
                if !load.cancelled.load(Ordering::Relaxed) {
                    let _ = load.result.set(result);
                } else {
                    drop(result);
                }
                drop(load);
                drop(slot);
                // No consumable message: readiness stays observable until the
                // owning opening takes the result (like the SoundFont wait).
                drop(done);
            })
            .context("Starting audio file intake")?;
        Ok(FileLoad { state, ready })
    }
}

#[derive(Default)]
struct Completion {
    cancelled: AtomicBool,
    result: OnceLock<Result<FileInput>>,
}

pub(crate) struct FileLoad {
    state: Arc<Completion>,
    ready: Receiver<()>,
}

impl FileLoad {
    pub(crate) fn ready(&self) -> &Receiver<()> {
        &self.ready
    }

    pub(crate) fn poll(&mut self) -> Result<Option<FileInput>> {
        if self.ready.try_recv() == Err(TryRecvError::Empty) {
            return Ok(None);
        }
        Arc::get_mut(&mut self.state)
            .and_then(|state| state.result.take())
            .context("Audio file intake stopped without a result")?
            .map(Some)
    }
}

impl Drop for FileLoad {
    fn drop(&mut self) {
        // This abandons the result, not the running syscall or its slot. The
        // parser observes cancellation at its existing work checkpoints.
        self.state.cancelled.store(true, Ordering::Relaxed);
    }
}
