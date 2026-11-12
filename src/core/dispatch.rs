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
    FfmpegLoaded,
    DevicesEnumerated(Result<Vec<String>>),
    MprisStatus,
    Shutdown(CoreLifecycle),
}

struct PublicationChanges {
    sections: StateSections,
    mpris_service: bool,
    mpris: bool,
    #[cfg(feature = "gui")]
    tray: bool,
}

impl PublicationChanges {
    fn between(current: &CoreState, previous: &CoreState, sections: StateSections) -> Self {
        let track_changed = match (&current.current_track, &previous.current_track) {
            (Some(current), Some(previous)) => !Arc::ptr_eq(current, previous),
            (None, None) => false,
            _ => true,
        };
        let transport_changed = track_changed
            || current.playback.status != previous.playback.status
            || current.playback.shuffle != previous.playback.shuffle
            || current.playback.repeat != previous.playback.repeat
            || current.system.shutting_down != previous.system.shutting_down;
        Self {
            sections,
            mpris_service: current.configuration.config.mpris_enabled
                != previous.configuration.config.mpris_enabled
                || current.system.shutting_down != previous.system.shutting_down,
            mpris: transport_changed
                || current.queue != previous.queue
                || current.playback.volume != previous.playback.volume
                || current.playback.duration != previous.playback.duration
                || current.playback.seek_revision != previous.playback.seek_revision,
            #[cfg(feature = "gui")]
            tray: transport_changed,
        }
    }

    fn includes(&self, interest: RevisionInterest) -> bool {
        match interest {
            RevisionInterest::Sections(sections) => {
                !self.sections.intersection(sections).is_empty()
            }
            RevisionInterest::MprisService => self.mpris_service,
            RevisionInterest::Mpris => self.mpris,
            #[cfg(feature = "gui")]
            RevisionInterest::Tray => self.tray,
        }
    }
}

impl Core {
    /// Event waiting and state transitions stay on the same dedicated owner.
    /// The shared runtime only drives I/O, timers and independent long jobs.
    pub(super) fn run(
        &mut self,
        mut requests: mpsc::Receiver<Request>,
        mut shutdown: mpsc::Receiver<()>,
        control: &CoreLifecycle,
    ) {
        match self
            .store
            .database_path()
            .and_then(|path| query_worker::ReadQueries::start(path, Arc::clone(&self.shared)))
        {
            Ok(reads) => self.read_queries = Some(reads),
            Err(error) => {
                self.state.system.last_error = Some(format!("Starting read queries: {error:#}"));
                self.publish();
                let error = error.context("Starting read queries");
                let error = match self.finalize() {
                    Ok(()) => error,
                    Err(finalization) => {
                        error.context(format!("Finalization also failed: {finalization:#}"))
                    }
                };
                control.record_result(Err(error));
                return;
            }
        }
        while let Some(event) =
            futures::executor::block_on(self.next_event(&mut requests, &mut shutdown, control))
        {
            let transition = self
                .read_queries
                .as_ref()
                .map(|reads| reads.fence.transition());
            let continuing = self.business_event(event);
            if let Some(transition) = transition {
                transition.finish(self.database_maintenance_active() || !continuing);
            }
            if !continuing {
                break;
            }
        }
        drop(self.read_queries.take());
        control.record_result(self.finalize());
    }

