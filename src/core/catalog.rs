use super::*;
use anyhow::{bail, ensure};

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
fn lookup_scan_source(
    sender: &Sender<ScanEvent>,
    id: u64,
    path: &Path,
    cancellation: &library::ScanCancellation,
) -> Result<Option<library::KnownFile>> {
    cancellation.check()?;
    let (reply, receive) = bounded(1);
    let event = ScanEvent::KnownSource {
        id,
        path: path.to_path_buf(),
        reply,
    };
    crossbeam_channel::select_biased! {
        recv(cancellation.cancelled()) -> _ => bail!("scan cancelled"),
        send(sender, event) -> sent => sent.context("scan owner disconnected")?,
    }
    let result = crossbeam_channel::select_biased! {
        recv(cancellation.cancelled()) -> _ => bail!("scan cancelled"),
        recv(receive) -> result => result.context("scan source lookup disconnected")?,
    };
    cancellation.check()?;
    result
}

#[derive(Debug)]
enum ScanInput {
    Paths { paths: Vec<PathBuf>, force: bool },
    Import { path: PathBuf, name: String },
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
        // Flush the queue before accepting a task, never fail its acknowledgement afterward.
        self.save_queue()?;
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
        let worker = thread::Builder::new()
            .name("rivu-scan".into())
            .spawn(move || {
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
                        if !progress_sender.is_full() {
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
                let mut import = None;
                let result = (|| {
                    let (paths, sheets, force) = match input {
                        ScanInput::Import { path, name } => {
                            import = Some((name, Vec::new()));
                            let prepared = library::prepare_playlist_import(
                                &path,
                                &worker_cancellation,
                                Some(&config.library_roots),
                            )
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
                        |path| {
                            if force {
                                Ok(None)
                            } else {
                                lookup_scan_source(&sender, id, path, &worker_cancellation)
                            }
                        },
                        ffmpeg_enabled,
                        &config.library_roots,
                        config.scan_max_depth,
                        (&worker_cancellation, sheets),
                        &mut progress,
                    )
                    .map(|mut result| {
                        result.min_duration_secs = config.scan_min_duration_secs;
                        result
                    })
                })();
                match &result {
                    Ok(result) => tracing::info!(
                        records = result.records.len(),
                        errors = result.errors.len(),
                        "library_scan_finished"
                    ),
                    Err(error) => tracing::info!(error = %error, "library_scan_finished"),
                }
                let _ = sender.send(ScanEvent::Finished(ScanFinished {
                    id,
                    result,
                    import,
                    progress: latest,
                }));
            })?;
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
                error: None,
            },
            cancellation,
            worker,
        });
        self.project_scan();
        Ok(())
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
        task.cancellation.cancel();
        Ok(())
    }

    pub(in crate::core) fn scan_event(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::KnownSource { id, path, reply } => {
                let result = match self.active_scan.as_ref() {
                    Some(task) if task.status.id == id => task
                        .cancellation
                        .check()
                        .and_then(|()| self.store.known_source(&path)),
                    _ => Err(anyhow::anyhow!("Scan task is not running")),
                };
                // The worker may have cancelled while this indexed read ran.
                let _ = reply.try_send(result);
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
            ScanEvent::Finished(scan) => {
                if !self
                    .active_scan
                    .as_ref()
                    .is_some_and(|task| task.status.id == scan.id)
                {
                    return;
                }
                let cancelled = self
                    .active_scan
                    .as_ref()
                    .unwrap()
                    .cancellation
                    .is_cancelled();
                if !cancelled {
                    let task = self.active_scan.as_mut().unwrap();
                    task.status.progress = ScanProgress {
                        phase: ScanPhase::Saving,
                        ..scan.progress.clone()
                    };
                    task.status.message = "Saving library…".into();
                    self.project_scan();
                    self.publish();
                }
                let scan_errors = scan
                    .result
                    .as_ref()
                    .ok()
                    .filter(|result| !result.errors().is_empty())
                    .map(|result| result.errors().join("\n"));
                let progress = scan.progress.clone();
                let outcome = if cancelled {
                    Ok(false)
                } else {
                    self.finish_scan(scan)
                };
                let mut task = self.active_scan.take().unwrap();
                let _ = task.worker.join();
                task.status.progress = progress;
                if cancelled {
                    task.status.progress.phase = ScanPhase::Cancelled;
                    task.status.message = "Scan cancelled".into();
                    task.status.error = Some("Scan cancelled".into());
                } else {
                    match outcome {
                        Ok(successful) => {
                            task.status.progress.phase = if successful {
                                ScanPhase::Completed
                            } else {
                                ScanPhase::Failed
                            };
                            task.status.error = scan_errors;
                        }
                        Err(error) => {
                            task.status.progress.phase = ScanPhase::Failed;
                            task.status.progress.errors =
                                task.status.progress.errors.saturating_add(1);
                            task.status.message = error.to_string();
                            task.status.error = Some(format!("{error:#}"));
                        }
                    }
                }
                if let Some(error) = &task.status.error {
                    self.state.system.last_error = Some(error.clone());
                }
                self.retain_scan_result(task.status);
            }
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
        if let Some(task) = &self.active_scan {
            task.cancellation.cancel();
        }
        // Disconnect before joining: final delivery may be backpressured.
        self.scan_rx = crossbeam_channel::never();
        if let Some(mut task) = self.active_scan.take() {
            // A blocking probe finishes naturally; traversal/hash observe cancellation.
            let _ = task.worker.join();
            task.status.progress.phase = ScanPhase::Cancelled;
            task.status.message = "Scan cancelled by shutdown".into();
            task.status.error = Some(task.status.message.clone());
            self.retain_scan_result(task.status);
        }
        self.project_scan();
    }

    pub(in crate::core) fn finish_scan(&mut self, scan: ScanFinished) -> Result<bool> {
        let result = scan.result?;
        let successful = result.errors().is_empty();
        let saved = self.store.apply_scan(&result);
        let impacts = if scan.import.is_some() {
            saved.context("Failed to save scan; playlist not created")
        } else {
            saved.context("Failed to save scan")
        }?;
        let summary = result.summary(impacts.skipped_short_tracks);
        if impacts.changed {
            let refreshed = self.apply_scan_impacts(impacts);
            if scan.import.is_some() {
                refreshed
                    .context("Scan saved, but library refresh failed before playlist creation; some projections may be stale")?;
            } else {
                refreshed.context(
                    "Scan committed, but refreshing views failed; some projections may be stale",
                )?;
            }
        }
        if let Some((name, items)) = scan.import {
            let id = (|| -> Result<i64> {
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
                    if let Some(id) = self.store.track_id_for_source(&path, item.cue_track)? {
                        ids.push(id);
                    }
                }
                ids.dedup();
                if ids.is_empty() {
                    bail!("Playlist contains no supported, accessible audio tracks");
                }
                self.store.create_playlist_with_tracks(&name, &ids)
            })()
            .context("Scan saved, but playlist creation failed")?;
            self.playlist_created(id, true)
                .context("Scan saved and playlist created, but playlist refresh failed")?;
        }
        if let Some(task) = &mut self.active_scan {
            task.status.message = summary;
        }
        Ok(successful)
    }
    pub(in crate::core) fn audio_event(&mut self, event: AudioEvent) -> Result<()> {
        match event {
            AudioEvent::Started { generation, info } if generation == self.generation => {
                if let Err(error) = self.playback_started(info) {
                    self.stop().with_context(|| format!("{error:#}"))?;
                    return Err(error);
                }
            }
            AudioEvent::Progress {
                generation,
                position_seconds,
                listened_seconds,
            } if generation == self.generation => {
                self.playback_progress(position_seconds, listened_seconds)?;
            }
            AudioEvent::Ended {
                generation,
                bitrate_bps,
            } if generation == self.generation => {
                self.playback_ended_bitrate(bitrate_bps)?;
                let status = self.state.playback.status;
                self.stop_playback(true)?;
                self.advance_with_status(true, status)?;
            }
            AudioEvent::Failed {
                generation,
                message,
            } if generation == self.generation => {
                self.stop().with_context(|| message.clone())?;
                self.state.system.last_error = Some(message);
            }
            AudioEvent::OutputSettingsFailed {
                request,
                device,
                auto_mix,
                message,
            } if request == self.output_settings_request
                && device == self.state.system.selected_device
                && auto_mix == self.state.system.config.pipewire_auto_mix =>
            {
                self.state.system.last_error = Some(message);
            }
            AudioEvent::DecoderStopped { generation } if generation == self.generation => {
                // The worker has already stopped and may still be sending
                // events while handling Configure; do not wait on a command.
                self.finish_playback(Ok(None), false, Ok(()))?;
            }
            _ => {}
        }
        Ok(())
    }
}
