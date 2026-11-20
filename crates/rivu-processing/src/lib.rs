//! Offline processing runs outside Core and never publishes directly into player state.
use anyhow::{Context, Result, ensure};
use async_channel::{Receiver, Sender, bounded};
use rivu_audio as audio;
use rivu_format::config::Config;
use rivu_protocol::audio::SourceIdentity;
use rivu_protocol::model::*;
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

mod export;
mod graph;
#[cfg(feature = "onnx")]
mod native_speech;
mod neural;
mod reclaim;
mod render;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MEMORY_LIMIT: u64 = 512 * 1024 * 1024;
pub const WORKER_REQUEST_VERSION: u64 = 4;
pub const WORKER_EVENT_VERSION: u64 = 4;

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static PANIC_ON_NEURAL_PROGRESS: std::cell::Cell<Option<Arc<AtomicBool>>> = const { std::cell::Cell::new(None) };
}

struct Cancellation {
    requested: AtomicBool,
    wake: OwnedFd,
}

impl Cancellation {
    fn new() -> Result<Self> {
        // SAFETY: eventfd creates a fresh descriptor and retains no pointers.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("Creating processing cancellation wake");
        }
        Ok(Self {
            requested: AtomicBool::new(false),
            // SAFETY: this fresh descriptor has no other owner.
            wake: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    fn flag(&self) -> &AtomicBool {
        &self.requested
    }

    fn cancel(&self) {
        if self.requested.swap(true, Ordering::AcqRel) {
            return;
        }
        // One write, never drained: readiness is sticky even if cancellation
        // races entry into poll. The initial counter is zero and only this swap
        // winner writes eight bytes once, so EAGAIN (counter overflow) is impossible.
        let value = 1u64;
        loop {
            // SAFETY: wake is live and value supplies exactly eight readable bytes.
            let written =
                unsafe { libc::write(self.wake.as_raw_fd(), std::ptr::from_ref(&value).cast(), 8) };
            if written == 8 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                panic!("Writing processing cancellation wake: {error}");
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceStamp(SourceIdentity);
impl SourceStamp {
    pub fn read(path: &Path) -> Result<Self> {
        let m = fs::metadata(path)?;
        ensure!(
            m.is_file() && m.len() <= MAX_SOURCE_BYTES,
            "source must be a regular file no larger than 8 GiB"
        );
        Ok(Self(audio::source_identity_from_metadata(&m)))
    }
    pub fn identity(self) -> SourceIdentity {
        self.0
    }
    pub fn validate_rendered(&self, identity: SourceIdentity) -> Result<()> {
        ensure!(
            self.0 == identity,
            "Rendered source differs from the source admitted for processing"
        );
        Ok(())
    }
}

pub struct GraphRequest {
    pub target: GraphPort,
    pub mode: GraphExecutionMode,
    pub export_path: Option<PathBuf>,
    pub source_run: Option<u64>,
    pub cached_output: Option<CachedGraphOutput>,
    pub audio_export: Option<AudioExportRequest>,
}

pub struct CachedGraphOutput {
    pub pcm: audio::PcmAudio,
    pub stamp: SourceStamp,
}

#[derive(Default)]
pub struct AudioOutputs {
    pub denoised: Option<audio::PcmAudio>,
    pub vocals: Option<audio::PcmAudio>,
    pub accompaniment: Option<audio::PcmAudio>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModelOutputRetention {
    Speech,
    Vocals,
    Accompaniment,
    BothStems,
}
impl ModelOutputRetention {
    fn streams(self) -> [bool; 3] {
        match self {
            Self::Speech => [true, false, false],
            Self::Vocals => [false, true, false],
            Self::Accompaniment => [false, false, true],
            Self::BothStems => [false, true, true],
        }
    }
}

/// Model inputs are independent from the enclosing processing operation/graph.
struct ModelRequest<'a> {
    pub task_id: u64,
    pub operation: ProcessingOperation,
    pub retain: ModelOutputRetention,
    pub input: &'a audio::PcmAudio,
    pub config: &'a rivu_format::config::NeuralConfig,
    pub budget: Arc<audio::PcmBudget>,
    pub cancel: &'a Cancellation,
}

#[derive(Default, Debug)]
pub struct ModelProvenance {
    pub id: Option<String>,
    pub sha256: Option<String>,
}

struct ModelOutcome {
    pub provenance: ModelProvenance,
    pub result: Result<AudioOutputs>,
}

pub struct GraphAudioContext {
    pub pcm: audio::PcmAudio,
    pub start_frame: i64,
}

pub struct GraphAudioOutput {
    pub pcm: audio::PcmAudio,
    pub context: Option<GraphAudioContext>,
}

pub struct ProcessingResult {
    pub track: Track,
    pub stamp: SourceStamp,
    pub status: ProcessingTaskStatus,
    pub outputs: AudioOutputs,
    pub graph_outputs: BTreeMap<GraphPort, GraphAudioOutput>,
    pub pending_export: Option<(tempfile::NamedTempFile, PathBuf)>,
    pub runner: Option<audio::GraphRunner>,
}
impl ProcessingResult {
    fn retains_audio(&self) -> bool {
        // Export scratch belongs to publication, not to the audition cache.
        // Live runs still retain their frozen graph even without cached PCM.
        self.status.settings.graph_export_path.is_none()
    }
}

pub struct RetainedResult {
    pub id: u64,
    pub track: Track,
    pub stamp: SourceStamp,
    pub outputs: AudioOutputs,
    pub graph_outputs: BTreeMap<GraphPort, GraphAudioOutput>,
    pub runner: Option<audio::GraphRunner>,
}
pub enum Event {
    Model {
        id: u64,
        provenance: ModelProvenance,
    },
    GraphPrepared {
        id: u64,
        mode: GraphExecutionMode,
        boundary: Option<GraphPort>,
        plan: Option<GraphPlanInfo>,
    },
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
    stamp: SourceStamp,
    cancelled: Arc<Cancellation>,
    worker: JoinHandle<()>,
}
struct RetainedAudition {
    id: u64,
    generation: u64,
    resource: AuditionResource,
}

enum AuditionResource {
    Cached {
        pcm: audio::PcmAudio,
        output: Option<GraphPort>,
    },
    Graph {
        pcm: Option<audio::PcmAudio>,
        snapshot: audio::GraphSnapshot,
        output: GraphPort,
        mode: GraphExecutionMode,
    },
}

pub struct Service {
    pub sender: Sender<Event>,
    pub receiver: Receiver<Event>,
    active: Option<ActiveTask>,
    results: VecDeque<RetainedResult>,
    records: BTreeMap<u64, Arc<ProcessingTaskStatus>>,
    current: Option<u64>,
    history: VecDeque<u64>,
    next_id: u64,
    audition: Option<RetainedAudition>,
    reclaimer: Option<reclaim::Reclaimer>,
    budget: Arc<audio::PcmBudget>,
    #[cfg(any(test, feature = "test-support"))]
    pub panic_after_processing: bool,
    #[cfg(any(test, feature = "test-support"))]
    pub panic_after_mix: bool,
    #[cfg(any(test, feature = "test-support"))]
    pub panic_on_neural_progress: Option<Arc<AtomicBool>>,
}
impl Default for Service {
    fn default() -> Self {
        Self::new()
    }
}

fn complete_prepared_nodes(
    status: &mut ProcessingTaskStatus,
    mode: GraphExecutionMode,
    boundary: Option<GraphPort>,
) {
    // Admitted nodes are in dependency order. The Hybrid boundary is the
    // runner's only input, and is itself produced by the offline prefix.
    let first = match (mode, boundary) {
        (GraphExecutionMode::Realtime, None) => 0,
        (GraphExecutionMode::Hybrid, Some(port)) => {
            status
                .graph_nodes
                .iter()
                .position(|node| node.node == port.node)
                .expect("admitted Hybrid boundary")
                + 1
        }
        _ => return,
    };
    for node in &mut status.graph_nodes[first..] {
        node.phase = ProcessingPhase::Complete;
    }
}

impl Service {
    pub fn new() -> Self {
        let (sender, receiver) = bounded(8);
        Self {
            sender,
            receiver,
            active: None,
            results: Default::default(),
            records: Default::default(),
            current: None,
            history: Default::default(),
            next_id: 1,
            reclaimer: None,
            budget: audio::PcmBudget::new(MEMORY_LIMIT),
            audition: None,
            #[cfg(any(test, feature = "test-support"))]
            panic_after_processing: false,
            #[cfg(any(test, feature = "test-support"))]
            panic_after_mix: false,
            #[cfg(any(test, feature = "test-support"))]
            panic_on_neural_progress: None,
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_memory_limit(limit: u64) -> Self {
        let mut service = Self::new();
        service.budget = audio::PcmBudget::new(limit);
        service
    }
    pub fn active_id(&self) -> Option<u64> {
        self.active.as_ref().map(|task| task.id)
    }

    /// Source admitted by a retained result or by the still-active run.
    pub fn source_stamp(&self, id: u64) -> Option<SourceStamp> {
        self.results
            .iter()
            .find(|result| result.id == id)
            .map(|result| result.stamp)
            .or_else(|| {
                self.active
                    .as_ref()
                    .filter(|task| task.id == id)
                    .map(|task| task.stamp)
            })
    }

    pub fn cancellation_requested(&self, id: u64) -> Option<bool> {
        self.active
            .as_ref()
            .filter(|task| task.id == id)
            .map(|task| task.cancelled.flag().load(Ordering::Acquire))
    }

    /// Retire the worker only after its reliable terminal event has been received.
    pub fn finish_task(&mut self, id: u64) -> Option<bool> {
        if self.active_id() != Some(id) {
            return None;
        }
        let task = self.active.take().unwrap();
        let cancelled = task.cancelled.flag().load(Ordering::Acquire);
        let _ = task.worker.join();
        Some(cancelled)
    }

    pub fn results(&self) -> &VecDeque<RetainedResult> {
        &self.results
    }

    pub fn current_projection(&self) -> Option<Arc<ProcessingTaskStatus>> {
        self.current.and_then(|id| self.records.get(&id).cloned())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn history(
        &self,
    ) -> impl ExactSizeIterator<Item = &ProcessingTaskStatus> + DoubleEndedIterator {
        self.history
            .iter()
            .map(|id| self.records.get(id).unwrap().as_ref())
    }

    pub fn status(&self, id: u64) -> Option<&ProcessingTaskStatus> {
        self.records.get(&id).map(Arc::as_ref)
    }

    fn accept_status(&mut self, status: ProcessingTaskStatus) {
        self.current = Some(status.id);
        self.records.insert(status.id, Arc::new(status));
        self.prune_records();
        self.refresh_availability();
    }

    fn prune_records(&mut self) {
        self.records.retain(|id, _| {
            self.current == Some(*id)
                || self.history.contains(id)
                || self.results.iter().any(|result| result.id == *id)
        });
    }

    pub fn progress(
        &mut self,
        id: u64,
        completed: u64,
        total: Option<u64>,
        phase: ProcessingPhase,
    ) {
        if self.active_id() == Some(id)
            && let Some(status) = self.records.get_mut(&id)
            && (status.completed != completed || status.total != total || status.phase != phase)
        {
            let status = Arc::make_mut(status);
            status.completed = completed;
            status.total = total;
            status.phase = phase;
        }
    }

    pub fn node_status(&mut self, id: u64, node: GraphNodeStatus) {
        if self.active_id() == Some(id)
            && let Some(status) = self.records.get_mut(&id)
            && let Some(index) = status
                .graph_nodes
                .iter()
                .position(|entry| entry.node == node.node)
            && status.graph_nodes[index] != node
        {
            Arc::make_mut(status).graph_nodes[index] = node;
        }
    }

    pub fn graph_prepared(
        &mut self,
        id: u64,
        mode: GraphExecutionMode,
        boundary: Option<GraphPort>,
        plan: Option<GraphPlanInfo>,
    ) {
        if self.active_id() == Some(id)
            && let Some(status) = self.records.get_mut(&id)
            && (status.settings.graph_mode != Some(mode)
                || status.settings.graph_boundary != boundary
                || status.graph_plan != plan)
        {
            let status = Arc::make_mut(status);
            status.settings.graph_mode = Some(mode);
            status.settings.graph_boundary = boundary;
            status.graph_plan = plan;
            complete_prepared_nodes(status, mode, boundary);
        }
    }

    pub fn model_provenance(&mut self, id: u64, provenance: ModelProvenance) {
        if self.active_id() == Some(id)
            && let Some(status) = self.records.get_mut(&id)
            && (status.model_id != provenance.id || status.model_sha256 != provenance.sha256)
        {
            let status = Arc::make_mut(status);
            status.model_id = provenance.id;
            status.model_sha256 = provenance.sha256;
        }
    }

    pub fn outcome(&mut self, id: u64, phase: ProcessingPhase, error: Option<String>) {
        if let Some(status) = self.records.get_mut(&id) {
            let aborting = matches!(phase, ProcessingPhase::Failed | ProcessingPhase::Cancelled);
            let unfinished_nodes = aborting
                && status
                    .graph_nodes
                    .iter()
                    .any(|node| !node.phase.is_terminal());
            if status.phase == phase && status.error == error && !unfinished_nodes {
                return;
            }
            let status = Arc::make_mut(status);
            status.phase = phase;
            status.error = error;
            if aborting {
                // The task owner closes every unfinished node, including queued
                // descendants that can no longer run. Keep completed work and
                // specifically attributed node failures intact.
                for node in &mut status.graph_nodes {
                    if !node.phase.is_terminal() {
                        node.phase = phase;
                    }
                }
            }
        }
    }

    pub fn retain_terminal(&mut self, id: u64) {
        if !self.records.contains_key(&id) || self.history.contains(&id) {
            return;
        }
        self.history.push_back(id);
        while self.history.len() > 32 {
            self.history.pop_front();
        }
        self.prune_records();
    }

    /// Derive availability only where the resource owner changes its directory.
    /// Publication merely clones the already prepared Arc.
    fn refresh_availability(&mut self) {
        fn availability(
            result: Option<&RetainedResult>,
            status: &ProcessingTaskStatus,
        ) -> (Option<ProcessingOutputs>, bool) {
            let outputs = result.and_then(|result| {
                let outputs = ProcessingOutputs {
                    denoised: result.outputs.denoised.is_some(),
                    vocals: result.outputs.vocals.is_some(),
                    accompaniment: result.outputs.accompaniment.is_some(),
                };
                (outputs.denoised || outputs.vocals || outputs.accompaniment).then_some(outputs)
            });
            let graph_matches = result.map_or(status.graph_outputs.is_empty(), |result| {
                status.graph_outputs.len() == result.graph_outputs.len()
                    && status.graph_outputs.iter().zip(&result.graph_outputs).all(
                        |(published, (port, output))| {
                            published.port == *port
                                && published
                                    .context
                                    .as_ref()
                                    .map(|context| context.start_frame)
                                    == output.context.as_ref().map(|context| context.start_frame)
                        },
                    )
            });
            (outputs, graph_matches)
        }
        fn project(
            result: Option<&RetainedResult>,
            status: &mut ProcessingTaskStatus,
            outputs: Option<ProcessingOutputs>,
            graph_matches: bool,
        ) {
            status.outputs = outputs;
            if !graph_matches {
                status.graph_outputs = result
                    .into_iter()
                    .flat_map(|result| &result.graph_outputs)
                    .map(|(port, output)| GraphNodeOutput {
                        port: *port,
                        context: output.context.as_ref().map(|context| GraphOutputContext {
                            start_frame: context.start_frame,
                        }),
                    })
                    .collect();
            }
        }
        // Frozen graphs remain actionable when a retained run releases its PCM.
        let mut graph_runs = [None; 4];
        for (slot, result) in graph_runs.iter_mut().zip(&self.results) {
            if self
                .records
                .get(&result.id)
                .is_some_and(|status| status.settings.graph.is_some())
            {
                *slot = Some(result.id);
            }
        }
        for (id, record) in &mut self.records {
            let retained = self.results.iter().find(|result| result.id == *id);
            let (outputs, graph_matches) = availability(retained, record);
            let graph_actions_available = record
                .graph_run_id
                .is_some_and(|id| graph_runs.contains(&Some(id)));
            if record.outputs != outputs
                || !graph_matches
                || record.graph_actions_available != graph_actions_available
            {
                let status = Arc::make_mut(record);
                status.graph_actions_available = graph_actions_available;
                project(retained, status, outputs, graph_matches);
            }
        }
    }

    /// Commit the complete cached selection only after audio source admission.
    pub fn select_cached_audition(
        &mut self,
        id: u64,
        generation: u64,
        pcm: audio::PcmAudio,
        output: Option<GraphPort>,
    ) {
        self.replace_audition(Some(RetainedAudition {
            id,
            generation,
            resource: AuditionResource::Cached { pcm, output },
        }));
    }

    pub fn select_graph_audition(
        &mut self,
        id: u64,
        generation: u64,
        pcm: Option<audio::PcmAudio>,
        snapshot: audio::GraphSnapshot,
        output: GraphPort,
        mode: GraphExecutionMode,
    ) {
        self.replace_audition(Some(RetainedAudition {
            id,
            generation,
            resource: AuditionResource::Graph {
                pcm,
                snapshot,
                output,
                mode,
            },
        }));
    }

    pub fn clear_audition(&mut self) {
        self.replace_audition(None);
    }

    /// A selected stem mix is owned only by its audition; its PCM must not be
    /// destroyed on the business thread.
    fn replace_audition(&mut self, next: Option<RetainedAudition>) {
        let previous = std::mem::replace(&mut self.audition, next);
        if let (Some(previous), Some(reclaimer)) = (previous, self.reclaimer.as_ref()) {
            reclaimer.retire_audition(previous);
        }
    }

    /// Bypass and resume replace audio without replacing the retained selection.
    pub fn retain_audition_for_generation(&mut self, generation: u64) {
        if let Some(audition) = &mut self.audition {
            audition.generation = generation;
        }
    }

    pub fn resume_audition(&self) -> Result<(Option<audio::PcmAudio>, Option<audio::GraphRunner>)> {
        let audition = self
            .audition
            .as_ref()
            .context("No retained audition to resume")?;
        match &audition.resource {
            AuditionResource::Cached { pcm, .. } => Ok((Some(pcm.clone()), None)),
            AuditionResource::Graph { pcm, snapshot, .. } => {
                Ok((pcm.clone(), Some(snapshot.prepare()?)))
            }
        }
    }

    pub fn audition_projection(&self) -> Option<ProcessingAudition> {
        self.audition.as_ref().map(|audition| {
            let (output, mode) = match &audition.resource {
                AuditionResource::Cached { output, .. } => {
                    (*output, output.map(|_| GraphExecutionMode::Offline))
                }
                AuditionResource::Graph { output, mode, .. } => (Some(*output), Some(*mode)),
            };
            ProcessingAudition {
                id: audition.id,
                output,
                mode,
            }
        })
    }

    pub fn audition_id(&self) -> Option<u64> {
        self.audition.as_ref().map(|audition| audition.id)
    }

    pub fn audition_selected(&self, id: u64, generation: u64) -> bool {
        self.audition
            .as_ref()
            .is_some_and(|audition| audition.id == id && audition.generation == generation)
    }

    /// A successful render/export is committed before optional playback admission.
    /// Later audition failure changes its operation receipt, never its resources.
    pub fn accept_result(&mut self, result: Box<ProcessingResult>) {
        let retains_audio = result.retains_audio();
        self.finish_result(result, retains_audio);
    }

    /// A worker succeeded but Core rejected its output. Keep the worker's computation
    /// record (including an empty node list for cached exports), not its resources.
    pub fn reject_publication(
        &mut self,
        mut result: Box<ProcessingResult>,
        phase: ProcessingPhase,
        error: anyhow::Error,
    ) {
        let status = &mut result.status;
        let error = if let Some(port) = status.settings.graph_target {
            let message = format!(
                "Graph output #{}:{} was not published: {error:#}",
                port.node, port.port
            );
            if let Some(node) = status
                .graph_nodes
                .iter_mut()
                .find(|node| node.node == port.node)
            {
                node.error = Some(message.clone());
            }
            message
        } else {
            format!("{error:#}")
        };
        status.phase = phase;
        status.error = Some(error);
        self.finish_result(result, false);
    }

    fn finish_result(&mut self, result: Box<ProcessingResult>, retains_audio: bool) {
        let ProcessingResult {
            track,
            stamp,
            status,
            outputs,
            graph_outputs,
            runner,
            pending_export,
        } = *result;
        let retained = RetainedResult {
            id: status.id,
            track,
            stamp,
            outputs,
            graph_outputs,
            runner,
        };
        if retains_audio {
            if self.results.len() >= 4 {
                let index = self
                    .results
                    .iter()
                    .position(|result| Some(result.id) != self.audition_id())
                    .expect("only one retained result can be pinned");
                let result = self.results.remove(index).unwrap();
                self.reclaimer
                    .as_ref()
                    .unwrap()
                    .retire_retained(result, None);
            }
            self.results.push_back(retained);
        } else {
            self.reclaimer
                .as_ref()
                .unwrap()
                .retire_retained(retained, pending_export.map(|(file, _)| file));
        }
        self.accept_status(status);
    }

    pub fn take_graph_runner(&mut self, id: u64) -> Option<audio::GraphRunner> {
        self.results
            .iter_mut()
            .find(|result| result.id == id)?
            .runner
            .take()
    }

    pub fn stop_live_audition(&mut self, id: u64) {
        let Some(status) = self.records.get(&id) else {
            return;
        };
        if !matches!(
            status.settings.graph_mode,
            Some(GraphExecutionMode::Realtime | GraphExecutionMode::Hybrid)
        ) {
            return;
        }
        self.outcome(
            id,
            ProcessingPhase::Cancelled,
            Some("Live audition stopped".into()),
        );
        self.retain_terminal(id);
    }
    pub fn start(
        &mut self,
        track: Track,
        config: Arc<Config>,
        operation: ProcessingOperation,
        profile: Option<NoiseSelection>,
    ) -> Result<u64> {
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
    ) -> Result<u64> {
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
    ) -> Result<u64> {
        let (target, mode, export_path, cached_output, source_run, audio_export) =
            if let Some(request) = request {
                (
                    Some(request.target),
                    request.mode,
                    request.export_path,
                    request.cached_output,
                    request.source_run,
                    request.audio_export,
                )
            } else {
                (None, GraphExecutionMode::Offline, None, None, None, None)
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
        let graph_order = target
            .map(|target| config.processing_graph.ancestors(target))
            .transpose()?;
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
        if let Some(cached) = &cached_output {
            ensure!(
                cached.stamp == stamp,
                "Processing result is stale: source file changed before export admission"
            );
        }
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
                graph_requested_mode: target.map(|_| mode),
                graph_mode: None,
                graph_boundary: None,
                graph_export_path: export_path,
                audio_export,
            },
            graph_revision: target.map(|_| config.processing_graph.revision),
            graph_run_id: target.map(|_| source_run.unwrap_or(id)),
            graph_actions_available: false,
            graph_outputs: Vec::new(),
            graph_exported_path: None,
            graph_nodes: if let Some(order) = graph_order {
                order
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
        let cancelled = Arc::new(Cancellation::new()?);
        let flag = cancelled.clone();
        let tx = self.sender.clone();
        if self.reclaimer.is_none() {
            self.reclaimer = Some(reclaim::Reclaimer::new()?);
        }
        let budget = Arc::clone(&self.budget);
        #[cfg(any(test, feature = "test-support"))]
        let panic_after_processing = std::mem::take(&mut self.panic_after_processing);
        #[cfg(any(test, feature = "test-support"))]
        let panic_on_neural_progress = std::mem::take(&mut self.panic_on_neural_progress);
        let (worker, release) =
            self.reclaimer
                .as_ref()
                .unwrap()
                .spawn_waiting("rivu-processing", move |retired| {
                    // This new thread executes only this task.
                    #[cfg(any(test, feature = "test-support"))]
                    PANIC_ON_NEURAL_PROGRESS.set(panic_on_neural_progress);
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        retired?;
                        render::check_cancel(flag.flag())?;
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
                        }
                        render::check_cancel(flag.flag())?;
                        let result = if let Some(cached_output) = cached_output {
                            render::run_cached_export(
                                track,
                                stamp,
                                budget,
                                initial,
                                cached_output,
                                flag.flag(),
                                &tx,
                            )
                        } else {
                            render::run(track, stamp, config, budget, initial, &flag, &tx)
                        }?;
                        #[cfg(any(test, feature = "test-support"))]
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
            stamp,
            cancelled,
            worker,
        });
        self.accept_status(status);
        Ok(id)
    }
    pub fn cancel(&self, id: u64) -> Result<()> {
        let task = self
            .active
            .as_ref()
            .filter(|t| t.id == id)
            .context("No matching active processing task")?;
        task.cancelled.cancel();
        Ok(())
    }
    pub fn shutdown(&mut self) {
        let task = self.active.take();
        if let Some(task) = &task {
            task.cancelled.cancel();
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
    pub fn export_destination(&self, destination: &Path) -> Result<PathBuf> {
        export::export_destination(destination)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn memory_bytes(&self) -> u64 {
        self.budget.used_bytes()
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn hold_reclamation(&self) -> std::sync::mpsc::Sender<()> {
        self.reclaimer
            .as_ref()
            .expect("reclamation requires an admitted worker")
            .hold(self.sender.clone())
    }
    fn release_unpinned_audio(&mut self) {
        let Some(reclaimer) = self.reclaimer.as_ref() else {
            return;
        };
        for result in &mut self.results {
            if Some(result.id) == self.audition.as_ref().map(|audition| audition.id) {
                continue;
            }
            let outputs = std::mem::take(&mut result.outputs);
            let graph_outputs = std::mem::take(&mut result.graph_outputs);
            let runner = result.runner.take();
            if outputs.denoised.is_some()
                || outputs.vocals.is_some()
                || outputs.accompaniment.is_some()
                || !graph_outputs.is_empty()
                || runner.is_some()
            {
                reclaimer.release_audio(outputs, graph_outputs, runner);
            }
        }
        self.refresh_availability();
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
    pub fn start_mix(
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
            .find(|result| result.id == id)
            .context("Processing result is no longer retained")?;
        let stamp = result.stamp;
        let vocals = result.outputs.vocals.clone().context("No vocal stem")?;
        let accompaniment = result
            .outputs
            .accompaniment
            .clone()
            .context("No accompaniment stem")?;
        let budget = Arc::clone(&self.budget);
        let cancelled = Arc::new(Cancellation::new()?);
        let cancellation = Arc::clone(&cancelled);
        let tx = self.sender.clone();
        #[cfg(any(test, feature = "test-support"))]
        let panic_after_mix = std::mem::take(&mut self.panic_after_mix);
        let worker = self
            .reclaimer
            .as_ref()
            .unwrap()
            .spawn("rivu-stem-mix", move |retired| {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || -> Result<audio::PcmAudio> {
                        retired?;
                        let cancel = cancellation.flag();
                        render::validate_output(
                            &accompaniment,
                            vocals.sample_rate(),
                            vocals.layout(),
                            vocals.frames(),
                            cancel,
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
                            render::check_cancel(cancel)?;
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
                        render::check_cancel(cancel)?;
                        #[cfg(any(test, feature = "test-support"))]
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
            stamp,
            cancelled,
            worker,
        });
        self.current = Some(id);
        self.outcome(id, ProcessingPhase::Processing, None);
        self.prune_records();
        Ok(())
    }
}
