use super::*;
use anyhow::{bail, ensure};

pub(in crate::core) enum Event {
    Progress(ScanProgress),
    KnownSources {
        paths: Vec<PathBuf>,
        reply: oneshot::Sender<Result<Vec<Option<library::KnownFile>>>>,
    },
    Prepared(Result<library::ExternalAdmission>),
    SavingRetired(Result<()>),
}

/// Evidence for the single admission transaction, independent of view reads.
#[derive(Debug, Default)]
enum AdmissionCommit {
    #[default]
    NotAttempted,
    Pending,
    NotCommitted,
    Confirmed {
        inserted: usize,
        impacts: crate::store::CatalogChanges,
    },
    Unknown,
}

impl AdmissionCommit {
    fn retire(&mut self) {
        if matches!(self, Self::Pending) {
            *self = Self::Unknown;
        }
    }
}

pub(in crate::core) struct Saved {
    commit: AdmissionCommit,
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
            commit: AdmissionCommit::NotAttempted,
            queue: None,
            track_total: None,
            history: None,
            current_track: None,
            rejected,
            errors,
            error: None,
        }
    }

    fn unknown(error: impl Into<anyhow::Error>) -> Self {
        let mut saved = Self::new(0, Vec::new());
        saved.commit = AdmissionCommit::Unknown;
        saved.error = Some(error.into());
        saved
    }

    fn failed(&mut self, error: anyhow::Error) {
        self.commit.retire();
        self.error = Some(match self.error.take() {
            Some(previous) => previous.context(error),
            None => error,
        });
    }

    fn admit(
        &mut self,
        transaction: impl FnOnce() -> Result<(usize, crate::store::CatalogChanges, usize)>,
    ) -> Option<crate::store::CatalogChanges> {
        self.commit = AdmissionCommit::Pending;
        match transaction() {
            Ok((inserted, impacts, _)) => {
                self.commit = AdmissionCommit::Confirmed { inserted, impacts };
                Some(impacts)
            }
            Err(error) => {
                self.commit = AdmissionCommit::NotCommitted;
                self.error = Some(error.context("External audio admission was not committed"));
                None
            }
        }
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
                saved.failed(anyhow::anyhow!(
                    "External audio saving panicked; recorded transaction outcomes are retained, but prepared projections may be stale"
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
    prepared: library::ExternalAdmission,
    index: Option<usize>,
    current_id: Option<u64>,
    queue_revision: u64,
    mut completion: SavingCompletion,
) {
    let saved = completion.saved.as_mut().unwrap();
    let store = match Store::open(&path) {
        Ok(store) => store,
        Err(error) => {
            saved.error = Some(error.context("Opening external audio saving database"));
            return;
        }
    };
    let Some(impacts) = saved.admit(|| store.admit_external_queue(&prepared, index, current_id))
    else {
        return;
    };
    // Publish the prepared bundle only after all reads finish. A projection
    // panic leaves every missing view dirty without erasing the commit facts.
    let dependencies = impacts.dependencies(false);
    let queue =
        store.queue_projection(queue_revision.wrapping_add(u64::from(dependencies.queue_revision)));
    let track_total = dependencies
        .refresh_track_total
        .then(|| store.track_count());
    let history = dependencies
        .refresh_history
        .then(|| store.history(MAX_QUERY_ROWS));
    let current_track = (impacts.current_track == Some(true)).then(|| {
        (|| {
            current_id
                .map(|id| store.queue_track_id(id))
                .transpose()?
                .map(|id| store.track(id))
                .transpose()
                .map(Option::flatten)
        })()
    });
    saved.queue = Some(queue);
    saved.track_total = track_total;
    saved.history = history;
    saved.current_track = current_track;
}

/// Preparation is cancellable. Saving is an explicit, non-cancellable phase;
/// both phases retain their physical worker until all resources have retired.
pub(in crate::core) struct ExternalAdmissionTask {
    pub(in crate::core) status: ScanTaskStatus,
    events: mpsc::Receiver<Event>,
    finished: oneshot::Receiver<Event>,
    saving_facts: Option<oneshot::Receiver<Saved>>,
    database_path: PathBuf,
    index: Option<usize>,
    expected_revision: Option<u64>,
    cancellation: library::ScanCancellation,
    worker: Option<async_executor::Task<()>>,
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
        let is_saving = self.is_saving();
        let finished = (&mut self.finished).fuse();
        let event = async {
            match self.events.recv().await {
                Ok(event) => event,
                Err(_) => std::future::pending().await,
            }
        }
        .fuse();
        futures::pin_mut!(finished, event);
        futures::select! {
            result = finished => result.unwrap_or_else(|_| {
                if is_saving {
                    Event::SavingRetired(Err(anyhow::anyhow!("External audio saving supervisor ended without a result")))
                } else {
                    Event::Prepared(Err(anyhow::anyhow!("External audio preparation ended without a result; nothing was registered or queued")))
                }
            }),
            event = event => event,
        }
    }

    fn saving_result(&mut self, result: Result<()>) -> Saved {
        let retired = self.join();
        let mut saved = self
            .saving_facts
            .take()
            .and_then(|mut facts| facts.try_recv().ok().flatten())
            .unwrap_or_else(|| {
                Saved::unknown(anyhow::anyhow!(
                    "External audio saving worker produced no completion facts"
                ))
            });
        for error in [result.err(), retired.err()].into_iter().flatten() {
            saved.failed(error);
        }
        saved
    }

    fn join(&mut self) -> Result<()> {
        super::retire_optional_worker(&mut self.worker, "External audio worker panicked")
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
    {
        let cancelled = cancellation.cancelled().fuse();
        let send = sender.send(Event::KnownSources { paths, reply }).fuse();
        futures::pin_mut!(cancelled, send);
        futures::select_biased! {
            _ = cancelled => bail!("External audio admission cancelled"),
            result = send => result.context("External audio admission owner disconnected")?,
        }
    }
    let result = {
        let cancelled = cancellation.cancelled().fuse();
        let receive = receive.fuse();
        futures::pin_mut!(cancelled, receive);
        futures::select_biased! {
            _ = cancelled => bail!("External audio admission cancelled"),
            result = receive => result.context("External audio source lookup disconnected")?,
        }
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
    ) -> Result<u64> {
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
        self.ensure_queue_control_ready()?;
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
        let ffmpeg_enabled = self.preferences.settings().ffmpeg_enabled;
        let (sender, events) = async_channel::bounded(3);
        let (finished_tx, finished) = oneshot::channel();
        let workers = self.workers.clone();
        let worker = self.executor.spawn(async move {
            let lookup_cancellation = worker_cancellation.clone();
            let lookup_sender = sender.clone();
            let mut last_phase = None;
            let mut last_emit = Instant::now() - Duration::from_millis(100);
            let preparation = library::prepare_external_admission(
                &workers,
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
                        if sender.is_empty() {
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
            saving_facts: None,
            database_path,
            index,
            expected_revision,
            cancellation,
            worker: Some(worker),
        });
        self.next_scan_id = next_id;
        self.project_scan();
        Ok(id)
    }

    pub(in crate::core) fn cancel_external_admission(&mut self, id: u64) -> Result<()> {
        self.active_external_admission
            .as_ref()
            .filter(|task| task.status.id == id)
            .context("Unknown or expired external audio admission task")?
            .cancel()
    }

    fn retain_external(&mut self, status: ScanTaskStatus) {
        self.retain_scan_outcome(status, rivu_protocol::model::MediaTaskOwner::Queue);
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
        let mut prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                task.status.progress.phase = ScanPhase::Failed;
                task.status.message = "External audio preparation failed".into();
                task.status.error = Some(format!("{error:#}"));
                self.retain_external(task.status.clone());
                return;
            }
        };
        let requested = prepared.occurrences().len() + prepared.rejected;
        task.status.progress.processed = requested;
        task.status.progress.total = Some(requested);
        task.status.progress.errors = prepared.rejected;
        if prepared.occurrences().is_empty() {
            task.status.progress.phase = ScanPhase::Failed;
            task.status.message = "No supported, accessible external audio files".into();
            task.status.error = Some(prepared.errors.join("\n"));
            self.retain_external(task.status.clone());
            return;
        }
        // Preparation can outlive a committed mutation whose projection failed.
        // Recover before validating insertion or capturing Saving's baseline.
        let baseline =
            self.ensure_queue_control_ready()
                .and_then(|()| match task.expected_revision {
                    Some(expected) => self.check_queue_insertion(
                        task.index.unwrap_or(self.state.queue.total),
                        expected,
                    ),
                    None => Ok(()),
                });
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
        let current_id = self.state.queue.current_id;
        let queue_revision = self.state.queue.revision;
        let workers = self.workers.clone();
        // Own completion before scheduling: even an unpolled supervisor or an
        // unexecuted blocking job reports NotAttempted when its closure drops.
        let completion = SavingCompletion {
            sender: Some(facts_tx),
            saved: Some(Saved::new(
                prepared.rejected,
                std::mem::take(&mut prepared.errors),
            )),
        };
        task.saving_facts = Some(facts_rx);
        task.worker = Some(self.executor.spawn(async move {
            let joined = workers
                .execute(move || {
                    save_external(
                        path,
                        prepared,
                        index,
                        current_id,
                        queue_revision,
                        completion,
                    )
                })
                .await;
            let _ = finished_tx.send(Event::SavingRetired(joined));
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
            saved.failed(error);
        }
        saved.commit.retire();
        let (committed, queued) = match saved.commit {
            AdmissionCommit::Confirmed { inserted, impacts } => {
                self.announce_catalog_changes(impacts);
                // Tokens and missing views are independent. None is not a
                // successful empty projection and must remain recoverable.
                self.transport.projection_stale |= saved.queue.is_none();
                self.projections.current_track |= impacts.current_track.unwrap_or(impacts.library)
                    && saved.current_track.is_none();
                (true, inserted)
            }
            _ => (false, 0),
        };
        task.status.library_committed = committed;
        if matches!(saved.commit, AdmissionCommit::Unknown) {
            self.database_outcome_uncertain();
            task.status.progress.phase = ScanPhase::Failed;
            task.status.message = "Database outcome is uncertain; restart Rivu".into();
            task.status.error = Some(saved.error.map_or_else(
                || task.status.message.clone(),
                |error| format!("{error:#}; restart Rivu"),
            ));
            self.retain_external(task.status.clone());
            return;
        }
        let invalid_diagnostics = saved.errors.len();
        let mut errors = saved.errors;
        if let Some(error) = saved.error {
            errors.push(format!("{error:#}"));
        }
        if let Some(queue) = saved.queue
            && let Err(error) = self.accept_queue_projection(queue)
        {
            errors.push(format!(
                "Queue committed but refreshing its projection failed: {error:#}"
            ));
        }
        if let Err(error) = self.accept_prepared_projections(projections::PreparedProjections {
            library_total: saved.track_total,
            history: saved.history,
            ..Default::default()
        }) {
            errors.push(format!("{error:#}"));
        }
        if let Some(track) = saved.current_track {
            self.projections.current_track = true;
            match track.and_then(|track| self.replace_current_track(track)) {
                Ok(()) => {
                    self.projections.current_track = false;
                }
                Err(error) => errors.push(format!(
                    "Refreshing current track after admission failed: {error:#}"
                )),
            }
        }
        task.status.message = if committed {
            format!(
                "Queued {queued} external audio occurrences; skipped {} invalid paths",
                saved.rejected
            )
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
                    self.store.known_sources(&paths)
                })();
                let _ = reply.send(result);
            }
            Event::Prepared(result) => self.finish_external_preparation(result),
            Event::SavingRetired(result) => {
                if let Some(task) = self.active_external_admission.as_mut() {
                    let saved = task.saving_result(result);
                    self.finish_external_saving(saved);
                }
            }
        }
    }

    pub(in crate::core) fn shutdown_external_admission(&mut self) {
        let Some(task) = self.active_external_admission.as_mut() else {
            return;
        };
        if task.is_saving() {
            let joined = task.join();
            let event = task.finished.try_recv().ok().flatten();
            let mut saved = match event {
                Some(Event::SavingRetired(result)) => task.saving_result(result),
                _ => task.saving_result(Err(anyhow::anyhow!(
                    "External audio saving supervisor ended without a result during shutdown"
                ))),
            };
            if let Err(error) = joined {
                saved.failed(error);
            }
            self.finish_external_saving(saved);
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

    // Keep authoritative facts independent from the supervisor's result, just
    // like production. Both the blocking closure and supervisor can unwind.
    fn install_panicking_saving(core: &mut Core, work: impl FnOnce(&mut Saved) + Send + 'static) {
        let (_, events) = async_channel::bounded(3);
        let (finished_tx, finished) = oneshot::channel();
        let (facts_tx, saving_facts) = oneshot::channel();
        let completion = SavingCompletion {
            sender: Some(facts_tx),
            saved: Some(Saved::new(0, Vec::new())),
        };
        let workers = core.workers.clone();
        let worker = core.executor.spawn(async move {
            let _finished = finished_tx;
            let _ = workers
                .execute(move || {
                    let mut completion = completion;
                    work(completion.saved.as_mut().unwrap());
                })
                .await;
            panic!("injected saving supervisor panic before forwarding retirement");
        });
        core.active_external_admission = Some(ExternalAdmissionTask {
            status: ScanTaskStatus {
                id: 1,
                progress: ScanProgress {
                    phase: ScanPhase::Saving,
                    processed: 1,
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
            saving_facts: Some(saving_facts),
            database_path: PathBuf::new(),
            index: None,
            expected_revision: None,
            cancellation: library::ScanCancellation::default(),
            worker: Some(worker),
        });
    }

    fn complete_test_saving(core: &mut Core, shutdown: bool) {
        if shutdown {
            core.shutdown_external_admission();
        } else {
            let event = futures::executor::block_on(
                core.active_external_admission
                    .as_mut()
                    .unwrap()
                    .next_event(),
            );
            core.external_admission_event(event);
        }
    }

    #[test]
    fn unstarted_saving_job_preserves_not_attempted_facts() {
        let (sender, mut facts) = oneshot::channel();
        let completion = SavingCompletion {
            sender: Some(sender),
            saved: Some(Saved::new(0, Vec::new())),
        };
        let job = move || {
            let mut completion = completion;
            completion
                .saved
                .as_mut()
                .unwrap()
                .admit(|| panic!("an unstarted job must never attempt its transaction"));
        };
        drop(job);
        let saved = facts.try_recv().unwrap().unwrap();
        assert!(matches!(saved.commit, AdmissionCommit::NotAttempted));
        assert!(saved.error.is_none());
    }

    #[test]
    fn saving_panics_only_quarantine_an_in_flight_transaction() {
        for shutdown in [false, true] {
            // Not attempted, confirmed rollback, and executing without a result
            // must survive both blocking and supervisor panic identically.
            for stage in 0..3 {
                let (_directory, mut core) = super::super::tests::fixture_with_database(None);
                let before = core.state.query_revisions();
                let queue = core.store.queue_ids().unwrap();
                install_panicking_saving(&mut core, move |saved| {
                    match stage {
                        0 => {}
                        1 => {
                            saved.admit(|| bail!("injected ordinary admission error"));
                            assert!(matches!(saved.commit, AdmissionCommit::NotCommitted));
                        }
                        2 => {
                            saved.admit(|| panic!("injected transaction panic"));
                        }
                        _ => unreachable!(),
                    }
                    panic!("injected panic outside transaction");
                });
                complete_test_saving(&mut core, shutdown);
                let status = core.scan_results.back().unwrap();
                assert!(!status.library_committed);
                assert_eq!(status.progress.phase, ScanPhase::Failed);
                assert!(status.error.is_some());
                assert_eq!(core.database_maintenance_active(), stage == 2);
                assert_eq!(core.state.query_revisions(), before);
                assert_eq!(core.store.queue_ids().unwrap(), queue);
                if stage == 2 {
                    assert!(
                        core.command(Command::Enqueue { track_ids: vec![1] })
                            .is_err()
                    );
                    assert!(core.save_queue().is_err());
                    core.command(Command::Volume { value: 0.37 }).unwrap();
                } else {
                    core.save_queue().unwrap();
                    core.command(Command::Enqueue { track_ids: vec![1] })
                        .unwrap();
                }
            }
        }
    }

    #[test]
    fn projection_panic_after_commit_publishes_facts_and_recovers_without_replaying() {
        for shutdown in [false, true] {
            let (directory, mut core) = super::super::tests::fixture_with_database(None);
            let before = core.state.query_revisions();
            let before_queue = core.store.queue_ids().unwrap();
            let before_total = core.state.library.track_total;
            let database_path = directory.path().join("library.db");
            let record = library::ScanRecord {
                path: directory.path().join("projection-panic.wav"),
                size: 1,
                modified_ns: 1,
                fingerprint: Some(blake3::hash(b"admission-panic").to_hex().to_string()),
                media: MediaInfo {
                    title: "Admission panic".into(),
                    artist: String::new(),
                    album: "Admission".into(),
                    duration: Some(1.0),
                    codec: "pcm".into(),
                    channels: 1,
                    sample_rate: 8_000,
                    bitrate_bps: None,
                    bitrate_exact: false,
                    track_number: None,
                    disc_number: None,
                    bits_per_sample: Some(16),
                    release_date: None,
                },
            };
            let mut recent = record.clone();
            recent.path = core.store.track(1).unwrap().unwrap().path;
            recent.media.title = "Updated recent".into();
            core.store.mark_played(1, 7).unwrap();
            let prepared =
                library::ExternalAdmission::from_test_records(vec![record, recent], vec![0, 1]);
            install_panicking_saving(&mut core, move |saved| {
                let store = Store::open(&database_path).unwrap();
                saved
                    .admit(|| store.admit_external_queue(&prepared, None, None))
                    .unwrap();
                // A successful early read is only part of a prepared bundle.
                let _partial_queue = store.queue_projection(0).unwrap();
                panic!("injected projection panic after confirmed commit and partial read");
            });
            complete_test_saving(&mut core, shutdown);
            let status = core.scan_results.back().unwrap();
            assert!(status.library_committed);
            assert_eq!(status.progress.phase, ScanPhase::Failed);
            assert!(status.message.contains("Queued 2"));
            assert!(status.error.is_some());
            assert!(!core.database_maintenance_active());
            let after = core.state.query_revisions();
            assert_ne!(after.library, before.library);
            assert_ne!(after.structure, before.structure);
            assert_ne!(after.queue, before.queue);
            assert_ne!(after.album, before.album);
            assert_ne!(after.duplicate_groups, before.duplicate_groups);
            assert_ne!(after.duplicate_members, before.duplicate_members);
            assert!(core.transport.projection_stale);
            assert!(core.projections.track_total);
            assert!(core.transport.history_pending);
            assert_eq!(core.state.queue.total, before_queue.len());
            assert_eq!(core.state.library.track_total, before_total);
            let committed_queue = core.store.queue_ids().unwrap();
            assert_eq!(&committed_queue[..before_queue.len()], &before_queue);
            assert_eq!(committed_queue.len(), before_queue.len() + 2);
            core.ensure_queue_control_ready().unwrap();
            let demand = core.projection_demand();
            let prepared = projections::PreparedProjections::read(&core.store, demand);
            core.complete_projection_recovery(demand, prepared);
            assert!(!core.transport.projection_stale);
            assert!(!core.projections.track_total);
            assert!(!core.transport.history_pending);
            assert_eq!(core.state.queue.total, before_queue.len() + 2);
            assert_eq!(core.state.library.track_total, before_total + 1);
            assert_eq!(core.store.queue_ids().unwrap(), committed_queue);
            assert_eq!(core.store.track_count().unwrap(), before_total + 1);
            let recent = core
                .state
                .library
                .history
                .iter()
                .find(|entry| entry.track_id == 1)
                .unwrap();
            assert_eq!(recent.title, "Updated recent");
            assert_eq!(recent.played_at, 7);
        }
    }

    #[test]
    fn preparation_drop_joins_physical_worker_after_cancellation() {
        if crate::test_support::run_in_subprocess(
            "core::admission::tests::preparation_drop_joins_physical_worker_after_cancellation",
            Duration::from_secs(15),
        ) {
            return;
        }
        let runtime = TaskRuntime::start().unwrap();
        let cancellation = library::ScanCancellation::default();
        let worker_cancellation = cancellation.clone();
        let retired = Arc::new(AtomicBool::new(false));
        let worker_retired = retired.clone();
        let (sender, events) = async_channel::bounded(3);
        let (finished_tx, finished) = oneshot::channel();
        let (entered_tx, entered) = std::sync::mpsc::sync_channel(0);
        let workers = runtime.workers.clone();
        let worker = runtime.handle.spawn(async move {
            workers
                .execute(move || {
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
            saving_facts: None,
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
    fn retirement_failure_preserves_known_committed_revisions_without_quarantine() {
        let (directory, mut core) = super::super::tests::fixture_with_database(None);
        let before = core.state.query_revisions();
        let (_, events) = async_channel::bounded(3);
        let (_, finished) = oneshot::channel();
        core.active_external_admission = Some(ExternalAdmissionTask {
            status: ScanTaskStatus {
                id: 1,
                progress: ScanProgress {
                    phase: ScanPhase::Saving,
                    processed: 1,
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
            saving_facts: None,
            database_path: directory.path().join("library.db"),
            index: None,
            expected_revision: None,
            cancellation: library::ScanCancellation::default(),
            worker: None,
        });
        let mut saved = Saved::new(0, Vec::new());
        saved.admit(|| {
            Ok((
                1,
                crate::store::CatalogChanges {
                    changed: true,
                    library: true,
                    structure: true,
                    queue: true,
                    albums: true,
                    duplicate_groups: true,
                    duplicate_members: true,
                    ..Default::default()
                },
                1,
            ))
        });
        saved.failed(anyhow::anyhow!("retirement failed after commit"));
        core.finish_external_saving(saved);
        let after = core.state.query_revisions();
        assert_ne!(after.library, before.library);
        assert_ne!(after.structure, before.structure);
        assert_ne!(after.queue, before.queue);
        assert_ne!(after.album, before.album);
        assert_ne!(after.duplicate_groups, before.duplicate_groups);
        assert_ne!(after.duplicate_members, before.duplicate_members);
        assert!(!core.database_maintenance_active());
        let status = core.scan_results.back().unwrap();
        assert!(status.library_committed);
        assert_eq!(status.progress.phase, ScanPhase::Failed);
    }

    #[test]
    fn shutdown_preserves_saving_supervisor_failure_after_completion() {
        let (directory, mut core) = super::super::tests::fixture_with_database(None);
        let (_, events) = async_channel::bounded(3);
        let (finished_tx, finished) = oneshot::channel();
        let (facts_tx, saving_facts) = oneshot::channel();
        let worker = core.executor.spawn(async move {
            let mut saved = Saved::new(0, Vec::new());
            saved.admit(|| Ok((0, crate::store::CatalogChanges::default(), 0)));
            let _ = facts_tx.send(saved);
            let _ = finished_tx.send(Event::SavingRetired(Ok(())));
            panic!("injected saving supervisor failure after completion");
        });
        core.active_external_admission = Some(ExternalAdmissionTask {
            status: ScanTaskStatus {
                id: 1,
                progress: ScanProgress {
                    phase: ScanPhase::Saving,
                    processed: 1,
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
            saving_facts: Some(saving_facts),
            database_path: directory.path().join("library.db"),
            index: None,
            expected_revision: None,
            cancellation: library::ScanCancellation::default(),
            worker: Some(worker),
        });
        core.shutdown_external_admission();
        assert!(!core.database_maintenance_active());
        let status = core.scan_results.back().unwrap();
        assert!(status.library_committed);
        assert_eq!(status.progress.phase, ScanPhase::Failed);
        assert!(status.error.is_some());
    }

    #[test]
    fn lost_commit_facts_block_database_operations_without_replaying_the_save() {
        let (directory, mut core) = super::super::tests::fixture_with_database(None);
        let path = directory.path().join("uncertain.wav");
        crate::test_support::write_i16_wav(&path, 1, 8_000, std::iter::repeat_n(0, 40_000))
            .unwrap();
        let before = core.store.queue_ids().unwrap();
        let before_total = core.state.queue.total;
        core.command(Command::QueueExternalPaths {
            paths: vec![path],
            index: None,
            expected_revision: None,
        })
        .unwrap();
        loop {
            let event = futures::executor::block_on(async {
                core.active_external_admission
                    .as_mut()
                    .unwrap()
                    .next_event()
                    .await
            });
            if matches!(event, Event::SavingRetired(_)) {
                let mut facts = core
                    .active_external_admission
                    .as_mut()
                    .unwrap()
                    .saving_facts
                    .take()
                    .unwrap();
                let saved = facts.try_recv().unwrap().unwrap();
                assert!(matches!(saved.commit, AdmissionCommit::Confirmed { .. }));
                drop(saved);
                core.external_admission_event(event);
                break;
            }
            core.external_admission_event(event);
        }
        assert!(core.database_maintenance_active());
        assert_eq!(core.state.queue.total, before_total);
        assert!(
            core.command(Command::Enqueue { track_ids: vec![1] })
                .is_err()
        );
        assert!(core.save_queue().is_err());
        core.command(Command::Volume { value: 0.37 }).unwrap();
        assert_eq!(core.preferences.live().volume, 0.37);
        drop(core);
        let reopened = Store::open(&directory.path().join("library.db")).unwrap();
        let restored = reopened.queue_ids().unwrap();
        assert_eq!(&restored[..before.len()], &before);
        assert_eq!(restored.len(), before.len() + 1);
    }
}
