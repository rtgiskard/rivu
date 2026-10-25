use super::*;
use anyhow::{bail, ensure};

pub(in crate::core) enum Event {
    Progress(ScanProgress),
    KnownSources {
        paths: Vec<PathBuf>,
        reply: oneshot::Sender<Result<Vec<Option<library::KnownFile>>>>,
    },
    Prepared(Result<library::ExternalAdmission>),
    Saved(Box<Saved>),
}

pub(in crate::core) struct Saved {
    committed: bool,
    uncertain: bool,
    impacts: Option<crate::store::ScanImpacts>,
    inserted: Option<Vec<u64>>,
    queue: Option<Result<QueueState>>,
    track_total: Option<Result<usize>>,
    history: Option<Result<Vec<HistoryEntry>>>,
    current_track: Option<Result<Option<Track>>>,
    rejected: usize,
    errors: Vec<String>,
    error: Option<anyhow::Error>,
}

impl Saved {
    fn new(rejected: usize, errors: Vec<String>) -> Self {
        Self {
            committed: false,
            uncertain: false,
            impacts: None,
            inserted: None,
            queue: None,
            track_total: None,
            history: None,
            current_track: None,
            rejected,
            errors,
            error: None,
        }
    }

    fn uncertain(error: impl Into<anyhow::Error>) -> Self {
        let mut saved = Self::new(0, Vec::new());
        saved.uncertain = true;
        saved.error = Some(error.into());
        saved
    }
}

struct SavingCompletion {
    sender: Option<oneshot::Sender<Saved>>,
    saved: Option<Saved>,
}

impl Drop for SavingCompletion {
    fn drop(&mut self) {
        if let Some(mut saved) = self.saved.take() {
            if thread::panicking() {
                saved.uncertain = true;
                saved.error = Some(anyhow::anyhow!(
                    "External audio saving panicked; database changes may already be committed; inspect the queue before retrying"
                ));
            }
            if let Some(sender) = self.sender.take() {
                let _ = sender.send(saved);
            }
        }
    }
}

fn save_external(
    path: PathBuf,
    mut prepared: library::ExternalAdmission,
    index: Option<usize>,
    queue_total: usize,
    current_id: Option<u64>,
    sender: oneshot::Sender<Saved>,
) {
    let mut completion = SavingCompletion {
        sender: Some(sender),
        saved: Some(Saved::new(
            prepared.rejected,
            std::mem::take(&mut prepared.errors),
        )),
    };
    let saved = completion.saved.as_mut().unwrap();
    let store = match Store::open(&path) {
        Ok(store) => store,
        Err(error) => {
            saved.error = Some(error.context("Opening external audio saving database"));
            return;
        }
    };
    let (inserted, impacts, _) = match store.admit_external_queue(&prepared, index, current_id) {
        Ok(committed) => committed,
        Err(error) => {
            saved.error = Some(error.context("External audio admission was not committed"));
            return;
        }
    };
    saved.committed = true;
    saved.impacts = Some(impacts);
    let total = queue_total.checked_add(inserted.len());
    saved.inserted = Some(inserted);
    saved.queue = Some(
        total
            .context("Queue count overflow after external admission")
            .and_then(|total| store.queue_projection(total, 0)),
    );
    if impacts.structure {
        saved.track_total = Some(store.track_count());
    }
    if impacts.library {
        saved.history = Some(store.history(MAX_QUERY_ROWS));
        saved.current_track = Some((|| {
            current_id
                .map(|id| store.queue_track_id(id))
                .transpose()?
                .map(|id| store.track(id))
                .transpose()
                .map(Option::flatten)
        })());
    }
}

