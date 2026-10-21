use super::*;
use anyhow::ensure;
use std::sync::atomic::Ordering;

impl Core {
    pub(in crate::core) fn processing_graph_edit(
        &mut self,
        revision: u64,
        edit: GraphEdit,
    ) -> Result<()> {
        let mut config = self.effective_config();
        config.processing_graph.edit(revision, edit)?;
        self.commit_processing_config(config)
    }

    pub(in crate::core) fn processing_graph_run(
        &mut self,
        track_id: i64,
        target: GraphPort,
        mode: GraphExecutionMode,
    ) -> Result<()> {
        let track = self.track(track_id)?;
        let request = crate::processing::GraphRequest {
            target,
            mode,
            export_path: None,
            cached_output: None,
        };
        let status =
            self.processing
                .start_graph(track, self.state.system.config.clone(), request)?;
        let id = status.id;
        self.state.system.processing = Some(status);
        if mode != GraphExecutionMode::Offline {
            self.processing.pending_graph_audition = Some((id, self.generation));
        }
        Ok(())
    }

    pub(in crate::core) fn processing_graph_audition(
        &mut self,
        id: u64,
        node: GraphPort,
    ) -> Result<()> {
        let result = self
            .processing
            .results
            .iter()
            .find(|r| r.status.id == id)
            .context("Graph run is no longer retained")?;
        let graph = result
            .status
            .settings
            .graph
            .clone()
            .context("Not a graph run")?;
        graph.ancestors(node)?;
        let track = self.track(result.track.id)?;
        ensure!(
            track.path == result.track.path
                && track.cue == result.track.cue
                && track.decoder_hint == result.track.decoder_hint
                && track.fingerprint == result.track.fingerprint
                && crate::processing::SourceStamp::read(&track.path)? == result.stamp,
            "Graph source changed; preview requires the original immutable source"
        );
        if let Some(output) = result.status.graph_outputs.iter().find(|o| o.port == node) {
            let path = output.path.clone();
            let track_id = result.track.id;
            self.establish_graph_source_context(track_id)?;
            self.audition_path(id, Some(path))?;
            self.state.processing_audition = Some(crate::model::ProcessingAudition {
                id,
                output: Some(node),
                mode: Some(GraphExecutionMode::Offline),
            });
            return Ok(());
        }
        let mut config = self.effective_config();
        config.processing_graph = graph;
        let request = crate::processing::GraphRequest {
            target: node,
            mode: GraphExecutionMode::Auto,
            export_path: None,
            cached_output: None,
        };
        let status = self
            .processing
            .start_graph(track, Arc::new(config), request)?;
        self.processing.pending_graph_audition = Some((status.id, self.generation));
        self.state.system.processing = Some(status);
        Ok(())
    }

    fn processing_graph_audition_prepared(
        &mut self,
        id: u64,
        mut runner: audio::GraphRunner,
    ) -> Result<()> {
        let status = self.processing_status(id).context("Unknown graph run")?;
        self.establish_graph_source_context(status.track_id)?;
        let path = status
            .settings
            .graph_boundary
            .map(|boundary| {
                status
                    .graph_outputs
                    .iter()
                    .find(|o| o.port == boundary)
                    .map(|output| -> Result<PathBuf> {
                        if let Some(context) = &output.context {
                            runner.set_source_context(
                                context.start_frame,
                                status
                                    .frames
                                    .context("Missing original graph source frame count")?,
                            )?;
                            Ok(context.path.clone())
                        } else {
                            Ok(output.path.clone())
                        }
                    })
                    .context("Hybrid prefix is not ready")?
            })
            .transpose()?;
        self.audition_path_with_graph(id, path, Some(runner))
    }

    fn establish_graph_source_context(&mut self, track_id: i64) -> Result<()> {
        if self
            .state
            .current_track
            .as_ref()
            .is_some_and(|t| t.id == track_id)
            && self.state.playback.status != PlaybackStatus::Stopped
        {
            return Ok(());
        }
        ensure!(
            !self.database_maintenance_active(),
            "Database maintenance is in progress; select the prepared graph again after it finishes"
        );
        self.engine.ensure_command_capacity(4)?;
        let status = if self.state.playback.status == PlaybackStatus::Playing {
            PlaybackStatus::Playing
        } else {
            PlaybackStatus::Paused
        };
        let queue_id = if let Some(id) = self.store.queued_track(track_id)? {
            id
        } else {
            self.enqueue(&[track_id])?;
            *self
                .queue_ids
                .last()
                .context("Graph source was not queued")?
        };
        self.start_with_status(queue_id, false, status)
    }

