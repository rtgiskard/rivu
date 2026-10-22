use crate::{
    config::{Config, ConfigPatch, ConfigPersistence, LibraryColumn},
    library::{self, M3uItem, ScanResult},
    logging,
    model::*,
    projection::{GuiSnapshot, MprisSnapshot, TraySnapshot},
    response::{
        Ack, AckStatus, PlaybackSnapshot, QueryError, QueryResponse, QueryRevisions, StateResponse,
        StateRevisions, StateSections, ViewResponse,
    },
    store::Store,
};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use parking_lot::{Condvar, Mutex, RwLock};
use rand::seq::SliceRandom;
use rivu_audio::analysis::{AnalysisControl, AnalysisFrame, DisplayDemand};
use rivu_audio::{self as audio, AudioCommand, AudioEngine, AudioEvent, DecoderHint, MediaInfo};
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

mod query_epoch;
pub(crate) use query_epoch::{QueryEpoch, QueryTicket};

const ADMISSION_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const REPLY_TIMEOUT: Duration = Duration::from_secs(12);
const OPTIMIZE_REPLY_TIMEOUT: Duration = Duration::from_secs(120);

type Wakeup = Arc<dyn Fn() + Send + Sync>;
type QueryTimeout = Arc<dyn Fn(QueryResponse) + Send + Sync>;
#[expect(
    clippy::large_enum_variant,
    reason = "Keep one-shot query replies inline instead of allocating a response box"
)]
enum CoreResponse {
    Query(QueryResponse),
    Ack(Ack),
}
enum Operation {
    Query {
        query: Query,
        ticket: Option<QueryTicket>,
    },
    Command(Command),
    GuiConfig(GuiConfigChange),
    Navigate(Navigation),
    WaveformSnapshot(Sender<std::result::Result<Option<audio::WaveformScanSnapshot>, String>>),
}

#[derive(Clone, Copy)]
pub(crate) enum Navigation {
    Next,
    Previous,
}

pub(crate) enum GuiConfigChange {
    ToggleStillMode,
    ToggleWaveformSeek,
    ToggleLibraryColumn(LibraryColumn),
    AddLibraryRoot(std::path::PathBuf),
    RemoveLibraryRoot(std::path::PathBuf),
    // Only the cold Settings request allocates; ordinary queue slots stay small.
    Settings(Box<ConfigPatch>),
}
struct Request {
    operation: Operation,
    reply: Option<Sender<CoreResponse>>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CoreState {
    pub(crate) library: LibrarySnapshot,
    pub(crate) queue: QueueState,
    pub(crate) current_track: Option<Arc<Track>>,
    pub(crate) playback: PlaybackState,
    pub(crate) processing_audition: Option<crate::model::ProcessingAudition>,
    pub(crate) system: SystemState,
    pub(crate) revisions: StateRevisions,
    pub(crate) playlist_content_sequence: u64,
}

impl CoreState {
    pub(crate) fn current_track(&self) -> Option<&Track> {
        self.current_track.as_deref()
    }
    pub(crate) fn query_revisions(&self) -> QueryRevisions {
        self.library.query_revisions(self.queue.revision, None)
    }

    pub(crate) fn playlist_query_revisions(&self, playlist_id: i64) -> QueryRevisions {
        self.library
            .query_revisions(self.queue.revision, Some(playlist_id))
    }

    pub(crate) fn query_revisions_for(&self, query: &Query) -> QueryRevisions {
        self.library.query_revisions_for(query, self.queue.revision)
    }

