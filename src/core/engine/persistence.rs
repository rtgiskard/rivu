use super::*;

impl Core {
    pub(in crate::core) fn update_gui_config(&mut self, change: GuiConfigChange) -> Result<()> {
        if let GuiConfigChange::Settings(mut config) = change {
            // These controls are not edited by the Settings dialog. Resolve at
            // execution, not before enqueueing behind pending playback commands.
            config.shuffle = self.state.playback.shuffle;
            config.repeat = self.state.playback.repeat;
            return self.command(Command::Configure { config });
        }
        let mut config = self.state.system.config.as_ref().clone();
        let still_mode = matches!(change, GuiConfigChange::ToggleStillMode);
        match change {
            GuiConfigChange::ToggleStillMode => config.still_mode = !config.still_mode,
            GuiConfigChange::ToggleWaveformSeek => {
                config.waveform_seek_enabled = !config.waveform_seek_enabled
            }
            GuiConfigChange::AddLibraryRoot(path) => {
                let path = library::logical_path(&path)?;
                if config.library_roots.contains(&path) {
                    return Ok(());
                }
                config.library_roots.push(path);
            }
            GuiConfigChange::Settings(_) => unreachable!(),
        }
        config.volume = self.state.playback.volume;
        config.shuffle = self.state.playback.shuffle;
        config.repeat = self.state.playback.repeat;
        config
            .output_device
            .clone_from(&self.state.system.selected_device);
        config.save(&self.state.system.config_path)?;
        if still_mode {
            self.audio(AudioCommand::StillMode(config.still_mode))?;
        }
        self.state.system.config = Arc::new(config);
        self.config_dirty = false;
        self.config_last_saved = Instant::now();
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
            let saved = SavedRef {
                queue: self.state.queue.entries.as_slice(),
                current: self.state.queue.current_id,
            };
            self.store
                .set_setting("playback", &serde_json::to_string(&saved)?)?;
            self.queue_dirty = false;
        }
        Ok(())
    }
    pub(in crate::core) fn reload_library(&mut self, structure_changed: bool) -> Result<()> {
        self.state.library.track_total = self.store.track_count()?;
        self.state.library.revision = self.state.library.revision.wrapping_add(1);
        if structure_changed {
            self.state.library.structure_revision =
                self.state.library.structure_revision.wrapping_add(1);
        }
        Ok(())
    }

    pub(in crate::core) fn update_track_stats(&mut self, track_id: i64) -> Result<()> {
        let track = self.store.track(track_id)?.context("Track not found")?;
        if let Ok(index) = self
            .state
            .queue
            .tracks
            .binary_search_by_key(&track_id, |row| row.id)
        {
            let row = &self.state.queue.tracks[index];
            if row.title != track.title
                || row.artist != track.artist
                || row.album != track.album
                || row.duration != track.duration
                || row.favorite != track.favorite
                || row.missing != track.missing
                || row.play_count != track.play_count
            {
                let row = &mut Arc::make_mut(&mut self.state.queue.tracks)[index];
                row.title.clone_from(&track.title);
                row.artist.clone_from(&track.artist);
                row.album.clone_from(&track.album);
                row.duration = track.duration;
                row.favorite = track.favorite;
                row.missing = track.missing;
                row.play_count = track.play_count;
            }
        }
        if self
            .state
            .current_track
            .as_ref()
            .is_some_and(|current| current.id == track_id && current.as_ref() != &track)
        {
            self.state.current_track = Some(Arc::new(track));
        }
        self.state.library.revision = self.state.library.revision.wrapping_add(1);
        Ok(())
    }

    pub(in crate::core) fn reload_playlists(&mut self) -> Result<()> {
        self.state.library.playlist_total = self.store.playlist_summary_page(0, 0)?.total;
        self.state.library.playlist_revision = self.state.library.playlist_revision.wrapping_add(1);
        Ok(())
    }

    pub(in crate::core) fn reload_catalog(&mut self, structure_changed: bool) -> Result<()> {
        self.reload_library(structure_changed)?;
        self.reload_playlists()?;
        self.state.library.history = Arc::new(self.store.history(MAX_QUERY_ROWS)?);
        Ok(())
    }

    pub(in crate::core) fn reload(&mut self, structure_changed: bool) -> Result<()> {
        self.reload_catalog(structure_changed)?;
        self.refresh_queue_tracks()
    }
}
