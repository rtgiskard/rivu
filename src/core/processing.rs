use super::*;
use anyhow::ensure;
use command::CommandExecution;

pub(super) struct PendingProcessingResume {
    id: u64,
    generation: u64,
    pub(super) reply: Option<oneshot::Sender<CoreResponse>>,
}

#[derive(Default)]
pub(super) struct NeuralActivations {
    waiting: VecDeque<(NeuralActivation, Option<oneshot::Sender<CoreResponse>>)>,
    running: Option<NeuralActivationTask>,
}

struct NeuralActivationTask {
    worker: async_executor::Task<Result<CheckedNeuralActivation>>,
    reply: Option<oneshot::Sender<CoreResponse>>,
}

struct SelectedWorker {
    worker: PathBuf,
    other: PathBuf,
}

pub(super) struct CheckedNeuralActivation {
    speech: bool,
    manifest: PathBuf,
    runtime: Option<NeuralRuntime>,
    baseline: Option<SelectedWorker>,
    coexistence: Result<()>,
}

impl NeuralActivations {
    pub(super) async fn next_finished(&mut self) -> Result<CheckedNeuralActivation> {
        match self.running.as_mut() {
            Some(task) => (&mut task.worker).await,
            None => std::future::pending().await,
        }
    }
}

fn neural_manifest(path: &Path) -> Result<serde_json::Value> {
    use std::io::Read;

    const LIMIT: usize = 1024 * 1024;
    let mut contents = Vec::new();
    rivu_utils::regular_file::open_regular_file(path)?
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut contents)?;
    ensure!(contents.len() <= LIMIT, "Model manifest exceeds 1 MiB");
    serde_json::from_slice(&contents).context("Decoding model manifest")
}

impl CheckedNeuralActivation {
    fn check(
        speech: bool,
        manifest: PathBuf,
        runtime: Option<NeuralRuntime>,
        baseline: Option<SelectedWorker>,
    ) -> Result<Self> {
        let metadata = neural_manifest(&manifest)
            .with_context(|| format!("Reading prepared model manifest {}", manifest.display()))?;
        ensure!(
            metadata["operation"] == if speech { "speech" } else { "separate" },
            "Prepared model operation does not match activation target"
        );
        let mut coexistence = Ok(());
        if let Some(runtime) = &runtime {
            ensure!(
                metadata.get("backend").is_none() || metadata["backend"] == "python",
                "Prepared model backend does not match Python activation"
            );
            let worker = runtime
                .worker
                .canonicalize()
                .context("Resolving prepared worker")?;
            ensure!(worker.is_file(), "Prepared worker is not a regular file");
            let manifest_path = manifest
                .canonicalize()
                .context("Resolving prepared manifest")?;
            let interpreter = std::path::absolute(&runtime.interpreter)?;
            let bound = &metadata["runtime"];
            ensure!(
                bound["worker"].as_str().map(Path::new) == Some(worker.as_path())
                    && bound["model_manifest"].as_str().map(Path::new)
                        == Some(manifest_path.as_path())
                    && bound["interpreter"].as_str().map(Path::new) == Some(interpreter.as_path()),
                "Activation runtime differs from the provisioned model runtime"
            );
            if let Some(baseline) = &baseline
                && baseline.worker != runtime.worker
                && !baseline
                    .worker
                    .canonicalize()
                    .is_ok_and(|selected| selected == worker)
            {
                coexistence = match neural_manifest(&baseline.other).with_context(|| {
                    format!(
                        "Reading other selected model manifest {}",
                        baseline.other.display()
                    )
                }) {
                    Ok(other) if other["backend"] == "onnx" => Ok(()),
                    Ok(_) => Err(anyhow::anyhow!(
                        "Activation would replace the shared worker and invalidate the other selected Python model ({}). Leave this bundle prepared or explicitly clear the other model first",
                        baseline.other.display()
                    )),
                    Err(error) => Err(error),
                };
            }
        } else {
            ensure!(
                metadata["backend"] == "onnx",
                "Prepared model backend does not match native speech activation"
            );
        }
        Ok(Self {
            speech,
            manifest,
            runtime,
            baseline,
            coexistence,
        })
    }
}

fn other_neural_manifest(
    neural: &rivu_format::config::NeuralConfig,
    speech: bool,
) -> Option<&Path> {
    if speech {
        neural.separation_manifest.as_deref()
    } else {
        neural.speech_manifest.as_deref()
    }
}

const MAX_NEURAL_ACTIVATIONS: usize = rivu_protocol::wire::MAX_CLIENTS;

impl Core {
    pub(in crate::core) fn toggle_processing(
        &mut self,
        target: ProcessingToggle,
    ) -> Result<CommandExecution> {
        if target == ProcessingToggle::Bypass {
            let bypassed = self.preferences.settings().processing_bypassed
                && self.pending_processing_resume.is_none();
            return self.processing_bypass(!bypassed);
        }
        let mut config = self.preferences.checkpoint();
        let enabled = match target {
            ProcessingToggle::Bypass => unreachable!(),
            ProcessingToggle::Hum => &mut config.denoise.hum.enabled,
            ProcessingToggle::Impulse => &mut config.denoise.impulse.enabled,
            ProcessingToggle::SpeechPostFilter => &mut config.neural.speech_post_filter,
        };
        *enabled = !*enabled;
        self.commit_processing_config(config)?;
        Ok(CommandExecution::Complete(CommandOutput::None))
    }

