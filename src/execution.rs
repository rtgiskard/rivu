use anyhow::Result;
use async_executor::Executor;
use futures::channel::oneshot;
use std::{
    sync::Arc,
    thread::{self, JoinHandle},
};

const BLOCKING_LIMIT: usize = 4;

/// Limits concurrent synchronous closures dispatched to the shared blocking executor.
/// The lease follows physical work, not the lifetime of its async waiter.
pub(crate) struct BlockingExecutor {
    slots: Arc<async_lock::Semaphore>,
    concurrency_limit: usize,
}
impl BlockingExecutor {
    pub(crate) fn new(slots: usize) -> Arc<Self> {
        assert!(slots > 0);
        Arc::new(Self {
            slots: Arc::new(async_lock::Semaphore::new(slots)),
            concurrency_limit: slots,
        })
    }

    /// The physical closure owns its lease, even if its async waiter disappears.
    pub(crate) async fn execute<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T> {
        let permit = self.slots.acquire_arc().await;
        blocking::unblock(move || {
            let _permit = permit;
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .map_err(|_| anyhow::anyhow!("Blocking worker panicked"))
        })
        .await
    }
}

/// Application-owned executor, retained through Core and IPC retirement.
pub(crate) struct TaskRuntime {
    pub(crate) handle: Arc<Executor<'static>>,
    pub(crate) workers: Arc<BlockingExecutor>,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}
impl TaskRuntime {
    pub(crate) fn start() -> Result<Self> {
        Self::with_blocking_limit(BLOCKING_LIMIT)
    }
    pub(crate) fn with_blocking_limit(limit: usize) -> Result<Self> {
        let workers = BlockingExecutor::new(limit);
        let handle = Arc::new(Executor::new());
        let executor = Arc::clone(&handle);
        let (stop, stopped) = oneshot::channel();
        let worker = thread::Builder::new()
            .name("rivu-async".into())
            .spawn(move || {
                async_io::block_on(executor.run(async {
                    let _ = stopped.await;
                }));
            })?;
        Ok(Self {
            handle,
            workers,
            stop: Some(stop),
            worker: Some(worker),
        })
    }
}
impl Drop for TaskRuntime {
    fn drop(&mut self) {
        // Even a cancelled async waiter may have dispatched a physical closure.
        // Keep driving jobs while collecting every application-owned lease.
        let _retired = futures::executor::block_on(async {
            let mut retired = Vec::with_capacity(self.workers.concurrency_limit);
            for _ in 0..self.workers.concurrency_limit {
                retired.push(self.workers.slots.acquire_arc().await);
            }
            retired
        });
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
