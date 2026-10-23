use super::*;
use anyhow::{bail, ensure};
use futures::FutureExt;
use std::collections::BTreeMap;

fn scan_message(progress: &ScanProgress) -> String {
    let unit = progress.total.map_or_else(
        || progress.processed.to_string(),
        |total| format!("{} / {total}", progress.processed),
    );
    let phase = match progress.phase {
        ScanPhase::Discovering => "Discovering files",
        ScanPhase::ReadingMetadata => "Reading audio metadata",
        ScanPhase::Saving => "Saving library",
        ScanPhase::Completed => "Scan complete",
        ScanPhase::Failed => "Scan failed",
        ScanPhase::Cancelled => "Scan cancelled",
    };
    progress.path.as_ref().map_or_else(
        || format!("{phase} ({unit})"),
        |path| format!("{phase} ({unit}): {}", path.display()),
    )
}
// Capacity three covers coalesced progress, one outstanding lookup (possibly
// abandoned by cancellation), and the reliable terminal event. Reliable sends
// await capacity; progress is droppable and never consumes more than one slot.
async fn deliver_scan_event(
    sender: &tokio::sync::mpsc::Sender<ScanEvent>,
    event: ScanEvent,
) -> Result<()> {
    sender
        .send(event)
        .await
        .map_err(|_| anyhow::anyhow!("scan owner disconnected"))
}

async fn lookup_scan_sources(
    sender: tokio::sync::mpsc::Sender<ScanEvent>,
    id: u64,
    paths: Vec<PathBuf>,
    cancellation: library::ScanCancellation,
) -> Result<Vec<Option<library::KnownFile>>> {
    cancellation.check()?;
    let (reply, receive) = tokio::sync::oneshot::channel();
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => bail!("scan cancelled"),
        result = deliver_scan_event(&sender, ScanEvent::KnownSources { id, paths, reply }) => result?,
    }
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => bail!("scan cancelled"),
        result = receive => result.context("scan source lookup disconnected")?,
    };
    cancellation.check()?;
    result
}

#[derive(Debug)]
enum ScanInput {
    Paths { paths: Vec<PathBuf>, force: bool },
    Import { path: PathBuf, name: String },
}

struct ImportPlaylistRevisions {
    revision: u64,
    entries: Arc<BTreeMap<i64, u64>>,
}

/// A worker's committed facts and prepared projections survive later failures.
pub(super) struct ScanSaved {
    id: u64,
    progress: ScanProgress,
    library_committed: bool,
    playlist_id: Option<i64>,
    playlist_entry_revisions: Option<Arc<BTreeMap<i64, u64>>>,
    outcome_uncertain: bool,
    impacts: Option<crate::store::ScanImpacts>,
    summary: String,
    scan_errors: Option<String>,
    error: Option<anyhow::Error>,
    library_total: Option<Result<usize>>,
    history: Option<Result<Vec<HistoryEntry>>>,
    playlist_total: Option<Result<usize>>,
}

struct ScanSavingCompletion {
    sender: Option<tokio::sync::oneshot::Sender<ScanSaved>>,
    saved: Option<ScanSaved>,
}

impl Drop for ScanSavingCompletion {
    fn drop(&mut self) {
        if let Some(mut saved) = self.saved.take() {
            if thread::panicking() {
                saved.outcome_uncertain = true;
                saved.error = Some(anyhow::anyhow!(
                    "Scan saving worker panicked; database changes may already be committed and some projections may be stale; inspect the library before retrying"
                ));
            }
            // The async supervisor joins the transaction before forwarding facts.
            if let Some(sender) = self.sender.take() {
                let _ = sender.send(saved);
            }
        }
    }
}

