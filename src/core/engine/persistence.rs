use super::*;

impl Core {
    pub(in crate::core) fn update_gui_config(&mut self, change: GuiConfigChange) -> Result<()> {
        if let GuiConfigChange::Settings(mut config) = change {
            // These controls are not edited by the Settings dialog. Resolve at
            // execution, not before enqueueing behind pending playback commands.
            config.shuffle = self.state.playback.shuffle;
            config.repeat = self.state.playback.repeat;
            config.library_columns = self.state.system.config.library_columns;
            return self.command(Command::Configure { config });
        }
        let mut config = self.state.system.config.as_ref().clone();
        let still_mode = matches!(change, GuiConfigChange::ToggleStillMode);
        match change {
            GuiConfigChange::ToggleStillMode => config.still_mode = !config.still_mode,
            GuiConfigChange::ToggleWaveformSeek => {
                config.waveform_seek_enabled = !config.waveform_seek_enabled
            }
            GuiConfigChange::ToggleLibraryColumn(column) => config.library_columns.toggle(column),
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
        self.write_config(&config)?;
        // A saved change remains the desired configuration even if audio is gone.
        self.state.system.config = Arc::new(config);
        if still_mode {
            self.audio(AudioCommand::StillMode(self.state.system.config.still_mode))
                .context("Configuration saved, but audio application failed")?;
        }
        Ok(())
    }

    fn save_queue(&mut self) -> Result<()> {
        self.store.set_queue_current(self.state.queue.current_id)?;
        self.state.queue = self.store.queue_projection(self.queue_ids.len())?;
        Ok(())
    }

    pub(in crate::core) fn save(&mut self, force_config: bool) -> Result<()> {
        // Independent persistence domains both get a final attempt even if one fails.
        let queue = self.save_queue();
        let config = self.flush_config(force_config).map(|_| ());
        queue.and(config)
    }

    pub(in crate::core) fn write_config(&mut self, config: &Config) -> Result<()> {
        self.config_last_attempt = Instant::now();
        let result = config
            .save(&self.state.system.config_path)
            .context("Saving configuration");
        match &result {
            Ok(()) => {
                if let Some(previous) = self.config_save_error.take()
                    && self.state.system.last_error.as_ref() == Some(&previous)
                {
                    self.state.system.last_error = None;
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                if self.config_save_error.as_ref() != Some(&message) {
                    tracing::error!(error = %message, "configuration_save_failed");
                    self.state.system.last_error = Some(message.clone());
                    self.config_save_error = Some(message);
                }
            }
        }
        result
    }

    pub(in crate::core) fn flush_config(&mut self, force: bool) -> Result<bool> {
        // The saved configuration is the baseline; no command whitelist owns dirty state.
        let changed = self.state.system.config.volume != self.state.playback.volume
            || self.state.system.config.shuffle != self.state.playback.shuffle
            || self.state.system.config.repeat != self.state.playback.repeat
            || self.state.system.config.output_device != self.state.system.selected_device;
        if !changed || (!force && self.config_last_attempt.elapsed() < Duration::from_millis(250)) {
            return Ok(false);
        }
        let mut config = self.state.system.config.as_ref().clone();
        config.volume = self.state.playback.volume;
        config.shuffle = self.state.playback.shuffle;
        config.repeat = self.state.playback.repeat;
        config
            .output_device
            .clone_from(&self.state.system.selected_device);
        self.write_config(&config)?;
        self.state.system.config = Arc::new(config);
        Ok(true)
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

    pub(in crate::core) fn update_track_stats(
        &mut self,
        track_id: i64,
        stats: crate::store::TrackStats,
    ) -> Result<()> {
        if let Some(index) =
            self.state.library.history.iter().position(|entry| {
                entry.track_id == track_id && entry.play_count != stats.play_count
            })
        {
            Arc::make_mut(&mut self.state.library.history)[index].play_count = stats.play_count;
        }
        self.state.queue = self.store.queue_projection(self.queue_ids.len())?;
        if let Some(current) = self.state.current_track.as_mut()
            && current.id == track_id
            && (current.play_count != stats.play_count || current.last_played != stats.last_played)
        {
            let current = Arc::make_mut(current);
            current.play_count = stats.play_count;
            current.last_played = stats.last_played;
        }
        self.state.library.revision = self.state.library.revision.wrapping_add(1);
        Ok(())
    }

    pub(in crate::core) fn refresh_history_track(&mut self, track: &Track) {
        let Some(index) = self
            .state
            .library
            .history
            .iter()
            .position(|entry| entry.track_id == track.id)
        else {
            return;
        };
        let entry = &self.state.library.history[index];
        if entry.title == track.title && entry.play_count == track.play_count {
            return;
        }
        let entry = &mut Arc::make_mut(&mut self.state.library.history)[index];
        entry.title.clone_from(&track.title);
        entry.play_count = track.play_count;
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