    pub(in crate::core) fn processing_bypass(
        &mut self,
        bypassed: bool,
    ) -> Result<CommandExecution> {
        if bypassed {
            self.cancel_pending_processing_resume("Audition resume superseded by enabling bypass");
        }
        if self.preferences.settings().processing_bypassed == bypassed {
            return Ok(CommandExecution::Complete(CommandOutput::None));
        }
        let selected = self.processing.audition_projection();
        if selected.is_some() {
            self.ensure_queue_control_ready()?;
        }
        let monitor_enabled = self.preferences.settings().equalizer.enabled;
        self.engine.ensure_command_capacity(
            usize::from(monitor_enabled) + 2 * usize::from(selected.is_some()),
        )?;
        if let Some(selection) = selected.as_ref()
            && !bypassed
        {
            self.validate_resume_target(selection.id)?;
            if self.processing.prepare_resume()? {
                // Reuse any eligible preparation for the same selection, but
                // only the latest command owns the pending receipt.
                self.reject_pending_processing_resume(
                    "Audition resume superseded by a newer request",
                );
                self.pending_processing_resume = Some(PendingProcessingResume {
                    id: selection.id,
                    generation: self.transport.generation,
                    reply: None,
                });
                return Ok(CommandExecution::DeferredProcessingResume);
            }
        }
        let mut config = self.preferences.checkpoint();
        config.processing_bypassed = bypassed;
        if let Some(selection) = selected {
            let pcm = if bypassed {
                None
            } else {
                self.processing.audition_pcm()
            };
            self.commit_processing_bypass_source(selection.id, pcm, &mut None, config)?;
        } else {
            let config = Arc::new(config);
            self.write_config(&config)?;
            self.preferences.accept(config);
            if monitor_enabled {
                self.engine
                    .send(AudioCommand::Equalizer(Arc::new(self.playback_equalizer())))?;
            }
        }
        // Background work retains its resources but loses the right to start audio.
        self.pending_processing_audition = None;
        Ok(CommandExecution::Complete(CommandOutput::None))
    }

    fn reject_pending_processing_resume(&mut self, reason: &str) {
        if let Some(pending) = self.pending_processing_resume.take() {
            self.reply_to_command(pending.reply, Err(anyhow::anyhow!("{reason}")));
        }
    }

    pub(in crate::core) fn cancel_pending_processing_resume(&mut self, reason: &str) {
        self.processing.cancel_resume();
        self.reject_pending_processing_resume(reason);
    }

    pub(in crate::core) fn clear_processing_audition(&mut self, reason: &str) {
        self.processing.clear_audition();
        self.reject_pending_processing_resume(reason);
    }

    fn validate_resume_target(&mut self, id: u64) -> Result<()> {
        let valid = self
            .processing
            .results()
            .iter()
            .find(|result| result.id == id)
            .context("Resume target is no longer retained")
            .and_then(|result| {
                let track = self.processing_source(&result.track, result.stamp)?;
                ensure!(
                    self.state.current_track.as_ref().is_some_and(|current| {
                        current.id == track.id
                            && current.path == track.path
                            && current.cue == track.cue
                            && current.decoder_hint == track.decoder_hint
                    }),
                    "Resume target does not belong to the current source"
                );
                Ok(())
            });
        if let Err(error) = valid {
            self.clear_processing_audition("Audition resume cancelled: target is stale");
            return Err(error.context("Resume target is stale; bypass remains enabled"));
        }
        Ok(())
    }

    fn commit_processing_bypass_source(
        &mut self,
        id: u64,
        pcm: Option<audio::PcmAudio>,
        runner: &mut Option<Box<audio::GraphRunner>>,
        config: Config,
    ) -> Result<()> {
        let bypassed = config.processing_bypassed;
        // Capacity and persistence failures do not revoke selection.
        self.replace_processing_source(id, pcm, runner, Some(config))?;
        self.processing
            .retain_audition_for_generation(self.transport.generation);
        if !bypassed {
            self.transport.processing_resume_generation = Some(self.transport.generation);
        }
        Ok(())
    }

    fn processing_resume_prepared(
        &mut self,
        id: u64,
        generation: u64,
        result: Result<Box<audio::GraphRunner>>,
    ) {
        let eligible = self.processing.finish_resume(id, generation);
        if eligible
            && self
                .pending_processing_resume
                .as_ref()
                .is_some_and(|pending| (id, generation) == (pending.id, pending.generation))
        {
            let reply = self.pending_processing_resume.take().unwrap().reply;
            let outcome = self.commit_prepared_processing_resume(id, generation, result);
            self.complete_background(reply, outcome);
            return;
        }
        if let Ok(runner) = result {
            self.processing.retire_runner(*runner);
        }
        if let Some(pending) = &self.pending_processing_resume {
            // A newer intent may be waiting for a cancelled worker to retire.
            let retry = self
                .ensure_current_processing_resume(pending.id, pending.generation)
                .and_then(|()| {
                    ensure!(
                        self.processing.prepare_resume()?,
                        "Missing live audition to resume"
                    );
                    Ok(())
                });
            if let Err(error) = retry {
                let reply = self.pending_processing_resume.take().unwrap().reply;
                self.complete_background(reply, Err(error));
            }
        }
    }

