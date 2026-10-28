use super::*;
use anyhow::ensure;
use std::sync::atomic::Ordering;

impl Core {
    pub(in crate::core) fn processing_bypass(&mut self) -> Result<()> {
        // Reserve the complete cutover before changing desired configuration.
        self.engine.ensure_command_capacity(3)?;
        if self.state.system.config.equalizer.enabled {
            self.equalizer_edit(EqualizerEdit::Enabled(false))?;
        }
        // Revoking audition intent does not cancel the rendering/export owner.
        self.processing.pending_audition = None;
        if let Some(id) = self.processing.audition
            && let Err(error) = self.audition_pcm(id, None)
        {
            self.stop()?;
            return Err(error);
        }
        Ok(())
    }

    pub(in crate::core) fn processing_graph_edit(
        &mut self,
        expected_revision: u64,
        edit: GraphEdit,
    ) -> Result<()> {
        let mut config = self.effective_config();
        config.processing_graph.edit(expected_revision, edit)?;
        self.commit_processing_config(config)
    }

    pub(in crate::core) fn processing_graph_run(
        &mut self,
        track_id: i64,
        target: GraphPort,
        mode: GraphExecutionMode,
    ) -> Result<u64> {
        let track = self.track(track_id)?;
        let request = crate::processing::GraphRequest {
            target,
            mode,
            export_path: None,
            source_run: None,
            cached_output: None,
        };
        let status = self
            .processing
            .start_graph(track, self.state.system.config.clone(), request);
        self.sync_processing_audio_availability();
        let status = status?;
        let id = status.id;
        self.state.system.processing = Some(status);
        if mode != GraphExecutionMode::Offline {
            self.processing.pending_audition = Some((id, self.generation));
        }
        Ok(id)
    }

    pub(in crate::core) fn processing_graph_audition(
        &mut self,
        id: u64,
        node: GraphPort,
    ) -> Result<Option<u64>> {
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
        let track = self.processing_source(result)?;
        if let Some(output) = result.graph_outputs.get(&node) {
            let pcm = output.pcm.clone();
            let track_id = result.track.id;
            self.establish_graph_source_context(track_id)?;
            self.audition_pcm(id, Some(pcm))?;
            self.state.processing_audition = Some(crate::projection::ProcessingAudition {
                id,
                output: Some(node),
                mode: Some(GraphExecutionMode::Offline),
            });
            return Ok(None);
        }
        let mut config = self.effective_config();
        config.processing_graph = graph;
        let request = crate::processing::GraphRequest {
            target: node,
            mode: GraphExecutionMode::Auto,
            export_path: None,
            source_run: None,
            cached_output: None,
        };
        let status = self
            .processing
            .start_graph(track, Arc::new(config), request);
        self.sync_processing_audio_availability();
        let status = status?;
        let task_id = status.id;
        self.processing.pending_audition = Some((status.id, self.generation));
        self.state.system.processing = Some(status);
        Ok(Some(task_id))
    }

    fn processing_graph_audition_prepared(
        &mut self,
        id: u64,
        mut runner: audio::GraphRunner,
    ) -> Result<()> {
        let status = self.processing_status(id).context("Unknown graph run")?;
        self.establish_graph_source_context(status.track_id)?;
        let pcm = status
            .settings
            .graph_boundary
            .map(|boundary| {
                let output = self
                    .processing
                    .results
                    .iter()
                    .find(|result| result.status.id == id)
                    .and_then(|result| result.graph_outputs.get(&boundary))
                    .context("Hybrid prefix is not ready")?;
                if let Some(context) = &output.context {
                    runner.set_source_context(
                        context.start_frame,
                        status
                            .frames
                            .context("Missing original graph source frame count")?,
                    )?;
                    Ok(context.pcm.clone())
                } else {
                    Ok::<_, anyhow::Error>(output.pcm.clone())
                }
            })
            .transpose()?;
        self.audition_pcm_with_graph(id, pcm, Some(runner))
    }