    /// Queue state is constant-size; Arc identity avoids traversing history/config.
    /// Replacing an Arc conservatively invalidates its section even if values match.
    fn changed_sections(&self, previous: &Self) -> StateSections {
        let current_track_changed = match (&self.current_track, &previous.current_track) {
            (Some(current), Some(previous)) => !Arc::ptr_eq(current, previous),
            (None, None) => false,
            _ => true,
        };
        let system = &self.system;
        let old = &previous.system;
        StateSections {
            playback: self.playback != previous.playback
                || current_track_changed
                || self.processing_audition != previous.processing_audition,
            queue: self.queue != previous.queue,
            library: self.library.revision != previous.library.revision
                || self.library.structure_revision != previous.library.structure_revision
                || self.library.playlist_revision != previous.library.playlist_revision
                || self.library.duplicate_groups_revision
                    != previous.library.duplicate_groups_revision
                || self.library.duplicate_members_revision
                    != previous.library.duplicate_members_revision
                || !Arc::ptr_eq(
                    &self.library.playlist_entry_revisions,
                    &previous.library.playlist_entry_revisions,
                )
                || self.library.track_total != previous.library.track_total
                || self.library.playlist_total != previous.library.playlist_total
                || !Arc::ptr_eq(&self.library.history, &previous.library.history),
            system: system.scanning != old.scanning
                || system.scan_message != old.scan_message
                || system.scan_progress != old.scan_progress
                || system.relocation_task != old.relocation_task
                || system.processing != old.processing
                || system.last_error != old.last_error
                || !Arc::ptr_eq(&system.devices, &old.devices)
                || system.selected_device != old.selected_device
                || !Arc::ptr_eq(&system.config, &old.config)
                || system.config_path != old.config_path
                || system.mpris_status != old.mpris_status
                || system.ffmpeg_status != old.ffmpeg_status
                || system.database_optimization != old.database_optimization
                || system.shutting_down != old.shutting_down,
        }
    }
}

/// Cloneable access to core business commands, queries, and published state.
#[derive(Clone)]
pub struct CoreHandle {
    state: Arc<RwLock<CoreState>>,
    pub(crate) analysis: Arc<RwLock<AnalysisFrame>>,
    analysis_control: Arc<AnalysisControl>,
    pub(crate) waveform: Arc<RwLock<audio::WaveformFrame>>,
    sender: Sender<Request>,
    wakeup: Arc<RwLock<Option<Wakeup>>>,
    subscribers: Arc<RwLock<Vec<Sender<()>>>>,
    // Window-scoped: cloning a GUI handle must not change IPC/CLI query policy.
    query_timeout: Option<QueryTimeout>,
}

pub(crate) struct MprisServiceState {
    pub enabled: bool,
    pub shutting_down: bool,
}

impl CoreHandle {
    /// Total caller waiting budget, including queue admission and the query reply.
    pub fn query_wait_timeout() -> Duration {
        ADMISSION_TIMEOUT + REPLY_TIMEOUT
    }

    /// Total caller waiting budget; timing out never revokes an admitted command.
    pub fn command_wait_timeout(command: &Command) -> Duration {
        ADMISSION_TIMEOUT + Self::command_reply_timeout(command)
    }

    fn command_reply_timeout(command: &Command) -> Duration {
        match command {
            Command::OptimizeDatabase => OPTIMIZE_REPLY_TIMEOUT,
            _ => REPLY_TIMEOUT,
        }
    }

    /// Publish complete display demand without entering the business command queue.
    pub(crate) fn set_display_demand(&self, demand: DisplayDemand) {
        self.analysis_control.set_display_demand(demand);
    }

    pub(crate) fn analysis_frame_is_current(&self, frame: &AnalysisFrame) -> bool {
        frame.display_token == self.analysis_control.display_token()
    }

    /// Coalesce a GUI consumer frame into at most one pending analysis job.
    pub(crate) fn request_analysis_frame(&self) {
        self.analysis_control.request_frame();
    }

    /// Capture the active audio source and processing state without waiting on the GUI thread.
    pub(crate) fn waveform_snapshot(
        &self,
    ) -> Result<Receiver<std::result::Result<Option<audio::WaveformScanSnapshot>, String>>> {
        let (reply, receiver) = bounded(1);
        self.sender
            .try_send(Request {
                operation: Operation::WaveformSnapshot(reply),
                reply: None,
            })
            .context("Rivu is busy or stopped")?;
        Ok(receiver)
    }

    pub(crate) fn update_gui_config(&self, change: GuiConfigChange) -> Result<()> {
        self.sender
            .try_send(Request {
                operation: Operation::GuiConfig(change),
                reply: None,
            })
            .context("Rivu is busy or stopped")
    }

    pub(crate) fn apply_gui_settings(&self, patch: ConfigPatch) -> Ack {
        self.operation_ack(
            Operation::GuiConfig(GuiConfigChange::Settings(Box::new(patch))),
            REPLY_TIMEOUT,
        )
    }

