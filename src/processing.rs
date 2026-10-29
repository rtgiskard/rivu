//! Offline processing runs outside Core and never publishes directly into player state.
use crate::config::Config;
use anyhow::{Context, Result, ensure};
use async_channel::{Receiver, Sender, bounded};
use rivu_audio as audio;
use rivu_protocol::model::*;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

mod export;
pub(crate) mod graph;
mod neural;
mod reclaim;
mod render;
#[cfg(test)]
mod tests;

const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub(crate) const MEMORY_LIMIT: u64 = 512 * 1024 * 1024;

#[cfg(test)]
thread_local! {
    static PANIC_ON_NEURAL_PROGRESS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceStamp(audio::SourceIdentity);
impl SourceStamp {
    pub(crate) fn read(path: &Path) -> Result<Self> {
        let m = fs::metadata(path)?;
        ensure!(
            m.is_file() && m.len() <= MAX_SOURCE_BYTES,
            "source must be a regular file no larger than 8 GiB"
        );
        Ok(Self(audio::SourceIdentity::from_metadata(&m)))
    }
    pub(crate) fn identity(self) -> audio::SourceIdentity {
        self.0
    }
    pub(crate) fn validate_rendered(&self, identity: audio::SourceIdentity) -> Result<()> {
        ensure!(
            self.0 == identity,
            "Rendered source differs from the source admitted for processing"
        );
        Ok(())
    }
}

pub(crate) struct GraphRequest {
    pub target: GraphPort,
    pub mode: GraphExecutionMode,
    pub export_path: Option<PathBuf>,
    pub source_run: Option<u64>,
    pub cached_output: Option<CachedGraphOutput>,
}

pub(crate) struct CachedGraphOutput {
    pub pcm: audio::PcmAudio,
}

#[derive(Default)]
pub(crate) struct AudioOutputs {
    pub denoised: Option<audio::PcmAudio>,
    pub vocals: Option<audio::PcmAudio>,
    pub accompaniment: Option<audio::PcmAudio>,
}

pub(crate) struct GraphAudioContext {
    pub pcm: audio::PcmAudio,
    pub start_frame: i64,
}

pub(crate) struct GraphAudioOutput {
    pub pcm: audio::PcmAudio,
    pub context: Option<GraphAudioContext>,
}

pub(crate) struct ProcessingResult {
    pub track: Track,
    pub stamp: SourceStamp,
    pub status: ProcessingTaskStatus,
    pub outputs: AudioOutputs,
    pub graph_outputs: BTreeMap<GraphPort, GraphAudioOutput>,
    pub mix: Option<audio::PcmAudio>,
    pub pending_export: Option<(tempfile::NamedTempFile, PathBuf)>,
    pub runner: Option<audio::GraphRunner>,
}
impl ProcessingResult {
    pub(crate) fn retains_audio(&self) -> bool {
        // Export scratch belongs to publication, not to the audition cache.
        // Live runs still retain their frozen graph even without cached PCM.
        self.status.settings.graph_export_path.is_none()
    }
}
pub(crate) enum Event {
    GraphNode {
        id: u64,
        status: GraphNodeStatus,
    },
    Progress {
        id: u64,
        completed: u64,
        total: Option<u64>,
        phase: ProcessingPhase,
    },
    Finished {
        id: u64,
        result: Result<Box<ProcessingResult>>,
    },
    MixFinished {
        id: u64,
        generation: u64,
        result: Result<audio::PcmAudio>,
    },
}
struct ActiveTask {
    id: u64,
    cancelled: Arc<AtomicBool>,
    worker: JoinHandle<()>,
}
pub(crate) struct Service {
    pub sender: Sender<Event>,
    pub receiver: Receiver<Event>,
    active: Option<ActiveTask>,
    results: std::collections::VecDeque<ProcessingResult>,
    next_id: u64,
    audition: Option<(u64, u64)>,
    reclaimer: Option<reclaim::Reclaimer>,
    budget: Arc<audio::PcmBudget>,
    #[cfg(test)]
    pub(crate) panic_after_processing: bool,
    #[cfg(test)]
    pub(crate) panic_after_mix: bool,
    #[cfg(test)]
    pub(crate) panic_on_neural_progress: bool,
}
impl Service {
    pub fn new() -> Self {
        let (sender, receiver) = bounded(8);
        Self {
            sender,
            receiver,
            active: None,
            results: Default::default(),
            next_id: 1,
            reclaimer: None,
            budget: audio::PcmBudget::new(MEMORY_LIMIT),
            audition: None,
            #[cfg(test)]
            panic_after_processing: false,
            #[cfg(test)]
            panic_after_mix: false,
            #[cfg(test)]
            panic_on_neural_progress: false,
        }
    }
    #[cfg(test)]
    pub(crate) fn with_memory_limit(limit: u64) -> Self {
        let mut service = Self::new();
        service.budget = audio::PcmBudget::new(limit);
        service
    }
    pub(crate) fn active_id(&self) -> Option<u64> {
        self.active.as_ref().map(|task| task.id)
    }

    pub(crate) fn cancellation_requested(&self, id: u64) -> Option<bool> {
        self.active
            .as_ref()
            .filter(|task| task.id == id)
            .map(|task| task.cancelled.load(Ordering::Acquire))
    }

    /// Retire the worker only after its reliable terminal event has been received.
    pub(crate) fn finish_task(&mut self, id: u64) -> Option<bool> {
        if self.active_id() != Some(id) {
            return None;
        }
        let task = self.active.take().unwrap();
        let cancelled = task.cancelled.load(Ordering::Acquire);
        let _ = task.worker.join();
        Some(cancelled)
    }

    pub(crate) fn results(&self) -> &std::collections::VecDeque<ProcessingResult> {
        &self.results
    }

    /// Keep the selected audio pinned until Core commits another audio source.
    pub(crate) fn select_audition(&mut self, selection: Option<(u64, u64)>) {
        self.audition = selection;
    }

    pub(crate) fn audition_id(&self) -> Option<u64> {
        self.audition.map(|(id, _)| id)
    }

    pub(crate) fn audition_selected(&self, id: u64, generation: u64) -> bool {
        self.audition == Some((id, generation))
    }

    /// Retention and retirement are one operation; Core publishes eviction effects.
    pub(crate) fn retain_result(&mut self, result: Box<ProcessingResult>) -> Option<u64> {
        debug_assert!(result.retains_audio());
        let evicted = if self.results.len() >= 4 {
            let index = self
                .results
                .iter()
                .position(|result| Some(result.status.id) != self.audition_id())
                .expect("only one retained result can be pinned");
            let result = self.results.remove(index).unwrap();
            let id = result.status.id;
            self.retire(Box::new(result));
            Some(id)
        } else {
            None
        };
        self.results.push_back(*result);
        evicted
    }

    pub(crate) fn take_graph_runner(&mut self, id: u64) -> Option<audio::GraphRunner> {
        self.results
            .iter_mut()
            .find(|result| result.status.id == id)?
            .runner
            .take()
    }

    pub(crate) fn retain_mix(&mut self, id: u64, pcm: audio::PcmAudio) {
        let retired = if let Some(result) = self.results.iter_mut().find(|r| r.status.id == id) {
            result.mix.replace(pcm)
        } else {
            Some(pcm)
        };
        if let Some(pcm) = retired {
            self.retire_pcm(pcm);
        }
    }

    pub(crate) fn stop_live_audition(&mut self, id: u64) -> Option<&ProcessingTaskStatus> {
        let result = self
            .results
            .iter_mut()
            .find(|result| result.status.id == id)?;
        let status = &mut result.status;
        if !matches!(
            status.settings.graph_mode,
            Some(GraphExecutionMode::Realtime | GraphExecutionMode::Hybrid)
        ) {
            return None;
        }
        status.phase = ProcessingPhase::Cancelled;
        status.error = Some("Live audition stopped".into());
        for node in &mut status.graph_nodes {
            if node.phase == ProcessingPhase::Processing {
                node.phase = ProcessingPhase::Cancelled;
            }
        }
        Some(status)
    }

    /// Update the outcome without replacing the frozen recipe or retained audio.
    pub(crate) fn record_outcome(&mut self, status: &ProcessingTaskStatus) {
        if let Some(result) = self
            .results
            .iter_mut()
            .find(|result| result.status.id == status.id)
        {
            result.status.phase = status.phase;
            result.status.error.clone_from(&status.error);
        }
    }
    pub fn start(
        &mut self,
        track: Track,
        config: Arc<Config>,
        operation: ProcessingOperation,
        profile: Option<NoiseSelection>,
    ) -> Result<ProcessingTaskStatus> {
        ensure!(
            operation != ProcessingOperation::Graph,
            "Use ProcessingGraphRun with an explicit target"
        );
        self.start_request(track, config, operation, profile, None)
    }
    pub fn start_graph(
        &mut self,
        track: Track,
        config: Arc<Config>,
        request: GraphRequest,
    ) -> Result<ProcessingTaskStatus> {
        config.processing_graph.ancestors(request.target)?;
        self.start_request(
            track,
            config,
            ProcessingOperation::Graph,
            None,
            Some(request),
        )
    }
    fn start_request(
        &mut self,
        track: Track,
        config: Arc<Config>,
        operation: ProcessingOperation,
        profile: Option<NoiseSelection>,
        request: Option<GraphRequest>,
    ) -> Result<ProcessingTaskStatus> {
        let (target, mode, export_path, cached_output, source_run) = if let Some(request) = request
        {
            (
                Some(request.target),
                request.mode,
                request.export_path,
                request.cached_output,
                request.source_run,
            )
        } else {
            (None, GraphExecutionMode::Offline, None, None, None)
        };
        ensure!(
            self.active.is_none(),
            "An audio processing task is already active"
        );
        ensure!(
            !self.sender.is_closed(),
            "Processing service is shutting down"
        );
        let export_path = export_path
            .as_deref()
            .map(|destination| self.export_destination(destination))
            .transpose()?;
        config.validate()?;
        if let Some(selection) = profile {
            ensure!(
                selection.start_seconds.is_finite()
                    && selection.end_seconds.is_finite()
                    && selection.start_seconds >= 0.0
                    && selection.end_seconds > selection.start_seconds,
                "Noise profile selection must be a finite nonempty source-relative range"
            );
        }
        let stamp = SourceStamp::read(&track.path)?;
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("Processing task id exhausted")?;
        let status = ProcessingTaskStatus {
            id,
            track_id: track.id,
            operation,
            phase: ProcessingPhase::Queued,
            completed: 0,
            total: None,
            error: None,
            outputs: None,
            sample_rate: None,
            channels: None,
            frames: None,
            model_id: None,
            model_sha256: None,
            source_path: track.path.clone(),
            source_version: format!("{stamp:?}"),
            source_cue: track.cue.clone(),
            settings: ProcessingSettings {
                denoise: config.denoise.clone(),
                neural: config.neural.clone(),
                profile,
                graph: target.map(|_| config.processing_graph.clone()),
                graph_target: target,
                graph_mode: target.map(|_| mode),
                graph_boundary: None,
                graph_export_path: export_path,
            },
            graph_revision: target.map(|_| config.processing_graph.revision),
            graph_run_id: target.map(|_| source_run.unwrap_or(id)),
            graph_outputs: Vec::new(),
            graph_exported_path: None,
            graph_nodes: if let Some(target) = target {
                config
                    .processing_graph
                    .ancestors(target)?
                    .into_iter()
                    .map(|node| GraphNodeStatus {
                        node,
                        phase: ProcessingPhase::Queued,
                        error: None,
                        model_id: None,
                        model_sha256: None,
                    })
                    .collect()
            } else {
                Vec::new()
            },
            graph_plan: None,
        };
        let initial = status.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let tx = self.sender.clone();
        if self.reclaimer.is_none() {
            self.reclaimer = Some(reclaim::Reclaimer::new()?);
        }
        let budget = Arc::clone(&self.budget);
        #[cfg(test)]
        let panic_after_processing = std::mem::take(&mut self.panic_after_processing);
        #[cfg(test)]
        let panic_on_neural_progress = std::mem::take(&mut self.panic_on_neural_progress);
        let (worker, release) =
            self.reclaimer
                .as_ref()
                .unwrap()
                .spawn_waiting("rivu-processing", move |retired| {
                    // This new thread executes only this task.
                    #[cfg(test)]
                    PANIC_ON_NEURAL_PROGRESS.set(panic_on_neural_progress);
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        retired?;
                        render::check_cancel(&flag)?;
                        if matches!(
                            operation,
                            ProcessingOperation::Speech | ProcessingOperation::Separate
                        ) {
                            let manifest = if operation == ProcessingOperation::Speech {
                                &config.neural.speech_manifest
                            } else {
                                &config.neural.separation_manifest
                            };
                            ensure!(
                                manifest
                                    .as_ref()
                                    .is_some_and(|p| p.is_absolute() && p.is_file()),
                                "Select an absolute local prepared model manifest first"
                            );
                            ensure!(
                                config.neural.worker.is_file(),
                                "Neural worker does not exist: {}",
                                config.neural.worker.display()
                            );
                        }
                        render::check_cancel(&flag)?;
                        let result = if let Some(cached_output) = cached_output {
                            render::run_cached_export(
                                track,
                                stamp,
                                budget,
                                initial,
                                cached_output,
                                &flag,
                                &tx,
                            )
                        } else {
                            render::run(track, stamp, config, budget, initial, &flag, &tx)
                        }?;
                        #[cfg(test)]
                        if panic_after_processing {
                            panic!("injected native processing worker panic");
                        }
                        Ok(result)
                    }))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("Audio processing worker panicked")));
                    let _ = tx.send_blocking(Event::Finished {
                        id,
                        result: result.map(Box::new),
                    });
                })?;
        if status.settings.graph_export_path.is_none() {
            self.release_unpinned_audio();
        }
        self.reclaimer
            .as_ref()
            .unwrap()
            .resume_after_release(release);
        self.active = Some(ActiveTask {
            id,
            cancelled,
            worker,
        });
        Ok(status)
    }
    pub fn cancel(&self, id: u64) -> Result<()> {
        let task = self
            .active
            .as_ref()
            .filter(|t| t.id == id)
            .context("No matching active processing task")?;
        task.cancelled.store(true, Ordering::Release);
        Ok(())
    }
    pub fn shutdown(&mut self) {
        let task = self.active.take();
        if let Some(task) = &task {
            task.cancelled.store(true, Ordering::Release);
        }
        // Closing admission wakes every reliable sender, including a worker
        // blocked behind buffered progress. Joining establishes retirement;
        // draining afterward drops unpublished outputs, not retained auditions.
        self.receiver.close();
        if let Some(task) = task {
            let _ = task.worker.join();
        }
        while let Ok(event) = self.receiver.try_recv() {
            match event {
                Event::Finished {
                    result: Ok(result), ..
                } => self.retire(result),
                Event::MixFinished {
                    result: Ok(pcm), ..
                } => self.retire_pcm(pcm),
                _ => {}
            }
        }
        if let Some(mut reclaimer) = self.reclaimer.take() {
            reclaimer.shutdown();
        }
    }
    pub(crate) fn export_destination(&self, destination: &Path) -> Result<PathBuf> {
        export::export_destination(destination)
    }

    #[cfg(test)]
    pub fn memory_bytes(&self) -> u64 {
        self.budget.used_bytes()
    }
    fn release_unpinned_audio(&mut self) {
        let Some(reclaimer) = self.reclaimer.as_ref() else {
            return;
        };
        for result in &mut self.results {
            if Some(result.status.id) == self.audition.map(|(id, _)| id) {
                continue;
            }
            let outputs = std::mem::take(&mut result.outputs);
            let graph_outputs = std::mem::take(&mut result.graph_outputs);
            let mix = result.mix.take();
            let runner = result.runner.take();
            result.status.outputs = None;
            result.status.graph_outputs.clear();
            if outputs.denoised.is_some()
                || outputs.vocals.is_some()
                || outputs.accompaniment.is_some()
                || !graph_outputs.is_empty()
                || mix.is_some()
                || runner.is_some()
            {
                reclaimer.release_audio(outputs, graph_outputs, mix, runner);
            }
        }
    }
    pub fn retire_pcm(&self, pcm: audio::PcmAudio) {
        self.reclaimer
            .as_ref()
            .expect("PCM requires an admitted worker")
            .retire_pcm(pcm);
    }
    pub fn retire(&self, result: Box<ProcessingResult>) {
        self.reclaimer
            .as_ref()
            .expect("result requires an admitted worker")
            .retire(result);
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.shutdown();
        let results = std::mem::take(&mut self.results);
        if !results.is_empty() {
            thread::scope(|scope| {
                scope.spawn(move || drop(results));
            });
        }
    }
}
impl Service {
    pub(crate) fn start_mix(
        &mut self,
        id: u64,
        generation: u64,
        vocal_gain_db: f32,
        accompaniment_gain_db: f32,
    ) -> Result<()> {
        ensure!(
            self.active.is_none(),
            "An audio processing task is already active"
        );
        ensure!(
            !self.sender.is_closed(),
            "Processing service is shutting down"
        );
        ensure!(
            vocal_gain_db.is_finite()
                && accompaniment_gain_db.is_finite()
                && (-96.0..=24.0).contains(&vocal_gain_db)
                && (-96.0..=24.0).contains(&accompaniment_gain_db),
            "Stem gains must be finite and within -96..24 dB"
        );
        let result = self
            .results
            .iter()
            .find(|r| r.status.id == id)
            .context("Processing result is no longer retained")?;
        let vocals = result.outputs.vocals.clone().context("No vocal stem")?;
        let accompaniment = result
            .outputs
            .accompaniment
            .clone()
            .context("No accompaniment stem")?;
        let budget = Arc::clone(&self.budget);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&cancelled);
        let tx = self.sender.clone();
        #[cfg(test)]
        let panic_after_mix = std::mem::take(&mut self.panic_after_mix);
        let worker = self
            .reclaimer
            .as_ref()
            .unwrap()
            .spawn("rivu-stem-mix", move |retired| {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || -> Result<audio::PcmAudio> {
                        retired?;
                        render::validate_output(
                            &accompaniment,
                            vocals.sample_rate(),
                            vocals.layout(),
                            vocals.frames(),
                            &cancel,
                        )?;
                        let mut builder = audio::PcmBuilder::new(
                            vocals.sample_rate(),
                            Arc::clone(vocals.layout()),
                            budget,
                        )?;
                        let mut v = vec![0.0; 8192 * usize::from(vocals.channels())];
                        let mut a = vec![0.0; v.len()];
                        let vg = 10.0_f32.powf(vocal_gain_db / 20.0);
                        let ag = 10.0_f32.powf(accompaniment_gain_db / 20.0);
                        let mut position = 0;
                        while position < vocals.frames() {
                            render::check_cancel(&cancel)?;
                            let frames = vocals.read_frames(position, &mut v);
                            let accompaniment_frames = accompaniment.read_frames(position, &mut a);
                            debug_assert_eq!(accompaniment_frames, frames);
                            let count = frames * usize::from(vocals.channels());
                            for i in 0..count {
                                v[i] = v[i] * vg + a[i] * ag;
                            }
                            builder.push(&v[..count])?;
                            position += frames as u64;
                        }
                        render::check_cancel(&cancel)?;
                        #[cfg(test)]
                        if panic_after_mix {
                            panic!("injected stem mix worker panic");
                        }
                        builder.finish()
                    },
                ))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Stem mix worker panicked")));
                let _ = tx.send_blocking(Event::MixFinished {
                    id,
                    generation,
                    result: outcome,
                });
            })?;
        self.active = Some(ActiveTask {
            id,
            cancelled,
            worker,
        });
        Ok(())
    }
}