    fn establish_graph_source_context(&mut self, track_id: i64) -> Result<()> {
        self.ensure_queue_control_ready()?;
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
            self.store
                .queue_id_at(self.state.queue.total - 1)?
                .context("Graph source was not queued")?
        };
        self.start_with_status(queue_id, false, status)
    }

    pub(in crate::core) fn processing_graph_export(
        &mut self,
        id: u64,
        node: GraphPort,
        path: PathBuf,
    ) -> Result<u64> {
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
        let track = self.processing_source(result)?;
        let cached_output =
            result
                .graph_outputs
                .get(&node)
                .map(|output| crate::processing::CachedGraphOutput {
                    pcm: output.pcm.clone(),
                });
        let mut config = self.effective_config();
        config.processing_graph = graph;
        let request = crate::processing::GraphRequest {
            target: node,
            mode: GraphExecutionMode::Offline,
            export_path: Some(path),
            source_run: Some(id),
            cached_output,
        };
        let status = self
            .processing
            .start_graph(track, Arc::new(config), request)?;
        let task_id = status.id;
        self.state.system.processing = Some(status);
        Ok(task_id)
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
            .pending_audition
            .is_some_and(|(task, _)| task == id)
        {
            self.processing.pending_audition = None;
        }
        if selected {
            if let Err(error) = self.audition_pcm(id, None) {
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
            if !active {
                self.task_completed(TaskTarget::Processing { id });
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
            self.engine
                .send(AudioCommand::Equalizer(Arc::new(
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
                    use std::io::Read;
                    let file = rivu_format::input::open_regular_file(&path)?;
                    ensure!(file.metadata()?.len() <= 65536, "Preset exceeds 64 KiB");
                    let mut bytes = Vec::new();
                    file.take(65537).read_to_end(&mut bytes)?;
                    ensure!(bytes.len() <= 65536, "Preset exceeds 64 KiB");
                    let value: rivu_dsp::equalizer::config::EqualizerConfig =
                        serde_json::from_slice(&bytes)?;
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
    ) -> Result<u64> {
        let track = self.track(track_id)?;
        let status =
            self.processing
                .start(track, self.state.system.config.clone(), operation, profile);
        self.sync_processing_audio_availability();
        let status = status?;
        let id = status.id;
        self.state.system.processing = Some(status);
        Ok(id)
    }
    fn sync_processing_audio_availability(&mut self) {
        for result in &self.processing.results {
            for status in self
                .state
                .system
                .processing
                .iter_mut()
                .chain(self.processing_results.iter_mut())
                .filter(|status| status.id == result.status.id)
            {
                status.outputs.clone_from(&result.status.outputs);
                status
                    .graph_outputs
                    .clone_from(&result.status.graph_outputs);
            }
        }
    }

    pub(in crate::core) fn processing_status(&self, id: u64) -> Option<ProcessingTaskStatus> {
        self.processing_status_ref(id).cloned()
    }

    pub(in crate::core) fn processing_status_ref(&self, id: u64) -> Option<&ProcessingTaskStatus> {
        self.state
            .system
            .processing
            .as_ref()
            .filter(|status| status.id == id)
            .or_else(|| {
                self.processing_results
                    .iter()
                    .find(|status| status.id == id)
            })
            .or_else(|| {
                self.processing
                    .results
                    .iter()
                    .find(|result| result.status.id == id)
                    .map(|result| &result.status)
            })
    }

    fn retain_processing_terminal(&mut self, id: u64) {
        let Some(status) = self.state.system.processing.as_ref().filter(|s| s.id == id) else {
            return;
        };
        if let Some(result) = self
            .processing
            .results
            .iter_mut()
            .find(|r| r.status.id == id)
        {
            // Mix changes the task outcome, not the frozen processing recipe or
            // the identity of the retained source/stem/graph outputs.
            result.status.phase = status.phase;
            result.status.error.clone_from(&status.error);
        }
        if let Some(retained) = self.processing_results.iter_mut().find(|s| s.id == id) {
            retained.clone_from(status);
        } else {
            self.processing_results.push_back(status.clone());
            while self.processing_results.len() > MAX_RETAINED_SCAN_RESULTS {
                self.processing_results.pop_front();
            }
        }
    }

    pub(in crate::core) fn shutdown_processing(&mut self) {
        let active = self.processing.active.as_ref().map(|task| task.id);
        self.processing.shutdown();
        if let Some(id) = active {
            let retains_audio = self.processing.results.iter().any(|r| r.status.id == id);
            if let Some(status) = self.state.system.processing.as_mut().filter(|s| s.id == id) {
                status.phase = ProcessingPhase::Cancelled;
                status.error = Some("Processing cancelled during shutdown".into());
                if !retains_audio {
                    status.outputs = None;
                    status.graph_outputs.clear();
                }
            }
            self.retain_processing_terminal(id);
            self.task_completed(TaskTarget::Processing { id });
        }
    }

    fn processing_source(&self, result: &crate::processing::ProcessingResult) -> Result<Track> {
        ensure!(
            !self.database_unavailable,
            "Database outcome is uncertain; restart Rivu before publishing processing results"
        );
        let track = self.track(result.track.id)?;
        ensure!(
            track.path == result.track.path
                && track.cue == result.track.cue
                && track.decoder_hint == result.track.decoder_hint
                && track.fingerprint == result.track.fingerprint,
            "Processing result is stale: source identity changed before publication"
        );
        ensure!(
            crate::processing::SourceStamp::read(&track.path)? == result.stamp,
            "Processing result is stale: source file changed before publication"
        );
        Ok(track)
    }

    fn admit_processing_result(
        &self,
        result: &crate::processing::ProcessingResult,
        cancelled: bool,
    ) -> Result<()> {
        ensure!(!cancelled, "Processing cancelled");
        self.processing_source(result)?;
        Ok(())
    }

    fn retire_processing_result(&mut self, result: crate::processing::ProcessingResult) {
        for status in self
            .processing_results
            .iter_mut()
            .filter(|s| s.id == result.status.id)
        {
            status.outputs = None;
            status.graph_outputs.clear();
            status.error = Some("Output cache evicted; run processing again to audition".into());
        }
        self.processing.retire(Box::new(result));
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
                    if let Ok(result) = result {
                        self.processing.retire(result);
                    }
                    return;
                }
                let cancelled = self
                    .processing
                    .active
                    .as_ref()
                    .unwrap()
                    .cancelled
                    .load(Ordering::Acquire);
                let result = result.and_then(|result| {
                    if let Err(error) = self.admit_processing_result(&result, cancelled) {
                        self.processing.retire(result);
                        return Err(error);
                    }
                    while result.retains_audio() && self.processing.results.len() >= 4 {
                        let index = self
                            .processing
                            .results
                            .iter()
                            .position(|r| Some(r.status.id) != self.processing.audition)
                            .unwrap();
                        let evicted = self.processing.results.remove(index).unwrap();
                        self.retire_processing_result(evicted);
                    }
                    Ok(result)
                });
                let task = self.processing.active.take().unwrap();
                let _ = task.worker.join();
                let result = result.and_then(|mut result| {
                    if let Some((temporary, destination)) = result.pending_export.take() {
                        if let Err(error) = self
                            .processing
                            .export_destination(&destination)
                            .and_then(|canonical| {
                                ensure!(
                                    canonical == destination,
                                    "Export destination parent changed after admission"
                                );
                                Ok(())
                            })
                        {
                            self.processing.retire(result);
                            return Err(error);
                        }
                        if let Err(error) = temporary.persist_noclobber(&destination) {
                            self.processing.retire(result);
                            return Err(error.error.into());
                        }
                        result.status.graph_exported_path = Some(destination);
                    }
                    Ok(result)
                });
                match result {
                    Ok(mut result) => {
                        if !result.retains_audio() {
                            // Export receipts do not advertise retired PCM.
                            result.status.outputs = None;
                            result.status.graph_outputs.clear();
                            self.state.system.processing = Some(result.status.clone());
                            self.processing.retire(result);
                        } else {
                            self.state.system.processing = Some(result.status.clone());
                            self.processing.results.push_back(*result);
                        }
                        if let Some((pending_id, generation)) = self.processing.pending_audition
                            && pending_id == id
                        {
                            self.processing.pending_audition = None;
                            let outcome = if generation != self.generation {
                                Ok(())
                            } else if let Some(runner) =
                                self.processing.results.back_mut().unwrap().runner.take()
                            {
                                self.processing_graph_audition_prepared(id, runner)
                            } else {
                                let target = self
                                    .state
                                    .system
                                    .processing
                                    .as_ref()
                                    .and_then(|status| status.settings.graph_target);
                                target.map_or(Ok(()), |target| {
                                    self.processing_graph_audition(id, target).map(|_| ())
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
                            }
                        }
                    }
                    Err(error) => {
                        if self
                            .processing
                            .pending_audition
                            .is_some_and(|(task, _)| task == id)
                        {
                            self.processing.pending_audition = None;
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
                self.retain_processing_terminal(id);
                self.task_completed(TaskTarget::Processing { id });
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
                    if let Ok(pcm) = result {
                        self.processing.retire_pcm(pcm);
                    }
                    return;
                }
                let task = self.processing.active.take().unwrap();
                let cancelled = task.cancelled.load(Ordering::Acquire);
                let _ = task.worker.join();
                let requested = self.processing.pending_audition == Some((id, generation));
                if self
                    .processing
                    .pending_audition
                    .is_some_and(|(task, _)| task == id)
                {
                    self.processing.pending_audition = None;
                }
                let outcome = match result {
                    Ok(pcm) if !cancelled && (!requested || generation == self.generation) => {
                        let outcome = (|| -> Result<()> {
                            let source = self
                                .processing
                                .results
                                .iter()
                                .find(|r| r.status.id == id)
                                .context("Processing result is no longer retained")?;
                            self.processing_source(source)?;
                            if requested {
                                self.audition_pcm(id, Some(pcm.clone()))?;
                            }
                            Ok(())
                        })();
                        if outcome.is_ok() {
                            if let Some(result) = self
                                .processing
                                .results
                                .iter_mut()
                                .find(|r| r.status.id == id)
                            {
                                if let Some(old) = result.mix.replace(pcm) {
                                    self.processing.retire_pcm(old);
                                }
                            } else {
                                self.processing.retire_pcm(pcm);
                            }
                        } else {
                            self.processing.retire_pcm(pcm);
                        }
                        outcome
                    }
                    Ok(pcm) => {
                        self.processing.retire_pcm(pcm);
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
                self.retain_processing_terminal(id);
                self.task_completed(TaskTarget::Processing { id });
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
    ) -> Result<Option<u64>> {
        let result = self
            .processing
            .results
            .iter()
            .find(|r| r.status.id == id)
            .context("Processing result is no longer retained")?;
        let track = self.processing_source(result)?;
        let outputs = &result.outputs;
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
            self.processing.pending_audition = Some((id, self.generation));
            status.phase = ProcessingPhase::Processing;
            status.error = None;
            self.state.system.processing = Some(status);
            return Ok(Some(id));
        }
        let result = self
            .processing
            .results
            .iter()
            .find(|r| r.status.id == id)
            .context("Processing result is no longer retained")?;
        let outputs = &result.outputs;
        let pcm = match mode {
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
        self.audition_pcm(id, pcm).map(|_| None)
    }
    fn audition_pcm(&mut self, id: u64, pcm: Option<audio::PcmAudio>) -> Result<()> {
        self.audition_pcm_with_graph(id, pcm, None)
    }

    fn audition_pcm_with_graph(
        &mut self,
        id: u64,
        pcm: Option<audio::PcmAudio>,
        graph: Option<audio::GraphRunner>,
    ) -> Result<()> {
        ensure!(
            !self.database_unavailable,
            "Database outcome is uncertain; restart Rivu before publishing processing results"
        );
        self.ensure_queue_control_ready()?;
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
        let admitted_source =
            if let Some(result) = self.processing.results.iter().find(|r| r.status.id == id) {
                self.processing_source(result)?;
                result.stamp
            } else {
                crate::processing::SourceStamp::read(&track.path)?
            };
        ensure!(
            track.path == status.source_path
                && track.cue == status.source_cue
                && self
                    .processing
                    .results
                    .iter()
                    .find(|result| result.status.id == id)
                    .is_none_or(|result| track.decoder_hint == result.track.decoder_hint)
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
        // PCM is immutable; graph admission still validates the original source.
        let expected_identity = graph.as_ref().map(|_| admitted_source.identity());
        self.engine.ensure_command_capacity(2)?;
        self.processing.pending_audition = None;
        let generation = self.generation;
        let mut bookkeeping = Ok(());
        let mut stop = self.engine.stop_request();
        let snapshot = futures::executor::block_on(stop.wait(|event| {
            if bookkeeping.is_ok() {
                bookkeeping = self.retiring_audio_event(generation, event);
            }
        }));
        self.engine.restore_events(stop.into_events());
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
        let original = pcm.is_none();
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
        let path = track.path.clone();
        let range = track.cue.as_ref().map(|cue| audio::PlaybackRange {
            start_seconds: cue.start_seconds(),
            end_seconds: cue.end_seconds(),
        });
        let decoder_hint = track.decoder_hint;
        let command = if let Some(runner) = graph {
            AudioCommand::GraphAudition {
                path,
                pcm,
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
                pcm,
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
        if let Err(error) = self.engine.send(command) {
            return self.finish_playback(Ok(None), false, Err(error));
        }
        self.processing.audition = (!original || is_graph).then_some(id);
        self.processing.audition_generation = self.processing.audition.map(|_| self.generation);
        self.state.processing_audition =
            self.processing
                .audition
                .map(|id| crate::projection::ProcessingAudition {
                    id,
                    output: is_graph.then_some(status.settings.graph_target).flatten(),
                    mode: is_graph.then_some(status.settings.graph_mode).flatten(),
                });
        Ok(())
    }
}
