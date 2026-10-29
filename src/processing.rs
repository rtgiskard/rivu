//! Offline processing runs outside Core and never publishes directly into player state.
use crate::config::Config;
use anyhow::{Context, Result, bail, ensure};
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
    time::{Duration, Instant},
};

pub(crate) mod graph;
mod neural;
mod reclaim;
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

fn export_destination(destination: &Path) -> Result<PathBuf> {
    ensure!(
        destination.is_absolute(),
        "Export destination must be an absolute path"
    );
    ensure!(
        !destination.try_exists()?,
        "Export destination already exists; choose a new file"
    );
    let parent = fs::canonicalize(
        destination
            .parent()
            .context("Export destination has no parent")?,
    )?;
    ensure!(
        parent.is_dir(),
        "Export destination parent must be a directory"
    );
    Ok(parent.join(
        destination
            .file_name()
            .context("Export destination has no file name")?,
    ))
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
pub(crate) struct Active {
    pub id: u64,
    pub cancelled: Arc<AtomicBool>,
    pub worker: JoinHandle<()>,
}
pub(crate) struct Service {
    pub sender: Sender<Event>,
    pub receiver: Receiver<Event>,
    pub active: Option<Active>,
    pub results: std::collections::VecDeque<ProcessingResult>,
    pub next_id: u64,
    pub audition: Option<u64>,
    pub audition_generation: Option<u64>,
    pub pending_audition: Option<(u64, u64)>,
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
            audition_generation: None,
            pending_audition: None,
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
                        check_cancel(&flag)?;
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
                        check_cancel(&flag)?;
                        let result = if let Some(cached_output) = cached_output {
                            run_cached_export(
                                track,
                                stamp,
                                budget,
                                initial,
                                cached_output,
                                &flag,
                                &tx,
                            )
                        } else {
                            run(track, stamp, config, budget, initial, &flag, &tx)
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
        self.active = Some(Active {
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
        export_destination(destination)
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
            if Some(result.status.id) == self.audition {
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
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Acquire), "Processing cancelled");
    Ok(())
}
struct ProgressReporter<'a> {
    sender: &'a Sender<Event>,
    id: u64,
    phase: Option<ProcessingPhase>,
    last_emit: Instant,
}

impl<'a> ProgressReporter<'a> {
    fn new(sender: &'a Sender<Event>, id: u64) -> Self {
        Self {
            sender,
            id,
            phase: None,
            last_emit: Instant::now(),
        }
    }

    fn report(&mut self, phase: ProcessingPhase, completed: u64, total: Option<u64>) {
        let transition = self.phase != Some(phase);
        if !transition && self.last_emit.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last_emit = Instant::now();
        let event = Event::Progress {
            id: self.id,
            phase,
            completed,
            total,
        };
        if transition {
            // Stage changes share the reliable terminal channel; blocks within
            // a stage are snapshots, not individual work that must be published.
            self.phase = Some(phase);
            let _ = self.sender.send_blocking(event);
        } else if self.sender.is_empty() {
            let _ = self.sender.try_send(event);
        }
    }
}
fn run_cached_export(
    track: Track,
    stamp: SourceStamp,
    _budget: Arc<audio::PcmBudget>,
    mut status: ProcessingTaskStatus,
    cached: CachedGraphOutput,
    cancel: &AtomicBool,
    tx: &Sender<Event>,
) -> Result<ProcessingResult> {
    let pcm = cached.pcm;
    let mut progress = ProgressReporter::new(tx, status.id);
    let destination = status
        .settings
        .graph_export_path
        .as_ref()
        .context("Missing export destination")?;
    let pending_export = Some(prepare_export(&pcm, destination, cancel, |done, total| {
        progress.report(ProcessingPhase::Processing, done, Some(total));
    })?);
    ensure!(
        SourceStamp::read(&track.path)? == stamp,
        "Source changed while exporting; output discarded"
    );
    status.phase = ProcessingPhase::Complete;
    status.graph_nodes.clear();
    status.sample_rate = Some(pcm.sample_rate());
    status.channels = Some(pcm.channels());
    status.frames = Some(pcm.frames());
    status.completed = pcm.frames();
    status.total = Some(pcm.frames());
    Ok(ProcessingResult {
        track,
        stamp,
        status,
        outputs: AudioOutputs::default(),
        graph_outputs: BTreeMap::new(),
        mix: None,
        pending_export,
        runner: None,
    })
}

fn run(
    track: Track,
    stamp: SourceStamp,
    config: Arc<Config>,
    budget: Arc<audio::PcmBudget>,
    mut status: ProcessingTaskStatus,
    cancel: &AtomicBool,
    tx: &Sender<Event>,
) -> Result<ProcessingResult> {
    let open_source = || -> Result<audio::SourceRenderer> {
        let renderer = audio::SourceRenderer::open(
            &track.path,
            track.cue.as_ref().map(|cue| audio::PlaybackRange {
                start_seconds: cue.start_seconds(),
                end_seconds: cue.end_seconds(),
            }),
            track.decoder_hint,
            config.media_read_buffer_mb,
            config.ffmpeg_enabled,
            cancel,
        )?;
        stamp.validate_rendered(renderer.format().source_identity)?;
        Ok(renderer)
    };
    let renderer = open_source()?;
    let mut progress = ProgressReporter::new(tx, status.id);
    let native = renderer.format().clone();
    stamp.validate_rendered(native.source_identity)?;
    ensure!(
        status.operation != ProcessingOperation::Separate || native.channels == 2,
        "Vocal separation requires a stereo source; no implicit channel conversion is performed"
    );
    ensure!(
        status.operation != ProcessingOperation::Speech || (1..=2).contains(&native.channels),
        "Speech enhancement supports mono/stereo only"
    );
    status.sample_rate = Some(native.sample_rate);
    status.channels = Some(native.channels);
    let mut runner = None;
    if let Some(mode) = status
        .settings
        .graph_mode
        .filter(|mode| *mode != GraphExecutionMode::Offline)
    {
        let (actual, boundary, prepared) = graph::prepare(
            status
                .settings
                .graph
                .as_ref()
                .context("Missing graph snapshot")?,
            status
                .settings
                .graph_target
                .context("Missing graph target")?,
            mode,
            native.sample_rate,
            usize::from(native.channels),
            cancel,
        )?;
        status.settings.graph_mode = Some(actual);
        status.settings.graph_boundary = boundary;
        status.graph_plan = prepared.as_ref().map(graph::plan_info);
        runner = prepared;
    }
    if status.settings.graph_mode == Some(GraphExecutionMode::Realtime) {
        check_cancel(cancel)?;
        ensure!(
            SourceStamp::read(&track.path)? == stamp,
            "Source changed while preparing graph"
        );
        for node in &mut status.graph_nodes {
            node.phase = ProcessingPhase::Processing;
        }
        status.phase = ProcessingPhase::Complete;
        return Ok(ProcessingResult {
            track,
            stamp,
            status,
            outputs: AudioOutputs::default(),
            graph_outputs: BTreeMap::new(),
            mix: None,
            pending_export: None,
            runner,
        });
    }
    progress.report(ProcessingPhase::Rendering, 0, None);
    let mut graph_outputs = BTreeMap::new();
    let outputs = if status.operation == ProcessingOperation::Denoise {
        let pcm = classical_native(
            renderer,
            open_source,
            &config.denoise,
            status.settings.profile,
            Arc::clone(&budget),
            cancel,
            |frames| {
                progress.report(
                    ProcessingPhase::Processing,
                    frames,
                    native.estimated_frames(),
                );
            },
        )?;
        status.frames = Some(pcm.frames());
        AudioOutputs {
            denoised: Some(pcm),
            ..Default::default()
        }
    } else if status.operation == ProcessingOperation::Graph && graph::native_only(&status)? {
        graph_outputs = graph::execute_native(
            renderer,
            &open_source,
            Arc::clone(&budget),
            &mut status,
            cancel,
            &mut progress,
        )?;
        AudioOutputs::default()
    } else {
        // Whole-signal model boundaries require seekable input. Native-only
        // paths above never allocate a complete decoded source alongside output.
        let input = renderer.render_pcm(Arc::clone(&budget), cancel)?;
        status.frames = Some(input.frames());
        if status.operation == ProcessingOperation::Graph {
            graph_outputs = graph::execute(
                &input,
                Arc::clone(&budget),
                &mut status,
                cancel,
                &mut progress,
            )?;
            AudioOutputs::default()
        } else {
            neural::run(
                &input,
                &config.neural,
                Arc::clone(&budget),
                cancel,
                &mut progress,
                &mut status,
            )?
        }
    };
    let frames = status
        .frames
        .context("Missing decoded source frame count")?;
    check_cancel(cancel)?;
    ensure!(
        SourceStamp::read(&track.path)? == stamp,
        "Source changed while processing; output discarded"
    );
    for pcm in [&outputs.denoised, &outputs.vocals, &outputs.accompaniment]
        .into_iter()
        .flatten()
    {
        validate_output(pcm, native.sample_rate, native.channels, frames, cancel)?;
    }
    let pending_export = if let Some(destination) = &status.settings.graph_export_path {
        let target = status
            .settings
            .graph_target
            .context("Missing export target")?;
        Some(prepare_export(
            &graph_outputs
                .get(&target)
                .context("Missing export output")?
                .pcm,
            destination,
            cancel,
            |done, total| {
                progress.report(ProcessingPhase::Processing, done, Some(total));
            },
        )?)
    } else {
        None
    };
    status.phase = ProcessingPhase::Complete;
    status.completed = frames;
    status.total = Some(frames);
    if status.operation != ProcessingOperation::Graph {
        status.outputs = Some(ProcessingOutputs {
            denoised: outputs.denoised.is_some(),
            vocals: outputs.vocals.is_some(),
            accompaniment: outputs.accompaniment.is_some(),
        });
    }
    check_cancel(cancel)?;
    Ok(ProcessingResult {
        track,
        stamp,
        status,
        outputs,
        graph_outputs,
        mix: None,
        pending_export,
        runner,
    })
}

fn prepare_export(
    source: &audio::PcmAudio,
    destination: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<(tempfile::NamedTempFile, PathBuf)> {
    let canonical = export_destination(destination)?;
    ensure!(
        canonical == destination,
        "Export destination parent changed after admission"
    );
    check_cancel(cancel)?;
    let mut temporary = tempfile::NamedTempFile::new_in(
        canonical
            .parent()
            .context("Export destination has no parent")?,
    )?;
    let spec = hound::WavSpec {
        channels: source.channels(),
        sample_rate: source.sample_rate(),
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::new(std::io::BufWriter::new(temporary.as_file_mut()), spec)?;
    let mut block = vec![0.0; 8192 * usize::from(source.channels())];
    let mut position = 0;
    progress(0, source.frames());
    while position < source.frames() {
        check_cancel(cancel)?;
        let frames = source.read_frames(position, &mut block);
        ensure!(frames > 0, "Truncated export PCM");
        for &sample in &block[..frames * usize::from(source.channels())] {
            writer.write_sample(sample)?;
        }
        position += frames as u64;
        progress(position, source.frames());
    }
    writer.finalize()?;
    temporary.as_file().sync_all()?;
    check_cancel(cancel)?;
    Ok((temporary, canonical))
}

fn validate_output(
    pcm: &audio::PcmAudio,
    rate: u32,
    channels: u16,
    frames: u64,
    cancel: &AtomicBool,
) -> Result<()> {
    check_cancel(cancel)?;
    ensure!(
        pcm.sample_rate() == rate && pcm.channels() == channels && pcm.frames() == frames,
        "Processing output format or alignment differs from source"
    );
    Ok(())
}

fn classical_native(
    mut renderer: audio::SourceRenderer,
    reopen: impl FnOnce() -> Result<audio::SourceRenderer>,
    config: &rivu_dsp::denoise::DenoiseConfig,
    selection: Option<NoiseSelection>,
    budget: Arc<audio::PcmBudget>,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<audio::PcmAudio> {
    use rivu_dsp::denoise::{DenoiseMode, Denoiser, ProfileCollector};
    let rate = renderer.format().sample_rate;
    let channels = renderer.format().channels;
    config.validate_for_format(rate, usize::from(channels))?;
    let profile = if config.mode == DenoiseMode::Profile {
        let selection =
            selection.context("Profile mode requires an explicit selected noise-only range")?;
        selection.validate()?;
        let start = (selection.start_seconds * f64::from(rate)).floor() as u64;
        let end = (selection.end_seconds * f64::from(rate)).floor() as u64;
        ensure!(
            start < end,
            "Noise profile selection contains no source frames"
        );
        let mut capture = reopen()?;
        ensure!(
            capture.format().sample_rate == rate && capture.format().channels == channels,
            "Source format changed during profile capture"
        );
        let mut collector =
            ProfileCollector::new(rate, usize::from(channels), config.fft_size, end - start)?;
        let mut block = Vec::new();
        let mut position = 0_u64;
        while capture.read_block(&mut block, cancel)? {
            ensure!(
                block.len().is_multiple_of(usize::from(channels)),
                "Incomplete profile source frame"
            );
            let next = position
                .checked_add((block.len() / usize::from(channels)) as u64)
                .context("Profile source length overflow")?;
            let from = position.max(start);
            let to = next.min(end);
            if from < to {
                collector.push_block(
                    &block[(from - position) as usize * usize::from(channels)
                        ..(to - position) as usize * usize::from(channels)],
                    &mut || cancel.load(Ordering::Acquire),
                )?;
            }
            position = next;
        }
        ensure!(
            end <= position,
            "Noise profile selection lies outside the source"
        );
        Some(collector.finish()?)
    } else {
        None
    };
    let mut denoiser = if graph::denoise_bypassed(config) {
        None
    } else {
        Some(Denoiser::new(
            rate,
            usize::from(channels),
            config.clone(),
            profile,
        )?)
    };
    let mut builder = audio::PcmBuilder::new(rate, channels, budget)?;
    let mut block = Vec::new();
    let mut processed = Vec::new();
    let mut frames = 0_u64;
    while renderer.read_block(&mut block, cancel)? {
        ensure!(
            block.len().is_multiple_of(usize::from(channels)),
            "Incomplete denoise source frame"
        );
        frames = frames
            .checked_add((block.len() / usize::from(channels)) as u64)
            .context("Denoise source length overflow")?;
        if let Some(denoiser) = &mut denoiser {
            processed.clear();
            denoiser.process_block(&block, &mut processed, &mut || {
                cancel.load(Ordering::Acquire)
            })?;
            builder.push(&processed)?;
        } else {
            builder.push(&block)?;
        }
        progress(frames);
    }
    if let Some(denoiser) = &mut denoiser {
        processed.clear();
        denoiser.finish(&mut processed, &mut || cancel.load(Ordering::Acquire))?;
        builder.push(&processed)?;
    }
    renderer.finish(cancel)?;
    let pcm = builder.finish()?;
    validate_output(&pcm, rate, channels, frames, cancel)?;
    Ok(pcm)
}

pub(crate) fn mix(
    service: &mut Service,
    id: u64,
    generation: u64,
    vocal_gain_db: f32,
    accompaniment_gain_db: f32,
) -> Result<()> {
    ensure!(
        service.active.is_none(),
        "An audio processing task is already active"
    );
    ensure!(
        !service.sender.is_closed(),
        "Processing service is shutting down"
    );
    ensure!(
        vocal_gain_db.is_finite()
            && accompaniment_gain_db.is_finite()
            && (-96.0..=24.0).contains(&vocal_gain_db)
            && (-96.0..=24.0).contains(&accompaniment_gain_db),
        "Stem gains must be finite and within -96..24 dB"
    );
    let result = service
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
    let budget = Arc::clone(&service.budget);
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel = Arc::clone(&cancelled);
    let tx = service.sender.clone();
    #[cfg(test)]
    let panic_after_mix = std::mem::take(&mut service.panic_after_mix);
    let worker = service
        .reclaimer
        .as_ref()
        .unwrap()
        .spawn("rivu-stem-mix", move |retired| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> Result<audio::PcmAudio> {
                    retired?;
                    validate_output(
                        &accompaniment,
                        vocals.sample_rate(),
                        vocals.channels(),
                        vocals.frames(),
                        &cancel,
                    )?;
                    let mut builder =
                        audio::PcmBuilder::new(vocals.sample_rate(), vocals.channels(), budget)?;
                    let mut v = vec![0.0; 8192 * usize::from(vocals.channels())];
                    let mut a = vec![0.0; v.len()];
                    let vg = 10.0_f32.powf(vocal_gain_db / 20.0);
                    let ag = 10.0_f32.powf(accompaniment_gain_db / 20.0);
                    let mut position = 0;
                    while position < vocals.frames() {
                        check_cancel(&cancel)?;
                        let frames = vocals.read_frames(position, &mut v);
                        ensure!(
                            accompaniment.read_frames(position, &mut a) == frames,
                            "Stem alignment mismatch"
                        );
                        let count = frames * usize::from(vocals.channels());
                        for i in 0..count {
                            v[i] = v[i] * vg + a[i] * ag;
                        }
                        builder.push(&v[..count])?;
                        position += frames as u64;
                    }
                    check_cancel(&cancel)?;
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
    service.active = Some(Active {
        id,
        cancelled,
        worker,
    });
    Ok(())
}
