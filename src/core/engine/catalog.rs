use super::*;
use anyhow::{bail, ensure};

impl Core {
    pub(in crate::core) fn scan(
        &mut self,
        paths: Vec<PathBuf>,
        import: Option<(String, Vec<M3uItem>)>,
        force: bool,
    ) -> Result<()> {
        if self.state.system.scanning {
            bail!("A library scan is already running");
        }
        if paths.is_empty() {
            bail!("No files or directories supplied");
        }
        let config = Arc::clone(&self.state.system.config);
        if config.library_roots.is_empty() {
            bail!(
                "No library roots are configured; add a library root in the configuration before scanning"
            );
        }
        let paths = paths
            .into_iter()
            .map(|path| library::logical_path(&path))
            .collect::<Result<Vec<_>>>()?;
        for requested in &paths {
            ensure!(
                config
                    .library_roots
                    .iter()
                    .any(|root| requested.starts_with(root)),
                "Scan path is outside configured library roots: {}",
                requested.display()
            );
        }
        let known = if force {
            Vec::new()
        } else {
            self.store.known_files()?
        };
        let ffmpeg_enabled = config.ffmpeg_enabled;
        tracing::info!(
            paths = ?paths,
            force,
            max_depth = config.scan_max_depth,
            "library_scan_started"
        );
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
                            let _ = progress_sender.try_send(ScanEvent::Progress(event));
                        }
                    }
                };
                let result = library::scan_paths(
                    &paths,
                    &known,
                    ffmpeg_enabled,
                    &config.library_roots,
                    config.scan_max_depth,
                    &mut progress,
                );
                match &result {
                    Ok(result) => tracing::info!(
                        records = result.records.len(),
                        errors = result.errors.len(),
                        "library_scan_finished"
                    ),
                    Err(error) => tracing::info!(error = %error, "library_scan_finished"),
                }
                let _ = sender.send(ScanEvent::Finished(ScanFinished {
                    result,
                    import,
                    progress: latest,
                }));
            })?;
        self.scan_workers.retain(|worker| !worker.is_finished());
        self.scan_workers.push(worker);
        self.state.system.scanning = true;
        self.state.system.last_error = None;
        self.state.system.scan_progress = Some(ScanProgress {
            phase: ScanPhase::Discovering,
            processed: 0,
            total: None,
            errors: 0,
            path: None,
        });
        self.state.system.scan_message = "Discovering audio files…".into();
        Ok(())
    }
    pub(in crate::core) fn finish_scan(&mut self, scan: ScanFinished) -> Result<bool> {
        let result = scan.result?;
        let successful = result.errors().is_empty();
        self.store.apply_scan(&result)?;
        self.state.system.scan_message = result.summary();
        if !result.errors().is_empty() {
            self.state.system.last_error = Some(result.errors().join("\n"));
        }
        self.reload(true)?;
        if let Some((name, items)) = scan.import {
            let mut ids = Vec::with_capacity(items.len());
            for item in items {
                let path = library::logical_path(&item.path)?;
                if let Some(id) = self.store.track_id_for_source(&path, item.cue_track)? {
                    ids.push(id);
                }
            }
            ids.dedup();
            if ids.is_empty() {
                bail!("Playlist contains no supported, accessible audio tracks");
            }
            let playlist_id = self.store.create_playlist(&name)?;
            self.store.add_playlist(playlist_id, &ids)?;
            self.reload(true)?;
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
            AudioEvent::Ended { generation } if generation == self.generation => {
                let snapshot = self.engine.stop_and_snapshot().map(Some);
                self.finish_playback(snapshot, true)?;
                self.advance(true)?;
            }
            AudioEvent::Failed {
                generation,
                message,
            } if generation == self.generation => {
                self.stop().with_context(|| message.clone())?;
                self.state.system.last_error = Some(message);
            }
            AudioEvent::DecoderStopped { generation } if generation == self.generation => {
                // The worker has already stopped and may still be sending
                // events while handling Configure; do not wait on a command.
                self.finish_playback(Ok(None), false)?;
            }
            _ => {}
        }
        Ok(())
    }
}