fn save_scan(
    database_path: &Path,
    current_id: Option<u64>,
    scan: ScanFinished,
    import_revisions: Option<ImportPlaylistRevisions>,
    sender: tokio::sync::oneshot::Sender<ScanSaved>,
) {
    let mut completion = ScanSavingCompletion {
        sender: Some(sender),
        saved: Some(ScanSaved {
            id: scan.id,
            progress: scan.progress,
            library_committed: false,
            playlist_id: None,
            playlist_entry_revisions: None,
            outcome_uncertain: false,
            impacts: None,
            summary: String::new(),
            scan_errors: None,
            error: None,
            library_total: None,
            history: None,
            playlist_total: None,
        }),
    };
    let saved = completion.saved.as_mut().unwrap();
    let result = match scan.result {
        Ok(result) => result,
        Err(error) => {
            saved.error = Some(error);
            return;
        }
    };
    if !result.errors().is_empty() {
        saved.scan_errors = Some(result.errors().join("\n"));
    }
    let store = match Store::open(database_path).context("Opening scan saving database failed") {
        Ok(store) => store,
        Err(error) => {
            saved.error = Some(error);
            return;
        }
    };
    if let Err(error) = store.set_queue_current(current_id) {
        saved.error = Some(error.context("Saving queue position failed; scan has not been saved"));
        return;
    }
    let save_started = Instant::now();
    let impacts = match store.apply_scan(&result).with_context(|| {
        if scan.import.is_some() {
            "Failed to save scan; playlist not created"
        } else {
            "Failed to save scan"
        }
    }) {
        Ok(impacts) => impacts,
        Err(error) => {
            saved.error = Some(error);
            return;
        }
    };
    saved.library_committed = true;
    saved.impacts = Some(impacts);
    saved.summary = result.summary(impacts.skipped_short_tracks);
    tracing::info!(
        id = saved.id,
        elapsed_ms = save_started.elapsed().as_millis(),
        "library_scan_saved"
    );
    if let Some((name, items)) = scan.import {
        let playlist = (|| -> Result<i64> {
            let accessible: HashSet<_> = result
                .records
                .iter()
                .map(|record| match &record.cue {
                    Some(cue) => (cue.sheet.as_path(), Some(cue.number)),
                    None => (record.path.as_path(), None),
                })
                .collect();
            let mut ids = Vec::with_capacity(items.len());
            for item in items {
                let path = library::logical_path(&item.path)?;
                if !accessible.contains(&(path.as_path(), item.cue_track)) {
                    continue;
                }
                if let Some(id) = store.track_id_for_source(&path, item.cue_track)? {
                    ids.push(id);
                }
            }
            ensure!(
                !ids.is_empty(),
                "Playlist contains no supported, accessible audio tracks"
            );
            store.create_playlist_with_tracks(&name, &ids)
        })()
        .context("Scan saved, but playlist creation failed");
        match playlist {
            Ok(id) => {
                // Record the DB fact before preparing projections, so unwinding
                // or any later refresh failure cannot erase the created id.
                saved.playlist_id = Some(id);
                let mut revisions =
                    import_revisions.expect("Import saving reserves its playlist content revision");
                Arc::make_mut(&mut revisions.entries).insert(id, revisions.revision);
                saved.playlist_entry_revisions = Some(revisions.entries);
            }
            Err(error) => saved.error = Some(error),
        }
    }
    // Counts and history can scan substantial DB state. Prepare them here;
    // Core only applies these projections and performs indexed queue reads.
    let refresh_started = Instant::now();
    if impacts.structure {
        saved.library_total = Some(store.track_count());
    }
    if impacts.library {
        saved.history = Some(store.history(MAX_QUERY_ROWS));
    }
    if saved.playlist_id.is_some() {
        saved.playlist_total = Some(
            store
                .playlist_summary_page(None, 0, 0)
                .map(|page| page.total),
        );
    }
    tracing::info!(
        id = saved.id,
        elapsed_ms = refresh_started.elapsed().as_millis(),
        "library_scan_refresh_prepared"
    );
}

impl Core {
    pub(in crate::core) fn scan(&mut self, paths: Vec<PathBuf>, force: bool) -> Result<()> {
        self.start_scan(ScanInput::Paths { paths, force })
    }

