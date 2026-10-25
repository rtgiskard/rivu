use super::*;

pub(super) enum BusinessEvent {
    Request(Request),
    Audio(AudioEvent),
    AudioStopped,
    Scan(ScanEvent),
    Relocation(RelocationFinished),
    Processing(crate::processing::Event),
    Export(Result<()>),
    ExternalAdmission(admission::Event),
    Optimization(Result<DatabaseOptimization>),
    FfmpegStatus,
    Shutdown(RuntimeControl),
}

/// Facts actually read in a query's SQLite snapshot. Failed/cancelled reads
/// cannot manufacture an observation from the response's fallback revisions.
struct QueryAuthority {
    album_revision: Option<u64>,
    playlist: Option<(i64, Option<u64>)>,
}

impl Core {
    /// Event waiting and state transitions stay on the same dedicated owner.
    /// The shared runtime only drives I/O, timers and independent long jobs.
    pub(super) fn run(
        &mut self,
        mut requests: mpsc::Receiver<Request>,
        mut shutdown: mpsc::Receiver<()>,
        control: &RuntimeControl,
    ) {
        let executor = self.executor.clone();
        let mut ffmpeg_status_ticks = None;
        while let Some(event) = wait_on(
            &executor,
            self.next_event(
                &mut requests,
                &mut shutdown,
                &mut ffmpeg_status_ticks,
                control,
            ),
        ) {
            if !self.business_event(event) {
                break;
            }
        }
        self.finalize();
    }

    async fn next_event(
        &mut self,
        requests: &mut mpsc::Receiver<Request>,
        shutdown: &mut mpsc::Receiver<()>,
        ffmpeg_status_ticks: &mut Option<tokio::time::Interval>,
        control: &RuntimeControl,
    ) -> Option<BusinessEvent> {
        if self.state.system.ffmpeg_status == "not loaded" {
            ffmpeg_status_ticks.get_or_insert_with(|| {
                let period = Duration::from_millis(100);
                let mut ticks =
                    tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                ticks
            });
        } else {
            *ffmpeg_status_ticks = None;
        }
        let Core {
            engine,
            scan_rx,
            relocation_rx,
            active_export,
            active_external_admission,
            active_optimization,
            processing,
            ..
        } = self;
        // Consume one event per iteration, with randomized selection to avoid
        // permanent priority for a hot input.
        Some(tokio::select! {
            _ = shutdown.recv() => BusinessEvent::Shutdown(control.clone()),
            request = requests.recv() => BusinessEvent::Request(request?),
            event = engine.events.as_mut().expect("Core owns audio events").recv() => {
                event.map_or(BusinessEvent::AudioStopped, BusinessEvent::Audio)
            }
            scan = scan_rx.recv() => BusinessEvent::Scan(scan?),
            relocation = relocation_rx.recv() => BusinessEvent::Relocation(relocation?),
            event = processing.receiver.recv() => BusinessEvent::Processing(event?),
            event = async {
                match active_external_admission.as_mut() {
                    Some(task) => task.next_event().await,
                    None => std::future::pending().await,
                }
            } => BusinessEvent::ExternalAdmission(event),
            result = async {
                match active_export.as_mut() {
                    Some(task) => (&mut task.finished).await,
                    None => std::future::pending().await,
                }
            } => BusinessEvent::Export(result.unwrap_or_else(|_| Err(anyhow::anyhow!(
                "Playlist export ended without a result; inspect the destination before retrying"
            )))),
            result = async {
                match active_optimization.as_mut() {
                    Some(task) => (&mut task.finished).await,
                    None => std::future::pending().await,
                }
            } => BusinessEvent::Optimization(result.unwrap_or_else(|_| Err(anyhow::anyhow!(
                "Database optimization ended without a result; the database may already have changed"
            )))),
            _ = async {
                match ffmpeg_status_ticks.as_mut() {
                    Some(ticks) => { ticks.tick().await; }
                    None => std::future::pending().await,
                }
            } => BusinessEvent::FfmpegStatus,
        })
    }
}