    pub(super) async fn next_event(
        &mut self,
        requests: &mut mpsc::Receiver<Request>,
        shutdown: &mut mpsc::Receiver<()>,
        control: &CoreLifecycle,
    ) -> Option<BusinessEvent> {
        let Core {
            engine,
            initial_devices,
            scan_rx,
            relocation_rx,
            active_export,
            active_external_admission,
            active_optimization,
            processing,
            mpris_status,
            ..
        } = self;
        let awaiting_ffmpeg = self.state.system.ffmpeg_status == "not loaded";
        // Consume one event per iteration, with randomized selection to avoid
        // permanent priority for a hot input.
        Some(futures::select! {
            _ = shutdown.recv().fuse() => BusinessEvent::Shutdown(control.clone()),
            _ = mpris_status.ready.recv().fuse() => BusinessEvent::MprisStatus,
            request = requests.recv().fuse() => BusinessEvent::Request(request.ok()?),
            event = engine.events.as_ref().expect("Core owns audio events").recv().fuse() => {
                event.map_or(BusinessEvent::AudioStopped, BusinessEvent::Audio)
            },
            scan = scan_rx.recv().fuse() => BusinessEvent::Scan(scan.ok()?),
            relocation = relocation_rx.recv().fuse() => BusinessEvent::Relocation(relocation.ok()?),
            event = processing.receiver.recv().fuse() => BusinessEvent::Processing(event.ok()?),
            _ = async {
                if awaiting_ffmpeg {
                    audio::ffmpeg_load_completed().await;
                } else {
                    std::future::pending::<()>().await;
                }
            }.fuse() => BusinessEvent::FfmpegLoaded,
            result = async {
                match initial_devices.as_mut() {
                    Some(task) => (&mut task.finished).await,
                    None => std::future::pending().await,
                }
            }.fuse() => BusinessEvent::DevicesEnumerated(result.unwrap_or_else(|_| {
                Err(anyhow::anyhow!("Initial device enumeration ended without a result"))
            })),
            event = async {
                match active_external_admission.as_mut() {
                    Some(task) => task.next_event().await,
                    None => std::future::pending().await,
                }
            }.fuse() => BusinessEvent::ExternalAdmission(event),
            result = async {
                match active_export.as_mut() {
                    Some(task) => (&mut task.finished).await,
                    None => std::future::pending().await,
                }
            }.fuse() => BusinessEvent::Export(result.unwrap_or_else(|_| Err(anyhow::anyhow!(
                "Playlist export ended without a result; inspect the destination before retrying"
            )))),
            result = async {
                match active_optimization.as_mut() {
                    Some(task) => (&mut task.finished).await,
                    None => std::future::pending().await,
                }
            }.fuse() => BusinessEvent::Optimization(result.unwrap_or_else(|_| Err(anyhow::anyhow!(
                "Database optimization ended without a result; the database may already have changed"
            )))),
        })
    }
}

