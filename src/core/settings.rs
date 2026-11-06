use super::*;

pub(super) struct LiveControls {
    pub(super) volume: f32,
    pub(super) shuffle: bool,
    pub(super) repeat: RepeatMode,
    pub(super) output_device: Option<String>,
}

/// Runtime preferences own desired values; published DTOs are observations only.
/// High-frequency controls never clone the graph or invalidate configuration.
pub(super) struct Preferences {
    settings: Arc<Config>,
    live: LiveControls,
    saved: Option<Config>,
    save_error: Option<String>,
}

impl Preferences {
    pub(super) fn new(settings: Arc<Config>, saved: Option<Config>) -> Self {
        let live = LiveControls {
            volume: settings.volume,
            shuffle: settings.shuffle,
            repeat: settings.repeat,
            output_device: settings.output_device.clone(),
        };
        Self {
            settings,
            live,
            saved,
            save_error: None,
        }
    }

    pub(super) fn settings(&self) -> &Arc<Config> {
        &self.settings
    }

    pub(super) fn live(&self) -> &LiveControls {
        &self.live
    }

    pub(super) fn set_volume(&mut self, volume: f32) {
        self.live.volume = volume;
    }

    pub(super) fn set_shuffle(&mut self, shuffle: bool) {
        self.live.shuffle = shuffle;
    }

    pub(super) fn set_repeat(&mut self, repeat: RepeatMode) {
        self.live.repeat = repeat;
    }

    pub(super) fn select_device(&mut self, device: Option<String>) {
        self.live.output_device = device;
    }

    pub(super) fn checkpoint(&self) -> Config {
        let mut config = self.settings.as_ref().clone();
        config.volume = self.live.volume;
        config.shuffle = self.live.shuffle;
        config.repeat = self.live.repeat;
        config.output_device.clone_from(&self.live.output_device);
        config
    }

    pub(super) fn accept(&mut self, settings: Arc<Config>) {
        self.live.volume = settings.volume;
        self.live.shuffle = settings.shuffle;
        self.live.repeat = settings.repeat;
        self.live.output_device.clone_from(&settings.output_device);
        self.settings = settings;
    }

    pub(super) fn project(&self, state: &mut CoreState) {
        if !Arc::ptr_eq(&state.configuration.config, &self.settings) {
            state.configuration.config = Arc::clone(&self.settings);
        }
        state.playback.volume = self.live.volume;
        state.playback.shuffle = self.live.shuffle;
        state.playback.repeat = self.live.repeat;
        if state.system.selected_device != self.live.output_device {
            state
                .system
                .selected_device
                .clone_from(&self.live.output_device);
        }
    }
}

