use super::*;

impl Core {
    pub(in crate::core) fn run(&mut self, requests: Receiver<Request>) {
        let config_ticks = crossbeam_channel::tick(Duration::from_millis(100));
        loop {
            select! {
                recv(requests) -> request => {
                    let Ok(request) = request else { break; };
                    let shutdown = matches!(&request.command, Command::Shutdown);
                    let is_view = matches!(
                        &request.command,
                        Command::LibraryPage { .. }
                            | Command::TrackPage { .. }
                            | Command::DirectoryPage { .. }
                            | Command::PlaylistSummaries { .. }
                            | Command::PlaylistEntries { .. }
                            | Command::Track { .. }
                            | Command::LibraryStats
                    );
                    let changed = !matches!(
                        &request.command,
                        Command::Status | Command::Overview | Command::ShowWindow
                    ) && !is_view;
                    let (result, view) = if is_view {
                        match self.view(&request.command) {
                            Ok(view) => (Ok(()), Some(view)),
                            Err(error) => (Err(error), None),
                        }
                    } else {
                        (self.command(request.command), None)
                    };
                    if !is_view && let Err(error) = &result {
                        self.state.system.last_error = Some(format!("{error:#}"));
                    }
                    if changed || (!is_view && result.is_err()) { self.publish(); }
                    if let Some(reply) = request.reply {
                        let value = if request.ack {
                            CoreResponse::Ack(Ack {
                                ok: result.is_ok(),
                                error: result.as_ref().err().map(|e| format!("{e:#}")),
                                revision: self.state.system.revision,
                            })
                        } else {
                            CoreResponse::State(Box::new(StateResponse {
                                ok: result.is_ok(),
                                error: result.err().map(|e| format!("{e:#}")),
                                state: ClientSnapshot::from_core(&self.state),
                                view,
                            }))
                        };
                        let _ = reply.send(value);
                    }
                    if shutdown { break; }
                }
                recv(self.engine.events) -> event => {
                    let Ok(event) = event else { self.state.system.last_error = Some("Audio worker stopped unexpectedly".into()); self.publish(); break; };
                    if let Err(error) = self.audio_event(event) { self.state.system.last_error = Some(format!("{error:#}")); }
                    self.publish();
                }
                recv(self.scan_rx) -> scan => {
                    if let Ok(scan) = scan {
                        if let Err(error) = self.finish_scan(scan) { self.state.system.last_error = Some(format!("{error:#}")); }
                        self.state.system.scanning = false;
                        self.publish();
                    }
                }
                recv(config_ticks) -> _ => {
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
        for worker in self.scan_workers.drain(..) {
            let _ = worker.join();
        }
        self.state.playback.status = PlaybackStatus::Stopped;
        self.state.system.shutting_down = true;
        self.publish();
        if let Some(callback) = self.gui_opener.read().clone() {
            callback();
        }
    }

    pub(in crate::core) fn publish(&mut self) {
        self.state.system.revision = self.state.system.revision.wrapping_add(1);
        *self.shared.write() = self.state.clone();
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
    pub(in crate::core) fn view(&self, command: &Command) -> Result<ViewResponse> {
        Ok(match command {
            Command::LibraryPage {
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
                (*limit).min(PAGE_SIZE),
            )?),
            Command::TrackPage {
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
                (*limit).min(PAGE_SIZE),
            )?),
            Command::DirectoryPage {
                path,
                offset,
                limit,
            } => ViewResponse::DirectoryPage(self.store.directory_page(
                path,
                *offset,
                (*limit).min(PAGE_SIZE),
            )?),
            Command::PlaylistSummaries { offset, limit } => ViewResponse::PlaylistSummaries(
                self.store
                    .playlist_summary_page(*offset, (*limit).min(PAGE_SIZE))?,
            ),
            Command::PlaylistEntries {
                playlist_id,
                offset,
                limit,
            } => ViewResponse::PlaylistEntries(self.store.playlist_entries_page(
                *playlist_id,
                *offset,
                (*limit).min(PAGE_SIZE),
            )?),
            Command::Track { track_id } => ViewResponse::Track(self.store.track(*track_id)?),
            Command::LibraryStats => ViewResponse::LibraryStats(self.store.library_stats()?),
            _ => bail!("Not a view command"),
        })
    }
}