    pub fn send(&self, command: Command) -> Result<()> {
        self.sender
            .try_send(Request {
                operation: Operation::Command(command),
                reply: None,
            })
            .context("Rivu is busy or stopped")
    }
    #[cfg(test)]
    pub fn is_shutting_down(&self) -> bool {
        self.state.read().system.shutting_down
    }
    pub fn config_snapshot(&self) -> Arc<Config> {
        Arc::clone(&self.state.read().system.config)
    }
    pub(crate) fn mpris_service_state(&self) -> MprisServiceState {
        let state = self.state.read();
        MprisServiceState {
            enabled: state.system.config.mpris_enabled,
            shutting_down: state.system.shutting_down,
        }
    }
    pub fn revision(&self) -> u64 {
        self.state.read().system.revision
    }
    pub fn mpris_snapshot(&self) -> MprisSnapshot {
        MprisSnapshot::from_core(&self.state.read())
    }
    pub fn gui_snapshot(&self) -> GuiSnapshot {
        GuiSnapshot::from_core(&self.state.read())
    }
    /// Compare and project under one lock so the returned revision owns its data.
    pub(crate) fn gui_snapshot_if_changed(&self, revision: u64) -> Option<GuiSnapshot> {
        let state = self.state.read();
        (state.system.revision != revision).then(|| GuiSnapshot::from_core(&state))
    }
    pub(crate) fn tray_snapshot(&self) -> TraySnapshot {
        TraySnapshot::from_core(&self.state.read())
    }
    pub fn set_wakeup(&self, callback: impl Fn() + Send + Sync + 'static) {
        *self.wakeup.write() = Some(Arc::new(callback));
        self.notify_subscribers();
    }
    pub fn clear_wakeup(&self) {
        *self.wakeup.write() = None;
        self.notify_subscribers();
    }
    fn notify_subscribers(&self) {
        self.subscribers.write().retain(|sender| {
            !matches!(
                sender.try_send(()),
                Err(crossbeam_channel::TrySendError::Disconnected(_))
            )
        });
    }
    pub fn subscribe(&self) -> Receiver<()> {
        let (sender, receiver) = bounded(1);
        self.subscribers.write().push(sender);
        receiver
    }
    /// Query metadata and rows are produced in the same Core dispatch.
    pub fn query(&self, query: Query) -> QueryResponse {
        self.query_with_ticket(query, None, false, REPLY_TIMEOUT)
    }

    /// Report GUI waiting failures without treating them as physical completion.
    pub(crate) fn with_query_timeout(
        mut self,
        notify: impl Fn(QueryResponse) + Send + Sync + 'static,
    ) -> Self {
        self.query_timeout = Some(Arc::new(notify));
        self
    }

    /// GUI reads retain their caller's physical slot until reply or disconnect.
    pub(crate) fn query_for_gui(&self, query: Query) -> QueryResponse {
        self.query_with_ticket(query, None, true, REPLY_TIMEOUT)
    }

    /// Cancel stale queued reads without interrupting an already executing read.
    /// A timeout notifies the GUI, then drains and discards the actual reply.
    pub(crate) fn query_if_current(&self, query: Query, ticket: QueryTicket) -> QueryResponse {
        self.query_with_ticket(query, Some(ticket), true, REPLY_TIMEOUT)
    }

    fn query_with_ticket(
        &self,
        query: Query,
        ticket: Option<QueryTicket>,
        physical_retirement: bool,
        timeout: Duration,
    ) -> QueryResponse {
        if ticket.as_ref().is_some_and(|ticket| !ticket.is_current()) {
            return QueryResponse {
                revisions: self.state.read().query_revisions_for(&query),
                result: Err(QueryError::Cancelled),
            };
        }
        let (tx, rx) = bounded(1);
        let request = Request {
            operation: Operation::Query {
                query,
                ticket: ticket.clone(),
            },
            reply: Some(tx),
        };
        let response = self
            .sender
            .send_timeout(request, ADMISSION_TIMEOUT)
            .map_err(|error| format!("Core unavailable: {error}"))
            .and_then(|()| self.receive_query(&rx, ticket.as_ref(), physical_retirement, timeout));
        match response {
            Ok(CoreResponse::Query(response)) => response,
            Ok(CoreResponse::Ack(_)) => unreachable!("query request returned Ack"),
            Err(error) => QueryResponse {
                revisions: self.state.read().query_revisions(),
                result: Err(QueryError::Message(error)),
            },
        }
    }