impl Core {
    /// Synchronous SQLite work and its state transition share one owner.
    /// Traversal/export/hash/probe/scan-save/VACUUM remain independent jobs.
    pub(super) fn business_event(&mut self, event: BusinessEvent) -> bool {
        match event {
            BusinessEvent::Request(request) => self.dispatch_request(request),
            BusinessEvent::Audio(event) => {
                let result = self.audio_event(event);
                if let Err(error) = result {
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
                self.complete_optimization(reply, result);
            }
            BusinessEvent::FfmpegLoaded => {
                self.state.system.ffmpeg_status = audio::ffmpeg_status()
                    .unwrap_or_else(|error| format!("unavailable: {error:#}"));
                self.publish();
            }
            BusinessEvent::DevicesEnumerated(result) => {
                if let Some(mut task) = self.initial_devices.take() {
                    match task.join().and(result) {
                        Ok(devices) => self.state.system.devices = Arc::new(devices),
                        Err(error) => {
                            tracing::error!(error = %error, "initial_device_enumeration_failed");
                            self.state.system.last_error =
                                Some(format!("Enumerating audio devices: {error:#}"));
                        }
                    }
                    self.publish();
                }
            }
            BusinessEvent::MprisStatus => {
                let status = self.mpris_status.latest.lock().take();
                if let Some(status) = status {
                    self.state.system.mpris_status = status;
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
            && (self.transport.write_deferred || self.transport.pending_navigation.is_some())
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
        self.initialize_with_device_enumerator(audio::devices);
    }

    pub(super) fn initialize_with_device_enumerator(
        &mut self,
        enumerate: impl FnOnce() -> Result<Vec<String>> + Send + 'static,
    ) {
        self.initial_devices = Some(runtime::InitialDevices::start(
            &self.executor,
            self.workers.clone(),
            enumerate,
        ));
        self.initialize_state();
    }

    pub(super) fn initialize_state(&mut self) {
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
        let _ = self.engine.send(AudioCommand::VisualizationEnabled(
            self.preferences.settings().visualization_enabled,
        ));
        let _ = self
            .engine
            .send(AudioCommand::Volume(self.preferences.live().volume));
        let _ = self.engine.send(AudioCommand::FfmpegEnabled(
            self.preferences.settings().ffmpeg_enabled,
        ));
        let _ = self.engine.send(AudioCommand::OutputSettings {
            request: self.output_settings_request,
            device: self.preferences.live().output_device.clone(),
            auto_mix: self.preferences.settings().pipewire_auto_mix,
        });
        let _ = self
            .engine
            .send(AudioCommand::Equalizer(Arc::new(self.playback_equalizer())));
        let _ = self.engine.send(AudioCommand::VisualizationSignal(
            self.preferences.settings().visualization_signal,
        ));
    }

    pub(super) fn dispatch_request(&mut self, request: Request) {
        let recover = matches!(
            &request.operation,
            Operation::Query { .. } | Operation::Command(_)
        );
        self.projections.request_attempts = recover.then(projections::ProjectionAttempts::default);
        self.apply_request(request);
        if recover {
            // Only untouched dirty domains receive this request's recovery
            // attempt. A synchronous failure must wait for a later demand.
            self.recover_projections_on_demand();
        }
        self.projections.request_attempts = None;
    }

    fn apply_request(&mut self, request: Request) {
        match request.operation {
            Operation::Query { query, ticket } => {
                if self.transport.projection_stale
                    && matches!(
                        &query,
                        Query::QueuePage { .. }
                            | Query::QueueBatch { .. }
                            | Query::QueueEntries { .. }
                    )
                    && !self.database_maintenance_active()
                    && !self.database_unavailable
                    && ticket.as_ref().is_none_or(QueryTicket::is_current)
                {
                    // Reconcile selection and publish before capturing the read's
                    // revisions or allowing its reply to expose committed rows.
                    let result = self.reload_queue();
                    self.publish();
                    if let Err(error) = result {
                        if let Some(reply) = request.reply {
                            let _ = reply.send(CoreResponse::Query(QueryResponse {
                                revisions: self.state.query_revisions_for(&query),
                                result: Err(error.context("Refreshing queue projection").into()),
                            }));
                        }
                        return;
                    }
                }
                if self.projections.playlist_tokens
                    && (matches!(
                        rivu_protocol::response::QueryRevisionScope::for_query(&query),
                        rivu_protocol::response::QueryRevisionScope::PlaylistEntries(_)
                    ) || matches!(&query, Query::PlaylistEntry { .. }))
                    && !self.database_maintenance_active()
                    && !self.database_unavailable
                    && ticket.as_ref().is_none_or(QueryTicket::is_current)
                {
                    // Capture query versions only after their authority is ready.
                    if let Err(error) = self.recover_playlist_tokens() {
                        if let Some(reply) = request.reply {
                            let _ = reply.send(CoreResponse::Query(QueryResponse {
                                revisions: self.state.query_revisions_for(&query),
                                result: Err(error
                                    .context("Initializing playlist entry revisions")
                                    .into()),
                            }));
                        }
                        return;
                    }
                    self.publish();
                }
                if let Some(reads) = &self.read_queries
                    && !matches!(
                        &query,
                        Query::ScanTask { .. }
                            | Query::RelocationTask { .. }
                            | Query::ProcessingTask { .. }
                    )
                    && !self.database_maintenance_active()
                {
                    if let Some(reply) = request.reply {
                        reads.submit(query, ticket, &self.state, reply);
                    }
                } else {
                    let response = self.query_response_if_current(&query, ticket.as_ref());
                    if let Some(reply) = request.reply {
                        let _ = reply.send(CoreResponse::Query(response));
                    }
                }
            }
            Operation::WaitTask {
                target,
                registration,
            } => {
                self.register_task_wait(target, registration);
            }
            Operation::RenderSnapshot { source, reply } => {
                if self.state.source_generation == source {
                    let _ = self.engine.send(AudioCommand::RenderSnapshot(reply));
                } else {
                    let _ = reply.send(Err("Audio source changed before capture".into()));
                }
            }
            Operation::Command(command) => {
                use command::CommandOutcome;
                let result = self.command(command);
                if let Err(error) = &result
                    && (!error.library_owned || request.reply.is_none())
                {
                    self.state.system.last_error = Some(format!("{:#}", error.error));
                }
                self.publish();
                let mut ack = Ack::from_result(Ok(()), self.state.system.revision);
                match result {
                    Ok(CommandOutcome::Immediate) => {}
                    Ok(CommandOutcome::Scan(id)) => ack.scan_id = Some(id),
                    Ok(CommandOutcome::Relocation(id)) => ack.relocation_id = Some(id),
                    Ok(CommandOutcome::Processing(id)) => ack.processing_id = Some(id),
                    Ok(CommandOutcome::DeferredExport) => {
                        self.active_export
                            .as_mut()
                            .expect("export was accepted")
                            .reply = request.reply;
                        return;
                    }
                    Ok(CommandOutcome::DeferredOptimization) => {
                        self.active_optimization
                            .as_mut()
                            .expect("optimization was accepted")
                            .reply = request.reply;
                        return;
                    }
                    Err(error) => {
                        ack = Ack::from_result(Err(error.error), self.state.system.revision);
                    }
                }
                if let Some(reply) = request.reply {
                    let _ = reply.send(CoreResponse::Ack(ack));
                }
            }
            Operation::MprisSeek { queue_id, offset } => {
                let result = self.mpris_seek(queue_id, offset);
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
            Operation::MprisSetPosition { queue_id, position } => {
                let result = self.mpris_set_position(queue_id, position);
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

    fn finalize(&mut self) -> Result<()> {
        self.state.system.shutting_down = true;
        // Retire background database users before final writes on Core's connection.
        if let Some((reply, result)) = self.shutdown_export() {
            self.complete_background(reply, result);
        }
        self.shutdown_external_admission();
        self.shutdown_scan();
        self.shutdown_relocation();
        self.shutdown_processing();
        if let Some((reply, result)) = self.shutdown_optimization() {
            self.complete_optimization(reply, result);
        }
        // Optional startup work is not cancelled or published during shutdown.
        // Completion includes the physical PipeWire connection's retirement.
        drop(self.initial_devices.take());
        // Every persistence domain gets its final attempt, even after a failure.
        let deferred = self
            .flush_deferred_playback()
            .context("Saving deferred playback statistics");
        let playback = self.stop().context("Saving play count");
        // Retry only an admitted runtime selection; an uninitialized projection
        // is not an intent to clear the database's saved occurrence.
        let queue = self.persist_queue_current().context("Saving queue");
        let config = self
            .flush_config()
            .map(|_| ())
            .context("Saving configuration");
        let errors: Vec<_> = [deferred, playback, queue, config]
            .into_iter()
            .filter_map(|result| result.err().map(|error| format!("{error:#}")))
            .collect();
        let result = if errors.is_empty() {
            Ok(())
        } else {
            let error = anyhow::anyhow!("Final save failed: {}", errors.join("; "));
            tracing::error!(error = %error, "shutdown_persistence_failed");
            self.state.system.last_error = Some(error.to_string());
            Err(error)
        };
        let _ = self.engine.send(AudioCommand::Shutdown);
        self.state.playback.status = PlaybackStatus::Stopped;
        self.state.system.shutting_down = true;
        self.publish();
        self.close_task_waiters();
        result
    }

    pub(in crate::core) fn complete_optimization(
        &mut self,
        reply: Option<oneshot::Sender<CoreResponse>>,
        result: Result<DatabaseOptimization>,
    ) {
        let report = result.as_ref().ok().cloned();
        if let Err(error) = &result {
            self.state.system.last_error = Some(format!("{error:#}"));
        }
        self.publish();
        if let Some(reply) = reply {
            let mut ack = Ack::from_result(result.map(|_| ()), self.state.system.revision);
            ack.database_optimization = report;
            let _ = reply.send(CoreResponse::Ack(ack));
        }
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

    /// Publish observations already captured by the serialized owner.
    pub(in crate::core) fn publish(&mut self) {
        self.preferences.project(&mut self.state);
        self.state.processing = self.processing.current_projection();
        self.state.processing_selection = self.processing.audition_projection();
        self.state.processing_audition = self
            .state
            .processing_selection
            .filter(|_| !self.preferences.settings().processing_bypassed);
        self.state.source_generation = self.transport.generation;
        self.state.system.revision = self.state.system.revision.wrapping_add(1);
        let changes = {
            let mut shared = self.shared.write();
            if self.state.configuration.config.mpris_enabled
                && !shared.configuration.config.mpris_enabled
            {
                self.state.mpris_enable_generation =
                    self.state.mpris_enable_generation.wrapping_add(1);
            }
            shared.mpris_enable_generation = self.state.mpris_enable_generation;
            let changed = self.state.changed_sections(&shared);
            let changes = PublicationChanges::between(&self.state, &shared, changed);
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
            if changed.configuration {
                revisions.configuration = revisions.configuration.wrapping_add(1);
            }
            if changed.processing {
                revisions.processing = revisions.processing.wrapping_add(1);
            }
            if changed.playback {
                shared.playback = self.state.playback.clone();
                shared.source_generation = self.state.source_generation;
                shared.current_track = self.state.current_track.clone();
                shared.processing_audition = self.state.processing_audition;
                shared.processing_selection = self.state.processing_selection;
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
                // Publication advances even without a System section change.
                shared.system.revision = self.state.system.revision;
            }
            if changed.processing {
                shared.processing = self.state.processing.clone();
            }
            if changed.configuration {
                shared.configuration = self.state.configuration.clone();
            }
            shared.revisions = *revisions;
            changes
        };
        self.flush_task_completions();
        if !changes.sections.is_empty() {
            self.revision_updates.write().retain(|subscriber| {
                !subscriber.sender.is_closed()
                    && (!changes.includes(subscriber.interest)
                        || !matches!(
                            subscriber.sender.try_send(()),
                            Err(mpsc::TrySendError::Closed(_))
                        ))
            });
        }
        #[cfg(test)]
        if let Some(observer) = &self.publication_observer {
            observer();
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
        self.read_query(query)
    }

    #[cfg(test)]
    pub(in crate::core) fn query_response(&self, query: &Query) -> QueryResponse {
        self.read_query(query)
    }

    fn read_query(&self, query: &Query) -> QueryResponse {
        let result = match query {
            Query::ProcessingTask { id } => Some(Ok(ViewResponse::ProcessingTask(
                self.processing_status(*id).map(Box::new),
            ))),
            Query::RelocationTask { id } => Some(
                self.relocation_status_ref(*id)
                    .map(task_wait::TaskStatusRef::status)
                    .cloned()
                    .context("Unknown or expired relocation task")
                    .map(ViewResponse::RelocationTask)
                    .map_err(Into::into),
            ),
            Query::ScanTask { id } => Some(
                self.scan_status_ref(*id)
                    .map(task_wait::TaskStatusRef::status)
                    .cloned()
                    .context("Unknown or expired scan task")
                    .map(ViewResponse::ScanTask)
                    .map_err(Into::into),
            ),
            _ => None,
        };
        if let Some(result) = result {
            QueryResponse {
                revisions: self.state.query_revisions_for(query),
                result,
            }
        } else {
            query_worker::read_snapshot(
                &self.store,
                &self.state,
                query,
                self.database_maintenance_active(),
                self.preferences.settings(),
            )
        }
    }

    #[cfg(test)]
    pub(in crate::core) fn view(&self, query: &Query) -> Result<ViewResponse, QueryError> {
        self.query_response(query).result
    }
}
