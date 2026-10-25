use super::*;
use anyhow::{bail, ensure};

impl Core {
    pub(in crate::core) fn command(&mut self, command: Command) -> Result<()> {
        if self.database_maintenance_active() && self.maintenance_control(&command)? {
            return Ok(());
        }
        self.apply_command(command)
    }

    fn apply_command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::PlayAlbum { key } => {
                ensure!(
                    self.store.album_tracks_page(&key, None, 0, 0)?.total > 0,
                    "Album is empty"
                );
                self.stop()?;
                self.store.queue_album(&key, true)?;
                self.state.current_track = None;
                self.played.clear();
                self.played_cursor = 0;
                self.reload_queue()?;
                self.start(self.queue_ids[0], true)?;
            }
            Command::EnqueueAlbum { key } => {
                self.store.queue_album(&key, false)?;
                self.append_queue_ids()?;
            }
            Command::Scan { paths, force } => return self.scan(paths, force),
            Command::CheckOtherTracks { force } => {
                self.start_other_tracks_check(force)?;
            }
            Command::QueueExternalPaths {
                paths,
                index,
                expected_revision,
            } => {
                self.start_external_admission(paths, index, expected_revision)?;
            }
            Command::CancelScan { id } => self.cancel_scan(id)?,
            Command::RelocateTrack {
                source_id,
                target_id,
            } => return self.relocate_track(source_id, target_id),
            Command::CancelRelocation { id } => self.cancel_relocation(id)?,
            Command::KeepTrackIndependent { track_id } => self.keep_track_independent(track_id)?,
            Command::ProcessingBypass => self.processing_bypass()?,
            Command::EqualizerEdit { edit } => self.equalizer_edit(edit)?,
            Command::DenoiseEdit { patch } => self.denoise_edit(*patch)?,
            Command::NeuralEdit { patch } => self.neural_edit(*patch)?,
            Command::ProcessingGraphEdit { revision, edit } => {
                self.processing_graph_edit(revision, *edit)?
            }
            Command::ProcessingGraphRun {
                track_id,
                target,
                mode,
            } => self.processing_graph_run(track_id, target, mode)?,
            Command::ProcessingGraphAudition { id, node } => {
                self.processing_graph_audition(id, node)?
            }
            Command::ProcessingGraphExport { id, node, path } => {
                self.processing_graph_export(id, node, path)?
            }
            Command::StartProcessing {
                track_id,
                operation,
                profile,
            } => self.start_processing(track_id, operation, profile)?,
            Command::CancelProcessing { id } => self.cancel_processing(id)?,
            Command::AuditionProcessing {
                id,
                mode,
                vocal_gain_db,
                accompaniment_gain_db,
            } => self.audition_processing(id, mode, vocal_gain_db, accompaniment_gain_db)?,
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
                let total = self
                    .store
                    .playlist_entries_page(playlist_id, None, 0, 0)?
                    .total;
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
                    self.engine.send(AudioCommand::Pause(false))?;
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
                    self.engine.send(AudioCommand::Pause(true))?;
                    if let Some(playback) = self.playback.as_mut() {
                        // Capture the wall-clock boundary once after Pause is accepted.
                        playback.paused_at.get_or_insert(now());
                    }
                    self.state.playback.status = PlaybackStatus::Paused;
                }
            }
            Command::Toggle => {
                return self.apply_command(
                    if self.state.playback.status == PlaybackStatus::Playing {
                        Command::Pause
                    } else {
                        Command::Resume
                    },
                );
            }
            Command::Stop => self.stop()?,
            Command::Next => self.advance(false)?,
            Command::Previous => self.previous()?,
            Command::Seek { seconds } => self.seek(seconds)?,
            Command::SeekRelative { queue_id, delta } => {
                ensure!(delta.is_finite(), "Seek delta must be finite");
                if self.state.queue.current_id == Some(queue_id)
                    && self.state.playback.status != PlaybackStatus::Stopped
                {
                    self.seek((self.state.playback.position + delta).max(0.0))?;
                }
                return Ok(());
            }
            Command::Volume { value } => {
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    bail!("Volume must be between 0 and 1");
                }
                self.engine.send(AudioCommand::Volume(value))?;
                self.state.playback.volume = value;
            }
            Command::AdjustVolume { delta } => {
                ensure!(delta.is_finite(), "Volume delta must be finite");
                return self.apply_command(Command::Volume {
                    value: (self.state.playback.volume + delta).clamp(0.0, 1.0),
                });
            }
            Command::Enqueue { track_ids } => self.enqueue(&track_ids)?,
            Command::EnqueueSources {
                other_tracks,
                directories,
                track_ids,
            } => {
                let roots =
                    other_tracks.then_some(self.state.system.config.library_roots.as_slice());
                self.store
                    .enqueue_sources(&directories, &track_ids, roots)?;
                self.append_queue_ids()?;
            }
            Command::InsertQueue {
                track_ids,
                index,
                expected_revision,
            } => {
                self.check_queue_insertion(index, expected_revision)?;
                let inserted =
                    self.store
                        .insert_queue_tracks(&track_ids, index, expected_revision)?;
                self.insert_queue_ids(index, inserted)?;
            }
            Command::InsertQueueSources {
                other_tracks,
                directories,
                track_ids,
                index,
                expected_revision,
            } => {
                self.check_queue_insertion(index, expected_revision)?;
                let inserted = self.store.insert_queue_sources(
                    &directories,
                    &track_ids,
                    other_tracks.then_some(self.state.system.config.library_roots.as_slice()),
                    index,
                    expected_revision,
                )?;
                self.insert_queue_ids(index, inserted)?;
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
            Command::Shuffle { enabled } => self.set_shuffle(enabled),
            Command::Repeat { mode } => self.state.playback.repeat = mode,
            Command::CycleRepeat => {
                self.state.playback.repeat = match self.state.playback.repeat {
                    RepeatMode::Off => RepeatMode::All,
                    RepeatMode::All => RepeatMode::One,
                    RepeatMode::One => RepeatMode::Off,
                };
            }
            Command::ToggleShuffle => self.set_shuffle(!self.state.playback.shuffle),
            Command::CreatePlaylist { name } => self.create_playlist(name)?,
            Command::CreatePlaylistWithTracks { name, track_ids } => {
                self.create_playlist_with_tracks(name, track_ids)?;
            }
            Command::CreatePlaylistFromQueue { name } => {
                let (id, has_members) = self.store.create_playlist_from_queue(&name)?;
                self.playlist_created(id, has_members)?;
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
                other_tracks,
                playlist_id,
                directories,
                track_ids,
            } => {
                if self.store.add_playlist_sources(
                    playlist_id,
                    &directories,
                    &track_ids,
                    other_tracks.then_some(self.state.system.config.library_roots.as_slice()),
                )? {
                    self.invalidate_playlist_members(&[playlist_id]);
                }
            }
            Command::InsertPlaylist {
                playlist_id,
                track_ids,
                index,
                expected_revision,
            } => {
                self.check_playlist_entry_revision(playlist_id, expected_revision)?;
                if self
                    .store
                    .insert_playlist_tracks(playlist_id, &track_ids, index)?
                {
                    self.invalidate_playlist_members(&[playlist_id]);
                }
            }
            Command::InsertPlaylistSources {
                other_tracks,
                playlist_id,
                directories,
                track_ids,
                index,
                expected_revision,
            } => {
                self.check_playlist_entry_revision(playlist_id, expected_revision)?;
                if self.store.insert_playlist_sources(
                    playlist_id,
                    &directories,
                    &track_ids,
                    other_tracks.then_some(self.state.system.config.library_roots.as_slice()),
                    index,
                )? {
                    self.invalidate_playlist_members(&[playlist_id]);
                }
            }
            Command::RemovePlaylistEntry { entry_id } => self.remove_playlist_entry(entry_id)?,
            Command::MovePlaylistEntry { entry_id, index } => {
                self.move_playlist_entry(entry_id, index)?;
            }
            Command::RemovePlaylistEntries {
                playlist_id,
                entry_ids,
                expected_revision,
            } => {
                self.remove_playlist_entries(playlist_id, &entry_ids, expected_revision)?;
            }
            Command::MovePlaylistEntries {
                playlist_id,
                entry_ids,
                before_entry_id,
                after,
                expected_revision,
            } => {
                self.move_playlist_entries(
                    playlist_id,
                    &entry_ids,
                    before_entry_id,
                    after,
                    expected_revision,
                )?;
            }
            Command::ImportPlaylist { path, name } => {
                if self.active_scan.is_some() {
                    bail!("A library scan is already running");
                }
                let name = name.unwrap_or_else(|| {
                    path.file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                });
                Store::validate_playlist_name(&name)?;
                return self.import_playlist(path, name);
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
            } => self.edit_track(track_id, title, artist, album, decoder_hint)?,
            Command::RemoveTracks { track_ids } => self.remove_tracks(track_ids)?,
            Command::SetFavorite {
                track_ids,
                favorite,
            } => self.set_favorite(&track_ids, favorite)?,
            Command::ToggleFavorite { track_ids } => self.toggle_favorite(&track_ids)?,
            Command::RemoveMissingTracks => self.remove_missing_tracks()?,
            Command::Device { name } => {
                let request = self.output_settings_request.wrapping_add(1);
                self.engine.send(AudioCommand::OutputSettings {
                    request,
                    device: name.clone(),
                    auto_mix: self.state.system.config.pipewire_auto_mix,
                })?;
                self.output_settings_request = request;
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
                    self.state.scan_owner = None;
                    self.state.system.scan_progress = None;
                    self.state.system.scan_task_id = None;
                    self.state.system.scan_message.clear();
                }
                return Ok(());
            }
            Command::Configure { config } => self.configure(config)?,
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
            Command::OptimizeDatabase => return self.optimize_database(),
        }
        Ok(())
    }
}