    fn receive_query(
        &self,
        receiver: &Receiver<CoreResponse>,
        ticket: Option<&QueryTicket>,
        physical_retirement: bool,
        timeout: Duration,
    ) -> std::result::Result<CoreResponse, String> {
        match receiver.recv_timeout(timeout) {
            Ok(response) => Ok(response),
            Err(error) => {
                let message = format!("Core response unavailable: {error}");
                if physical_retirement && error == crossbeam_channel::RecvTimeoutError::Timeout {
                    if ticket.is_none_or(QueryTicket::is_current)
                        && let Some(notify) = &self.query_timeout
                    {
                        notify(QueryResponse {
                            revisions: self.state.read().query_revisions(),
                            result: Err(QueryError::Message(message.clone())),
                        });
                    }
                    // The request still owns its Core queue/execution slot. Neither
                    // timeout nor cancellation can retire it; never return late data.
                    let _ = receiver.recv();
                }
                Err(message)
            }
        }
    }

    /// Read only requested state sections under one short shared-state lock.
    pub fn state(&self, sections: StateSections) -> StateResponse {
        let state = self.state.read();
        StateResponse {
            revisions: state.revisions,
            playback: sections.playback.then(|| PlaybackSnapshot {
                playback: state.playback.clone(),
                current_track: state.current_track.clone(),
            }),
            queue: sections.queue.then(|| state.queue.clone()),
            library: sections.library.then(|| state.library.clone()),
            system: sections.system.then(|| state.system.clone()),
        }
    }

    pub fn state_revisions(&self) -> StateRevisions {
        self.state.read().revisions
    }

    pub fn request_ack(&self, command: Command) -> Ack {
        let duration = Self::command_reply_timeout(&command);
        self.operation_ack(Operation::Command(command), duration)
    }

    /// Navigate in one Core dispatch, preserving its status at execution time.
    pub(crate) fn navigate(&self, direction: Navigation) -> Ack {
        self.operation_ack(Operation::Navigate(direction), REPLY_TIMEOUT)
    }

    fn operation_ack(&self, operation: Operation, duration: Duration) -> Ack {
        let (tx, rx) = bounded(1);
        let request = Request {
            operation,
            reply: Some(tx),
        };
        if let Err(error) = self.sender.send_timeout(request, ADMISSION_TIMEOUT) {
            return Ack::new(
                AckStatus::NotAdmitted,
                Some(format!("Core command not admitted: {error}")),
                self.revision(),
            );
        }
        match rx.recv_timeout(duration) {
            Ok(CoreResponse::Ack(ack)) => ack,
            Ok(CoreResponse::Query(_)) => unreachable!("command request returned query"),
            Err(error) => Ack::new(
                AckStatus::Unknown,
                Some(format!(
                    "Core command outcome unknown: {error}; do not automatically retry"
                )),
                self.revision(),
            ),
        }
    }
}

/// Runtime lifecycle control, separate from business requests and view wakeups.
#[derive(Clone)]
pub(crate) struct RuntimeControl {
    shutdown: Sender<()>,
    completion: Arc<Completion>,
}

#[derive(Default)]
struct Completion {
    state: Mutex<CompletionState>,
    acknowledged: Condvar,
}

#[derive(Default)]
struct CompletionState {
    ack: Option<Ack>,
    finished: bool,
    callback: Option<Wakeup>,
}

impl RuntimeControl {
    fn new() -> (Self, Receiver<()>) {
        let (shutdown, receiver) = bounded(1);
        (
            Self {
                shutdown,
                completion: Arc::new(Completion::default()),
            },
            receiver,
        )
    }

    pub(crate) fn request_shutdown(&self) -> Result<()> {
        match self.shutdown.try_send(()) {
            Ok(()) | Err(crossbeam_channel::TrySendError::Full(())) => Ok(()),
            Err(crossbeam_channel::TrySendError::Disconnected(())) => {
                anyhow::bail!("Rivu is stopped")
            }
        }
    }