    pub(in crate::core) fn import_playlist(&mut self, path: PathBuf, name: String) -> Result<()> {
        self.start_scan(ScanInput::Import { path, name })
    }

    fn start_scan(&mut self, mut input: ScanInput) -> Result<()> {
        if self.active_scan.is_some() {
            bail!("A library scan is already running");
        }
        let config = Arc::clone(&self.state.system.config);
        if config.library_roots.is_empty() {
            bail!(
                "No library roots are configured; add a library root in the configuration before scanning"
            );
        }
        if let ScanInput::Paths { paths, .. } = &mut input {
            ensure!(!paths.is_empty(), "No files or directories supplied");
            for requested in paths {
                *requested = library::logical_path(requested)?;
                ensure!(
                    config
                        .library_roots
                        .iter()
                        .any(|root| requested.starts_with(root)),
                    "Scan path is outside configured library roots: {}",
                    requested.display()
                );
            }
        }
        let ffmpeg_enabled = config.ffmpeg_enabled;
        tracing::info!(
            input = ?input,
            max_depth = config.scan_max_depth,
            min_duration_secs = config.scan_min_duration_secs,
            "library_scan_started"
        );
        let id = self.next_scan_id;
        let next_id = id.checked_add(1).context("Scan task IDs exhausted")?;
        let cancellation = library::ScanCancellation::default();
        let worker_cancellation = cancellation.clone();
        let sender = self.scan_tx.clone();
        let worker = self.spawn_scan_preparation(id, async move {
            let mut latest = ScanProgress {
                phase: ScanPhase::Discovering,
                processed: 0,
                total: None,
                errors: 0,
                path: None,
            };
            let mut last_phase = None;
            let mut last_emit = Instant::now() - Duration::from_millis(100);
            let progress_sender = sender.clone();
            let mut progress = |phase, processed, total, errors, path: Option<&Path>| {
                latest.phase = phase;
                latest.processed = processed;
                latest.total = total;
                latest.errors = errors;
                let transition = last_phase != Some(phase);
                if transition || last_emit.elapsed() >= Duration::from_millis(100) {
                    last_phase = Some(phase);
                    last_emit = Instant::now();
                    if progress_sender.capacity() == progress_sender.max_capacity() {
                        let event = ScanProgress {
                            phase,
                            processed,
                            total,
                            errors,
                            path: path.map(Path::to_path_buf),
                        };
                        let _ = progress_sender.try_send(ScanEvent::Progress {
                            id,
                            progress: event,
                        });
                    }
                }
            };
            let preparation_started = Instant::now();
            let mut import = None;
            let result = async {
                let (paths, sheets, force) = match input {
                    ScanInput::Import { path, name } => {
                        import = Some((name, Vec::new()));
                        let prepared = library::prepare_playlist_import(
                            &path,
                            &worker_cancellation,
                            Some(&config.library_roots),
                        )
                        .await
                        .context(
                            "Preparing playlist import; scan not saved, playlist not created",
                        )?;
                        let paths = prepared
                            .items
                            .iter()
                            .map(|item| item.path.clone())
                            .collect::<Vec<_>>();
                        import.as_mut().unwrap().1 = prepared.items;
                        (paths, Some(prepared.sheets), false)
                    }
                    ScanInput::Paths { paths, force } => (paths, None, force),
                };
                library::scan_paths_with_cues(
                    &paths,
                    |paths| {
                        let sender = sender.clone();
                        let cancellation = worker_cancellation.clone();
                        async move {
                            if force {
                                Ok(vec![None; paths.len()])
                            } else {
                                lookup_scan_sources(sender, id, paths, cancellation).await
                            }
                        }
                    },
                    ffmpeg_enabled,
                    &config.library_roots,
                    config.scan_max_depth,
                    (&worker_cancellation, sheets),
                    &mut progress,
                )
                .await
                .map(|mut result| {
                    result.min_duration_secs = config.scan_min_duration_secs;
                    result
                })
            }
            .await;
            tracing::info!(
                id,
                elapsed_ms = preparation_started.elapsed().as_millis(),
                "library_scan_prepared"
            );
            match &result {
                Ok(result) => tracing::info!(
                    records = result.records.len(),
                    errors = result.errors.len(),
                    "library_scan_finished"
                ),
                Err(error) => tracing::info!(error = %error, "library_scan_finished"),
            }
            ScanFinished {
                id,
                result,
                import,
                progress: latest,
            }
        });
        self.next_scan_id = next_id;
        self.active_scan = Some(ScanTask {
            status: ScanTaskStatus {
                id,
                progress: ScanProgress {
                    phase: ScanPhase::Discovering,
                    processed: 0,
                    total: None,
                    errors: 0,
                    path: None,
                },
                message: "Discovering audio files…".into(),
                library_committed: false,
                playlist_id: None,
                error: None,
            },
            cancellation,
            worker,
            pending_save: None,
        });
        self.project_scan();
        Ok(())
    }