    pub(in crate::core) fn processing_graph_export(
        &mut self,
        id: u64,
        node: GraphPort,
        path: PathBuf,
    ) -> Result<()> {
        let result = self
            .processing
            .results
            .iter()
            .find(|r| r.status.id == id)
            .context("Graph run is no longer retained")?;
        let graph = result
            .status
            .settings
            .graph
            .clone()
            .context("Not a graph run")?;
        graph.ancestors(node)?;
        let track = self.track(result.track.id)?;
        ensure!(
            track.path == result.track.path
                && track.cue == result.track.cue
                && track.decoder_hint == result.track.decoder_hint
                && track.fingerprint == result.track.fingerprint
                && crate::processing::SourceStamp::read(&track.path)? == result.stamp,
            "Graph source changed; export requires the original immutable source"
        );
        ensure!(
            path.is_absolute() && !path.exists() && path.parent().is_some_and(|p| p.is_dir()),
            "Export requires an absolute, new file in an existing directory"
        );
        let cached_output = result
            .status
            .graph_outputs
            .iter()
            .find(|output| output.port == node)
            .map(|output| {
                Ok::<_, anyhow::Error>(crate::processing::CachedGraphOutput {
                    path: output.path.clone(),
                    sample_rate: result
                        .status
                        .sample_rate
                        .context("Missing cached sample rate")?,
                    channels: result.status.channels.context("Missing cached channels")?,
                    frames: result.status.frames.context("Missing cached frame count")?,
                })
            })
            .transpose()?;
        let mut config = self.effective_config();
        config.processing_graph = graph;
        let request = crate::processing::GraphRequest {
            target: node,
            mode: GraphExecutionMode::Offline,
            export_path: Some(path),
            cached_output,
        };
        let status = self
            .processing
            .start_graph(track, Arc::new(config), request)?;
        self.state.system.processing = Some(status);
        Ok(())
    }

