use super::*;
use anyhow::ensure;

impl Core {
    pub(in crate::core) fn command(&mut self, command: Command) -> Result<()> {
        // Scan acceptance is the asynchronous commit boundary. scan() flushes
        // pending state before spawning; never turn acceptance into an error later.
        if matches!(
            &command,
            Command::Scan { .. } | Command::ImportPlaylist { .. } | Command::RelocateTrack { .. }
        ) {
            return self.apply_command(command);
        }
        // Queue edits commit within their SQL operation; persist the current
        // occurrence even if a later audio load fails after replacing the queue.
        let result = self.apply_command(command);
        let saved = self.save(false);
        result.and(saved)
    }

    fn apply_command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Scan { paths, force } => return self.scan(paths, None, force),
            Command::CancelScan { id } => self.cancel_scan(id)?,
            Command::RelocateTrack {
                source_id,
                target_id,
            } => return self.relocate_track(source_id, target_id),
            Command::CancelRelocation { id } => self.cancel_relocation(id)?,
            Command::KeepTrackIndependent { track_id } => {
                if self.store.keep_independent(track_id)? {
                    self.state.library.revision = self.state.library.revision.wrapping_add(1);
                }
            }
            Command::Play { track_id } => {
                self.track(track_id)?;
                let existing = self.store.queued_track(track_id)?;
                let id = if let Some(id) = existing {
                    id
                } else {
                    self.enqueue(&[track_id])?;
                    *self.queue_ids.last().unwrap()
                };
                self.start(id, true)?;
            }
            Command::PlayQueue { queue_id } => self.start(queue_id, true)?,
            Command::PlayPlaylist { playlist_id } => {
                let total = self.store.playlist_entries_page(playlist_id, 0, 0)?.total;
                ensure!(total > 0, "Playlist is empty");
                self.stop()?;
                self.store.replace_queue_from_playlist(playlist_id)?;
                self.state.current_track = None;
                self.played.clear();
                self.played_cursor = 0;
                self.reload_queue()?;
                self.start(self.queue_ids[0], true)?;
            }
            Command::Resume => {
                if self.state.playback.status == PlaybackStatus::Stopped {
                    let id = self
                        .state
                        .queue
                        .current_id
                        .or_else(|| self.queue_ids.first().copied())
                        .context("Queue is empty")?;
                    self.start(id, true)?;
                } else {
                    self.audio(AudioCommand::Pause(false))?;
                    if let Some(playback) = self.playback.as_mut() {
                        // A resumed session may continue with a larger cumulative
                        // backend counter; release the old timestamp bound.
                        playback.paused_at = None;
                    }
                    self.state.playback.status = PlaybackStatus::Playing;
                }
            }
            Command::Pause => {
                if self.state.playback.status == PlaybackStatus::Playing {
                    self.audio(AudioCommand::Pause(true))?;
                    if let Some(playback) = self.playback.as_mut() {
                        // Capture the wall-clock boundary once after Pause is accepted.
                        playback.paused_at.get_or_insert(now());
                    }
                    self.state.playback.status = PlaybackStatus::Paused;
                }
            }
            Command::Toggle => {
                return self.command(if self.state.playback.status == PlaybackStatus::Playing {
                    Command::Pause
                } else {
                    Command::Resume
                });
            }
            Command::Stop => self.stop()?,
            Command::Next => self.advance(false)?,
            Command::Previous => self.previous()?,
            Command::Seek { seconds } => self.seek(seconds)?,
            Command::Volume { value } => {
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    bail!("Volume must be between 0 and 1");
                }
                self.audio(AudioCommand::Volume(value))?;
                self.state.playback.volume = value;
            }
            Command::Enqueue { track_ids } => self.enqueue(&track_ids)?,
            Command::EnqueueSources {
                directories,
                track_ids,
            } => {
                self.store.enqueue_sources(&directories, &track_ids)?;
                self.append_queue_ids()?;
            }
            Command::RemoveQueue { queue_id } => self.remove_queue_entries(&[queue_id])?,
            Command::RemoveQueueEntries { queue_ids } => self.remove_queue_entries(&queue_ids)?,
            Command::MoveQueue { queue_id, index } => {
                ensure!(
                    index < self.queue_ids.len(),
                    "Queue target position out of range"
                );
                self.move_queue_entries(&[queue_id], index)?;
            }
            Command::MoveQueueEntries { queue_ids, index } => {
                self.move_queue_entries(&queue_ids, index)?;
            }
            Command::ClearQueue => {
                self.stop()?;
                self.store.clear_queue()?;
                self.reload_queue()?;
                self.state.current_track = None;
                self.played.clear();
                self.played_cursor = 0;
            }
            Command::RandomizeQueue => self.randomize_queue(&mut rand::rng())?,
            Command::DeduplicateQueue => self.deduplicate_queue()?,
            Command::Shuffle { enabled } => {
                if self.state.playback.shuffle != enabled {
                    self.state.playback.shuffle = enabled;
                    self.played.clear();
                    self.played_cursor = 0;
                    if let Some(id) = self.state.queue.current_id {
                        self.played.push(id);
                        self.played_cursor = 1;
                    }
                }
            }
            Command::Repeat { mode } => self.state.playback.repeat = mode,
            Command::CreatePlaylist { name } => self.create_playlist(name)?,
            Command::CreatePlaylistWithTracks { name, track_ids } => {
                self.create_playlist_with_tracks(name, track_ids)?;
            }
            Command::CreatePlaylistFromQueue { name } => {
                self.store.create_playlist_from_queue(&name)?;
                self.reload_playlists()?;
            }
            Command::RenamePlaylist { playlist_id, name } => {
                self.rename_playlist(playlist_id, name)?;
            }
            Command::DeletePlaylist { playlist_id } => self.delete_playlist(playlist_id)?,
            Command::AddPlaylist {
                playlist_id,
                track_ids,
            } => self.add_playlist(playlist_id, track_ids)?,
            Command::AddPlaylistSources {
                playlist_id,
                directories,
                track_ids,
            } => {
                if self
                    .store
                    .add_playlist_sources(playlist_id, &directories, &track_ids)?
                {
                    self.reload_playlists()?;
                }
            }
            Command::RemovePlaylistEntry { entry_id } => self.remove_playlist_entry(entry_id)?,
            Command::MovePlaylistEntry { entry_id, index } => {
                self.move_playlist_entry(entry_id, index)?;
            }
            Command::ImportPlaylist { path, name } => {
                if self.active_scan.is_some() {
                    bail!("A library scan is already running");
                }
                let items = library::import_playlist(&path)?;
                let paths = items
                    .iter()
                    .map(|item| item.path.clone())
                    .collect::<Vec<_>>();
                let name = name.unwrap_or_else(|| {
                    path.file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                });
                return self.scan(paths, Some((name, items)), false);
            }
            Command::ExportPlaylist { playlist_id, path } => {
                self.export_playlist(playlist_id, &path)?;
            }
            Command::EditTrack {
                track_id,
                title,
                artist,
                album,
                decoder_hint,
            } => {
                if self.store.edit_track(
                    track_id,
                    title.as_deref(),
                    artist.as_deref(),
                    album.as_deref(),
                    decoder_hint,
                )? {
                    self.refresh_affected_queue_tracks(&[track_id])?;
                    if title.is_some()
                        && self
                            .state
                            .library
                            .history
                            .iter()
                            .any(|entry| entry.track_id == track_id)
                    {
                        let track = self.store.track(track_id)?.context("Track not found")?;
                        self.refresh_history_track(&track);
                    }
                    self.state.library.revision = self.state.library.revision.wrapping_add(1);
                }
            }
            Command::RemoveTracks { mut track_ids } => {
                track_ids.sort_unstable();
                track_ids.dedup();
                let removal = self.store.prepare_track_removal(&track_ids)?;
                if self
                    .state
                    .current_track()
                    .is_some_and(|track| track_ids.contains(&track.id))
                {
                    self.stop()?;
                }
                // Keep the selection on failure; only a committed cascade clears it.
                self.store.remove_prepared_tracks(removal)?;
                self.reload_queue()?;
                self.refresh_queue_tracks()?;
                self.reload_catalog(true)?;
            }
            Command::SetFavorite {
                track_ids,
                favorite,
            } => {
                if self.store.set_favorite(&track_ids, favorite)? {
                    self.state.library.revision = self.state.library.revision.wrapping_add(1);
                    self.refresh_affected_queue_tracks(&track_ids)?;
                }
            }
            Command::RemoveMissingTracks => {
                if self
                    .state
                    .current_track()
                    .is_some_and(|track| track.missing)
                {
                    self.stop()?;
                }
                // A stopped decoder does not imply the database deletion succeeded.
                self.store.remove_missing_tracks()?;
                self.reload_queue()?;
                self.refresh_queue_tracks()?;
                self.reload_catalog(true)?;
            }
            Command::Device { name } => {
                if let Some(name) = &name
                    && !self.state.system.devices.contains(name)
                {
                    bail!("Output device not found: {name}");
                }
                self.audio(AudioCommand::OutputSettings {
                    device: name.clone(),
                    auto_mix: self.state.system.config.pipewire_auto_mix,
                })?;
                self.state.system.selected_device = name;
            }
            Command::Analysis { enabled } => {
                self.engine.analysis_control.set_display_enabled(enabled);
                return Ok(());
            }
            Command::DismissError => {
                self.state.system.last_error = None;
                return Ok(());
            }
            Command::DismissScanResult => {
                if !self.state.system.scanning {
                    self.state.system.scan_progress = None;
                    self.state.system.scan_message.clear();
                }
                return Ok(());
            }
            Command::Configure { mut config } => {
                config.validate()?;
                config.library_roots = config
                    .library_roots
                    .iter()
                    .map(|root| library::logical_path(root))
                    .collect::<Result<_>>()?;
                if let Some(device) = &config.output_device
                    && !self.state.system.devices.contains(device)
                {
                    bail!("Output device not found: {device}");
                }
                let ffmpeg_status = if !config.ffmpeg_enabled {
                    "disabled".to_owned()
                } else {
                    audio::ffmpeg_status().unwrap_or_else(|error| format!("unavailable: {error:#}"))
                };
                if let Some(logging) = self.logging.clone() {
                    logging.reconfigure_with(&config, || self.write_config(&config))?;
                } else {
                    self.write_config(&config)?;
                }
                // Persistence and logging have committed the desired settings.
                // Audio delivery may partially succeed; never roll this state back.
                self.store.set_track_cache_page_size(config.page_size);
                self.state.system.selected_device = config.output_device.clone();
                self.state.playback.volume = config.volume;
                if self.state.playback.shuffle != config.shuffle {
                    self.played.clear();
                    self.played_cursor = 0;
                }
                self.state.playback.shuffle = config.shuffle;
                self.state.playback.repeat = config.repeat;
                self.state.system.ffmpeg_status = ffmpeg_status;
                self.state.system.config = Arc::new(config);
                tracing::info!("configuration_updated");
                let config = &self.state.system.config;
                (|| -> Result<()> {
                    self.audio(AudioCommand::StillMode(config.still_mode))?;
                    self.audio(AudioCommand::MediaReadBuffer(config.media_read_buffer_mb))?;
                    self.audio(AudioCommand::Volume(config.volume))?;
                    self.audio(AudioCommand::AnalysisSettings(config.as_ref().into()))?;
                    self.audio(AudioCommand::FfmpegEnabled(config.ffmpeg_enabled))?;
                    self.audio(AudioCommand::OutputSettings {
                        device: config.output_device.clone(),
                        auto_mix: config.pipewire_auto_mix,
                    })
                })()
                .context("Configuration saved, but audio application failed")?;
            }
            Command::MprisStatus { status } => {
                self.state.system.mpris_status = status;
                return Ok(());
            }
            Command::SeekQueue { queue_id, seconds } => {
                if self.state.queue.current_id == Some(queue_id) {
                    self.seek(seconds)?;
                }
                return Ok(());
            }
            Command::OptimizeDatabase => {
                self.state.system.database_optimization = None;
                if self.state.playback.status != PlaybackStatus::Stopped
                    || self.state.system.scanning
                {
                    bail!("Database optimization requires stopped playback and no active scan");
                }
                self.save(false)?;
                self.state.system.database_optimization = Some(self.store.optimize()?);
                // Current state is already persisted; finalization does not rewrite queue rows.
                return Ok(());
            }
        }
        Ok(())
    }
}
