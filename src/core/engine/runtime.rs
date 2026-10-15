use super::*;

impl Core {
    pub(in crate::core) fn run(
        &mut self,
        requests: Receiver<Request>,
        shutdown: Receiver<()>,
        control: &RuntimeControl,
    ) {
        let config_ticks = crossbeam_channel::tick(Duration::from_millis(100));
        loop {
            select! {
                recv(shutdown) -> _ => {
                    self.state.system.shutting_down = true;
                    let result = self.stop();
                    if let Err(error) = &result {
                        self.state.system.last_error = Some(format!("{error:#}"));
                    }
                    self.publish();
                    control.acknowledge(Ack {
                        ok: result.is_ok(),
                        error: result.err().map(|error| format!("{error:#}")),
                        revision: self.state.system.revision,
                        scan_id: None,
                        relocation_id: None,
                    });
                    break;
                }
                recv(requests) -> request => {
                    let Ok(request) = request else { break; };
                    match request.operation {
                        Operation::Query(query) => {
                            let response = self.query_response(&query);
                            if let Some(reply) = request.reply {
                                let _ = reply.send(CoreResponse::Query(response));
                            }
                        }
                        Operation::Command(command) => {
                            let starts_scan = matches!(&command, Command::Scan { .. } | Command::ImportPlaylist { .. });
                            let starts_relocation = matches!(&command, Command::RelocateTrack { .. });
                            let result = self.command(command);
                            if let Err(error) = &result {
                                self.state.system.last_error = Some(format!("{error:#}"));
                            }
                            self.publish();
                            let scan_id = if result.is_ok() && starts_scan { self.active_scan.as_ref().map(|task| task.status.id) } else { None };
                            let relocation_id = if result.is_ok() && starts_relocation { self.active_relocation.as_ref().map(|task| task.status.id) } else { None };
                            if let Some(reply) = request.reply {
                                let _ = reply.send(CoreResponse::Ack(Ack {
                                    ok: result.is_ok(),
                                    error: result.err().map(|error| format!("{error:#}")),
                                    revision: self.state.system.revision,
                                    scan_id,
                                    relocation_id,
                                }));
                            }
                        }
                        operation @ (Operation::GuiConfig(_) | Operation::Navigate(_)) => {
                            let result = match operation {
                                Operation::GuiConfig(change) => self.update_gui_config(change),
                                Operation::Navigate(direction) => self.navigate(direction),
                                _ => unreachable!("operation matched above"),
                            };
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
                                    relocation_id: None,
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
                recv(self.relocation_rx) -> event => {
                    if let Ok(event) = event {
                        self.relocation_event(event);
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
                    match self.flush_config(false) {
                        Ok(true) => self.publish(),
                        Err(_) => {
                            let changed = self.state.system.last_error != self.shared.read().system.last_error;
                            if changed { self.publish(); }
                        }
                        Ok(false) => {}
                    }
            }
                }
        }

        if let Err(error) = self.stop() {
            self.state.system.last_error = Some(format!("Saving play count: {error:#}"));
        }
        if let Err(error) = self.save(true) {
            tracing::error!(error = %error, "shutdown_persistence_failed");
            self.state.system.last_error = Some(format!("Final save failed: {error:#}"));
        }
        let _ = self.engine.send(AudioCommand::Shutdown);
        self.shutdown_scan();
        self.shutdown_relocation();
        self.state.playback.status = PlaybackStatus::Stopped;
        self.state.system.shutting_down = true;
        self.publish();
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
    pub(in crate::core) fn audio(&self, command: AudioCommand) -> Result<()> {
        self.engine
            .commands
            .send(command)
            .context("Audio worker unavailable")
    }
    pub(in crate::core) fn track(&self, id: i64) -> Result<Track> {
        self.store.track(id)?.context("Track not found")
    }
    pub(in crate::core) fn refresh_queue_tracks(&mut self) -> Result<()> {
        self.state.queue = self.store.queue_projection(self.queue_ids.len())?;
        self.state.current_track = self
            .state
            .queue
            .current_id
            .map(|id| self.store.queue_track_id(id))
            .transpose()?
            .map(|id| self.store.track(id))
            .transpose()?
            .flatten()
            .map(Arc::new);
        Ok(())
    }
    pub(in crate::core) fn refresh_affected_queue_tracks(
        &mut self,
        track_ids: &[i64],
    ) -> Result<()> {
        self.state.queue = self.store.queue_projection(self.queue_ids.len())?;
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
    pub(in crate::core) fn query_response(&self, query: &Query) -> QueryResponse {
        let result = self.view(query);
        let playlist_id = match (query, &result) {
            (
                Query::PlaylistEntries { playlist_id, .. }
                | Query::PlaylistEntryBatch { playlist_id, .. }
                | Query::PlaylistEntryLocations { playlist_id, .. },
                _,
            ) => Some(*playlist_id),
            (Query::PlaylistEntry { .. }, Ok(ViewResponse::PlaylistEntry(Some(entry)))) => {
                Some(entry.playlist_id)
            }
            _ => None,
        };
        QueryResponse {
            revisions: playlist_id
                .map(|id| self.state.playlist_query_revisions(id))
                .unwrap_or_else(|| self.state.query_revisions()),
            result,
        }
    }

    pub(in crate::core) fn view(&self, query: &Query) -> Result<ViewResponse, QueryError> {
        let current = match query {
            Query::PlaylistEntryBatch { playlist_id, .. } => {
                self.state.playlist_query_revisions(*playlist_id)
            }
            _ => self.state.query_revisions(),
        };
        if let Query::QueueBatch {
            expected: Some(expected),
            ..
        } = query
            && expected.queue != current.queue
        {
            return Err(QueryError::RevisionChanged);
        }
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
        if let Query::PlaylistEntryBatch {
            after,
            expected: Some(expected),
            ..
        } = query
            && match expected.playlist_entries {
                Some(revision) => Some(revision) != current.playlist_entries,
                // A catalog baseline may start an entry traversal, never continue it.
                None => after.is_some(),
            }
        {
            return Err(QueryError::RevisionChanged);
        }
        Ok(match query {
            Query::Albums {
                query,
                sort,
                offset,
                limit,
            } => ViewResponse::Albums(self.store.albums_page(
                query.as_deref(),
                *sort,
                *offset,
                *limit,
            )?),
            Query::AlbumTracks { key, offset, limit } => {
                ViewResponse::AlbumTracks(self.store.album_tracks_page(key, *offset, *limit)?)
            }
            Query::DuplicateGroups { offset, limit } => ViewResponse::DuplicateGroups(
                self.store
                    .duplicate_groups_page(*offset, (*limit).min(MAX_QUERY_ROWS))?,
            ),
            Query::DuplicateMembers {
                fingerprint,
                offset,
                limit,
            } => ViewResponse::DuplicateMembers(self.store.duplicate_members_page(
                fingerprint,
                *offset,
                (*limit).min(MAX_QUERY_ROWS),
            )?),
            Query::RelocationTask { id } => ViewResponse::RelocationTask(
                self.active_relocation
                    .as_ref()
                    .map(|task| &task.status)
                    .filter(|status| status.id == *id)
                    .or_else(|| {
                        self.relocation_results
                            .iter()
                            .find(|status| status.id == *id)
                    })
                    .cloned()
                    .context("Unknown or expired relocation task")?,
            ),
            Query::QueuePage { offset, limit } => ViewResponse::QueuePage(
                self.store
                    .queue_page_with_total(*offset, *limit, self.queue_ids.len())?,
            ),
            Query::QueueBatch { after, limit, .. } => {
                ViewResponse::QueueBatch(self.store.queue_batch(*after, *limit)?)
            }
            Query::QueueEntries { queue_ids } => {
                ViewResponse::QueueEntries(self.store.queue_entries(queue_ids)?)
            }
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
            Query::LibraryLocate {
                track_id,
                path,
                tree,
                query,
                favorite,
                missing,
                sort,
            } => ViewResponse::LibraryLocate(self.store.library_location(
                *track_id,
                path,
                *tree,
                query.as_deref(),
                *favorite,
                *missing,
                *sort,
            )?),
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
                missing,
                offset,
                limit,
            } => ViewResponse::DirectoryPage(self.store.directory_page(
                path,
                query.as_deref(),
                *favorite,
                *missing,
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
            Query::PlaylistEntry { entry_id } => {
                ViewResponse::PlaylistEntry(self.store.playlist_entry(*entry_id)?)
            }
            Query::PlaylistEntryLocations {
                playlist_id,
                entry_ids,
            } => ViewResponse::PlaylistEntryLocations(
                self.store
                    .playlist_entry_locations(*playlist_id, entry_ids)?,
            ),
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
