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
    Shutdown(RuntimeControl),
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

    async fn next_event(
        &mut self,
        requests: &mut mpsc::Receiver<Request>,
        shutdown: &mut mpsc::Receiver<()>,
        control: &RuntimeControl,
    ) -> Option<BusinessEvent> {
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
        let awaiting_ffmpeg = self.state.system.ffmpeg_status == "not loaded";
        // Consume one event per iteration, with randomized selection to avoid
        // permanent priority for a hot input.
        Some(futures::select! {
            _ = shutdown.recv().fuse() => BusinessEvent::Shutdown(control.clone()),
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
            Operation::WaveformSnapshot(reply) => {
                let _ = self.engine.send(AudioCommand::WaveformSnapshot(reply));
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
                let result = self.command(command);
                if let Err(error) = &result
                    && (!library_owned || request.reply.is_none())
                {
                    self.state.system.last_error = Some(format!("{error:#}"));
                }
                self.publish();
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
                let processing_id = result.as_ref().ok().copied().flatten();
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
                        ..Ack::from_result(result.map(|_| ()), self.state.system.revision)
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
        // Every persistence domain gets its final attempt, even after a failure.
        let deferred = self
            .flush_deferred_playback()
            .context("Saving deferred playback statistics");
        let playback = self.stop().context("Saving play count");
        let queue = self.save_queue().context("Saving queue");
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
        self.state.source_generation = self.generation;
        self.state.system.revision = self.state.system.revision.wrapping_add(1);
        let revisions_changed = {
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
                shared.source_generation = self.state.source_generation;
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
            let revisions_changed = shared.revisions != *revisions;
            shared.revisions = *revisions;
            revisions_changed
        };
        self.flush_task_completions();
        if revisions_changed {
            self.revision_updates.write().retain(|sender| {
                !matches!(sender.try_send(()), Err(mpsc::TrySendError::Closed(_)))
            });
        }
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
                    .context("Unknown or expired relocation task")
                    .map(ViewResponse::RelocationTask)
                    .map_err(Into::into),
            ),
            Query::ScanTask { id } => Some(
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
            )
        }
    }

    #[cfg(test)]
    pub(in crate::core) fn view(&self, query: &Query) -> Result<ViewResponse, QueryError> {
        self.query_response(query).result
    }
}