    pub(in crate::core) fn cancel_processing(&mut self, id: u64) -> Result<()> {
        let active = self
            .processing
            .active
            .as_ref()
            .is_some_and(|task| task.id == id);
        let selected = self.processing.audition == Some(id)
            && self.processing.audition_generation == Some(self.generation);
        ensure!(
            active || selected,
            "No matching active processing task or live audition"
        );
        if active {
            self.processing.cancel(id)?;
        }
        if self
            .processing
            .pending_graph_audition
            .is_some_and(|(task, _)| task == id)
        {
            self.processing.pending_graph_audition = None;
        }
        if selected {
            if let Err(error) = self.audition_path(id, None) {
                self.stop()?;
                return Err(error);
            }
            let stop_status = |status: &mut ProcessingTaskStatus| {
                if status.id == id
                    && matches!(
                        status.settings.graph_mode,
                        Some(GraphExecutionMode::Realtime | GraphExecutionMode::Hybrid)
                    )
                {
                    status.phase = ProcessingPhase::Cancelled;
                    status.error = Some("Live audition stopped".into());
                    for node in &mut status.graph_nodes {
                        if node.phase == ProcessingPhase::Processing {
                            node.phase = ProcessingPhase::Cancelled;
                        }
                    }
                }
            };
            for status in self
                .state
                .system
                .processing
                .iter_mut()
                .chain(self.processing_results.iter_mut())
            {
                stop_status(status);
            }
            for result in &mut self.processing.results {
                stop_status(&mut result.status);
            }
        }
        Ok(())
    }
    fn commit_processing_config(&mut self, config: Config) -> Result<()> {
        config.validate()?;
        let equalizer_changed = config.equalizer != self.state.system.config.equalizer;
        if equalizer_changed {
            self.engine.ensure_command_capacity(1)?;
        }
        self.write_config(&config)?;
        self.state.system.config = Arc::new(config);
        if equalizer_changed {
            self.audio(AudioCommand::Equalizer(Arc::new(
                self.state.system.config.equalizer.clone(),
            )))
            .context("Configuration saved, but equalizer delivery failed")?;
        }
        Ok(())
    }
    pub(in crate::core) fn equalizer_edit(&mut self, edit: EqualizerEdit) -> Result<()> {
        use EqualizerEdit::*;
        let mut config = self.effective_config();
        let sample_rate = self
            .state
            .current_track
            .as_ref()
            .map(|track| track.sample_rate);
        if let Some(edit) = edit.apply_config(&mut config.equalizer, sample_rate)? {
            match edit {
                SwapAb => {
                    let previous = config.equalizer_ab.take().unwrap_or_default();
                    config.equalizer_ab = Some(std::mem::replace(&mut config.equalizer, previous));
                }
                SavePreset { name } => {
                    if let Some(preset) =
                        config.equalizer_presets.iter_mut().find(|p| p.name == name)
                    {
                        preset.config = config.equalizer.clone();
                    } else {
                        config
                            .equalizer_presets
                            .push(rivu_format::config::EqualizerPreset {
                                name,
                                config: config.equalizer.clone(),
                            });
                    }
                }
                LoadPreset { name } => {
                    config.equalizer = config
                        .equalizer_presets
                        .iter()
                        .find(|p| p.name == name)
                        .context("Unknown equalizer preset")?
                        .config
                        .clone()
                }
                DeletePreset { name } => {
                    ensure!(
                        config.equalizer_presets.iter().any(|p| p.name == name),
                        "Unknown equalizer preset"
                    );
                    config.equalizer_presets.retain(|p| p.name != name);
                }
                ImportPreset { path, name } => {
                    let metadata = std::fs::metadata(&path)?;
                    ensure!(metadata.len() <= 65536, "Preset exceeds 64 KiB");
                    let value: rivu_dsp::equalizer::EqualizerConfig =
                        serde_json::from_slice(&std::fs::read(path)?)?;
                    value.validate()?;
                    if let Some(preset) =
                        config.equalizer_presets.iter_mut().find(|p| p.name == name)
                    {
                        preset.config = value;
                    } else {
                        config
                            .equalizer_presets
                            .push(rivu_format::config::EqualizerPreset {
                                name,
                                config: value,
                            });
                    }
                }
                ExportPreset { path, name } => {
                    let preset = config
                        .equalizer_presets
                        .iter()
                        .find(|p| p.name == name)
                        .context("Unknown equalizer preset")?;
                    use std::io::Write;
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(path)?;
                    file.write_all(&serde_json::to_vec_pretty(&preset.config)?)?;
                    return Ok(());
                }
                _ => unreachable!("parameter edits are handled by apply_config"),
            }
        }
        self.commit_processing_config(config)
    }
    pub(in crate::core) fn denoise_edit(&mut self, patch: DenoisePatch) -> Result<()> {
        let mut config = self.effective_config();
        let target = &mut config.denoise;
        macro_rules! apply { ($($field:ident),*)=>{$(if let Some(v)=patch.$field {target.$field=v;})*}; }
        apply!(
            mode,
            strength,
            noise_floor_db,
            attack_ms,
            release_ms,
            smoothing_bins,
            transient_protection,
            wet,
            fft_size,
            stationary_noise_dbfs
        );
        if let Some(v) = patch.hum_enabled {
            target.hum.enabled = v;
        }
        if let Some(v) = patch.hum_fundamental_hz {
            target.hum.fundamental_hz = v;
        }
        if let Some(v) = patch.hum_harmonics {
            target.hum.harmonics = v;
        }
        if let Some(v) = patch.hum_q {
            target.hum.q = v;
        }
        if let Some(v) = patch.impulse_enabled {
            target.impulse.enabled = v;
        }
        if let Some(v) = patch.impulse_max_width {
            target.impulse.max_width = v;
        }
        if let Some(v) = patch.impulse_threshold {
            target.impulse.threshold = v;
        }
        if let Some(v) = patch.impulse_floor_dbfs {
            target.impulse.floor_dbfs = v;
        }
        if let Some(v) = patch.impulse_transient_protection {
            target.impulse.transient_protection = v;
        }
        self.commit_processing_config(config)
    }
    pub(in crate::core) fn neural_edit(&mut self, patch: NeuralPatch) -> Result<()> {
        let mut config = self.effective_config();
        let target = &mut config.neural;
        macro_rules! apply {($($field:ident),*)=>{$(if let Some(v)=patch.$field {target.$field=v;})*};}
        apply!(
            speech_interpreter,
            separation_interpreter,
            worker,
            speech_manifest,
            separation_manifest,
            device,
            threads,
            speech_wet_mix,
            speech_attenuation_limit_db,
            speech_post_filter,
            separation_overlap
        );
        self.commit_processing_config(config)
    }
    pub(in crate::core) fn start_processing(
        &mut self,
        track_id: i64,
        operation: ProcessingOperation,
        profile: Option<NoiseSelection>,
    ) -> Result<()> {
        let track = self.track(track_id)?;
        let status =
            self.processing
                .start(track, self.state.system.config.clone(), operation, profile)?;
        self.state.system.processing = Some(status);
        Ok(())
    }
    pub(in crate::core) fn processing_status(&self, id: u64) -> Option<ProcessingTaskStatus> {
        self.state
            .system
            .processing
            .as_ref()
            .filter(|s| s.id == id)
            .cloned()
            .or_else(|| self.processing_results.iter().find(|s| s.id == id).cloned())
            .or_else(|| {
                self.processing
                    .results
                    .iter()
                    .find(|r| r.status.id == id)
                    .map(|r| r.status.clone())
            })
    }
    pub(in crate::core) fn processing_event(&mut self, event: crate::processing::Event) {
        use crate::processing::Event;
        match event {
            Event::GraphNode { id, status: node } => {
                if self
                    .processing
                    .active
                    .as_ref()
                    .is_some_and(|task| task.id == id)
                    && let Some(status) =
                        self.state.system.processing.as_mut().filter(|s| s.id == id)
                {
                    if let Some(existing) = status
                        .graph_nodes
                        .iter_mut()
                        .find(|entry| entry.node == node.node)
                    {
                        *existing = node;
                    } else {
                        status.graph_nodes.push(node);
                    }
                }
            }
            Event::Progress {
                id,
                completed,
                total,
                phase,
            } => {
                if self
                    .processing
                    .active
                    .as_ref()
                    .is_some_and(|task| task.id == id)
                    && let Some(status) =
                        self.state.system.processing.as_mut().filter(|s| s.id == id)
                {
                    status.completed = completed;
                    status.total = total;
                    status.phase = phase;
                }
            }
            Event::Finished { id, result } => {
                if !self
                    .processing
                    .active
                    .as_ref()
                    .is_some_and(|task| task.id == id)
                {
                    return;
                }
                let task = self.processing.active.take().unwrap();
                let cancelled = task.cancelled.load(Ordering::Acquire);
                let _ = task.worker.join();
                let result = result.map(|result| *result).and_then(|mut result| {
                    ensure!(!cancelled, "Processing cancelled");
                    let track = self.track(result.track.id)?;
                    ensure!(
                        track.path == result.track.path
                            && track.cue == result.track.cue
                            && track.decoder_hint == result.track.decoder_hint
                            && track.fingerprint == result.track.fingerprint,
                        "Source identity changed before publication"
                    );
                    ensure!(
                        crate::processing::SourceStamp::read(&track.path)? == result.stamp,
                        "Source file changed before publication"
                    );
                    let pinned_bytes = self.processing.results.iter()
                        .filter(|r| Some(r.status.id) == self.processing.audition)
                        .map(|r| r.cache_bytes).sum::<u64>();
                    ensure!(pinned_bytes.checked_add(result.cache_bytes).is_some_and(|bytes|
                        bytes <= crate::processing::graph::MAX_RETAINED_CACHE_BYTES),
                        "Processing cache plus selected audition exceeds the 4 GiB retention limit; stop that audition before running again");
                    if let Some((temporary, destination)) = result.pending_export.take() {
                        temporary.persist_noclobber(&destination).map_err(|error| error.error)?;
                        result.status.graph_exported_path = Some(destination);
                    }
                    Ok(result)
                });
                match result {
                    Ok(mut result) => {
                        let runner = result.runner.take();
                        self.state.system.processing = Some(result.status.clone());
                        self.processing.results.push_back(result);
                        // A selected audition must remain seekable while other jobs complete.
                        while self.processing.results.len() > 4
                            || (self.processing.results.len() > 1
                                && self
                                    .processing
                                    .results
                                    .iter()
                                    .map(|r| r.cache_bytes)
                                    .sum::<u64>()
                                    > crate::processing::graph::MAX_RETAINED_CACHE_BYTES)
                        {
                            let index = self
                                .processing
                                .results
                                .iter()
                                .position(|r| Some(r.status.id) != self.processing.audition)
                                .unwrap();
                            if let Some(evicted) = self.processing.results.remove(index) {
                                for status in self
                                    .processing_results
                                    .iter_mut()
                                    .filter(|s| s.id == evicted.status.id)
                                {
                                    status.outputs = None;
                                    status.graph_outputs.clear();
                                    status.error = Some(
                                        "Output cache evicted; run processing again to audition"
                                            .into(),
                                    );
                                }
                            }
                        }
                        if let Some((pending_id, generation)) =
                            self.processing.pending_graph_audition
                            && pending_id == id
                        {
                            self.processing.pending_graph_audition = None;
                            let outcome = if generation != self.generation {
                                Ok(())
                            } else if let Some(runner) = runner {
                                self.processing_graph_audition_prepared(id, runner)
                            } else {
                                let target = self
                                    .state
                                    .system
                                    .processing
                                    .as_ref()
                                    .and_then(|status| status.settings.graph_target);
                                target.map_or(Ok(()), |target| {
                                    self.processing_graph_audition(id, target)
                                })
                            };
                            if let Err(error) = outcome {
                                let error = format!("{error:#}");
                                self.state.system.last_error = Some(error.clone());
                                if let Some(status) =
                                    self.state.system.processing.as_mut().filter(|s| s.id == id)
                                {
                                    status.phase = ProcessingPhase::Failed;
                                    status.error = Some(error.clone());
                                }
                                if let Some(result) = self
                                    .processing
                                    .results
                                    .iter_mut()
                                    .find(|r| r.status.id == id)
                                {
                                    result.status.phase = ProcessingPhase::Failed;
                                    result.status.error = Some(error);
                                }
                            }
                        }
                    }
                    Err(error) => {
                        if self
                            .processing
                            .pending_graph_audition
                            .is_some_and(|(task, _)| task == id)
                        {
                            self.processing.pending_graph_audition = None;
                        }
                        if let Some(status) =
                            self.state.system.processing.as_mut().filter(|s| s.id == id)
                        {
                            status.phase = if cancelled {
                                ProcessingPhase::Cancelled
                            } else {
                                ProcessingPhase::Failed
                            };
                            status.error = Some(format!("{error:#}"));
                            status.outputs = None;
                            status.graph_outputs.clear();
                        }
                    }
                }
                if let Some(status) = self.state.system.processing.as_ref() {
                    self.processing_results.push_back(status.clone());
                    while self.processing_results.len() > 32 {
                        self.processing_results.pop_front();
                    }
                }
            }
            Event::MixFinished {
                id,
                generation,
                result,
            } => {
                if !self
                    .processing
                    .active
                    .as_ref()
                    .is_some_and(|task| task.id == id)
                {
                    return;
                }
                let task = self.processing.active.take().unwrap();
                let cancelled = task.cancelled.load(Ordering::Acquire);
                let _ = task.worker.join();
                let outcome = match result {
                    Ok(path) if !cancelled && generation == self.generation => {
                        let outcome = self.audition_path(id, Some(path.clone()));
                        if outcome.is_ok() {
                            if let Some(result) = self
                                .processing
                                .results
                                .iter_mut()
                                .find(|r| r.status.id == id)
                                && let Some(old) = result.mix.replace(path)
                            {
                                let _ = std::fs::remove_file(old);
                            }
                        } else {
                            let _ = std::fs::remove_file(path);
                        }
                        outcome
                    }
                    Ok(path) => {
                        let _ = std::fs::remove_file(path);
                        Err(anyhow::anyhow!(
                            "Mix audition cancelled or source playback changed"
                        ))
                    }
                    Err(error) => Err(error),
                };
                if let Some(status) = self.state.system.processing.as_mut().filter(|s| s.id == id) {
                    status.phase = if outcome.is_ok() {
                        ProcessingPhase::Complete
                    } else if cancelled {
                        ProcessingPhase::Cancelled
                    } else {
                        ProcessingPhase::Failed
                    };
                    status.error = outcome.as_ref().err().map(|e| format!("{e:#}"));
                }
                if let Err(error) = outcome
                    && !cancelled
                {
                    self.state.system.last_error = Some(format!("{error:#}"));
                }
            }
        }
        self.publish();
    }
    pub(in crate::core) fn audition_processing(
        &mut self,
        id: u64,
        mode: AuditionMode,
        vocal_gain_db: f32,
        accompaniment_gain_db: f32,
    ) -> Result<()> {
        let result = self
            .processing
            .results
            .iter()
            .find(|r| r.status.id == id)
            .context("Processing result is no longer retained")?;
        let track = self.track(result.track.id)?;
        ensure!(
            track.path == result.track.path
                && track.cue == result.track.cue
                && track.fingerprint == result.track.fingerprint
                && crate::processing::SourceStamp::read(&track.path)? == result.stamp,
            "Processing result is stale"
        );
        let outputs = result
            .status
            .outputs
            .as_ref()
            .context("No processing outputs")?;
        match mode {
            AuditionMode::Denoised => ensure!(outputs.denoised.is_some(), "No denoised output"),
            AuditionMode::Vocals => ensure!(outputs.vocals.is_some(), "No vocal stem"),
            AuditionMode::Accompaniment => {
                ensure!(outputs.accompaniment.is_some(), "No accompaniment stem")
            }
            AuditionMode::Mix => {
                ensure!(
                    outputs.vocals.is_some() && outputs.accompaniment.is_some(),
                    "No separated stems"
                );
                ensure!(
                    self.processing.active.is_none(),
                    "An audio processing task is already active"
                );
                ensure!(
                    vocal_gain_db.is_finite()
                        && accompaniment_gain_db.is_finite()
                        && (-96.0..=24.0).contains(&vocal_gain_db)
                        && (-96.0..=24.0).contains(&accompaniment_gain_db),
                    "Stem gains must be finite and within -96..24 dB"
                );
            }
            AuditionMode::Original => {}
        }
        let same_source = self
            .state
            .current_track
            .as_ref()
            .is_some_and(|t| t.id == track.id);
        if !same_source {
            self.command(Command::Play { track_id: track.id })?;
        } else if self.state.playback.status == PlaybackStatus::Stopped {
            let queue_id = self
                .state
                .queue
                .current_id
                .context("Current source has no queue occurrence")?;
            self.start(queue_id, false)?;
        }
        if mode == AuditionMode::Mix {
            let result = self
                .processing
                .results
                .iter()
                .find(|r| r.status.id == id)
                .context("Processing result is no longer retained")?;
            let mut status = result.status.clone();
            crate::processing::mix(
                &mut self.processing,
                id,
                self.generation,
                vocal_gain_db,
                accompaniment_gain_db,
            )?;
            status.phase = ProcessingPhase::Processing;
            status.error = None;
            self.state.system.processing = Some(status);
            return Ok(());
        }
        let result = self
            .processing
            .results
            .iter()
            .find(|r| r.status.id == id)
            .context("Processing result is no longer retained")?;
        let outputs = result
            .status
            .outputs
            .as_ref()
            .context("No processing outputs")?;
        let path = match mode {
            AuditionMode::Original => None,
            AuditionMode::Denoised => Some(outputs.denoised.clone().context("No denoised output")?),
            AuditionMode::Vocals => Some(outputs.vocals.clone().context("No vocal stem")?),
            AuditionMode::Accompaniment => Some(
                outputs
                    .accompaniment
                    .clone()
                    .context("No accompaniment stem")?,
            ),
            AuditionMode::Mix => unreachable!(),
        };
        self.audition_path(id, path)
    }
    fn audition_path(&mut self, id: u64, path: Option<PathBuf>) -> Result<()> {
        self.audition_path_with_graph(id, path, None)
    }