impl Core {
    /// Synchronous SQLite work and its state transition share one owner.
    /// Traversal/export/hash/probe/scan-save/VACUUM remain independent jobs.
    fn business_event(&mut self, event: BusinessEvent) -> bool {
        match event {
            BusinessEvent::Request(request) => self.dispatch_request(request),
            BusinessEvent::Audio(event) => {
                if let Err(error) = self.audio_event(event) {
                    self.state.system.last_error = Some(format!("{error:#}"));
                }
                self.publish();
            }
            BusinessEvent::AudioStopped => {
                self.state.system.last_error = Some("Audio worker stopped unexpectedly".into());
                self.publish();
                return false;
            }
            BusinessEvent::Scan(scan) => self.scan_event(scan),
            BusinessEvent::Relocation(event) => {
                self.relocation_event(event);
                self.resume_pending_scan();
            }
            BusinessEvent::Processing(event) => self.processing_event(event),
            BusinessEvent::ExternalAdmission(event) => self.external_admission_event(event),
            BusinessEvent::Export(result) => {
                let (reply, result) = self.finish_export(result);
                self.complete_background(reply, result);
            }
            BusinessEvent::Optimization(result) => {
                let (reply, result) = self.finish_optimization(result);
                self.complete_background(reply, result);
            }
            BusinessEvent::FfmpegStatus => {
                if audio::ffmpeg_load_attempted() {
                    self.state.system.ffmpeg_status = audio::ffmpeg_status()
                        .unwrap_or_else(|error| format!("unavailable: {error:#}"));
                    self.publish();
                }
            }
            BusinessEvent::Shutdown(control) => {
                self.state.system.shutting_down = true;
                self.publish();
                control.acknowledge(Ack::from_result(Ok(()), self.state.system.revision));
                if let Err(error) = self.stop() {
                    self.state.system.last_error = Some(format!("{error:#}"));
                    tracing::error!(error = %error, "shutdown_playback_stop_failed");
                }
                self.publish();
                return false;
            }
        }
        if !self.database_maintenance_active()
            && (self.playback_write_deferred || self.pending_navigation.is_some())
        {
            if let Err(error) = self.flush_deferred_playback() {
                self.state.system.last_error =
                    Some(format!("Saving deferred playback statistics: {error:#}"));
            }
            self.publish();
        }
        true
    }