/// Preparation is cancellable. Saving is an explicit, non-cancellable phase;
/// both phases retain their physical worker until all resources have retired.
pub(in crate::core) struct ExternalAdmissionTask {
    pub(in crate::core) status: ScanTaskStatus,
    events: mpsc::Receiver<Event>,
    finished: oneshot::Receiver<Event>,
    database_path: PathBuf,
    index: Option<usize>,
    expected_revision: Option<u64>,
    cancellation: library::ScanCancellation,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl ExternalAdmissionTask {
    pub(in crate::core) fn is_saving(&self) -> bool {
        self.status.progress.phase == ScanPhase::Saving
    }

    pub(in crate::core) fn cancel(&self) -> Result<()> {
        ensure!(
            !self.is_saving(),
            "External audio is already saving; cancellation is no longer available"
        );
        self.cancellation.cancel();
        Ok(())
    }

    pub(in crate::core) async fn next_event(&mut self) -> Event {
        tokio::select! {
            result = &mut self.finished => result.unwrap_or_else(|_| {
                if self.is_saving() {
                    Event::Saved(Box::new(Saved::uncertain(anyhow::anyhow!("External audio saving ended without a result; database changes may already be committed"))))
                } else {
                    Event::Prepared(Err(anyhow::anyhow!("External audio preparation ended without a result; nothing was registered or queued")))
                }
            }),
            event = async {
                match self.events.recv().await {
                    Some(event) => event,
                    None => std::future::pending().await,
                }
            } => event,
        }
    }

    fn join(&mut self) -> Result<()> {
        if let Some(worker) = self.worker.take() {
            futures::executor::block_on(worker).context("External audio worker panicked")?;
        }
        Ok(())
    }

    fn retire(&mut self) -> Result<()> {
        if !self.is_saving() {
            self.cancellation.cancel();
        }
        self.events.close();
        while self.events.try_recv().is_ok() {}
        self.join()
    }
}

impl Drop for ExternalAdmissionTask {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

async fn lookup_sources(
    sender: mpsc::Sender<Event>,
    paths: Vec<PathBuf>,
    cancellation: library::ScanCancellation,
) -> Result<Vec<Option<library::KnownFile>>> {
    cancellation.check()?;
    let (reply, receive) = oneshot::channel();
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => bail!("External audio admission cancelled"),
        result = sender.send(Event::KnownSources { paths, reply }) => {
            result.context("External audio admission owner disconnected")?;
        }
    }
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => bail!("External audio admission cancelled"),
        result = receive => result.context("External audio source lookup disconnected")?,
    };
    cancellation.check()?;
    result
}

