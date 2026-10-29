use super::*;

impl Core {
    // Compose on the Core thread; live playback controls override the saved snapshot.
    pub(in crate::core) fn effective_config(&self) -> Config {
        let mut config = self.state.configuration.config.as_ref().clone();
        config.volume = self.state.playback.volume;
        config.shuffle = self.state.playback.shuffle;
        config.repeat = self.state.playback.repeat;
        config
            .output_device
            .clone_from(&self.state.system.selected_device);
        config
    }

    pub(in crate::core) fn update_gui_config(&mut self, change: GuiConfigChange) -> Result<()> {
        if let GuiConfigChange::Settings(patch) = change {
            // Playback controls can be newer than the last persisted config.
            // Only explicit draft changes override the owner's execution-time state.
            let mut config = self.effective_config();
            patch.apply_to(&mut config);
            return self
                .command(Command::Configure {
                    config: std::sync::Arc::new(config),
                })
                .map(|_| ())
                .map_err(|error| error.error);
        }
        let mut config = self.effective_config();
        let still_mode = matches!(change, GuiConfigChange::ToggleStillMode);
        let roots_changed = matches!(
            change,
            GuiConfigChange::AddLibraryRoot(_) | GuiConfigChange::RemoveLibraryRoot(_)
        );
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
                let path = library::logical_path(&path)?;
                if !config.library_roots.contains(&path) {
                    return Ok(());
                }
                config.library_roots.retain(|root| root != &path);
            }
            GuiConfigChange::Settings(_) => unreachable!(),
        }
        config.validate()?;
        if still_mode {
            self.engine.ensure_command_capacity(1)?;
        }
        // Quick controls change desired memory state; Apply or shutdown persists it.
        self.state.configuration.config = Arc::new(config);
        if roots_changed {
            self.state.library.structure_revision =
                self.state.library.structure_revision.wrapping_add(1);
        }
        if still_mode {
            self.engine
                .send(AudioCommand::StillMode(
                    self.state.configuration.config.still_mode,
                ))
                .context("Configuration applied, but audio application failed")?;
        }
        Ok(())
    }
    pub(in crate::core) fn configure(&mut self, mut config: Arc<Config>) -> Result<()> {
        anyhow::ensure!(
            config.processing_graph == self.state.configuration.config.processing_graph,
            "Processing graph changes require revision-checked ProcessingGraphEdit commands"
        );
        config.validate()?;
        let roots = library::normalize_roots(&config.library_roots)?;
        Arc::make_mut(&mut config).library_roots = roots;
        let roots_changed = config.library_roots != self.state.configuration.config.library_roots;
        // No other producer can consume these slots while Core persists.
        // Reject a busy batch before committing any desired settings.
        self.engine.ensure_command_capacity(7)?;
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
        self.set_shuffle(config.shuffle);
        self.state.playback.repeat = config.repeat;
        self.state.system.ffmpeg_status = ffmpeg_status;
        self.state.configuration.config = config;
        if roots_changed {
            self.state.library.structure_revision =
                self.state.library.structure_revision.wrapping_add(1);
        }
        tracing::info!("configuration_updated");
        let request = self.output_settings_request.wrapping_add(1);
        let config = &self.state.configuration.config;
        (|| -> Result<()> {
            self.engine
                .send(AudioCommand::StillMode(config.still_mode))?;
            self.engine
                .send(AudioCommand::MediaReadBuffer(config.media_read_buffer_mb))?;
            self.engine.send(AudioCommand::Volume(config.volume))?;
            self.engine
                .send(AudioCommand::AnalysisSettings(config.as_ref().into()))?;
            self.engine
                .send(AudioCommand::Equalizer(Arc::new(config.equalizer.clone())))?;
            self.engine
                .send(AudioCommand::FfmpegEnabled(config.ffmpeg_enabled))?;
            self.engine.send(AudioCommand::OutputSettings {
                request,
                device: config.output_device.clone(),
                auto_mix: config.pipewire_auto_mix,
            })
        })()
        .context("Configuration saved, but audio application failed")?;
        self.output_settings_request = request;
        Ok(())
    }
    pub(in crate::core) fn write_config(&mut self, config: &Config) -> Result<()> {
        if self.saved_config.as_ref() == Some(config) {
            return Ok(());
        }
        let result = config
            .save_with_baseline(
                &self.state.configuration.config_path,
                self.saved_config.as_ref(),
            )
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
        let config = self.effective_config();
        if self.saved_config.as_ref() == Some(&config) {
            return Ok(false);
        }
        self.write_config(&config)?;
        self.state.configuration.config = Arc::new(config);
        Ok(true)
    }
}
