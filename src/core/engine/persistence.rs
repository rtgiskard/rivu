use super::*;

impl Core {
    pub(in crate::core) fn update_gui_config(&mut self, change: GuiConfigChange) -> Result<()> {
        if let GuiConfigChange::Settings(mut config) = change {
            // These controls are not edited by the Settings dialog. Resolve at
            // execution, not before enqueueing behind pending playback commands.
            config.shuffle = self.state.playback.shuffle;
            config.repeat = self.state.playback.repeat;
            config.library_columns = self.state.system.config.library_columns;
            return self.command(Command::Configure {
                config: std::sync::Arc::new(config),
            });
        }
        let mut config = self.state.system.config.as_ref().clone();
        let still_mode = matches!(change, GuiConfigChange::ToggleStillMode);
        match change {
            GuiConfigChange::ToggleStillMode => config.still_mode = !config.still_mode,
            GuiConfigChange::ToggleWaveformSeek => {
                config.waveform_seek_enabled = !config.waveform_seek_enabled
            }
            GuiConfigChange::ToggleAlbumArtworkRotation => {
                use crate::config::AlbumArtworkMode;
                config.album_artwork_mode = match config.album_artwork_mode {
                    AlbumArtworkMode::Static => AlbumArtworkMode::Rotating,
                    AlbumArtworkMode::Rotating => AlbumArtworkMode::Static,
                };
            }
            GuiConfigChange::ToggleLibraryColumn(column) => config.library_columns.toggle(column),
            GuiConfigChange::AlbumArtworkStyle(style) => config.album_artwork_style = style,
            GuiConfigChange::AddLibraryRoot(path) => {
                let path = library::logical_path(&path)?;
                if config.library_roots.contains(&path) {
                    return Ok(());
                }
                config.library_roots.push(path);
            }
            GuiConfigChange::RemoveLibraryRoot(path) => {
                if !config.library_roots.contains(&path) {
                    return Ok(());
                }
                config.library_roots.retain(|root| root != &path);
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

    pub(in crate::core) fn update_track_stats(
        &mut self,
        track_id: i64,
        stats: crate::store::TrackStats,
    ) -> Result<()> {
        self.state.library.revision = self.state.library.revision.wrapping_add(1);
        if let Some(index) =
            self.state.library.history.iter().position(|entry| {
                entry.track_id == track_id && entry.play_count != stats.play_count
            })
        {
            Arc::make_mut(&mut self.state.library.history)[index].play_count = stats.play_count;
        }
        if let Some(current) = self.state.current_track.as_mut()
            && current.id == track_id
            && (current.play_count != stats.play_count || current.last_played != stats.last_played)
        {
            let current = Arc::make_mut(current);
            current.play_count = stats.play_count;
            current.last_played = stats.last_played;
        }
        self.state.queue = self.store.queue_projection(self.queue_ids.len())
            .context("Playback statistics committed, but refreshing queue failed; previous queue projection retained")?;
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
        self.invalidate_playlists();
        self.state.library.playlist_total = self
            .store
            .playlist_summary_page(0, 0)
            .context("Playlist change committed, but refreshing playlists failed; previous projection retained")?
            .total;
        Ok(())
    }

    fn invalidate_playlists(&mut self) {
        // The catalog epoch is also the floor for scalar entry tokens. Advancing
        // past every scoped token prevents an old command becoming valid after reset.
        self.state.library.playlist_revision = self
            .state
            .library
            .playlist_entry_revisions
            .values()
            .copied()
            .max()
            .unwrap_or(self.state.library.playlist_revision)
            .max(self.state.library.playlist_revision)
            .wrapping_add(1);
        // Retention is bounded by playlists reordered within this catalog epoch.
        if !self.state.library.playlist_entry_revisions.is_empty() {
            self.state.library.playlist_entry_revisions = Arc::default();
        }
    }

    pub(in crate::core) fn reload_catalog(&mut self, structure_changed: bool) -> Result<()> {
        // Announce every committed domain before reading any fallible projection.
        self.state.library.revision = self.state.library.revision.wrapping_add(1);
        if structure_changed {
            self.state.library.structure_revision =
                self.state.library.structure_revision.wrapping_add(1);
        }
        self.invalidate_playlists();
        let library = self
            .store
            .track_count()
            .map(|total| {
                self.state.library.track_total = total;
            })
            .context("Refreshing library count failed; previous count retained");
        let playlists = self
            .store
            .playlist_summary_page(0, 0)
            .map(|page| {
                self.state.library.playlist_total = page.total;
            })
            .context("Refreshing playlist count failed; previous count retained");
        let history = self
            .store
            .history(MAX_QUERY_ROWS)
            .map(|history| {
                self.state.library.history = Arc::new(history);
            })
            .context("Refreshing history failed; previous history retained");
        library.and(playlists).and(history)
    }

    pub(in crate::core) fn reload(&mut self, structure_changed: bool) -> Result<()> {
        let catalog = self.reload_catalog(structure_changed);
        // A failed catalog projection cannot hide a committed queue revision.
        let queue = self.refresh_queue_tracks();
        catalog.and(queue)
    }
}