impl Core {
    pub(in crate::core) fn start_external_admission(
        &mut self,
        paths: Vec<PathBuf>,
        index: Option<usize>,
        expected_revision: Option<u64>,
    ) -> Result<()> {
        ensure!(!paths.is_empty(), "No external audio files supplied");
        ensure!(
            self.active_external_admission.is_none() && self.active_scan.is_none(),
            "A media task is already running"
        );
        ensure!(
            self.active_relocation.is_none(),
            "A relocation verification is already running"
        );
        ensure!(
            !self.database_maintenance_active(),
            "Database maintenance is in progress; external audio admission was not started"
        );
        if let Some(index) = index {
            self.check_queue_insertion(
                index,
                expected_revision
                    .context("Positional external admission requires a queue revision")?,
            )?;
        } else if let Some(expected) = expected_revision {
            ensure!(
                self.state.queue.revision == expected,
                "Queue revision changed"
            );
        }
        let database_path = self.store.database_path()?;
        let id = self.next_scan_id;
        let next_id = id.checked_add(1).context("Media task IDs exhausted")?;
        let total = paths.len();
        let cancellation = library::ScanCancellation::default();
        let worker_cancellation = cancellation.clone();
        let ffmpeg_enabled = self.state.system.config.ffmpeg_enabled;
        let (sender, events) = mpsc::channel(3);
        let (finished_tx, finished) = oneshot::channel();
        let worker = self.executor.spawn(async move {
            let lookup_cancellation = worker_cancellation.clone();
            let lookup_sender = sender.clone();
            let mut last_phase = None;
            let mut last_emit = Instant::now() - Duration::from_millis(100);
            let preparation = library::prepare_external_admission(
                paths,
                move |paths| {
                    lookup_sources(lookup_sender.clone(), paths, lookup_cancellation.clone())
                },
                ffmpeg_enabled,
                &worker_cancellation,
                move |progress| {
                    if last_phase != Some(progress.phase)
                        || last_emit.elapsed() >= Duration::from_millis(100)
                    {
                        last_phase = Some(progress.phase);
                        last_emit = Instant::now();
                        if sender.capacity() == sender.max_capacity() {
                            let _ = sender.try_send(Event::Progress(progress));
                        }
                    }
                },
            );
            let result = std::panic::AssertUnwindSafe(preparation)
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!(
                        "External audio preparation panicked; nothing was registered or queued"
                    ))
                });
            let _ = finished_tx.send(Event::Prepared(result));
        });
        self.active_external_admission = Some(ExternalAdmissionTask {
            status: ScanTaskStatus {
                id,
                progress: ScanProgress {
                    phase: ScanPhase::Discovering,
                    processed: 0,
                    total: Some(total),
                    errors: 0,
                    path: None,
                },
                message: "Preparing external audio files…".into(),
                library_committed: false,
                playlist_id: None,
                error: None,
            },
            events,
            finished,
            database_path,
            index,
            expected_revision,
            cancellation,
            worker: Some(worker),
        });
        self.next_scan_id = next_id;
        self.project_scan();
        Ok(())
    }

    pub(in crate::core) fn cancel_external_admission(&mut self, id: u64) -> Result<()> {
        self.active_external_admission
            .as_ref()
            .filter(|task| task.status.id == id)
            .context("Unknown or expired external audio admission task")?
            .cancel()
    }

    fn retain_external(&mut self, status: ScanTaskStatus) {
        self.retain_scan_outcome(status, crate::projection::MediaTaskOwner::Queue);
        self.project_scan();
        self.publish();
    }

    fn finish_external_preparation(&mut self, result: Result<library::ExternalAdmission>) {
        let Some(mut task) = self.active_external_admission.take() else {
            return;
        };
        let result = task.join().and(result);
        if task.cancellation.is_cancelled() {
            task.status.progress.phase = ScanPhase::Cancelled;
            task.status.message =
                "External audio admission cancelled; nothing was registered or queued".into();
            if let Err(error) = result {
                tracing::debug!(error = %error, "external_admission_cancelled");
            }
            self.retain_external(task.status.clone());
            return;
        }
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                task.status.progress.phase = ScanPhase::Failed;
                task.status.message = "External audio preparation failed".into();
                task.status.error = Some(format!("{error:#}"));
                self.retain_external(task.status.clone());
                return;
            }
        };
        let requested = prepared.occurrences.len() + prepared.rejected;
        task.status.progress.processed = requested;
        task.status.progress.total = Some(requested);
        task.status.progress.errors = prepared.rejected;
        if prepared.occurrences.is_empty() {
            task.status.progress.phase = ScanPhase::Failed;
            task.status.message = "No supported, accessible external audio files".into();
            task.status.error = Some(prepared.errors.join("\n"));
            self.retain_external(task.status.clone());
            return;
        }
        let baseline = match task.expected_revision {
            Some(expected) => {
                self.check_queue_insertion(task.index.unwrap_or(self.queue_ids.len()), expected)
            }
            None => Ok(()),
        };
        if let Err(error) = baseline {
            task.status.progress.phase = ScanPhase::Failed;
            task.status.message = error.to_string();
            task.status.error = Some(format!("{error:#}"));
            self.retain_external(task.status.clone());
            return;
        }
        task.status.progress.phase = ScanPhase::Saving;
        task.status.message = "Saving external audio and queue…".into();
        task.status.progress.path = None;
        let (finished_tx, finished) = oneshot::channel();
        let (facts_tx, facts_rx) = oneshot::channel();
        let path = std::mem::take(&mut task.database_path);
        let index = task.index;
        let queue_total = self.queue_ids.len();
        let current_id = self.state.queue.current_id;
        task.worker = Some(self.executor.spawn(async move {
            let joined = tokio::task::spawn_blocking(move || {
                save_external(path, prepared, index, queue_total, current_id, facts_tx);
            }).await;
            let mut saved = facts_rx.await.unwrap_or_else(|_| Saved::uncertain(anyhow::anyhow!("External audio saving worker produced no completion facts")));
            if let Err(error) = joined {
                saved.uncertain = true;
                saved.error = Some(anyhow::anyhow!("External audio saving worker failed: {error}; database changes may already be committed"));
            }
            let _ = finished_tx.send(Event::Saved(Box::new(saved)));
        }));
        task.finished = finished;
        self.active_external_admission = Some(task);
        self.project_scan();
        self.publish();
    }

    fn finish_external_saving(&mut self, mut saved: Saved) {
        let Some(mut task) = self.active_external_admission.take() else {
            return;
        };
        if let Err(error) = task.join() {
            saved.uncertain = true;
            saved.error = Some(error);
        }
        if saved.uncertain {
            self.database_outcome_uncertain();
            task.status.library_committed = saved.committed;
            task.status.progress.phase = ScanPhase::Failed;
            task.status.message = "Database outcome is uncertain; restart Rivu".into();
            task.status.error = Some(saved.error.map_or_else(
                || task.status.message.clone(),
                |error| format!("{error:#}; restart Rivu"),
            ));
            self.retain_external(task.status.clone());
            return;
        }
        task.status.library_committed = saved.committed;
        let invalid_diagnostics = saved.errors.len();
        let mut errors = saved.errors;
        if let Some(error) = saved.error {
            errors.push(format!("{error:#}"));
        }
        if saved.committed || saved.uncertain {
            self.store.invalidate_caches();
        }
        let impacts = saved.impacts;
        if let Some(impacts) = impacts {
            self.invalidate_scan_impacts(impacts);
        }
        let queued = saved.inserted.as_ref().map_or(0, Vec::len);
        if let Some(inserted) = saved.inserted {
            let index = task.index.unwrap_or(self.queue_ids.len());
            self.queue_ids.splice(index..index, inserted);
        }
        if let Some(queue) = saved.queue {
            match queue {
                Ok(mut queue) => {
                    queue.revision = self.state.queue.revision;
                    self.state.queue = queue;
                }
                Err(error) => errors.push(format!(
                    "Queue committed but refreshing its projection failed: {error:#}"
                )),
            }
        }
        if let Some(total) = saved.track_total {
            match total {
                Ok(total) => self.state.library.track_total = total,
                Err(error) => errors.push(format!("Refreshing library count failed: {error:#}")),
            }
        }
        if let Some(history) = saved.history {
            match history {
                Ok(history) => self.state.library.history = Arc::new(history),
                Err(error) => errors.push(format!("Refreshing library history failed: {error:#}")),
            }
        }
        if let Some(track) = saved.current_track
            && let Err(error) = track.and_then(|track| self.replace_current_track(track))
        {
            errors.push(format!(
                "Refreshing current track after admission failed: {error:#}"
            ));
        }
        task.status.message = if saved.committed {
            format!(
                "Queued {queued} external audio occurrences; skipped {} invalid paths",
                saved.rejected
            )
        } else if saved.uncertain {
            "External audio outcome is uncertain; inspect the queue before retrying".into()
        } else {
            "External audio admission was not committed".into()
        };
        task.status.progress.errors = saved
            .rejected
            .saturating_add(errors.len().saturating_sub(invalid_diagnostics));
        task.status.error = (!errors.is_empty()).then(|| errors.join("\n"));
        task.status.progress.phase = if task.status.error.is_some() {
            ScanPhase::Failed
        } else {
            ScanPhase::Completed
        };
        self.retain_external(task.status.clone());
    }

    pub(in crate::core) fn external_admission_event(&mut self, event: Event) {
        match event {
            Event::Progress(progress) => {
                if let Some(task) = self
                    .active_external_admission
                    .as_mut()
                    .filter(|task| !task.is_saving())
                {
                    task.status.progress = progress;
                    task.status.message = "Reading external audio files…".into();
                    self.project_scan();
                    self.publish();
                }
            }
            Event::KnownSources { paths, reply } => {
                let result = (|| {
                    ensure!(
                        self.active_external_admission
                            .as_ref()
                            .is_some_and(|task| !task.is_saving()),
                        "External audio preparation is no longer active"
                    );
                    ensure!(
                        paths.len() <= library::SOURCE_BATCH_SIZE,
                        "External source lookup exceeds batch bound"
                    );
                    paths
                        .iter()
                        .map(|path| self.store.known_source(path))
                        .collect()
                })();
                let _ = reply.send(result);
            }
            Event::Prepared(result) => self.finish_external_preparation(result),
            Event::Saved(saved) => self.finish_external_saving(*saved),
        }
    }

    pub(in crate::core) fn shutdown_external_admission(&mut self) {
        let Some(task) = self.active_external_admission.as_mut() else {
            return;
        };
        if task.is_saving() {
            let joined = task.join();
            let event = task.finished.try_recv().unwrap_or_else(|_| {
                Event::Saved(Box::new(Saved::uncertain(anyhow::anyhow!(
                    "External audio saving ended without completion facts during shutdown"
                ))))
            });
            if let Err(error) = joined {
                tracing::error!(error = %error, "external_admission_shutdown_join_failed");
            }
            self.external_admission_event(event);
        } else {
            task.cancellation.cancel();
            let result = task.retire();
            let mut task = self.active_external_admission.take().unwrap();
            task.status.progress.phase = ScanPhase::Cancelled;
            task.status.message = "External audio admission cancelled during shutdown; nothing was registered or queued".into();
            if let Err(error) = result {
                task.status.error = Some(format!("{error:#}"));
            }
            self.retain_external(task.status.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn preparation_drop_joins_physical_worker_after_cancellation() {
        let runtime = TaskRuntime::start().unwrap();
        let cancellation = library::ScanCancellation::default();
        let worker_cancellation = cancellation.clone();
        let retired = Arc::new(AtomicBool::new(false));
        let worker_retired = retired.clone();
        let (sender, events) = mpsc::channel(3);
        let (finished_tx, finished) = oneshot::channel();
        let (entered_tx, entered) = std::sync::mpsc::sync_channel(0);
        let worker = runtime.handle.spawn(async move {
            tokio::task::spawn_blocking(move || {
                entered_tx.send(()).unwrap();
                futures::executor::block_on(worker_cancellation.cancelled());
                worker_retired.store(true, Ordering::Release);
            })
            .await
            .unwrap();
            drop(sender);
            let _ = finished_tx.send(Event::Prepared(Ok(library::ExternalAdmission::default())));
        });
        let task = ExternalAdmissionTask {
            status: ScanTaskStatus {
                id: 1,
                progress: ScanProgress {
                    phase: ScanPhase::ReadingMetadata,
                    processed: 0,
                    total: Some(1),
                    errors: 0,
                    path: None,
                },
                message: String::new(),
                library_committed: false,
                playlist_id: None,
                error: None,
            },
            events,
            finished,
            database_path: PathBuf::new(),
            index: None,
            expected_revision: None,
            cancellation,
            worker: Some(worker),
        };
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!retired.load(Ordering::Acquire));
        drop(task);
        assert!(retired.load(Ordering::Acquire));
    }

    #[test]
    fn lost_commit_facts_block_database_operations_without_replaying_the_save() {
        let (directory, mut core) = super::super::tests::fixture_with_database(None, true);
        let path = directory.path().join("uncertain.wav");
        let mut writer = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 8_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for _ in 0..40_000 {
            writer.write_sample(0_i16).unwrap();
        }
        writer.finalize().unwrap();
        let before = core.queue_ids.clone();
        core.command(Command::QueueExternalPaths {
            paths: vec![path],
            index: None,
            expected_revision: None,
        })
        .unwrap();
        loop {
            let event = wait_on(&core.executor.clone(), async {
                core.active_external_admission
                    .as_mut()
                    .unwrap()
                    .next_event()
                    .await
            });
            if let Event::Saved(saved) = event {
                assert!(saved.committed);
                core.external_admission_event(Event::Saved(Box::new(Saved::uncertain(
                    anyhow::anyhow!("completion facts lost after commit"),
                ))));
                break;
            }
            core.external_admission_event(event);
        }
        assert!(core.database_maintenance_active());
        assert_eq!(core.queue_ids, before);
        assert!(
            core.command(Command::Enqueue { track_ids: vec![1] })
                .is_err()
        );
        assert!(core.save_queue().is_err());
        core.command(Command::Volume { value: 0.37 }).unwrap();
        assert_eq!(core.state.playback.volume, 0.37);
        drop(core);
        let reopened = Store::open(&directory.path().join("library.db")).unwrap();
        let restored = reopened.queue_ids().unwrap();
        assert_eq!(&restored[..before.len()], &before);
        assert_eq!(restored.len(), before.len() + 1);
    }
}