    pub(super) fn initialize(&mut self) {
        let started = Instant::now();
        tracing::debug!("initial_load_started");
        if let Err(error) = self.load_initial_state() {
            tracing::error!(error = %error, "initial_load_failed");
            self.state.system.last_error = Some(format!("Loading library: {error:#}"));
            self.publish();
        } else {
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis(),
                "initial_load_completed"
            );
        }
        let _ = self
            .engine
            .send(AudioCommand::StillMode(self.state.system.config.still_mode));
        let _ = self
            .engine
            .send(AudioCommand::Volume(self.state.playback.volume));
        let _ = self.engine.send(AudioCommand::FfmpegEnabled(
            self.state.system.config.ffmpeg_enabled,
        ));
        let _ = self.engine.send(AudioCommand::OutputSettings {
            request: self.output_settings_request,
            device: self.state.system.selected_device.clone(),
            auto_mix: self.state.system.config.pipewire_auto_mix,
        });
        let _ = self.engine.send(AudioCommand::Equalizer(Arc::new(
            self.state.system.config.equalizer.clone(),
        )));
        let _ = self.engine.send(AudioCommand::AnalysisSettings(
            self.state.system.config.as_ref().into(),
        ));
    }

    fn dispatch_request(&mut self, request: Request) {
        match request.operation {
            Operation::Query { query, ticket } => {
                let response = self.query_response_if_current(&query, ticket.as_ref());
                if let Some(reply) = request.reply {
                    let _ = reply.send(CoreResponse::Query(response));
                }
            }
            Operation::WaveformSnapshot(reply) => {
                if let Err(error) = self
                    .engine
                    .send(AudioCommand::WaveformSnapshot(reply.clone()))
                {
                    let _ = reply.try_send(Err(format!("{error:#}")));
                }
            }
            Operation::Command(command) => {
                let starts_scan = matches!(
                    &command,
                    Command::Scan { .. }
                        | Command::ImportPlaylist { .. }
                        | Command::CheckOtherTracks { .. }
                );
                let library_owned = matches!(
                    &command,
                    Command::Scan { .. } | Command::CheckOtherTracks { .. }
                );
                let starts_relocation = matches!(&command, Command::RelocateTrack { .. });
                let starts_export = matches!(&command, Command::ExportPlaylist { .. });
                let starts_external = matches!(&command, Command::QueueExternalPaths { .. });
                let starts_optimization = matches!(&command, Command::OptimizeDatabase);
                let starts_processing = matches!(
                    &command,
                    Command::StartProcessing { .. }
                        | Command::ProcessingGraphRun { .. }
                        | Command::ProcessingGraphExport { .. }
                        | Command::ProcessingGraphAudition { .. }
                );
                let maintenance = self.database_maintenance_active();
                let result = self.command(command);
                if let Err(error) = &result
                    && (!library_owned || request.reply.is_none())
                {
                    self.state.system.last_error = Some(format!("{error:#}"));
                }
                if !maintenance {
                    // command() attempted the token refresh without changing its result.
                    self.publish_snapshot();
                } else {
                    self.publish();
                }
                let scan_id = if result.is_ok() {
                    if starts_external {
                        self.active_external_admission
                            .as_ref()
                            .map(|task| task.status.id)
                    } else if starts_scan {
                        self.active_scan.as_ref().map(|task| task.status.id)
                    } else {
                        None
                    }
                } else {
                    None
                };
                let relocation_id = if result.is_ok() && starts_relocation {
                    self.active_relocation.as_ref().map(|task| task.status.id)
                } else {
                    None
                };
                let processing_id = if result.is_ok() && starts_processing {
                    self.processing.active.as_ref().map(|task| task.id)
                } else {
                    None
                };
                if result.is_ok() && starts_export {
                    self.active_export
                        .as_mut()
                        .expect("export was accepted")
                        .reply = request.reply;
                    return;
                }
                if result.is_ok() && starts_optimization {
                    self.active_optimization
                        .as_mut()
                        .expect("optimization was accepted")
                        .reply = request.reply;
                    return;
                }
                if let Some(reply) = request.reply {
                    let _ = reply.send(CoreResponse::Ack(Ack {
                        scan_id,
                        relocation_id,
                        processing_id,
                        ..Ack::from_result(result, self.state.system.revision)
                    }));
                }
            }
            operation @ (Operation::GuiConfig(_) | Operation::Navigate(_)) => {
                let result = if self.database_maintenance_active() {
                    Err(anyhow::anyhow!(
                        "Database maintenance is in progress; this operation was not executed"
                    ))
                } else {
                    match operation {
                        Operation::GuiConfig(change) => self.update_gui_config(change),
                        Operation::Navigate(direction) => self.navigate(direction),
                        _ => unreachable!("operation matched above"),
                    }
                };
                if let Err(error) = &result {
                    self.state.system.last_error = Some(format!("{error:#}"));
                }
                self.publish();
                if let Some(reply) = request.reply {
                    let _ = reply.send(CoreResponse::Ack(Ack::from_result(
                        result,
                        self.state.system.revision,
                    )));
                }
            }
        }
    }

    fn finalize(&mut self) {
        self.state.system.shutting_down = true;
        // Retire background database users before final writes on Core's connection.
        if let Some((reply, result)) = self.shutdown_export() {
            self.complete_background(reply, result);
        }
        self.shutdown_external_admission();
        self.shutdown_scan();
        self.shutdown_relocation();
        self.processing.shutdown();
        if let Some((reply, result)) = self.shutdown_optimization() {
            self.complete_background(reply, result);
        }
        if let Err(error) = self.flush_deferred_playback() {
            self.state.system.last_error =
                Some(format!("Saving deferred playback statistics: {error:#}"));
        }
        if let Err(error) = self.stop() {
            self.state.system.last_error = Some(format!("Saving play count: {error:#}"));
        }
        // Independent persistence domains both get a final attempt even if one fails.
        let queue = self.save_queue();
        let config = self.flush_config().map(|_| ());
        if let Err(error) = queue.and(config) {
            tracing::error!(error = %error, "shutdown_persistence_failed");
            self.state.system.last_error = Some(format!("Final save failed: {error:#}"));
        }
        let _ = self.engine.send(AudioCommand::Shutdown);
        self.state.playback.status = PlaybackStatus::Stopped;
        self.state.system.shutting_down = true;
        self.publish();
    }

    pub(in crate::core) fn complete_background(
        &mut self,
        reply: Option<oneshot::Sender<CoreResponse>>,
        result: Result<()>,
    ) {
        if let Err(error) = &result {
            self.state.system.last_error = Some(format!("{error:#}"));
        }
        self.publish();
        if let Some(reply) = reply {
            let _ = reply.send(CoreResponse::Ack(Ack::from_result(
                result,
                self.state.system.revision,
            )));
        }
    }

    pub(in crate::core) fn refresh_album_revision(&mut self) {
        match self.store.album_revision() {
            Ok(revision) => self.state.library.album_revision = revision,
            Err(error) => {
                self.state.system.last_error = Some(format!(
                    "Refreshing album collection token failed: {error:#}"
                ))
            }
        }
    }

    pub(in crate::core) fn publish(&mut self) {
        self.refresh_album_revision();
        self.publish_snapshot();
    }

    /// Publish observations already captured by the serialized owner.
    fn publish_snapshot(&mut self) {
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
                shared.processing_audition = self.state.processing_audition;
            }
            if changed.queue {
                shared.queue = self.state.queue.clone();
            }
            if changed.library {
                shared.library = self.state.library.clone();
            }
            if changed.system {
                shared.system = self.state.system.clone();
                shared.scan_owner = self.state.scan_owner;
            } else {
                // The in-process wakeup sequence advances even without a System change.
                shared.system.revision = self.state.system.revision;
            }
            shared.revisions = *revisions;
        }
        self.revision_updates.send_if_modified(|revisions| {
            if *revisions == self.state.revisions {
                false
            } else {
                *revisions = self.state.revisions;
                true
            }
        });
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

    /// This is the read dispatch boundary: cancellation never interrupts SQL
    /// that has passed this check, and its normal reply retires the caller.
    pub(in crate::core) fn query_response_if_current(
        &mut self,
        query: &Query,
        ticket: Option<&QueryTicket>,
    ) -> QueryResponse {
        if ticket.is_some_and(|ticket| !ticket.is_current()) {
            return QueryResponse {
                revisions: self.state.query_revisions_for(query),
                result: Err(QueryError::Cancelled),
            };
        }
        let (response, authority) = self.read_query(query);
        if let Some(authority) = authority {
            let mut changed = false;
            if let Some(revision) = authority.album_revision {
                changed = self.state.library.album_revision != revision;
                self.state.library.album_revision = revision;
            }
            if let Some((playlist_id, revision)) = authority.playlist {
                changed |= self.set_playlist_entry_revision(playlist_id, revision);
            }
            if changed {
                // Core publishes before dispatch_request releases the response;
                // GUI completion refreshes this snapshot before checking stamps.
                self.publish_snapshot();
            }
        }
        response
    }

    #[cfg(test)]
    pub(in crate::core) fn query_response(&self, query: &Query) -> QueryResponse {
        self.read_query(query).0
    }

    fn read_query(&self, query: &Query) -> (QueryResponse, Option<QueryAuthority>) {
        let mut revisions = self.state.query_revisions_for(query);
        if !matches!(
            query,
            Query::Albums { .. }
                | Query::AlbumTracks { .. }
                | Query::PlaylistEntries { .. }
                | Query::PlaylistEntryBatch { .. }
                | Query::PlaylistEntryLocations { .. }
                | Query::PlaylistEntry { .. }
                | Query::QueuePage { .. }
                | Query::QueueBatch { .. }
                | Query::QueueEntries { .. }
                | Query::DuplicateMembers { .. }
                | Query::DuplicateMemberSelection { .. }
        ) {
            return (
                QueryResponse {
                    revisions,
                    result: self.view_in_snapshot(query, revisions),
                },
                None,
            );
        }
        let mut authority = None;
        let result = (|| {
            let _snapshot = self.store.query_snapshot()?;
            revisions = self.database_query_revisions(query)?;
            let playlist_id = match query {
                Query::PlaylistEntries { playlist_id, .. }
                | Query::PlaylistEntryBatch { playlist_id, .. }
                | Query::PlaylistEntryLocations { playlist_id, .. } => Some(*playlist_id),
                _ => None,
            };
            authority = Some(QueryAuthority {
                album_revision: matches!(query, Query::Albums { .. } | Query::AlbumTracks { .. })
                    .then_some(revisions.album),
                playlist: playlist_id
                    .map(|id| (id, revisions.playlist_entries.map(|scope| scope.revision))),
            });
            let result = self.view_in_snapshot(query, revisions)?;
            if let (Query::PlaylistEntry { .. }, ViewResponse::PlaylistEntry(Some(entry))) =
                (query, &result)
            {
                revisions.playlist_entries = self
                    .store
                    .playlist_entry_revision(entry.playlist_id)?
                    .map(|revision| rivu_protocol::response::PlaylistEntryRevision {
                        playlist_id: entry.playlist_id,
                        revision,
                    });
                authority.as_mut().unwrap().playlist = Some((
                    entry.playlist_id,
                    revisions.playlist_entries.map(|scope| scope.revision),
                ));
            }
            Ok(result)
        })();
        (QueryResponse { revisions, result }, authority)
    }

    fn database_query_revisions(&self, query: &Query) -> Result<QueryRevisions, QueryError> {
        let mut revisions = self.state.query_revisions_for(query);
        match query {
            Query::Albums { .. } | Query::AlbumTracks { .. } => {
                revisions.album = self.store.album_revision()?;
            }
            Query::QueuePage { .. }
            | Query::QueueBatch { .. }
            | Query::QueueEntries { .. }
            | Query::DuplicateMembers { .. }
            | Query::DuplicateMemberSelection { .. } => {
                revisions.queue = self.store.queue_revision()?;
            }
            _ => {}
        }
        let playlist_id = match query {
            Query::PlaylistEntries { playlist_id, .. }
            | Query::PlaylistEntryBatch { playlist_id, .. }
            | Query::PlaylistEntryLocations { playlist_id, .. } => Some(*playlist_id),
            _ => None,
        };
        if let Some(playlist_id) = playlist_id {
            revisions.playlist_entries =
                self.store
                    .playlist_entry_revision(playlist_id)?
                    .map(|revision| rivu_protocol::response::PlaylistEntryRevision {
                        playlist_id,
                        revision,
                    });
            if revisions.playlist_entries.is_none() {
                revisions.playlist_metadata = None;
            }
        }
        Ok(revisions)
    }

    #[cfg(test)]
    pub(in crate::core) fn view(&self, query: &Query) -> Result<ViewResponse, QueryError> {
        self.query_response(query).result
    }

    fn view_in_snapshot(
        &self,
        query: &Query,
        current: QueryRevisions,
    ) -> Result<ViewResponse, QueryError> {
        if self.database_maintenance_active()
            && !matches!(
                query,
                Query::ScanTask { .. }
                    | Query::RelocationTask { .. }
                    | Query::ProcessingTask { .. }
            )
        {
            return Err(QueryError::Message(
                "Database maintenance is in progress; retry this read after it finishes".into(),
            ));
        }
        let current_stamp = query.stamp(current);
        let expected = match query {
            Query::TrackBatch { expected, .. }
            | Query::PlaylistSummaryBatch { expected, .. }
            | Query::PlaylistEntryBatch { expected, .. }
            | Query::QueueBatch { expected, .. } => *expected,
            _ => None,
        };
        if let Some(mut expected) = expected {
            if let Query::PlaylistEntryBatch { after, .. } = query
                && expected.playlist_entries.is_none()
                && after.is_none()
            {
                // A non-scoped baseline can begin, never continue, a traversal.
                if expected.library != current.library {
                    return Err(QueryError::RevisionChanged);
                }
                expected.playlist_metadata = current.playlist_metadata;
                expected.playlist_entries = current.playlist_entries;
            }
            if query.stamp(expected) != query.stamp(current) {
                return Err(QueryError::RevisionChanged);
            }
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
            Query::AlbumTracks {
                key,
                query,
                offset,
                limit,
            } => ViewResponse::AlbumTracks(self.store.album_tracks_page(
                key,
                query.as_deref(),
                *offset,
                *limit,
            )?),
            Query::DuplicateGroups { offset, limit } => {
                ViewResponse::DuplicateGroups(self.store.duplicate_groups_page(*offset, *limit)?)
            }
            Query::DuplicateMembers {
                fingerprint,
                offset,
                limit,
            } => ViewResponse::DuplicateMembers(self.store.duplicate_members_page(
                fingerprint,
                *offset,
                *limit,
            )?),
            Query::DuplicateMemberSelection {
                fingerprint,
                track_ids,
            } => ViewResponse::DuplicateMembers(
                self.store
                    .duplicate_member_selection(fingerprint, track_ids)?,
            ),
            Query::ProcessingTask { id } => {
                ViewResponse::ProcessingTask(self.processing_status(*id).map(Box::new))
            }
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
            Query::QueuePage {
                query,
                offset,
                limit,
            } => {
                ViewResponse::QueuePage(self.store.queue_page(query.as_deref(), *offset, *limit)?)
            }
            Query::QueueBatch {
                query,
                after,
                limit,
                ..
            } => ViewResponse::QueueBatch(self.store.queue_batch(
                query.as_deref(),
                *after,
                *limit,
                current_stamp,
            )?),
            Query::QueueEntries { query, queue_ids } => {
                ViewResponse::QueueEntries(self.store.queue_entries(query.as_deref(), queue_ids)?)
            }
            Query::ScanTask { id } => ViewResponse::ScanTask(
                self.active_scan
                    .as_ref()
                    .map(|task| &task.status)
                    .filter(|status| status.id == *id)
                    .or_else(|| {
                        self.active_external_admission
                            .as_ref()
                            .map(|task| &task.status)
                            .filter(|status| status.id == *id)
                    })
                    .or_else(|| self.scan_results.iter().find(|status| status.id == *id))
                    .cloned()
                    .context("Unknown or expired scan task")?,
            ),
            Query::OtherTracksPage {
                query,
                favorite,
                missing,
                sort,
                offset,
                limit,
            } => ViewResponse::LibraryPage(self.store.other_tracks_page(
                &self.state.system.config.library_roots,
                query.as_deref(),
                *favorite,
                *missing,
                *sort,
                *offset,
                *limit,
            )?),
            Query::OtherTracksCount => ViewResponse::OtherTracksCount(
                self.store
                    .other_tracks_count(&self.state.system.config.library_roots)?,
            ),
            Query::OtherTracksLocate {
                track_id,
                query,
                favorite,
                missing,
                sort,
            } => ViewResponse::LibraryLocate(self.store.other_tracks_location(
                &self.state.system.config.library_roots,
                *track_id,
                query.as_deref(),
                *favorite,
                *missing,
                *sort,
            )?),
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
            Query::PlaylistSummaries {
                query,
                offset,
                limit,
            } => ViewResponse::PlaylistSummaries(self.store.playlist_summary_page(
                query.as_deref(),
                *offset,
                *limit,
            )?),
            Query::PlaylistEntries {
                playlist_id,
                query,
                offset,
                limit,
            } => ViewResponse::PlaylistEntries(self.store.playlist_entries_page(
                *playlist_id,
                query.as_deref(),
                *offset,
                *limit,
            )?),
            Query::Track { track_id } => ViewResponse::Track(self.store.track(*track_id)?),
            Query::PlaylistEntry { entry_id } => {
                ViewResponse::PlaylistEntry(self.store.playlist_entry(*entry_id)?)
            }
            Query::PlaylistEntryLocations {
                playlist_id,
                query,
                entry_ids,
            } => ViewResponse::PlaylistEntryLocations(self.store.playlist_entry_locations(
                *playlist_id,
                query.as_deref(),
                entry_ids,
            )?),
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
            Query::PlaylistSummaryBatch {
                query,
                after,
                limit,
                ..
            } => ViewResponse::PlaylistSummaryBatch(self.store.playlist_summary_batch(
                query.as_deref(),
                *after,
                *limit,
            )?),
            Query::PlaylistEntryBatch {
                playlist_id,
                query,
                after,
                limit,
                ..
            } => ViewResponse::PlaylistEntryBatch(self.store.playlist_entry_batch(
                *playlist_id,
                query.as_deref(),
                *after,
                *limit,
                current_stamp,
            )?),
        })
    }
}