    /// Acknowledges the shutdown request, not completion of runtime teardown.
    pub(crate) fn shutdown_ack(&self, revision: impl Fn() -> u64) -> Ack {
        let requested = self.request_shutdown();
        let mut state = self.completion.state.lock();
        let deadline = Instant::now() + REPLY_TIMEOUT;
        while requested.is_ok() && state.ack.is_none() && !state.finished {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            self.completion.acknowledged.wait_for(&mut state, remaining);
        }
        state.ack.clone().unwrap_or_else(|| {
            let status = if requested.is_err() {
                AckStatus::NotAdmitted
            } else {
                AckStatus::Unknown
            };
            Ack::new(
                status,
                Some(if status == AckStatus::NotAdmitted {
                    "Core stopped; shutdown request not admitted".into()
                } else {
                    "Core shutdown outcome unknown; do not automatically retry".into()
                }),
                revision(),
            )
        })
    }

    fn acknowledge(&self, ack: Ack) {
        self.completion.state.lock().ack = Some(ack);
        self.completion.acknowledged.notify_all();
    }

    pub(crate) fn set_completion_callback(&self, callback: impl Fn() + Send + Sync + 'static) {
        let callback: Wakeup = Arc::new(callback);
        let mut state = self.completion.state.lock();
        state.callback = Some(Arc::clone(&callback));
        let finished = state.finished;
        drop(state);
        if finished {
            callback();
        }
    }

    fn complete(&self) {
        let mut state = self.completion.state.lock();
        state.finished = true;
        let callback = state.callback.clone();
        drop(state);
        self.completion.acknowledged.notify_all();
        if let Some(callback) = callback {
            callback();
        }
    }
}

// Runs on normal return and panic, including failure before Core::run starts.
struct CompletionGuard(RuntimeControl);
impl Drop for CompletionGuard {
    fn drop(&mut self) {
        self.0.complete();
    }
}

pub struct Runtime {
    pub handle: CoreHandle,
    pub(crate) control: RuntimeControl,
    worker: Option<JoinHandle<()>>,
}
impl Runtime {
    /// Load host configuration only when its path can be published over IPC.
    pub(crate) fn load_startup_config(config_path: &Path) -> Result<Config> {
        config_path
            .to_str()
            .context("Configuration path is not valid UTF-8; IPC requires UTF-8 paths")?;
        Config::load(config_path)
    }

    #[cfg(test)]
    /// Start a standalone core without installing or owning process logging.
    pub fn start(data_dir: &Path, config_path: &Path) -> Result<Self> {
        Self::start_inner(
            data_dir,
            config_path,
            Self::load_startup_config(config_path)?,
            None,
        )
    }

    // App loads through load_startup_config before starting process logging.
    pub(crate) fn start_with_logging(
        data_dir: &Path,
        config_path: &Path,
        config: Config,
        logging: logging::Control,
    ) -> Result<Self> {
        Self::start_inner(data_dir, config_path, config, Some(logging))
    }

