use super::*;
use anyhow::ensure;

/// The owner outlives request timeouts and retires only after its connection closes.
pub(in crate::core) struct OptimizationTask {
    pub(in crate::core) finished: oneshot::Receiver<Result<DatabaseOptimization>>,
    pub(in crate::core) reply: Option<oneshot::Sender<CoreResponse>>,
    worker: Option<async_executor::Task<()>>,
}

impl OptimizationTask {
    fn join(&mut self) -> Result<()> {
        super::retire_optional_worker(
            &mut self.worker,
            "Database optimization worker panicked; the database may already have changed",
        )
    }
}

impl Drop for OptimizationTask {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

impl Core {
    pub(in crate::core) fn optimize_database(&mut self) -> Result<()> {
        ensure!(
            self.active_optimization.is_none(),
            "Database optimization is already running"
        );
        ensure!(
            self.active_external_admission.is_none(),
            "Database optimization requires no active external file admission"
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
        self.ensure_queue_control_ready().context(
            "Recovering queue before database optimization failed; optimization has not started",
        )?;
        let path = self.store.database_path()?;
        let (finished_tx, finished) = oneshot::channel();
        let workers = self.workers.clone();
        let reads = self
            .read_queries
            .as_ref()
            .map(|reads| Arc::clone(&reads.fence));
        let worker = self.executor.spawn(async move {
            if let Some(reads) = reads { reads.idle().await; }
            // SQLite VACUUM/ANALYZE/checkpoint are synchronous and may commit.
            // Await their real completion even during shutdown; cancellation of
            // an async waiter cannot retire the connection or undo those facts.
            let result = workers.execute(move || {
                Store::open(&path).and_then(|store| {
                    store
                        .optimize()
                        .context("Database optimization failed; the database may already have changed")
                })
            })
            .await
            .unwrap_or_else(|error| {
                Err(anyhow::anyhow!(
                    "Database optimization worker failed: {error}; the database may already have changed"
                ))
            });
            // Single-flight has exactly one completion slot. Delivery cannot
            // block shutdown, including after the requester has timed out.
            let _ = finished_tx.send(result);
        });
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
    ) -> (
        Option<oneshot::Sender<CoreResponse>>,
        Result<DatabaseOptimization>,
    ) {
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
        let result = result.inspect(|optimization| {
            self.state.system.database_optimization = Some(optimization.clone());
        });
        (task.reply.take(), result)
    }

    pub(in crate::core) fn shutdown_optimization(
        &mut self,
    ) -> Option<(
        Option<oneshot::Sender<CoreResponse>>,
        Result<DatabaseOptimization>,
    )> {
        let task = self.active_optimization.as_mut()?;
        // VACUUM is not cancellable here. Shutdown waits for physical completion,
        // with no hard duration bound and no replay after an unknown outcome.
        let result = task.join().and_then(|()| {
            task.finished.try_recv().ok().flatten().unwrap_or_else(|| {
                Err(anyhow::anyhow!(
                    "Database optimization worker ended without a result; the database may already have changed"
                ))
            })
        });
        Some(self.finish_optimization(result))
    }
}