impl Core {
    pub(in crate::core) fn mutate_config(&mut self, change: ConfigMutation) -> Result<()> {
        if let ConfigMutation::Settings(patch) = change {
            // Playback controls can be newer than the last persisted config.
            // Only explicit draft changes override the owner's execution-time state.
            let mut config = self.preferences.checkpoint();
            patch.apply_to(&mut config);
            return self.configure(Arc::new(config));
        }
        let mut config = self.preferences.checkpoint();
        let visualization_changed = matches!(change, ConfigMutation::ToggleVisualization);
        let roots_changed = matches!(
            change,
            ConfigMutation::AddLibraryRoot(_) | ConfigMutation::RemoveLibraryRoot(_)
        );
        match change {
            ConfigMutation::ToggleVisualization => {
                config.visualization_enabled = !config.visualization_enabled
            }
            ConfigMutation::ToggleWaveformSeek => {
                config.waveform_seek_enabled = !config.waveform_seek_enabled
            }
            ConfigMutation::ToggleLibraryColumn(column) => config.library_columns.toggle(column),
            ConfigMutation::AddLibraryRoot(path) => {
                let path = library::logical_path(&path)?;
                if config.library_roots.contains(&path) {
                    return Ok(());
                }
                config.library_roots.push(path);
            }
            ConfigMutation::RemoveLibraryRoot(path) => {
                let path = library::logical_path(&path)?;
                if !config.library_roots.contains(&path) {
                    return Ok(());
                }
                config.library_roots.retain(|root| root != &path);
            }
            ConfigMutation::Settings(_) => unreachable!(),
        }
        config.validate()?;
        if visualization_changed {
            self.engine.ensure_command_capacity(1)?;
        }
        // Quick controls change desired memory state; Apply or shutdown persists it.
        self.preferences.accept(Arc::new(config));
        if roots_changed {
            self.state.library.structure_revision =
                self.state.library.structure_revision.wrapping_add(1);
        }
        if visualization_changed {
            self.engine
                .send(AudioCommand::VisualizationEnabled(
                    self.preferences.settings().visualization_enabled,
                ))
                .context("Configuration applied, but audio application failed")?;
        }
        Ok(())
    }
    pub(in crate::core) fn configure(&mut self, mut config: Arc<Config>) -> Result<()> {
        anyhow::ensure!(
            config.processing_graph == self.preferences.settings().processing_graph,
            "Processing graph changes require revision-checked ProcessingGraphEdit commands"
        );
        config.validate()?;
        let roots = library::normalize_roots(&config.library_roots)?;
        Arc::make_mut(&mut config).library_roots = roots;
        let roots_changed = config.library_roots != self.preferences.settings().library_roots;
        let previous = self.preferences.settings();
        let changed = [
            config.visualization_enabled != previous.visualization_enabled,
            config.media_read_buffer_mib != previous.media_read_buffer_mib,
            config.volume != self.preferences.live().volume,
            config.visualization_signal != previous.visualization_signal,
            config.equalizer != previous.equalizer,
            config.ffmpeg_enabled != previous.ffmpeg_enabled,
        ];
        // No other producer can consume these slots while Core persists.
        // Reject a busy batch before committing any desired settings.
        // Explicit Apply always validates output, including an unchanged idle sink.
        // The engine compares admitted output targets, so validation alone does
        // not revoke the PCM identity or discard a client's partial FFT window.
        self.engine
            .ensure_command_capacity(1 + changed.iter().filter(|&&changed| changed).count())?;
        let [
            visualization_changed,
            buffer_changed,
            volume_changed,
            signal_changed,
            equalizer_changed,
            ffmpeg_changed,
        ] = changed;
        let ffmpeg_status = if !config.ffmpeg_enabled {
            "disabled".to_owned()
        } else {
            audio::ffmpeg_status().unwrap_or_else(|error| format!("unavailable: {error:#}"))
        };
        if let Some(logging) = self.logging.clone() {
            logging.reconfigure_with(&crate::config::logging_options(&config), || {
                self.write_config(&config)
            })?;
        } else {
            self.write_config(&config)?;
        }
        // Persistence and logging have committed the desired settings.
        // Audio delivery may partially succeed; never roll this state back.
        self.store
            .set_track_cache_query_batch_size(config.query_batch_size);
        self.set_shuffle(config.shuffle);
        self.state.system.ffmpeg_status = ffmpeg_status;
        self.preferences.accept(config);
        if roots_changed {
            self.state.library.structure_revision =
                self.state.library.structure_revision.wrapping_add(1);
        }
        tracing::info!("configuration_updated");
        let request = self.output_settings_request.wrapping_add(1);
        let config = self.preferences.settings();
        (|| -> Result<()> {
            if visualization_changed {
                self.engine.send(AudioCommand::VisualizationEnabled(
                    config.visualization_enabled,
                ))?;
            }
            if buffer_changed {
                self.engine
                    .send(AudioCommand::MediaReadBuffer(config.media_read_buffer_mib))?;
            }
            if volume_changed {
                self.engine.send(AudioCommand::Volume(config.volume))?;
            }
            if signal_changed {
                self.engine.send(AudioCommand::VisualizationSignal(
                    config.visualization_signal,
                ))?;
            }
            if equalizer_changed {
                self.engine
                    .send(AudioCommand::Equalizer(Arc::new(config.equalizer.clone())))?;
            }
            if ffmpeg_changed {
                self.engine
                    .send(AudioCommand::FfmpegEnabled(config.ffmpeg_enabled))?;
            }
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
        if self.preferences.saved.as_ref() == Some(config) {
            return Ok(());
        }
        let result = config
            .save_with_baseline(
                &self.state.configuration.config_path,
                self.preferences.saved.as_ref(),
            )
            .context("Saving configuration");
        match &result {
            Ok(()) => {
                self.preferences.saved = Some(config.clone());
                if let Some(previous) = self.preferences.save_error.take()
                    && self.state.system.last_error.as_ref() == Some(&previous)
                {
                    self.state.system.last_error = None;
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                if self.preferences.save_error.as_ref() != Some(&message) {
                    tracing::error!(error = %message, "configuration_save_failed");
                    self.state.system.last_error = Some(message.clone());
                    self.preferences.save_error = Some(message);
                }
            }
        }
        result
    }

    pub(in crate::core) fn flush_config(&mut self) -> Result<bool> {
        // Compare the complete desired config, including quick controls, to the
        // last successful save rather than only checking playback fields.
        let config = self.preferences.checkpoint();
        if self.preferences.saved.as_ref() == Some(&config) {
            return Ok(false);
        }
        self.write_config(&config)?;
        self.preferences.accept(Arc::new(config));
        Ok(true)
    }
}
