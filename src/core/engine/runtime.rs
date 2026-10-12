use super::*;

impl Core {
    pub(in crate::core) fn run(&mut self, requests: Receiver<Request>) {
        let config_ticks = crossbeam_channel::tick(Duration::from_millis(100));
        loop {
            select! {
                recv(requests) -> request => {
                    let Ok(request) = request else { break; };
                    match request.operation {
                        Operation::Query(query) => {
                            let result = self.view(&query);
                            if let Some(reply) = request.reply {
                                let _ = reply.send(CoreResponse::Query(QueryResponse {
                                    revisions: self.state.query_revisions(),
                                    result,
                                }));
                            }
                        }
                        Operation::Command(command) => {
                            let shutdown = matches!(&command, Command::Shutdown);
                            let starts_scan = matches!(&command, Command::Scan { .. } | Command::ImportPlaylist { .. });
                            let changed = !matches!(&command, Command::ShowWindow);
                            let result = self.command(command);
                            if let Err(error) = &result {
                                self.state.system.last_error = Some(format!("{error:#}"));
                            }
                            if changed || result.is_err() { self.publish(); }
                            let scan_id = if result.is_ok() && starts_scan { self.active_scan.as_ref().map(|task| task.status.id) } else { None };
                            if let Some(reply) = request.reply {
                                let _ = reply.send(CoreResponse::Ack(Ack {
                                    ok: result.is_ok(),
                                    error: result.err().map(|error| format!("{error:#}")),
                                    revision: self.state.system.revision,
                                    scan_id,
                                }));
                            }
                            if shutdown { break; }
                        }
                        Operation::GuiConfig(change) => {
                            let result = self.update_gui_config(change);
                            if let Err(error) = &result {
                                self.state.system.last_error = Some(format!("{error:#}"));
                            }
                            self.publish();
                            if let Some(reply) = request.reply {
                                let _ = reply.send(CoreResponse::Ack(Ack {
                                    ok: result.is_ok(),
                                    error: result.err().map(|error| format!("{error:#}")),
                                    revision: self.state.system.revision,
                                    scan_id: None,
                                }));
                            }
                        }
                    }
                }
                recv(self.engine.events) -> event => {
                    let Ok(event) = event else { self.state.system.last_error = Some("Audio worker stopped unexpectedly".into()); self.publish(); break; };
                    if let Err(error) = self.audio_event(event) { self.state.system.last_error = Some(format!("{error:#}")); }
                    self.publish();
                }
                recv(self.scan_rx) -> scan => {
                    if let Ok(scan) = scan {
                        self.scan_event(scan);
                    }
                }
                recv(config_ticks) -> _ => {
                    // Reuse the save tick to publish the one-time load result,
                    // including loads from GUI waveform work outside the engine.
                    if self.state.system.ffmpeg_status == "not loaded"
                        && audio::ffmpeg_load_attempted()
                    {
                        self.state.system.ffmpeg_status = audio::ffmpeg_status()
                            .unwrap_or_else(|error| format!("unavailable: {error:#}"));
                        self.publish();
                    }
                    if self.config_dirty
                        && self.config_last_saved.elapsed() >= Duration::from_millis(250)
                        && self.save(false).is_ok()
                    {
                        self.publish();
                    }
            }
                }
        }

        if let Err(error) = self.stop() {
            self.state.system.last_error = Some(format!("Saving play count: {error:#}"));
        }
        let _ = self.save(true);
        let _ = self.engine.send(AudioCommand::Shutdown);
        self.shutdown_scan();
        self.state.playback.status = PlaybackStatus::Stopped;
        self.state.system.shutting_down = true;
        self.publish();
        if let Some(callback) = self.gui_opener.read().clone() {
            callback();
        }
    }

