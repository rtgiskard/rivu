use super::*;
use tokio::{runtime, sync::oneshot};

// Scan preparation, relocation verification and export may coexist. Each
// advances through bounded async stages instead of reserving a blocking slot
// while awaiting Core or another stage. SQLite maintenance excludes these jobs.
const BLOCKING_THREADS: usize = 4;

/// One continuously driven runtime, retained through Core and IPC retirement.
/// Async jobs await Core replies, cancellation and filesystem progress here.
/// Synchronous probes, SQLite units and Tokio's ordinary-file backend use the
/// blocking pool; replies, admission and real-time audio never borrow its slots.
/// Task retirement means the closure and its database resources have ended and
/// its JoinHandle completed, not that a reusable pool thread has exited.
/// Scheduling uses Tokio's fatal resource-exhaustion semantics: spawn panic is
/// never translated into a safe rejection or a promise that no work was queued.
pub(in crate::core) struct TaskRuntime {
    pub(in crate::core) handle: runtime::Handle,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl TaskRuntime {
    pub(in crate::core) fn start() -> Result<Self> {
        Self::with_blocking_threads(BLOCKING_THREADS)
    }

    pub(in crate::core) fn with_blocking_threads(blocking_threads: usize) -> Result<Self> {
        let runtime = runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .max_blocking_threads(blocking_threads)
            .thread_name("rivu-blocking")
            .build()?;
        let handle = runtime.handle().clone();
        let (stop, stopped) = oneshot::channel();
        let worker = thread::Builder::new()
            .name("rivu-tokio".into())
            .spawn(move || {
                runtime.block_on(async {
                    let _ = stopped.await;
                })
            })?;
        Ok(Self {
            handle,
            stop: Some(stop),
            worker: Some(worker),
        })
    }
}

impl Drop for TaskRuntime {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Poll on the calling synchronous owner, using the shared reactor for I/O/timers.
/// Core and GUI/MPRIS use this; never call it on the reactor. IPC awaits directly.
pub(in crate::core) fn wait_on<T>(handle: &runtime::Handle, future: impl Future<Output = T>) -> T {
    let _guard = handle.enter();
    futures::executor::block_on(future)
}