    pub(in crate::core) fn spawn_scan_preparation(
        &self,
        id: u64,
        prepare: impl Future<Output = ScanFinished> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let sender = self.scan_tx.clone();
        self.executor.spawn(async move {
            let finished = std::panic::AssertUnwindSafe(prepare)
                .catch_unwind()
                .await
                .unwrap_or_else(|_| ScanFinished {
                    id,
                    result: Err(anyhow::anyhow!(
                        "Scan preparation worker panicked; scan has not been saved"
                    )),
                    import: None,
                    progress: ScanProgress {
                        phase: ScanPhase::Discovering,
                        processed: 0,
                        total: None,
                        errors: 1,
                        path: None,
                    },
                });
            let _ = deliver_scan_event(&sender, ScanEvent::Finished(finished)).await;
        })
    }

    /// Presentation fields are projections; task status is the only result authority.
    fn project_scan(&mut self) {
        self.state.system.scanning = self.active_scan.is_some();
        if let Some(status) = self
            .active_scan
            .as_ref()
            .map(|task| &task.status)
            .or_else(|| self.scan_results.back())
        {
            self.state.system.scan_progress = Some(status.progress.clone());
            self.state.system.scan_message.clone_from(&status.message);
        }
    }

    pub(in crate::core) fn cancel_scan(&mut self, id: u64) -> Result<()> {
        let task = self
            .active_scan
            .as_ref()
            .filter(|task| task.status.id == id)
            .context("Scan task is not running")?;
        ensure!(
            task.status.progress.phase != ScanPhase::Saving,
            "Scan is saving; its commit can no longer be cancelled"
        );
        task.cancellation.cancel();
        if let Some(scan) = self.active_scan.as_mut().unwrap().pending_save.take() {
            self.begin_scan_save(scan);
            self.project_scan();
        }
        Ok(())
    }

