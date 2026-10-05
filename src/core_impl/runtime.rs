use super::*;

impl Core {
    pub(in crate::core) fn run(&mut self, requests: Receiver<Request>) {
        let config_ticks = crossbeam_channel::tick(Duration::from_millis(100));
        loop {
            select! {
                recv(requests) -> request => {
                    let Ok(request) = request else { break; };
                    let shutdown = matches!(request.command, Command::Shutdown);
                    let changed = !matches!(request.command, Command::Status | Command::Overview | Command::ShowWindow);
                    let result = self.command(request.command);
                    if let Err(error) = &result { self.state.system.last_error = Some(format!("{error:#}")); }
                    if changed || result.is_err() { self.publish(); }
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
                                state: self.state.clone(),
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
        if let Some(library) = self.library.snapshot() {
            self.state.library.tracks = library;
            self.state.library.revision = self.state.library.revision.wrapping_add(1);
        }
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
    pub(in crate::core) fn save(&mut self, force_config: bool) -> Result<()> {
        let config_changed = self.state.system.config.volume != self.state.playback.volume
            || self.state.system.config.shuffle != self.state.playback.shuffle
            || self.state.system.config.repeat != self.state.playback.repeat
            || self.state.system.config.output_device != self.state.system.selected_device;
        if self.config_dirty
            && config_changed
            && (force_config || self.config_last_saved.elapsed() >= Duration::from_millis(250))
        {
            let mut config = self.state.system.config.as_ref().clone();
            config.volume = self.state.playback.volume;
            config.shuffle = self.state.playback.shuffle;
            config.repeat = self.state.playback.repeat;
            config
                .output_device
                .clone_from(&self.state.system.selected_device);
            config.save(&self.state.system.config_path)?;
            self.state.system.config = Arc::new(config);
            self.config_last_saved = Instant::now();
            self.config_dirty = false;
        } else if !config_changed {
            self.config_dirty = false;
        }
        if self.queue_dirty {
            let saved = Saved {
                queue: self.state.queue.entries.as_ref().clone(),
                current: self.state.queue.current_id,
            };
            self.store
                .set_setting("playback", &serde_json::to_string(&saved)?)?;
            self.queue_dirty = false;
        }
        Ok(())
    }
    pub(in crate::core) fn reload_library(&mut self, structure_changed: bool) -> Result<()> {
        let tracks = self.store.tracks()?;
        self.state.library.tracks = self.library.replace(tracks);
        self.state.library.revision = self.state.library.revision.wrapping_add(1);
        if structure_changed {
            self.state.library.structure_revision =
                self.state.library.structure_revision.wrapping_add(1);
        }
        Ok(())
    }

    pub(in crate::core) fn update_track_stats(
        &mut self,
        track_id: i64,
        played_at: Option<i64>,
        increment_play_count: bool,
    ) -> Result<()> {
        self.library.update(track_id, |track| {
            if let Some(played_at) = played_at {
                track.last_played = Some(played_at);
            }
            if increment_play_count {
                track.play_count = track.play_count.saturating_add(1);
            }
        })
    }
    pub(in crate::core) fn reload(&mut self, structure_changed: bool) -> Result<()> {
        self.reload_library(structure_changed)?;
        self.state.library.playlists = Arc::new(self.store.playlists()?);
        self.state.library.history = Arc::new(self.store.history(200)?);
        Ok(())
    }
    pub(in crate::core) fn audio(&self, command: AudioCommand) -> Result<()> {
        self.engine
            .commands
            .send(command)
            .context("Audio worker unavailable")
    }
    pub(in crate::core) fn track(&self, id: i64) -> Result<&Track> {
        let index = self
            .library
            .tracks()
            .binary_search_by_key(&id, |track| track.id)
            .ok()
            .context("Track not found")?;
        Ok(&self.library.tracks()[index])
    }
}