    fn start_inner(
        data_dir: &Path,
        config_path: &Path,
        mut config: Config,
        logging: Option<logging::Control>,
    ) -> Result<Self> {
        config.library_roots = config
            .library_roots
            .iter()
            .map(|root| library::logical_path(root))
            .collect::<Result<_>>()?;
        config.validate()?;
        // An existing file establishes an effective, normalized baseline without
        // rewriting user formatting or materializing omitted defaults at startup.
        let saved_config = config_path.try_exists()?.then(|| config.clone());
        std::fs::create_dir_all(data_dir)?;
        tracing::info!(path = %config_path.display(), "configuration_loaded");
        // Keep the handle usable with a config-only snapshot. Database views,
        // device enumeration and optional decoder probing run on the core
        // worker so the GUI is not blocked by startup I/O.
        let store = Store::open(&data_dir.join("library.db"))?;
        store.set_track_cache_page_size(config.page_size);
        tracing::debug!("store_ready");
        let engine = AudioEngine::new(config.media_read_buffer_mb)?;
        tracing::info!("audio_engine_ready");
        let (sender, receiver) = bounded(64);
        let (control, shutdown_receiver) = RuntimeControl::new();
        let shared = Arc::new(RwLock::new(CoreState::default()));
        let wakeup = Arc::new(RwLock::new(None));
        let subscribers = Arc::new(RwLock::new(Vec::new()));
        let initial = {
            let mut state = CoreState::default();
            state.playback.volume = config.volume;
            state.playback.shuffle = config.shuffle;
            state.playback.repeat = config.repeat;
            state.system.selected_device = config.output_device.clone();
            state.system.config = Arc::new(config);
            state.system.config_path = config_path.to_path_buf();
            state
        };
        *shared.write() = initial.clone();
        let handle = CoreHandle {
            state: shared.clone(),
            analysis: engine.analysis.clone(),
            analysis_control: Arc::clone(&engine.analysis_control),
            waveform: Arc::clone(&engine.waveform),
            sender,
            wakeup: wakeup.clone(),
            subscribers: subscribers.clone(),
            query_timeout: None,
        };
        let worker_control = control.clone();
        let processing_data_dir = data_dir.to_path_buf();
        let worker = thread::Builder::new()
            .name("rivu-core".into())
            .spawn(move || {
                let _completion = CompletionGuard(worker_control.clone());
                let (scan_tx, scan_rx) = bounded(1);
                let (relocation_tx, relocation_rx) = bounded(1);
                let mut core = Core {
                    store,
                    engine,
                    logging,
                    state: initial,
                    shared,
                    wakeup,
                    subscribers,
                    generation: 0,
                    output_settings_request: 0,
                    queue_ids: Vec::new(),
                    playback: None,
                    retired_playback: None,
                    playback_write_deferred: false,
                    pending_bitrate: None,
                    pending_navigation: None,
                    active_export: None,
                    active_optimization: None,
                    processing: crate::processing::Service::new(processing_data_dir),
                    processing_results: VecDeque::new(),
                    scan_tx,
                    scan_rx,
                    active_scan: None,
                    scan_results: VecDeque::new(),
                    next_scan_id: 1,
                    relocation_tx,
                    relocation_rx,
                    active_relocation: None,
                    relocation_results: VecDeque::new(),
                    next_relocation_id: 1,
                    played: Vec::new(),
                    played_cursor: 0,
                    saved_config,
                    config_save_error: None,
                };
                tracing::debug!("core_worker_started");
                let initial_load_started = Instant::now();
                tracing::debug!("initial_load_started");
                if let Err(error) = core.load_initial_state() {
                    tracing::error!(error = %error, "initial_load_failed");
                    core.state.system.last_error = Some(format!("Loading library: {error:#}"));
                    core.publish();
                } else {
                    tracing::info!(
                        elapsed_ms = initial_load_started.elapsed().as_millis(),
                        "initial_load_completed"
                    );
                }
                let _ = core.audio(AudioCommand::StillMode(core.state.system.config.still_mode));
                let _ = core.audio(AudioCommand::Volume(core.state.playback.volume));
                let _ = core.audio(AudioCommand::FfmpegEnabled(
                    core.state.system.config.ffmpeg_enabled,
                ));
                let _ = core.audio(AudioCommand::OutputSettings {
                    request: core.output_settings_request,
                    device: core.state.system.selected_device.clone(),
                    auto_mix: core.state.system.config.pipewire_auto_mix,
                });
                let _ = core.audio(AudioCommand::AnalysisSettings(
                    core.state.system.config.as_ref().into(),
                ));
                let _ = core.audio(AudioCommand::Equalizer(Arc::new(
                    core.state.system.config.equalizer.clone(),
                )));
                core.run(receiver, shutdown_receiver, &worker_control);
            })?;
        Ok(Self {
            handle,
            control,
            worker: Some(worker),
        })
    }
    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(|worker| worker.is_finished())
    }
    pub fn join(&mut self) -> thread::Result<()> {
        if let Some(worker) = self.worker.take() {
            worker.join()
        } else {
            Ok(())
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            let _ = self.control.request_shutdown();
        }
        let _ = self.join();
    }
}

struct PlaybackStats {
    track_id: i64,
    source: Option<(crate::store::PlaybackSource, std::fs::Metadata)>,
    heard: f64,
    position: Option<f64>,
    started: bool,
    counted: bool,
    last_heard_at: Option<i64>,
    activity_updated: bool,
    paused_at: Option<i64>,
}
impl PlaybackStats {
    fn record_heard(&mut self, heard: f64, heard_at: i64) -> bool {
        if !heard.is_finite() || heard <= self.heard {
            return false;
        }
        self.heard = heard;
        self.last_heard_at = Some(
            self.last_heard_at
                .map_or(heard_at, |previous| previous.max(heard_at)),
        );
        self.activity_updated = true;
        true
    }
}
struct RetiredPlayback {
    stats: PlaybackStats,
    duration: Option<f64>,
}