    fn audition_path_with_graph(
        &mut self,
        id: u64,
        path: Option<PathBuf>,
        graph: Option<audio::GraphRunner>,
    ) -> Result<()> {
        let status = self
            .processing_status(id)
            .context("Processing run is no longer retained")?;
        let track = self
            .state
            .current_track
            .as_ref()
            .filter(|t| t.id == status.track_id)
            .context("Audition requires the processing source to be the current queue track")?
            .as_ref()
            .clone();
        let admitted_source = crate::processing::SourceStamp::read(&track.path)?;
        ensure!(
            track.path == status.source_path
                && track.cue == status.source_cue
                && format!("{admitted_source:?}") == status.source_version,
            "Processing result is stale"
        );
        ensure!(
            self.processing.results.iter().any(|r| r.status.id == id)
                || ((graph.is_some() || self.processing.audition == Some(id))
                    && self.processing.active.as_ref().is_some_and(|a| a.id == id)),
            "Processing result is no longer retained"
        );
        ensure!(
            self.state.playback.status != PlaybackStatus::Stopped,
            "Start the source track before auditioning"
        );
        // Capture before stopping playback or queueing worker work. For a cached
        // boundary this is the cache identity, not the original source identity.
        let expected_identity = if graph.is_some() {
            Some(if let Some(path) = &path {
                let metadata = std::fs::metadata(path)?;
                ensure!(
                    metadata.is_file(),
                    "Cached graph boundary must be a regular file"
                );
                audio::SourceIdentity::from_metadata(&metadata)
            } else {
                admitted_source.identity()
            })
        } else {
            None
        };
        self.engine.ensure_command_capacity(2)?;
        let generation = self.generation;
        let mut bookkeeping = Ok(());
        let snapshot = self.engine.stop_request().wait(|event| {
            if bookkeeping.is_ok() {
                bookkeeping = self.retiring_audio_event(generation, event);
            }
        });
        let snapshot = match snapshot {
            Ok(snapshot) if bookkeeping.is_ok() => snapshot,
            other => return self.finish_playback(other.map(Some), false, bookkeeping),
        };
        if snapshot.generation != generation {
            return self.finish_playback(
                Ok(Some(snapshot)),
                false,
                Err(anyhow::anyhow!(
                    "Audio stop snapshot belongs to a different playback generation"
                )),
            );
        }
        if let Err(error) =
            self.playback_progress(snapshot.position_seconds, snapshot.listened_seconds)
        {
            return self.finish_playback(Ok(Some(snapshot)), false, Err(error));
        }
        let heard = self.playback.as_ref().map_or(0.0, |p| p.heard);
        self.generation = self.generation.wrapping_add(1);
        let is_graph = graph.is_some();
        let original = path.is_none();
        let source_info = if original {
            None
        } else {
            Some(MediaInfo {
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                duration: track.duration,
                bitrate_bps: track.bitrate_bps,
                bitrate_exact: track.bitrate_exact,
                track_number: track.track_number,
                disc_number: track.disc_number,
                bits_per_sample: track.bits_per_sample,
                release_date: track.release_date.clone(),
                codec: track.codec.clone(),
                channels: status.channels.unwrap_or(track.channels),
                sample_rate: status.sample_rate.unwrap_or(track.sample_rate),
            })
        };
        let path = path.unwrap_or_else(|| track.path.clone());
        let range = if original {
            track.cue.as_ref().map(|cue| audio::PlaybackRange {
                start_seconds: cue.start_seconds(),
                end_seconds: cue.end_seconds(),
            })
        } else {
            None
        };
        let decoder_hint = if original {
            track.decoder_hint
        } else {
            DecoderHint::Native
        };
        let command = if let Some(runner) = graph {
            AudioCommand::GraphAudition {
                path,
                expected_identity: expected_identity
                    .expect("graph identity captured before admission"),
                range,
                decoder_hint,
                source_info,
                metadata: Self::playback_metadata(&track),
                generation: self.generation,
                start_seconds: self.state.playback.position,
                paused: self.state.playback.status == PlaybackStatus::Paused,
                listened_seconds: heard,
                runner: Box::new(runner),
            }
        } else {
            AudioCommand::Audition {
                path,
                range,
                decoder_hint,
                source_info,
                metadata: Self::playback_metadata(&track),
                generation: self.generation,
                start_seconds: self.state.playback.position,
                paused: self.state.playback.status == PlaybackStatus::Paused,
                listened_seconds: heard,
            }
        };
        if let Err(error) = self.audio(command) {
            return self.finish_playback(Ok(None), false, Err(error));
        }
        self.processing.audition = (!original || is_graph).then_some(id);
        self.processing.audition_generation = self.processing.audition.map(|_| self.generation);
        self.state.processing_audition =
            self.processing
                .audition
                .map(|id| crate::model::ProcessingAudition {
                    id,
                    output: is_graph.then_some(status.settings.graph_target).flatten(),
                    mode: is_graph.then_some(status.settings.graph_mode).flatten(),
                });
        Ok(())
    }
}