    fn ensure_current_processing_resume(&self, id: u64, generation: u64) -> Result<()> {
        ensure!(
            generation == self.transport.generation
                && self.preferences.settings().processing_bypassed
                && self.processing.audition_selected(id, generation),
            "Audition resume target is no longer selected"
        );
        Ok(())
    }

    fn commit_prepared_processing_resume(
        &mut self,
        id: u64,
        generation: u64,
        result: Result<Box<audio::GraphRunner>>,
    ) -> Result<()> {
        let valid = self
            .ensure_current_processing_resume(id, generation)
            .and_then(|()| self.validate_resume_target(id));
        let mut runner = match result {
            Ok(runner) => Some(runner),
            Err(error) => {
                valid?;
                self.processing.clear_audition();
                return Err(error.context("Resume target is unavailable; bypass remains enabled"));
            }
        };
        let outcome = valid.and_then(|()| {
            let mut config = self.preferences.checkpoint();
            config.processing_bypassed = false;
            self.commit_processing_bypass_source(
                id,
                self.processing.audition_pcm(),
                &mut runner,
                config,
            )
        });
        if let Some(runner) = runner {
            self.processing.retire_runner(*runner);
        }
        outcome
    }

    pub(in crate::core) fn playback_equalizer(
        &self,
    ) -> rivu_dsp::equalizer::config::EqualizerConfig {
        let mut equalizer = self.preferences.settings().equalizer.clone();
        if self.preferences.settings().processing_bypassed {
            equalizer.enabled = false;
        }
        equalizer
    }

    pub(in crate::core) fn processing_graph_edit(
        &mut self,
        expected_revision: u64,
        edit: GraphEdit,
    ) -> Result<()> {
        let config = self
            .preferences
            .checkpoint()
            .with_processing_graph_edit(expected_revision, edit)?;
        self.commit_validated_processing_config(config)
    }

    pub(in crate::core) fn processing_graph_run(
        &mut self,
        track_id: i64,
        target: GraphPort,
        mode: GraphExecutionMode,
    ) -> Result<u64> {
        let track = self.track(track_id)?;
        let request = rivu_processing::GraphRequest {
            target,
            mode,
            export_path: None,
            source_run: None,
            cached_output: None,
            audio_export: None,
        };
        let id =
            self.processing
                .start_graph(track, self.preferences.settings().clone(), request)?;
        if mode != GraphExecutionMode::Offline && !self.preferences.settings().processing_bypassed {
            self.pending_processing_audition = Some((id, self.transport.generation));
        }
        Ok(id)
    }

    pub(in crate::core) fn processing_graph_audition(
        &mut self,
        id: u64,
        node: GraphPort,
    ) -> Result<Option<u64>> {
        ensure!(
            !self.preferences.settings().processing_bypassed,
            "Playback processing is bypassed; turn bypass off before auditioning"
        );
        let result = self
            .processing
            .results()
            .iter()
            .find(|r| r.id == id)
            .context("Graph run is no longer retained")?;
        let graph = self
            .processing
            .status(result.id)
            .context("Graph run status is no longer retained")?
            .settings
            .graph
            .clone()
            .context("Not a graph run")?;
        let track = self.processing_source(&result.track, result.stamp)?;
        if let Some(output) = result.graph_outputs.get(&node) {
            graph.ancestors(node)?;
            let pcm = output.pcm.clone();
            self.establish_processing_source_context(&track)?;
            self.select_cached_processing_audition(id, pcm, Some(node))
                .map_err(|(_, error)| error)?;
            return Ok(None);
        }
        let mut config = self.preferences.checkpoint();
        config.processing_graph = graph;
        let request = rivu_processing::GraphRequest {
            target: node,
            mode: GraphExecutionMode::Auto,
            export_path: None,
            source_run: None,
            cached_output: None,
            audio_export: None,
        };
        let task_id = self
            .processing
            .start_graph(track, Arc::new(config), request)?;
        self.pending_processing_audition = Some((task_id, self.transport.generation));
        Ok(Some(task_id))
    }

