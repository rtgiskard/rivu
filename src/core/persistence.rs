use super::*;

impl Core {
    pub(in crate::core) fn update_gui_config(&mut self, change: GuiConfigChange) -> Result<()> {
        if let GuiConfigChange::Settings(patch) = change {
            // Playback controls can be newer than the last persisted config.
            // Only explicit draft changes override the owner's execution-time state.
            let mut config = self.state.system.config.as_ref().clone();
            config.volume = self.state.playback.volume;
            config.shuffle = self.state.playback.shuffle;
            config.repeat = self.state.playback.repeat;
            config
                .output_device
                .clone_from(&self.state.system.selected_device);
            patch.apply_to(&mut config);
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
            GuiConfigChange::ToggleLibraryColumn(column) => config.library_columns.toggle(column),
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
        config.validate()?;
        if still_mode {
            self.engine.ensure_command_capacity(1)?;
        }
        // Quick controls change desired memory state; Apply or shutdown persists it.
        self.state.system.config = Arc::new(config);
        if still_mode {
            self.audio(AudioCommand::StillMode(self.state.system.config.still_mode))
                .context("Configuration applied, but audio application failed")?;
        }
        Ok(())
    }

    pub(in crate::core) fn save_queue(&mut self) -> Result<()> {
        self.store.set_queue_current(self.state.queue.current_id)?;
        self.state.queue = self.store.queue_projection(self.queue_ids.len())?;
        Ok(())
    }

    pub(in crate::core) fn write_config(&mut self, config: &Config) -> Result<()> {
        if self.saved_config.as_ref() == Some(config) {
            return Ok(());
        }
        let result = config
            .save(&self.state.system.config_path)
            .context("Saving configuration");
        match &result {
            Ok(()) => {
                self.saved_config = Some(config.clone());
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

    pub(in crate::core) fn flush_config(&mut self) -> Result<bool> {
        // Compare the complete desired config, including quick controls, to the
        // last successful save rather than only checking playback fields.
        let mut config = self.state.system.config.as_ref().clone();
        config.volume = self.state.playback.volume;
        config.shuffle = self.state.playback.shuffle;
        config.repeat = self.state.playback.repeat;
        config
            .output_device
            .clone_from(&self.state.system.selected_device);
        if self.saved_config.as_ref() == Some(&config) {
            return Ok(false);
        }
        self.write_config(&config)?;
        self.state.system.config = Arc::new(config);
        Ok(true)
    }

    pub(in crate::core) fn update_track_stats(
        &mut self,
        track_id: i64,
        stats: crate::store::TrackStats,
    ) -> Result<()> {
        self.state
            .library
            .change_metadata(Some(&[track_id]), Some(&[]));
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

    /// Announce committed producer facts before any fallible cached projection read.
    pub(in crate::core) fn apply_scan_impacts(
        &mut self,
        impacts: crate::store::ScanImpacts,
    ) -> Result<()> {
        if impacts.library {
            self.state.library.revision = self.state.library.revision.wrapping_add(1);
        }
        if impacts.structure {
            self.state.library.structure_revision =
                self.state.library.structure_revision.wrapping_add(1);
        }
        if impacts.duplicate_groups {
            self.state.library.duplicate_groups_revision =
                self.state.library.duplicate_groups_revision.wrapping_add(1);
        }
        if impacts.duplicate_members {
            self.state.library.duplicate_members_revision = self
                .state
                .library
                .duplicate_members_revision
                .wrapping_add(1);
        }
        let library = if impacts.structure {
            self.store
                .track_count()
                .map(|total| self.state.library.track_total = total)
                .context("Refreshing library count failed; previous count retained")
        } else {
            Ok(())
        };
        let history = if impacts.library {
            self.store
                .history(MAX_QUERY_ROWS)
                .map(|history| self.state.library.history = Arc::new(history))
                .context("Refreshing history failed; previous history retained")
        } else {
            Ok(())
        };
        // A failed projection read cannot hide another committed domain.
        let queue = if impacts.library {
            self.refresh_queue_tracks()
        } else {
            Ok(())
        };
        library.and(history).and(queue)
    }
}
