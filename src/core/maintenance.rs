use super::*;
use anyhow::ensure;

/// The owner outlives request timeouts and retires only after its connection closes.
pub(in crate::core) struct OptimizationTask {
    pub(in crate::core) finished: Receiver<Result<DatabaseOptimization>>,
    pub(in crate::core) reply: Option<Sender<CoreResponse>>,
    worker: Option<JoinHandle<()>>,
}

impl OptimizationTask {
    fn join(&mut self) -> Result<()> {
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| {
                anyhow::anyhow!(
                    "Database optimization worker panicked; the database may already have changed"
                )
            })?;
        }
        Ok(())
    }
}

impl Drop for OptimizationTask {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

impl Core {
    pub(in crate::core) fn optimize_database(&mut self) -> Result<()> {
        self.state.system.database_optimization = None;
        ensure!(
            self.active_optimization.is_none(),
            "Database optimization is already running"
        );
        ensure!(
            self.state.playback.status == PlaybackStatus::Stopped
                && self.active_scan.is_none()
                && !self.state.system.scanning,
            "Database optimization requires stopped playback and no active scan"
        );
        ensure!(
            self.active_relocation.is_none(),
            "Database optimization requires no active relocation"
        );
        ensure!(
            self.active_export.is_none(),
            "Database optimization requires no active playlist export"
        );
        let current_id = self.state.queue.current_id;
        let path = self.store.database_path()?;
        let (finished_tx, finished) = bounded(1);
        let worker = thread::Builder::new()
            .name("rivu-database-optimization".into())
            .spawn(move || {
                let result = Store::open(&path).and_then(|store| {
                    store.set_queue_current(current_id)?;
                    store.optimize().context(
                        "Database optimization failed; the database may already have changed",
                    )
                });
                // Single-flight has exactly one completion slot. Delivery cannot
                // block shutdown, including after the requester has timed out.
                let _ = finished_tx.try_send(result);
            })?;
        self.active_optimization = Some(OptimizationTask {
            finished,
            reply: None,
            worker: Some(worker),
        });
        tracing::info!("database_optimization_started");
        Ok(())
    }

    pub(in crate::core) fn finish_optimization(
        &mut self,
        result: Result<DatabaseOptimization>,
    ) -> (Option<Sender<CoreResponse>>, Result<()>) {
        let Some(mut task) = self.active_optimization.take() else {
            return (
                None,
                Err(anyhow::anyhow!("No database optimization is running")),
            );
        };
        // A completion message is not physical retirement: close the worker's
        // Store before releasing maintenance admission or publishing the result.
        let result = task.join().and(result);
        // VACUUM may have committed even when optimize/checkpoint later failed.
        self.store.invalidate_caches();
        let result = result.map(|optimization| {
            self.state.system.database_optimization = Some(optimization);
        });
        (task.reply.take(), result)
    }

    pub(in crate::core) fn shutdown_optimization(
        &mut self,
    ) -> Option<(Option<Sender<CoreResponse>>, Result<()>)> {
        let task = self.active_optimization.as_mut()?;
        // VACUUM is not cancellable here. Shutdown waits for physical completion,
        // with no hard duration bound and no replay after an unknown outcome.
        let result = task.join().and_then(|()| {
            task.finished.recv().unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "Database optimization worker ended without a result; the database may already have changed"
                ))
            })
        });
        Some(self.finish_optimization(result))
    }
}