    fn processing_graph_audition_prepared(
        &mut self,
        id: u64,
        mut runner: audio::GraphRunner,
    ) -> Result<()> {
        let status = self.processing_status(id).context("Unknown graph run")?;
        let result = self
            .processing
            .results()
            .iter()
            .find(|result| result.id == id)
            .context("Graph run is no longer retained")?;
        let track = self.processing_source(&result.track, result.stamp)?;
        self.establish_processing_source_context(&track)?;
        let pcm = status
            .settings
            .graph_boundary
            .map(|boundary| {
                let output = self
                    .processing
                    .results()
                    .iter()
                    .find(|result| result.id == id)
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
        self.select_live_processing_audition(id, pcm, runner)
    }

    fn establish_processing_source_context(&mut self, source: &Track) -> Result<()> {
        self.ensure_queue_control_ready()?;
        let same_track = self
            .state
            .current_track
            .as_ref()
            .is_some_and(|track| track.id == source.id);
        let same_loaded_source = self.transport.session.as_ref().is_some_and(|session| {
            let loaded = &session.loaded_track;
            loaded.id == source.id
                && loaded.path == source.path
                && loaded.cue == source.cue
                && loaded.decoder_hint == source.decoder_hint
                && loaded.fingerprint == source.fingerprint
        });
        if same_loaded_source && self.state.playback.status != PlaybackStatus::Stopped {
            return Ok(());
        }
        ensure!(
            !self.database_maintenance_active(),
            "Database maintenance is in progress; select the prepared audition again after it finishes"
        );
        self.engine.ensure_command_capacity(4)?;
        let status = if self.state.playback.status == PlaybackStatus::Playing {
            PlaybackStatus::Playing
        } else {
            PlaybackStatus::Paused
        };
        let queue_id = if same_track {
            self.state
                .queue
                .current_id
                .context("Current source has no queue occurrence")?
        } else if let Some(id) = self.store.queued_track(source.id)? {
            id
        } else {
            self.enqueue(&[source.id])?;
            self.store
                .queue_id_at(self.state.queue.total - 1)?
                .context("Processing source was not queued")?
        };
        let history = if same_track {
            QueueHistory::Preserve
        } else {
            QueueHistory::Record
        };
        self.start_with_status(queue_id, history, status)
    }

    pub(in crate::core) fn audio_export(&mut self, request: AudioExportRequest) -> Result<u64> {
        request.encoding.validate()?;
        if let Some(eq) = &request.monitor_eq {
            eq.validate()?;
        }
        let mut config = self.preferences.checkpoint();
        let (track, target, source_run, cached_output) = match request.source {
            AudioExportSource::Original { track_id } => {
                config.processing_graph = ProcessingGraph::default();
                (
                    self.track(track_id)?,
                    GraphPort { node: 1, port: 0 },
                    None,
                    None,
                )
            }
            AudioExportSource::Graph {
                track_id,
                target,
                revision,
            } => {
                ensure!(
                    config.processing_graph.revision == revision,
                    "Processing graph changed before export admission"
                );
                (self.track(track_id)?, target, None, None)
            }
            AudioExportSource::Frozen { id, target } => {
                let result = self
                    .processing
                    .results()
                    .iter()
                    .find(|r| r.id == id)
                    .context("Graph run is no longer retained")?;
                config.processing_graph = self
                    .processing
                    .status(id)
                    .context("Graph run status is no longer retained")?
                    .settings
                    .graph
                    .clone()
                    .context("Not a graph run")?;
                let track = self.processing_source(&result.track, result.stamp)?;
                let cached = result.graph_outputs.get(&target).map(|output| {
                    rivu_processing::CachedGraphOutput {
                        pcm: output.pcm.clone(),
                        stamp: result.stamp,
                    }
                });
                (track, target, Some(id), cached)
            }
        };
        let graph_request = rivu_processing::GraphRequest {
            target,
            mode: GraphExecutionMode::Offline,
            export_path: Some(request.path.clone()),
            source_run,
            cached_output,
            audio_export: Some(request),
        };
        self.processing
            .start_graph(track, Arc::new(config), graph_request)
    }

    pub(in crate::core) fn cancel_processing(&mut self, id: u64) -> Result<()> {
        let active = self.processing.active_id() == Some(id);
        let selected = self
            .processing
            .audition_selected(id, self.transport.generation);
        ensure!(
            active || selected,
            "No matching active processing task or live audition"
        );
        if active {
            self.processing.cancel(id)?;
        }
        self.take_pending_processing_audition(id);
        if selected {
            if let Err(error) = self.restore_original_processing_source(id) {
                self.stop()?;
                return Err(error);
            }
            self.processing.stop_live_audition(id);
            if !active {
                self.task_completed(TaskTarget::Processing { id });
            }
        }
        Ok(())
    }
    fn commit_processing_config(&mut self, config: Config) -> Result<()> {
        self.commit_validated_processing_config(config.into_validated()?)
    }

    fn commit_validated_processing_config(
        &mut self,
        config: crate::config::ValidatedConfig,
    ) -> Result<()> {
        let candidate = config.config();
        let equalizer_changed = !candidate.processing_bypassed
            && candidate.equalizer != self.preferences.settings().equalizer;
        if equalizer_changed {
            self.engine.ensure_command_capacity(1)?;
        }
        self.write_validated_config(&config)?;
        self.preferences.accept(config.into_config());
        if equalizer_changed {
            self.engine
                .send(AudioCommand::Equalizer(Arc::new(
                    self.preferences.settings().equalizer.clone(),
                )))
                .context("Configuration saved, but equalizer delivery failed")?;
        }
        Ok(())
    }
    pub(in crate::core) fn equalizer_edit(
        &mut self,
        expected_revision: u64,
        edit: EqualizerEdit,
    ) -> Result<()> {
        ensure!(
            expected_revision == self.state.revisions.configuration,
            "Equalizer configuration changed; refresh before editing"
        );
        use EqualizerEdit::*;
        let mut config = self.preferences.checkpoint();
        let sample_rate = self.state.current_track.as_ref().and_then(|track| {
            let (rate, _) = self
                .state
                .playback
                .source_pcm_format(self.transport.generation, track);
            (rate > 0).then_some(rate)
        });
        if let Some(edit) = edit.apply_config(&mut config.equalizer, sample_rate)? {
            match edit {
                SavePreset { name, overwrite } => {
                    let existing = config.equalizer_presets.iter_mut().find(|p| p.name == name);
                    ensure!(
                        existing.is_some() == overwrite,
                        if overwrite {
                            "Unknown equalizer preset"
                        } else {
                            "Equalizer preset already exists"
                        }
                    );
                    if let Some(preset) = existing {
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
                ImportPreset {
                    path,
                    name,
                    overwrite,
                } => {
                    let existing = config.equalizer_presets.iter_mut().find(|p| p.name == name);
                    ensure!(
                        existing.is_some() == overwrite,
                        if overwrite {
                            "Unknown equalizer preset"
                        } else {
                            "Equalizer preset already exists"
                        }
                    );
                    use std::io::Read;
                    let (file, metadata) =
                        rivu_utils::regular_file::open_regular_file_with_metadata(&path)?;
                    ensure!(metadata.len() <= 65536, "Preset exceeds 64 KiB");
                    let mut bytes = Vec::new();
                    file.take(65537).read_to_end(&mut bytes)?;
                    ensure!(bytes.len() <= 65536, "Preset exceeds 64 KiB");
                    let value: rivu_dsp::equalizer::config::EqualizerConfig =
                        serde_json::from_slice(&bytes)?;
                    value.validate()?;
                    if let Some(preset) = existing {
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
                    let bytes = serde_json::to_vec_pretty(&preset.config)?;
                    let parent = path
                        .parent()
                        .filter(|parent| !parent.as_os_str().is_empty())
                        .unwrap_or_else(|| Path::new("."));
                    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
                    temporary.write_all(&bytes)?;
                    temporary
                        .persist_noclobber(&path)
                        .map_err(|error| error.error)?;
                    return Ok(());
                }
                _ => unreachable!("parameter edits are handled by apply_config"),
            }
        }
        self.commit_processing_config(config)
    }
    pub(in crate::core) fn denoise_edit(&mut self, patch: DenoisePatch) -> Result<()> {
        let mut config = self.preferences.checkpoint();
        let target = &mut config.denoise;
        macro_rules! apply { ($($field:ident),*)=>{$(if let Some(v)=patch.$field {target.$field=v;})*}; }
        apply!(
            mode,
            strength,
            spectral_gain_floor_db,
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
        let mut config = self.preferences.checkpoint();
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

    pub(super) fn queue_neural_activation(
        &mut self,
        activation: NeuralActivation,
        reply: Option<oneshot::Sender<CoreResponse>>,
    ) {
        let pending = &mut self.neural_activations;
        if pending.waiting.len() + pending.running.is_some() as usize == MAX_NEURAL_ACTIVATIONS {
            self.complete_background(
                reply,
                Err(anyhow::anyhow!("Too many pending model activations")),
            );
            return;
        }
        pending.waiting.push_back((activation, reply));
        self.start_neural_activation();
    }

    fn start_neural_activation(&mut self) {
        let pending = &mut self.neural_activations;
        if pending.running.is_some() {
            return;
        }
        let Some((activation, reply)) = pending.waiting.pop_front() else {
            return;
        };
        let (speech, manifest, runtime) = match activation {
            NeuralActivation::Speech { manifest, runtime } => (true, manifest, runtime),
            NeuralActivation::Separation { manifest, runtime } => (false, manifest, Some(runtime)),
        };
        let baseline = runtime.as_ref().and_then(|_| {
            let current = &self.preferences.settings().neural;
            other_neural_manifest(current, speech).map(|other| SelectedWorker {
                worker: current.worker.clone(),
                other: other.to_path_buf(),
            })
        });
        let workers = self.workers.clone();
        let worker = self.executor.spawn(async move {
            workers
                .execute(move || {
                    CheckedNeuralActivation::check(speech, manifest, runtime, baseline)
                })
                .await?
        });
        self.neural_activations.running = Some(NeuralActivationTask { worker, reply });
    }

    pub(super) fn finish_neural_activation(&mut self, result: Result<CheckedNeuralActivation>) {
        let task = self
            .neural_activations
            .running
            .take()
            .expect("active model activation");
        let result = result.and_then(|checked| self.commit_neural_activation(checked));
        self.complete_background(task.reply, result);
        self.start_neural_activation();
    }

    pub(super) fn retire_neural_activations(&mut self) {
        if let Some(task) = self.neural_activations.running.take() {
            let _ = futures::executor::block_on(
                std::panic::AssertUnwindSafe(task.worker).catch_unwind(),
            );
            self.reply_to_command(
                task.reply,
                Err(anyhow::anyhow!("Core stopped before model activation")),
            );
        }
        while let Some((_, reply)) = self.neural_activations.waiting.pop_front() {
            self.reply_to_command(
                reply,
                Err(anyhow::anyhow!("Core stopped before model activation")),
            );
        }
    }

    fn commit_neural_activation(&mut self, checked: CheckedNeuralActivation) -> Result<()> {
        let CheckedNeuralActivation {
            speech,
            manifest,
            runtime,
            baseline,
            coexistence,
        } = checked;
        if let Some(runtime) = &runtime {
            let current = &self.preferences.settings().neural;
            if current.worker != runtime.worker
                && let Some(other) = other_neural_manifest(current, speech)
            {
                ensure!(
                    baseline.is_some_and(|baseline| {
                        current.worker == baseline.worker && other == baseline.other.as_path()
                    }),
                    "Other selected model or shared worker changed during activation. Try activation again"
                );
                coexistence?;
            }
        }

        let mut config = self.preferences.checkpoint();
        let target = &mut config.neural;
        let (selected, interpreter) = if speech {
            (&mut target.speech_manifest, &mut target.speech_interpreter)
        } else {
            (
                &mut target.separation_manifest,
                &mut target.separation_interpreter,
            )
        };
        *selected = Some(manifest);
        if let Some(runtime) = runtime {
            target.worker = runtime.worker;
            *interpreter = runtime.interpreter;
        }
        self.commit_processing_config(config)
    }

    pub(in crate::core) fn start_processing(
        &mut self,
        track_id: i64,
        operation: ProcessingOperation,
        profile: Option<NoiseSelection>,
    ) -> Result<u64> {
        let track = self.track(track_id)?;
        self.processing.start(
            track,
            self.preferences.settings().clone(),
            operation,
            profile,
        )
    }
    pub(in crate::core) fn processing_status(&self, id: u64) -> Option<ProcessingTaskStatus> {
        self.processing.status(id).cloned()
    }

    fn take_pending_processing_audition(&mut self, id: u64) -> Option<u64> {
        let (pending_id, generation) = self.pending_processing_audition?;
        if pending_id != id {
            return None;
        }
        self.pending_processing_audition = None;
        Some(generation)
    }

    fn complete_processing_task(&mut self, id: u64) {
        self.processing.retain_terminal(id);
        self.task_completed(TaskTarget::Processing { id });
    }

    pub(in crate::core) fn shutdown_processing(&mut self) {
        self.cancel_pending_processing_resume("Audition resume cancelled during shutdown");
        let active = self.processing.active_id();
        self.processing.shutdown();
        if let Some(id) = active {
            self.complete_processing_task(id);
        }
    }

    fn processing_source(
        &self,
        source: &Track,
        stamp: rivu_processing::SourceStamp,
    ) -> Result<Track> {
        ensure!(
            !self.database_unavailable,
            "Database outcome is uncertain; restart Rivu before publishing processing results"
        );
        let track = self.track(source.id)?;
        ensure!(
            track.path == source.path
                && track.cue == source.cue
                && track.decoder_hint == source.decoder_hint
                && track.fingerprint == source.fingerprint,
            "Processing result is stale: source identity changed before publication"
        );
        ensure!(
            rivu_processing::SourceStamp::read(&track.path)? == stamp,
            "Processing result is stale: source file changed before publication"
        );
        Ok(track)
    }

    fn publish_processing_result(
        &self,
        result: &mut rivu_processing::ProcessingResult,
        cancelled: bool,
    ) -> Result<()> {
        ensure!(!cancelled, "Processing cancelled");
        self.processing_source(&result.track, result.stamp)?;
        if let Some((_, destination)) = &result.pending_export {
            ensure!(
                self.processing.export_destination(destination)? == *destination,
                "Export destination parent changed after admission"
            );
        }
        if let Some((temporary, destination)) = result.pending_export.take() {
            if let Err(error) = temporary.persist_noclobber(&destination) {
                result.pending_export = Some((error.file, destination));
                return Err(error.error.into());
            }
            result.status.graph_exported_path = Some(destination);
        }
        Ok(())
    }

    pub(in crate::core) fn processing_event(&mut self, event: rivu_processing::Event) {
        use rivu_processing::Event;
        match event {
            Event::GraphPrepared {
                id,
                mode,
                boundary,
                plan,
            } => self.processing.graph_prepared(id, mode, boundary, plan),
            Event::Model { id, provenance } => self.processing.model_provenance(id, provenance),
            Event::GraphNode { id, status } => self.processing.node_status(id, status),
            Event::Progress {
                id,
                completed,
                total,
                phase,
            } => self.processing.progress(id, completed, total, phase),
            Event::ResumePrepared {
                id,
                generation,
                result,
            } => return self.processing_resume_prepared(id, generation, result),
            Event::Finished { id, result } => {
                let Some(cancelled) = self.processing.finish_task(id) else {
                    if let Ok(result) = result {
                        self.processing.retire(result);
                    }
                    return;
                };
                let abort_phase = if cancelled {
                    ProcessingPhase::Cancelled
                } else {
                    ProcessingPhase::Failed
                };
                let published = match result {
                    Ok(mut result) => {
                        match self.publish_processing_result(&mut result, cancelled) {
                            Ok(()) => {
                                self.processing.accept_result(result);
                                true
                            }
                            Err(error) => {
                                self.processing
                                    .reject_publication(result, abort_phase, error);
                                false
                            }
                        }
                    }
                    Err(error) => {
                        self.processing
                            .outcome(id, abort_phase, Some(format!("{error:#}")));
                        false
                    }
                };
                let audition_generation = self.take_pending_processing_audition(id);
                if published
                    && !self.preferences.settings().processing_bypassed
                    && audition_generation == Some(self.transport.generation)
                {
                    let outcome = if let Some(runner) = self.processing.take_graph_runner(id) {
                        self.processing_graph_audition_prepared(id, runner)
                    } else {
                        let target = self
                            .processing
                            .status(id)
                            .and_then(|status| status.settings.graph_target);
                        target.map_or(Ok(()), |target| {
                            self.processing_graph_audition(id, target).map(|_| ())
                        })
                    };
                    if let Err(error) = outcome {
                        let error = format!("{error:#}");
                        self.state.system.last_error = Some(error.clone());
                        self.processing
                            .outcome(id, ProcessingPhase::Failed, Some(error));
                    }
                }
                self.complete_processing_task(id);
            }
            Event::MixFinished {
                id,
                generation,
                result,
            } => {
                let Some(cancelled) = self.processing.finish_task(id) else {
                    if let Ok(pcm) = result {
                        self.processing.retire_pcm(pcm);
                    }
                    return;
                };
                let audition_generation = self.take_pending_processing_audition(id);
                let requested = !self.preferences.settings().processing_bypassed
                    && audition_generation == Some(generation);
                let outcome = match result {
                    Ok(pcm)
                        if !cancelled
                            && (!requested || generation == self.transport.generation) =>
                    {
                        let outcome = (|| -> Result<()> {
                            let source = self
                                .processing
                                .results()
                                .iter()
                                .find(|r| r.id == id)
                                .context("Processing result is no longer retained")?;
                            let track = self.processing_source(&source.track, source.stamp)?;
                            if requested {
                                self.establish_processing_source_context(&track)?;
                            }
                            Ok(())
                        })();
                        match outcome {
                            Ok(()) if requested => self
                                .select_cached_processing_audition(id, pcm, None)
                                .map_err(|(pcm, error)| {
                                    self.processing.retire_pcm(pcm);
                                    error
                                }),
                            outcome => {
                                self.processing.retire_pcm(pcm);
                                outcome
                            }
                        }
                    }
                    Ok(pcm) => {
                        self.processing.retire_pcm(pcm);
                        Err(anyhow::anyhow!(
                            "Mix audition cancelled or source playback changed"
                        ))
                    }
                    Err(error) => Err(error),
                };
                self.processing.outcome(
                    id,
                    if outcome.is_ok() {
                        ProcessingPhase::Complete
                    } else if cancelled {
                        ProcessingPhase::Cancelled
                    } else {
                        ProcessingPhase::Failed
                    },
                    outcome.as_ref().err().map(|error| format!("{error:#}")),
                );
                if let Err(error) = outcome
                    && !cancelled
                {
                    self.state.system.last_error = Some(format!("{error:#}"));
                }
                self.complete_processing_task(id);
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
        ensure!(
            !self.preferences.settings().processing_bypassed,
            "Playback processing is bypassed; turn bypass off before auditioning"
        );
        let result = self
            .processing
            .results()
            .iter()
            .find(|r| r.id == id)
            .context("Processing result is no longer retained")?;
        let track = self.processing_source(&result.track, result.stamp)?;
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
                    self.processing.active_id().is_none(),
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
        if mode == AuditionMode::Mix {
            self.processing.start_mix(
                id,
                self.transport.generation,
                vocal_gain_db,
                accompaniment_gain_db,
            )?;
            self.pending_processing_audition = Some((id, self.transport.generation));
            return Ok(Some(id));
        }
        self.establish_processing_source_context(&track)?;
        let result = self
            .processing
            .results()
            .iter()
            .find(|r| r.id == id)
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
        if let Some(pcm) = pcm {
            self.select_cached_processing_audition(id, pcm, None)
                .map_err(|(_, error)| error)?;
        } else {
            self.restore_original_processing_source(id)?;
        }
        Ok(None)
    }

    // Failed admission returns ownership: cached aliases can drop immediately,
    // while a newly rendered mix must retire its final PCM owner off Core.
    fn select_cached_processing_audition(
        &mut self,
        id: u64,
        pcm: audio::PcmAudio,
        output: Option<GraphPort>,
    ) -> std::result::Result<(), (audio::PcmAudio, anyhow::Error)> {
        if let Err(error) = self.replace_processing_source(id, Some(pcm.clone()), &mut None, None) {
            return Err((pcm, error));
        }
        self.processing
            .select_cached_audition(id, self.transport.generation, pcm, output);
        Ok(())
    }

    fn select_live_processing_audition(
        &mut self,
        id: u64,
        pcm: Option<audio::PcmAudio>,
        runner: audio::GraphRunner,
    ) -> Result<()> {
        let status = self.processing.status(id).context("Unknown graph run")?;
        let output = status
            .settings
            .graph_target
            .context("Missing graph target")?;
        let mode = status
            .settings
            .graph_mode
            .context("Missing resolved graph mode")?;
        let snapshot = runner.snapshot();
        let mut runner = Some(Box::new(runner));
        if let Err(error) = self.replace_processing_source(id, pcm.clone(), &mut runner, None) {
            if let Some(runner) = runner {
                self.processing.retire_runner(*runner);
            }
            return Err(error);
        }
        self.processing.select_graph_audition(
            id,
            self.transport.generation,
            pcm,
            snapshot,
            output,
            mode,
        );
        Ok(())
    }

    fn restore_original_processing_source(&mut self, id: u64) -> Result<()> {
        self.replace_processing_source(id, None, &mut None, None)?;
        self.processing.clear_audition();
        Ok(())
    }

    /// Replace the audio source without deciding which processing result is selected.
    fn replace_processing_source(
        &mut self,
        id: u64,
        pcm: Option<audio::PcmAudio>,
        graph: &mut Option<Box<audio::GraphRunner>>,
        config: Option<Config>,
    ) -> Result<()> {
        let bypassing = config
            .as_ref()
            .is_some_and(|config| config.processing_bypassed);
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
        let admitted_source = if bypassing {
            rivu_processing::SourceStamp::read(&track.path)?
        } else {
            let stamp = self
                .processing
                .source_stamp(id)
                .context("Processing result is no longer retained")?;
            if let Some(result) = self.processing.results().iter().find(|r| r.id == id) {
                self.processing_source(&result.track, stamp)?;
            } else {
                ensure!(
                    rivu_processing::SourceStamp::read(&track.path)? == stamp,
                    "Processing result is stale"
                );
            }
            stamp
        };
        ensure!(
            bypassing
                || (track.path == status.source_path
                    && track.cue == status.source_cue
                    && self
                        .processing
                        .results()
                        .iter()
                        .find(|result| result.id == id)
                        .is_none_or(|result| track.decoder_hint == result.track.decoder_hint)),
            "Processing result is stale"
        );
        ensure!(
            self.processing.results().iter().any(|r| r.id == id)
                || ((graph.is_some() || self.processing.audition_id() == Some(id))
                    && self.processing.active_id() == Some(id)),
            "Processing result is no longer retained"
        );
        ensure!(
            self.state.playback.status != PlaybackStatus::Stopped,
            "Start the source track before auditioning"
        );
        // PCM is immutable; graph admission still validates the original source.
        let expected_identity = graph.as_ref().map(|_| admitted_source.identity());
        self.engine.ensure_command_capacity(
            2 + usize::from(
                config
                    .as_ref()
                    .is_some_and(|config| config.equalizer.enabled),
            ),
        )?;
        let config = config.map(Arc::new);
        let previous = config.as_ref().map(|_| self.preferences.checkpoint());
        if let Some(config) = config.as_ref() {
            self.write_config(config)?;
        }
        let replacement = self.prepare_source_replacement().and_then(|replacement| {
            if let Some(config) = config.as_ref().filter(|config| config.equalizer.enabled) {
                let mut equalizer = config.equalizer.clone();
                equalizer.enabled = !config.processing_bypassed;
                if let Err(error) = self
                    .engine
                    .send(AudioCommand::Equalizer(Arc::new(equalizer)))
                {
                    self.finish_playback(Ok(None), Err(error))?;
                }
            }
            Ok(replacement)
        });
        let replacement = match replacement {
            Ok(replacement) => replacement,
            Err(error) => {
                return Err(self.rollback_processing_route(previous, error));
            }
        };
        self.pending_processing_audition = None;
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
                replaygain_track_gain: track.replaygain_track_gain,
                codec: track.codec.clone(),
                channels: status.channels.unwrap_or(track.channels),
                sample_rate: status.sample_rate.unwrap_or(track.sample_rate),
            })
        };
        let path = track.path.clone();
        let range = track
            .cue
            .as_ref()
            .map(|cue| rivu_protocol::audio::PlaybackRange {
                start_seconds: cue.start_seconds(),
                end_seconds: cue.end_seconds(),
            });
        let decoder_hint = track.decoder_hint;
        let command = if let Some(runner) = graph.take() {
            AudioCommand::GraphAudition {
                path,
                pcm,
                expected_identity: expected_identity
                    .expect("graph identity captured before admission"),
                range,
                decoder_hint,
                source_info,
                metadata: Self::playback_metadata(&track),
                generation: replacement.generation,
                start_seconds: replacement.position,
                paused: replacement.status == PlaybackStatus::Paused,
                listened_seconds: replacement.heard,
                runner,
            }
        } else {
            AudioCommand::Audition {
                path,
                pcm,
                range,
                decoder_hint,
                source_info,
                metadata: Self::playback_metadata(&track),
                generation: replacement.generation,
                start_seconds: replacement.position,
                paused: replacement.status == PlaybackStatus::Paused,
                listened_seconds: replacement.heard,
            }
        };
        if let Err(error) = self.commit_source_replacement(command) {
            return Err(self.rollback_processing_route(previous, error));
        }
        if let Some(config) = config {
            self.preferences.accept(config);
        }
        self.cancel_pending_processing_resume("Audition resume superseded by a replacement route");
        Ok(())
    }

    fn rollback_processing_route(
        &mut self,
        previous: Option<Config>,
        error: anyhow::Error,
    ) -> anyhow::Error {
        if let Some(previous) = previous
            && let Err(rollback) = self.write_config(&Arc::new(previous))
        {
            return error.context(format!(
                "Restoring bypass settings also failed: {rollback:#}"
            ));
        }
        error
    }
}