    pub(in crate::core) fn publish(&mut self) {
        self.state.system.revision = self.state.system.revision.wrapping_add(1);
        {
            let mut shared = self.shared.write();
            let changed = self.state.changed_sections(&shared);
            let revisions = &mut self.state.revisions;
            if changed.playback {
                revisions.playback = revisions.playback.wrapping_add(1);
            }
            if changed.queue {
                revisions.queue = revisions.queue.wrapping_add(1);
            }
            if changed.library {
                revisions.library = revisions.library.wrapping_add(1);
            }
            if changed.system {
                revisions.system = revisions.system.wrapping_add(1);
            }
            if changed.playback {
                shared.playback = self.state.playback.clone();
                shared.current_track = self.state.current_track.clone();
            }
            if changed.queue {
                shared.queue = self.state.queue.clone();
            }
            if changed.library {
                shared.library = self.state.library.clone();
            }
            if changed.system {
                shared.system = self.state.system.clone();
            } else {
                // The in-process wakeup sequence advances even without a System change.
                shared.system.revision = self.state.system.revision;
            }
            shared.revisions = *revisions;
        }
        self.subscribers.write().retain(|sender| {
            !matches!(
                sender.try_send(()),
                Err(crossbeam_channel::TrySendError::Disconnected(_))
            )
        });
        let callback = self.wakeup.read().clone();
        if let Some(callback) = callback {
            callback();
        }
    }
    pub(in crate::core) fn request_raise(&self) -> Result<()> {
        let opener = self.gui_opener.read();
        let callback = opener
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Rivu has no graphical host"))?;
        self.raise_requested.store(true, Ordering::Release);
        callback();
        Ok(())
    }
    pub(in crate::core) fn audio(&self, command: AudioCommand) -> Result<()> {
        self.engine
            .commands
            .send(command)
            .context("Audio worker unavailable")
    }
    pub(in crate::core) fn track(&self, id: i64) -> Result<Track> {
        self.store.track(id)?.context("Track not found")
    }
    pub(in crate::core) fn refresh_queue_rows(&mut self) -> Result<()> {
        let mut ids = self
            .state
            .queue
            .entries
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        self.state.queue.tracks = Arc::new(self.store.queue_rows(&ids)?);
        let current_track_id = self.state.queue.current_id.and_then(|id| {
            self.state
                .queue
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .map(|entry| entry.track_id)
        });
        if self
            .state
            .current_track
            .as_ref()
            .is_some_and(|track| Some(track.id) != current_track_id)
        {
            self.state.current_track = None;
        }
        Ok(())
    }
    pub(in crate::core) fn refresh_queue_tracks(&mut self) -> Result<()> {
        self.refresh_queue_rows()?;
        let current_track_id = self.state.queue.current_id.and_then(|id| {
            self.state
                .queue
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .map(|entry| entry.track_id)
        });
        self.state.current_track = current_track_id
            .map(|id| self.store.track(id))
            .transpose()?
            .flatten()
            .map(Arc::new);
        Ok(())
    }
    /// Metadata edits keep unrelated queue rows and playback projections intact.
    /// Queue rows remain ID-sorted and distinct even when entries repeat a track.
    pub(in crate::core) fn refresh_affected_queue_tracks(
        &mut self,
        track_ids: &[i64],
    ) -> Result<()> {
        let mut queued_ids = track_ids
            .iter()
            .copied()
            .filter(|id| {
                self.state
                    .queue
                    .tracks
                    .binary_search_by_key(id, |row| row.id)
                    .is_ok()
            })
            .collect::<Vec<_>>();
        queued_ids.sort_unstable();
        queued_ids.dedup();
        if !queued_ids.is_empty() {
            for row in self.store.queue_rows(&queued_ids)? {
                let index = self
                    .state
                    .queue
                    .tracks
                    .binary_search_by_key(&row.id, |row| row.id)
                    .expect("affected row is already queued");
                if self.state.queue.tracks[index] != row {
                    Arc::make_mut(&mut self.state.queue.tracks)[index] = row;
                }
            }
        }
        if let Some(current) = self.state.current_track.as_ref()
            && track_ids.contains(&current.id)
        {
            let track = self.store.track(current.id)?;
            if track.as_ref() != Some(current.as_ref()) {
                self.state.current_track = track.map(Arc::new);
            }
        }
        Ok(())
    }
    pub(in crate::core) fn view(&self, query: &Query) -> Result<ViewResponse, QueryError> {
        let current = self.state.query_revisions();
        let (expected, playlist) = match query {
            Query::TrackBatch { expected, .. } => (*expected, false),
            Query::PlaylistSummaryBatch { expected, .. }
            | Query::PlaylistEntryBatch { expected, .. } => (*expected, true),
            _ => (None, false),
        };
        if let Some(expected) = expected
            && (expected.library != current.library
                || (playlist && expected.playlist != current.playlist))
        {
            return Err(QueryError::RevisionChanged);
        }
        Ok(match query {
            Query::ScanTask { id } => ViewResponse::ScanTask(
                self.active_scan
                    .as_ref()
                    .map(|task| &task.status)
                    .filter(|status| status.id == *id)
                    .or_else(|| self.scan_results.iter().find(|status| status.id == *id))
                    .cloned()
                    .context("Unknown or expired scan task")?,
            ),
            Query::LibraryPage {
                query,
                favorite,
                missing,
                sort,
                offset,
                limit,
            } => ViewResponse::LibraryPage(self.store.library_view_page(
                query.as_deref(),
                *favorite,
                *missing,
                *sort,
                *offset,
                *limit,
            )?),
            Query::LibraryRootPage {
                path,
                query,
                favorite,
                missing,
                sort,
                offset,
                limit,
            } => ViewResponse::LibraryPage(self.store.library_root_page(
                path,
                query.as_deref(),
                *favorite,
                *missing,
                *sort,
                *offset,
                *limit,
            )?),
            Query::LibraryRootCounts { roots } => {
                ViewResponse::LibraryRootCounts(self.store.library_root_counts(roots)?)
            }
            Query::TrackPage {
                query,
                favorite,
                missing,
                offset,
                limit,
            } => ViewResponse::TrackPage(self.store.library_page(
                query.as_deref(),
                *favorite,
                *missing,
                *offset,
                *limit,
            )?),
            Query::DirectoryPage {
                path,
                query,
                favorite,
                offset,
                limit,
            } => ViewResponse::DirectoryPage(self.store.directory_page(
                path,
                query.as_deref(),
                *favorite,
                *offset,
                *limit,
            )?),
            Query::PlaylistSummaries { offset, limit } => {
                ViewResponse::PlaylistSummaries(self.store.playlist_summary_page(*offset, *limit)?)
            }
            Query::PlaylistEntries {
                playlist_id,
                offset,
                limit,
            } => ViewResponse::PlaylistEntries(self.store.playlist_entries_page(
                *playlist_id,
                *offset,
                *limit,
            )?),
            Query::Track { track_id } => ViewResponse::Track(self.store.track(*track_id)?),
            Query::LibraryStats => ViewResponse::LibraryStats(self.store.library_stats()?),
            Query::TrackBatch {
                query,
                favorite,
                missing,
                after,
                limit,
                ..
            } => ViewResponse::TrackBatch(self.store.track_batch(
                query.as_deref(),
                *favorite,
                *missing,
                *after,
                *limit,
            )?),
            Query::PlaylistSummaryBatch { after, limit, .. } => ViewResponse::PlaylistSummaryBatch(
                self.store.playlist_summary_batch(*after, *limit)?,
            ),
            Query::PlaylistEntryBatch {
                playlist_id,
                after,
                limit,
                ..
            } => ViewResponse::PlaylistEntryBatch(self.store.playlist_entry_batch(
                *playlist_id,
                *after,
                *limit,
            )?),
        })
    }
}