    pub(in crate::core) fn scan_event(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::KnownSources { id, paths, reply } => {
                let result = (|| {
                    let task = self
                        .active_scan
                        .as_ref()
                        .filter(|task| task.status.id == id)
                        .context("Scan task is not running")?;
                    task.cancellation.check()?;
                    ensure!(
                        paths.len() <= library::SOURCE_BATCH_SIZE,
                        "Scan source lookup batch exceeds limit"
                    );
                    paths
                        .iter()
                        .map(|path| {
                            task.cancellation.check()?;
                            self.store.known_source(path)
                        })
                        .collect()
                })();
                // The worker may have cancelled while this indexed read ran.
                let _ = reply.send(result);
                return;
            }
            ScanEvent::Progress { id, progress } => {
                let Some(task) = self
                    .active_scan
                    .as_mut()
                    .filter(|task| task.status.id == id)
                else {
                    return;
                };
                task.status.message = scan_message(&progress);
                task.status.progress = progress;
            }
            ScanEvent::Finished(scan) => self.begin_scan_save(scan),
            ScanEvent::Saved(saved) => self.complete_saved_scan(saved),
        }
        self.project_scan();
        self.publish();
    }

    fn retain_scan_result(&mut self, status: ScanTaskStatus) {
        if self.scan_results.len() == MAX_RETAINED_SCAN_RESULTS {
            self.scan_results.pop_front();
        }
        self.scan_results.push_back(status);
    }

    pub(in crate::core) fn shutdown_scan(&mut self) {
        if self
            .active_scan
            .as_ref()
            .is_some_and(|task| task.status.progress.phase == ScanPhase::Saving)
        {
            // Saving is an accepted commit, not cancellable traversal. Drain its
            // sole completion before joining, even if delivery is backpressured.
            while self.active_scan.is_some() {
                match self.scan_rx.blocking_recv() {
                    Some(event) => self.scan_event(event),
                    None => {
                        let task = self.active_scan.take().unwrap();
                        self.complete_unsaved_scan(
                            task,
                            Err(anyhow::anyhow!(
                                "Scan saving completion disconnected; database changes may already be committed; inspect the library before retrying"
                            )),
                            false,
                        );
                    }
                }
            }
        } else {
            if let Some(task) = &self.active_scan {
                task.cancellation.cancel();
            }
            // Disconnect before joining: final traversal delivery may be full.
            self.scan_rx.close();
            if let Some(task) = self.active_scan.take() {
                self.complete_unsaved_scan(task, Ok(()), true);
            }
        }
        self.scan_rx.close();
        self.project_scan();
    }

    pub(in crate::core) fn resume_pending_scan(&mut self) {
        if self.active_relocation.is_some() {
            return;
        }
        if let Some(scan) = self
            .active_scan
            .as_mut()
            .and_then(|task| task.pending_save.take())
        {
            self.begin_scan_save(scan);
            self.project_scan();
            self.publish();
        }
    }

    fn begin_scan_save(&mut self, scan: ScanFinished) {
        if !self.active_scan.as_ref().is_some_and(|task| {
            task.status.id == scan.id && task.status.progress.phase != ScanPhase::Saving
        }) {
            return;
        }
        let mut task = self.active_scan.take().unwrap();
        task.status.progress = scan.progress.clone();
        if task.cancellation.is_cancelled() {
            self.complete_unsaved_scan(task, Ok(()), true);
            return;
        }
        if scan.result.is_err() {
            self.complete_unsaved_scan(task, scan.result.map(|_| ()), false);
            return;
        }
        if self.active_relocation.is_some() {
            task.status.message = "Waiting for track relocation before saving library…".into();
            task.pending_save = Some(scan);
            self.active_scan = Some(task);
            return;
        }
        if self.retired_playback.is_some()
            && let Err(error) = self.flush_deferred_playback()
        {
            self.complete_unsaved_scan(
                task,
                Err(error.context(
                    "Saving deferred playback statistics before committing scan failed; scan has not been saved",
                )),
                false,
            );
            return;
        }
        let database_path = match self.store.database_path() {
            Ok(path) => path,
            Err(error) => {
                self.complete_unsaved_scan(task, Err(error), false);
                return;
            }
        };
        // Capture only after relocation has retired: Saving rejects DB edits,
        // so this O(1) snapshot cannot overwrite concurrent playlist changes.
        // The worker pays for the shared BTreeMap copy only if import succeeds.
        let import_revisions = scan.import.as_ref().map(|_| ImportPlaylistRevisions {
            revision: self.next_playlist_entry_revision(),
            entries: Arc::clone(&self.state.library.playlist_entry_revisions),
        });
        let sender = self.scan_tx.clone();
        let current_id = self.state.queue.current_id;
        // Finished no longer awaits Core. Join preparation before scheduling the
        // accepted, non-cancellable transaction, then supervise its retirement.
        let _ = futures::executor::block_on(task.worker);
        task.worker = self.executor.spawn(async move {
            let id = scan.id;
            let progress = scan.progress.clone();
            let (completed, completion) = tokio::sync::oneshot::channel();
            let joined = tokio::task::spawn_blocking(move || {
                save_scan(&database_path, current_id, scan, import_revisions, completed)
            }).await;
            let saved = completion.await.unwrap_or_else(|_| ScanSaved {
                id,
                progress,
                library_committed: false,
                playlist_id: None,
                playlist_entry_revisions: None,
                outcome_uncertain: true,
                impacts: None,
                summary: String::new(),
                scan_errors: None,
                error: Some(anyhow::anyhow!(
                    "Scan saving completion failed ({joined:?}); database changes may already be committed; inspect the library before retrying"
                )),
                library_total: None,
                history: None,
                playlist_total: None,
            });
            let _ = deliver_scan_event(&sender, ScanEvent::Saved(saved)).await;
        });
        task.status.progress.phase = ScanPhase::Saving;
        task.status.message = "Saving library…".into();
        self.active_scan = Some(task);
    }

    fn complete_unsaved_scan(&mut self, mut task: ScanTask, result: Result<()>, cancelled: bool) {
        let _ = futures::executor::block_on(task.worker);
        if cancelled {
            task.status.progress.phase = ScanPhase::Cancelled;
            task.status.message = "Scan cancelled".into();
            task.status.error = Some(task.status.message.clone());
        } else if let Err(error) = result {
            task.status.progress.phase = ScanPhase::Failed;
            task.status.progress.errors = task.status.progress.errors.saturating_add(1);
            task.status.message = error.to_string();
            task.status.error = Some(format!("{error:#}"));
        }
        self.retain_scan_outcome(task.status);
    }

    fn complete_saved_scan(&mut self, saved: ScanSaved) {
        if !self.active_scan.as_ref().is_some_and(|task| {
            task.status.id == saved.id && task.status.progress.phase == ScanPhase::Saving
        }) {
            return;
        }
        // Retire maintenance ownership before any projection refresh may stop
        // playback. The worker has released its writable connection already.
        let mut task = self.active_scan.take().unwrap();
        let _ = futures::executor::block_on(task.worker);
        task.status.progress = saved.progress;
        task.status.library_committed = saved.library_committed;
        task.status.playlist_id = saved.playlist_id;
        task.status.message = saved.summary;
        task.status.error = saved.scan_errors;
        let refresh_started = Instant::now();
        let mut errors = Vec::new();
        if let Some(error) = saved.error {
            errors.push(error);
        }
        if saved.library_committed || saved.outcome_uncertain {
            self.store.invalidate_caches();
        }
        let impacts = if saved.outcome_uncertain {
            // An unwind can happen after SQLite committed but before the next
            // fact assignment. Invalidate possibly affected domains, without
            // inventing a successful commit or doing unbounded recovery SQL.
            if saved.playlist_id.is_none() {
                self.state.library.playlist_revision =
                    self.state.library.playlist_revision.wrapping_add(1);
            }
            Some(crate::store::ScanImpacts {
                changed: true,
                library: true,
                structure: true,
                duplicate_groups: true,
                duplicate_members: true,
                ..crate::store::ScanImpacts::default()
            })
        } else {
            saved.impacts
        };
        if let Some(impacts) = impacts {
            self.invalidate_scan_impacts(impacts);
            if let Some(total) = saved.library_total {
                match total {
                    Ok(total) => self.state.library.track_total = total,
                    Err(error) => errors.push(
                        error.context("Refreshing library count failed; previous count retained"),
                    ),
                }
            }
            if let Some(history) = saved.history {
                match history {
                    Ok(history) => self.state.library.history = Arc::new(history),
                    Err(error) => errors.push(
                        error.context("Refreshing history failed; previous history retained"),
                    ),
                }
            }
            if impacts.library
                && let Err(error) = self.refresh_queue_tracks()
            {
                errors.push(error.context(if saved.library_committed {
                    "Scan committed, but refreshing views failed; some projections may be stale"
                } else {
                    "Scan saving failed and refreshing views failed; database changes may already be committed"
                }));
            }
        }
        if saved.playlist_id.is_some() {
            if let Some(revisions) = saved.playlist_entry_revisions {
                self.state.library.playlist_entry_revisions = revisions;
            }
            self.invalidate_playlist_creation(true);
            if let Some(total) = saved.playlist_total {
                match total {
                    Ok(total) => self.state.library.playlist_total = total,
                    Err(error) => {
                        errors.push(error.context(
                            "Scan saved and playlist created, but playlist refresh failed",
                        ))
                    }
                }
            }
        }
        if !errors.is_empty() {
            task.status.progress.errors = task.status.progress.errors.saturating_add(errors.len());
            task.status.message = errors[0].to_string();
            let mut detail = task.status.error.take().unwrap_or_default();
            for error in errors {
                if !detail.is_empty() {
                    detail.push('\n');
                }
                detail.push_str(&format!("{error:#}"));
            }
            task.status.error = Some(detail);
        }
        task.status.progress.phase = if task.status.error.is_some() {
            ScanPhase::Failed
        } else {
            ScanPhase::Completed
        };
        tracing::info!(
            id = saved.id,
            elapsed_ms = refresh_started.elapsed().as_millis(),
            "library_scan_refreshed"
        );
        self.retain_scan_outcome(task.status);
    }

    fn retain_scan_outcome(&mut self, status: ScanTaskStatus) {
        if let Some(error) = &status.error {
            self.state.system.last_error = Some(error.clone());
        }
        self.retain_scan_result(status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_unwind_reports_uncertainty_and_preserves_confirmed_commit_facts() {
        for committed in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let database_path = directory.path().join("library.db");
            drop(Store::open(&database_path).unwrap());
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let worker = thread::spawn(move || {
                let mut completion = ScanSavingCompletion {
                    sender: Some(sender),
                    saved: Some(ScanSaved {
                        id: 1,
                        progress: ScanProgress {
                            phase: ScanPhase::Saving,
                            processed: 0,
                            total: Some(0),
                            errors: 0,
                            path: None,
                        },
                        library_committed: false,
                        playlist_id: None,
                        playlist_entry_revisions: None,
                        outcome_uncertain: false,
                        impacts: None,
                        summary: String::new(),
                        scan_errors: None,
                        error: None,
                        library_total: None,
                        history: None,
                        playlist_total: None,
                    }),
                };
                if committed {
                    let store = Store::open(&database_path).unwrap();
                    let saved = completion.saved.as_mut().unwrap();
                    saved.impacts = Some(store.apply_scan(&ScanResult::default()).unwrap());
                    saved.library_committed = true;
                    saved.playlist_id = Some(store.create_playlist("Saved before panic").unwrap());
                    saved.playlist_entry_revisions =
                        Some(Arc::new(BTreeMap::from([(saved.playlist_id.unwrap(), 1)])));
                }
                panic!("saving worker failure");
            });
            let saved = futures::executor::block_on(receiver).unwrap();
            assert!(worker.join().is_err());
            assert!(saved.outcome_uncertain);
            assert_eq!(saved.library_committed, committed);
            assert_eq!(saved.playlist_id.is_some(), committed);
            assert_eq!(
                saved
                    .playlist_entry_revisions
                    .as_ref()
                    .map(|entries| entries.len()),
                committed.then_some(1)
            );
            if let Some(id) = saved.playlist_id {
                assert_eq!(
                    saved.playlist_entry_revisions.as_ref().unwrap().get(&id),
                    Some(&1)
                );
            }
            assert!(
                saved
                    .error
                    .unwrap()
                    .to_string()
                    .contains("may already be committed")
            );
            let store = Store::open(&directory.path().join("library.db")).unwrap();
            assert_eq!(
                store.playlist_summary_page(None, 0, 0).unwrap().total,
                usize::from(committed)
            );
        }
    }
}
