use super::*;
use anyhow::ensure;

impl Core {
    pub(in crate::core) fn command(&mut self, command: Command) -> Result<()> {
        let persist_queue = matches!(
            &command,
            Command::Enqueue { .. }
                | Command::EnqueueSources { .. }
                | Command::RemoveQueueEntries { .. }
                | Command::MoveQueue { .. }
                | Command::MoveQueueEntries { .. }
                | Command::ClearQueue
                | Command::RandomizeQueue
                | Command::DeduplicateQueue
                | Command::RemoveTracks { .. }
                | Command::RemoveMissingTracks
        );
        let persist_config = matches!(
            &command,
            Command::Volume { .. }
                | Command::Shuffle { .. }
                | Command::Repeat { .. }
                | Command::Device { .. }
                | Command::Configure { .. }
        );
        match command {
            Command::Scan { paths, force } => self.scan(paths, None, force)?,
            Command::Play { track_id } => {
                self.track(track_id)?;
                let existing = self
                    .state
                    .queue
                    .entries
                    .iter()
                    .find(|q| q.track_id == track_id)
                    .map(|q| q.id);
                let id = if let Some(id) = existing {
                    id
                } else {
                    self.enqueue(&[track_id])?;
                    self.state.queue.entries.last().unwrap().id
                };
                self.start(id, true)?;
            }
            Command::PlayQueue { queue_id } => self.start(queue_id, true)?,
            Command::PlayPlaylist { playlist_id } => {
                let total = self.store.playlist_entries_page(playlist_id, 0, 0)?.total;
                ensure!(
                    total <= self.state.system.config.queue_limit as usize,
                    "Playlist contains {total} entries, exceeding the queue limit of {}",
                    self.state.system.config.queue_limit
                );
                let ids = self.store.playlist_track_ids(
                    playlist_id,
                    self.state.system.config.queue_limit as usize,
                )?;
                if ids.is_empty() {
                    bail!("Playlist is empty");
                }
                self.stop()?;
                self.state.queue.entries = Arc::new(Vec::new());
                self.state.queue.tracks = Arc::new(Vec::new());
                self.state.queue.current_id = None;
                self.state.current_track = None;
                self.played.clear();
                self.played_cursor = 0;
                self.enqueue(&ids)?;
                self.start(self.state.queue.entries[0].id, true)?;
            }
            Command::Resume => {
                if self.state.playback.status == PlaybackStatus::Stopped {
                    let id = self
                        .state
                        .queue
                        .current_id
                        .or_else(|| self.state.queue.entries.first().map(|q| q.id))
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
                let remaining = self
                    .state
                    .system
                    .config
                    .queue_limit
                    .saturating_sub(self.state.queue.entries.len() as u32)
                    as usize;
                let ids = self
                    .store
                    .source_track_ids(&directories, &track_ids, remaining)?;
                self.enqueue(&ids)?;
            }
            Command::RemoveQueue { queue_id } => {
                if !self.state.queue.entries.iter().any(|q| q.id == queue_id) {
                    bail!("Queue entry not found");
                }
                if self.state.queue.current_id == Some(queue_id) {
                    self.stop()?;
                    self.state.queue.current_id = None;
                }
                Arc::make_mut(&mut self.state.queue.entries).retain(|q| q.id != queue_id);
                self.refresh_queue_rows()?;
                self.prune_queue_history();
            }
            Command::RemoveQueueEntries { queue_ids } => self.remove_queue_entries(&queue_ids)?,
            Command::MoveQueue { queue_id, index } => {
                let old = self
                    .state
                    .queue
                    .entries
                    .iter()
                    .position(|q| q.id == queue_id)
                    .context("Queue entry not found")?;
                ensure!(
                    index < self.state.queue.entries.len(),
                    "Queue target position out of range"
                );
                let queue = Arc::make_mut(&mut self.state.queue.entries);
                let entry = queue.remove(old);
                queue.insert(index, entry);
            }
            Command::MoveQueueEntries { queue_ids, index } => {
                self.move_queue_entries(&queue_ids, index)?;
            }
            Command::ClearQueue => {
                self.stop()?;
                self.state.queue.entries = Arc::new(Vec::new());
                self.state.queue.current_id = None;
                self.state.queue.tracks = Arc::new(Vec::new());
                self.state.current_track = None;
                self.played.clear();
                self.played_cursor = 0;
                self.shuffle_bag.clear();
            }
            Command::RandomizeQueue => self.randomize_queue(&mut rand::rng()),
            Command::DeduplicateQueue => self.deduplicate_queue(),
            Command::Shuffle { enabled } => {
                self.state.playback.shuffle = enabled;
                self.shuffle_bag.clear();
                self.played.clear();
                self.played_cursor = 0;
                if let Some(id) = self.state.queue.current_id {
                    self.played.push(id);
                    self.played_cursor = 1;
                }
            }
            Command::Repeat { mode } => self.state.playback.repeat = mode,
            Command::CreatePlaylist { name } => self.create_playlist(name)?,
            Command::CreatePlaylistWithTracks { name, track_ids } => {
                self.create_playlist_with_tracks(name, track_ids)?;
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
                self.store
                    .add_playlist_sources(playlist_id, &directories, &track_ids)?;
                self.reload(true)?;
            }
            Command::RemovePlaylistEntry { entry_id } => self.remove_playlist_entry(entry_id)?,
            Command::MovePlaylistEntry { entry_id, index } => {
                self.move_playlist_entry(entry_id, index)?;
            }
            Command::ImportPlaylist { path, name } => {
                if self.state.system.scanning {
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
                self.scan(paths, Some((name, items)), false)?;
            }
            Command::ExportPlaylist { playlist_id, path } => {
                self.export_playlist(playlist_id, &path)?;
            }
            Command::EditTrack {
                track_id,
                title,
                artist,
                album,
            } => {
                self.store.edit_track(track_id, &title, &artist, &album)?;
                self.refresh_queue_tracks()?;
                self.state.library.revision = self.state.library.revision.wrapping_add(1);
            }
            Command::RemoveTracks { track_ids } => {
                for id in &track_ids {
                    self.track(*id)?;
                }
                if self
                    .state
                    .current_track()
                    .is_some_and(|track| track_ids.contains(&track.id))
                {
                    self.stop()?;
                    self.state.queue.current_id = None;
                    self.state.current_track = None;
                }
                self.store.remove_tracks(&track_ids)?;
                Arc::make_mut(&mut self.state.queue.entries)
                    .retain(|q| !track_ids.contains(&q.track_id));
                self.refresh_queue_tracks()?;
                self.prune_queue_history();
                self.reload(true)?;
            }
            Command::SetFavorite {
                track_ids,
                favorite,
            } => {
                self.store.set_favorite(&track_ids, favorite)?;
                self.state.library.revision = self.state.library.revision.wrapping_add(1);
                self.refresh_queue_tracks()?;
            }
            Command::RemoveMissingTracks => {
                let mut missing_ids = self
                    .state
                    .queue
                    .tracks
                    .iter()
                    .filter(|track| track.missing)
                    .map(|track| track.id)
                    .collect::<Vec<_>>();
                missing_ids.sort_unstable();
                if self
                    .state
                    .current_track()
                    .is_some_and(|track| track.missing)
                {
                    self.stop()?;
                    self.state.queue.current_id = None;
                    self.state.current_track = None;
                }
                self.store.remove_missing_tracks()?;
                Arc::make_mut(&mut self.state.queue.entries)
                    .retain(|entry| missing_ids.binary_search(&entry.track_id).is_err());
                self.refresh_queue_tracks()?;
                self.prune_queue_history();
                self.reload(true)?;
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
                self.audio(AudioCommand::Analysis(enabled))?;
                return Ok(());
            }
            Command::DismissError => {
                self.state.system.last_error = None;
                return Ok(());
            }
            Command::Configure { mut config } => {
                config.validate()?;
                config.library_roots = config
                    .library_roots
                    .iter()
                    .map(|root| library::logical_path(root))
                    .collect::<Result<_>>()?;
                ensure!(
                    config.queue_limit as usize >= self.state.queue.entries.len(),
                    "queue_limit cannot be lower than current queue length ({})",
                    self.state.queue.entries.len()
                );
                if let Some(device) = &config.output_device
                    && !self.state.system.devices.contains(device)
                {
                    bail!("Output device not found: {device}");
                }
                let enabling_ffmpeg =
                    config.ffmpeg_enabled && !self.state.system.config.ffmpeg_enabled;
                let ffmpeg_status = if !config.ffmpeg_enabled {
                    "disabled".to_owned()
                } else if enabling_ffmpeg {
                    audio::ffmpeg_status()
                        .context("FFmpeg extension audio decoding is unavailable")?
                } else {
                    self.state.system.ffmpeg_status.clone()
                };
                crate::logging::reconfigure(&config)?;
                config.save(&self.state.system.config_path)?;
                self.audio(AudioCommand::MediaReadBuffer(config.media_read_buffer_mb))?;
                self.audio(AudioCommand::Volume(config.volume))?;
                self.audio(AudioCommand::AnalysisSettings((&config).into()))?;
                self.audio(AudioCommand::FfmpegEnabled(config.ffmpeg_enabled))?;
                self.audio(AudioCommand::OutputSettings {
                    device: config.output_device.clone(),
                    auto_mix: config.pipewire_auto_mix,
                })?;
                self.store.set_track_cache_page_size(config.page_size);
                self.state.system.selected_device = config.output_device.clone();
                self.state.playback.volume = config.volume;
                if self.state.playback.shuffle != config.shuffle {
                    self.shuffle_bag.clear();
                    self.played.clear();
                    self.played_cursor = 0;
                }
                self.state.playback.shuffle = config.shuffle;
                self.state.playback.repeat = config.repeat;
                self.state.system.ffmpeg_status = ffmpeg_status;
                self.state.system.config = Arc::new(config);
                tracing::info!("configuration_updated");
            }
            Command::ShowWindow => return self.request_raise(),
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
                self.state.system.database_optimization = Some(self.store.optimize()?);
                // Saving playback here would immediately create new WAL pages
                // after maintenance has truncated them.
                return Ok(());
            }
            Command::Shutdown => {
                self.state.system.shutting_down = true;
                self.stop()?;
            }
        }
        self.queue_dirty |= persist_queue;
        self.config_dirty |= persist_config;
        self.save(false)
    }
}