struct ScanFinished {
    id: u64,
    result: Result<ScanResult>,
    import: Option<(String, Vec<M3uItem>)>,
    progress: ScanProgress,
}
enum ScanEvent {
    Progress {
        id: u64,
        progress: ScanProgress,
    },
    KnownSource {
        id: u64,
        path: PathBuf,
        reply: Sender<Result<Option<library::KnownFile>>>,
    },
    Finished(ScanFinished),
    Saved(catalog::ScanSaved),
}
const MAX_RETAINED_SCAN_RESULTS: usize = 32;
struct ScanTask {
    status: ScanTaskStatus,
    cancellation: library::ScanCancellation,
    worker: JoinHandle<()>,
    pending_save: Option<ScanFinished>,
}
struct RelocationTask {
    status: RelocationTaskStatus,
    prepared: crate::store::PreparedRelocation,
    cancellation: library::ScanCancellation,
    worker: Option<JoinHandle<()>>,
}
impl Drop for RelocationTask {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
struct RelocationFinished {
    id: u64,
    result: Result<relocation::VerifiedTarget>,
}
struct Core {
    store: Store,
    engine: AudioEngine,
    logging: Option<logging::Control>,
    state: CoreState,
    shared: Arc<RwLock<CoreState>>,
    wakeup: Arc<RwLock<Option<Wakeup>>>,
    subscribers: Arc<RwLock<Vec<Sender<()>>>>,
    generation: u64,
    output_settings_request: u64,
    // The only full queue projection: compact occurrence IDs, never shared with clients.
    queue_ids: Vec<u64>,
    playback: Option<PlaybackStats>,
    // At most one playback can retire while database writes are excluded.
    retired_playback: Option<RetiredPlayback>,
    playback_write_deferred: bool,
    pending_bitrate: Option<(u64, bool)>,
    pending_navigation: Option<PlaybackStatus>,
    active_export: Option<export::ExportTask>,
    active_optimization: Option<maintenance::OptimizationTask>,
    processing: crate::processing::Service,
    processing_results: VecDeque<ProcessingTaskStatus>,
    scan_tx: Sender<ScanEvent>,
    scan_rx: Receiver<ScanEvent>,
    active_scan: Option<ScanTask>,
    scan_results: VecDeque<ScanTaskStatus>,
    next_scan_id: u64,
    relocation_tx: Sender<RelocationFinished>,
    relocation_rx: Receiver<RelocationFinished>,
    active_relocation: Option<RelocationTask>,
    relocation_results: VecDeque<RelocationTaskStatus>,
    next_relocation_id: u64,
    played: Vec<u64>,
    // Number of history entries through the current (or just removed) entry.
    played_cursor: usize,
    saved_config: Option<Config>,
    config_save_error: Option<String>,
}
impl Core {
    fn load_initial_state(&mut self) -> Result<()> {
        self.state.library.track_total = self.store.track_count()?;
        self.state.library.playlist_total = self.store.playlist_summary_page(None, 0, 0)?.total;
        for id in self.store.playlist_ids()? {
            self.state.playlist_content_sequence = self
                .state
                .playlist_content_sequence
                .checked_add(1)
                .expect("Playlist content revision exhausted");
            Arc::make_mut(&mut self.state.library.playlist_entry_revisions)
                .insert(id, self.state.playlist_content_sequence);
        }
        self.state.library.history = Arc::new(self.store.history(MAX_QUERY_ROWS)?);
        self.state.system.devices = Arc::new(audio::devices().unwrap_or_default());
        self.state.system.ffmpeg_status = if self.state.system.config.ffmpeg_enabled {
            audio::ffmpeg_status().unwrap_or_else(|error| format!("unavailable: {error:#}"))
        } else {
            "disabled".to_owned()
        };
        self.queue_ids = self.store.queue_ids()?;
        self.refresh_queue_tracks()?;
        self.publish();
        Ok(())
    }
}
mod catalog;
mod command;
mod dispatch;
mod export;
mod maintenance;
mod persistence;
mod playback;
mod playlists;
mod processing;
mod queue;
mod relocation;
mod settings;
mod tracks;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests;
