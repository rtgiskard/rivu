use super::*;
use async_executor::Executor;
use futures::channel::oneshot;

const BLOCKING_THREADS: usize = 4;

/// Concrete execution resources for bounded synchronous stages. Callers pass
/// this owner explicitly; business futures never require ambient runtime state.
pub(crate) struct BlockingPool {
    slots: Arc<async_lock::Semaphore>,
    capacity: usize,
}
impl BlockingPool {
    pub(crate) fn new(slots: usize) -> Arc<Self> {
        assert!(slots > 0);
        Arc::new(Self {
            slots: Arc::new(async_lock::Semaphore::new(slots)),
            capacity: slots,
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
pub(in crate::core) struct TaskRuntime {
    pub(in crate::core) handle: Arc<Executor<'static>>,
    pub(in crate::core) workers: Arc<BlockingPool>,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}
impl TaskRuntime {
    pub(in crate::core) fn start() -> Result<Self> {
        Self::with_blocking_threads(BLOCKING_THREADS)
    }
    pub(in crate::core) fn with_blocking_threads(threads: usize) -> Result<Self> {
        let workers = BlockingPool::new(threads);
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
            let mut retired = Vec::with_capacity(self.workers.capacity);
            for _ in 0..self.workers.capacity {
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
