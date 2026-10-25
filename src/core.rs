//! Core waits for events and applies state transitions on one dedicated owner.
//! Tokio drives bounded admission, async replies, IPC and independent long jobs.
//! Caller timeout never cancels admitted work or authorizes replay. Real-time
//! audio stays independent; shutdown completion includes physical retirement.

use crate::{
    config::{Config, ConfigPatch, ConfigPersistence, LibraryColumn},
    library::{self, M3uItem, ScanResult},
    logging,
    projection::{GuiSnapshot, MprisSnapshot, TraySnapshot},
    store::Store,
};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use futures::FutureExt;
use parking_lot::{Mutex, RwLock};
use rand::seq::SliceRandom;
use rivu_audio::analysis::{AnalysisControl, AnalysisFrame, DisplayDemand};
use rivu_audio::{self as audio, AudioCommand, AudioEngine, AudioEvent, MediaInfo};
use rivu_format::media::DecoderHint;
use rivu_protocol::{
    model::*,
    response::{
        Ack, AckStatus, PlaybackSnapshot, QueryError, QueryResponse, QueryRevisions, StateResponse,
        StateRevisions, StateSections, ViewResponse,
    },
};
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, mpsc, oneshot, watch};

mod tasks;
use tasks::{TaskRuntime, wait_on};

mod query_epoch;
pub(crate) use query_epoch::{QueryEpoch, QueryTicket};

use rivu_protocol::{ADMISSION_TIMEOUT, REPLY_TIMEOUT, command_reply_timeout};

type Wakeup = Arc<dyn Fn() + Send + Sync>;
type QueryTimeout = Arc<dyn Fn(QueryResponse) + Send + Sync>;
#[expect(
    clippy::large_enum_variant,
    reason = "Keep one-shot query replies inline instead of allocating a response box"
)]
#[derive(Debug)]
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
    reply: Option<oneshot::Sender<CoreResponse>>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CoreState {
    pub(crate) library: LibrarySnapshot,
    pub(crate) queue: QueueState,
    pub(crate) current_track: Option<Arc<Track>>,
    pub(crate) playback: PlaybackState,
    pub(crate) processing_audition: Option<crate::projection::ProcessingAudition>,
    pub(crate) system: SystemState,
    pub(crate) scan_owner: Option<crate::projection::MediaTaskOwner>,
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
                || system.scan_task_id != old.scan_task_id
                || self.scan_owner != previous.scan_owner
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
    sender: mpsc::Sender<Request>,
    executor: tokio::runtime::Handle,
    revision_updates: watch::Receiver<StateRevisions>,
    wakeup: Arc<RwLock<Option<Wakeup>>>,
    subscribers: Arc<RwLock<Vec<Sender<()>>>>,
    // Window-scoped: cloning a GUI handle must not change IPC/CLI query policy.
    query_timeout: Option<QueryTimeout>,
    #[cfg(test)]
    _test_tasks: Option<Arc<TaskRuntime>>,
}

pub(crate) struct MprisServiceState {
    pub enabled: bool,
    pub shutting_down: bool,
}

impl CoreHandle {
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

    pub(crate) fn subscribe_revisions(&self) -> watch::Receiver<StateRevisions> {
        self.revision_updates.clone()
    }

    pub(crate) fn executor(&self) -> &tokio::runtime::Handle {
        &self.executor
    }

    pub(crate) async fn query_async(&self, query: Query) -> QueryResponse {
        self.query_with_ticket_async(query, None, false, REPLY_TIMEOUT)
            .await
    }
    #[cfg(test)]
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
        wait_on(
            &self.executor,
            self.query_with_ticket_async(query, ticket, physical_retirement, timeout),
        )
    }

    async fn query_with_ticket_async(
        &self,
        query: Query,
        ticket: Option<QueryTicket>,
        physical_retirement: bool,
        duration: Duration,
    ) -> QueryResponse {
        if ticket.as_ref().is_some_and(|ticket| !ticket.is_current()) {
            return QueryResponse {
                revisions: self.state.read().query_revisions_for(&query),
                result: Err(QueryError::Cancelled),
            };
        }
        let (tx, mut rx) = oneshot::channel();
        let request = Request {
            operation: Operation::Query {
                query,
                ticket: ticket.clone(),
            },
            reply: Some(tx),
        };
        let response = match self.admit(request).await {
            Err(error) => Err(format!("Core unavailable: {error}")),
            Ok(()) => match tokio::time::timeout(duration, &mut rx).await {
                Ok(result) => result.map_err(|error| format!("Core response unavailable: {error}")),
                Err(error) => {
                    let message = format!("Core response unavailable: {error}");
                    if physical_retirement {
                        if ticket.as_ref().is_none_or(QueryTicket::is_current)
                            && let Some(notify) = &self.query_timeout
                        {
                            notify(QueryResponse {
                                revisions: self.state.read().query_revisions(),
                                result: Err(QueryError::Message(message.clone())),
                            });
                        }
                        // Timeout ends logical waiting, never physical ownership.
                        // GUI slots remain occupied until Core sends or disconnects.
                        let _ = rx.await;
                    }
                    Err(message)
                }
            },
        };
        match response {
            Ok(CoreResponse::Query(response)) => response,
            Ok(CoreResponse::Ack(_)) => unreachable!("query request returned Ack"),
            Err(error) => QueryResponse {
                revisions: self.state.read().query_revisions(),
                result: Err(QueryError::Message(error)),
            },
        }
    }

    async fn admit(&self, request: Request) -> Result<()> {
        tokio::time::timeout(ADMISSION_TIMEOUT, self.sender.send(request))
            .await
            .context("Core queue admission timed out")?
            .context("Rivu is stopped")
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
        let duration = command_reply_timeout(&command);
        self.operation_ack(Operation::Command(command), duration)
    }

    /// Navigate in one Core dispatch, preserving its status at execution time.
    pub(crate) fn navigate(&self, direction: Navigation) -> Ack {
        self.operation_ack(Operation::Navigate(direction), REPLY_TIMEOUT)
    }

    fn operation_ack(&self, operation: Operation, duration: Duration) -> Ack {
        wait_on(
            &self.executor,
            self.operation_ack_async(operation, duration),
        )
    }

    pub(crate) async fn request_ack_async(&self, command: Command) -> Ack {
        let duration = command_reply_timeout(&command);
        self.operation_ack_async(Operation::Command(command), duration)
            .await
    }

    async fn operation_ack_async(&self, operation: Operation, duration: Duration) -> Ack {
        let (tx, rx) = oneshot::channel();
        let request = Request {
            operation,
            reply: Some(tx),
        };
        if let Err(error) = self.admit(request).await {
            return Ack::new(
                AckStatus::NotAdmitted,
                Some(format!("Core command not admitted: {error}")),
                self.revision(),
            );
        }
        match tokio::time::timeout(duration, rx).await {
            Ok(Ok(CoreResponse::Ack(ack))) => ack,
            Ok(Ok(CoreResponse::Query(_))) => unreachable!("command request returned query"),
            result => Ack::new(
                AckStatus::Unknown,
                Some(format!(
                    "Core command outcome unknown: {}; do not automatically retry",
                    match result {
                        Ok(Err(error)) => error.to_string(),
                        Err(error) => error.to_string(),
                        Ok(Ok(_)) => unreachable!("successful responses matched above"),
                    }
                )),
                self.revision(),
            ),
        }
    }
}

/// Runtime lifecycle control, separate from business requests and view wakeups.
#[derive(Clone)]
pub(crate) struct RuntimeControl {
    shutdown: mpsc::Sender<()>,
    completion: Arc<Completion>,
}

#[derive(Default)]
struct Completion {
    state: Mutex<CompletionState>,
    acknowledged: Notify,
}

#[derive(Default)]
struct CompletionState {
    ack: Option<Ack>,
    finished: bool,
    callback: Option<Wakeup>,
}

impl RuntimeControl {
    fn new() -> (Self, mpsc::Receiver<()>) {
        let (shutdown, receiver) = mpsc::channel(1);
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
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(())) => {
                anyhow::bail!("Rivu is stopped")
            }
        }
    }

    /// Acknowledges the shutdown request, not completion of runtime teardown.
    pub(crate) async fn shutdown_ack_async(&self, revision: impl Fn() -> u64) -> Ack {
        let requested = self.request_shutdown();
        let waiting = async {
            loop {
                let notified = self.completion.acknowledged.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                {
                    let state = self.completion.state.lock();
                    if state.ack.is_some() || state.finished || requested.is_err() {
                        return;
                    }
                }
                notified.await;
            }
        };
        let _ = tokio::time::timeout(REPLY_TIMEOUT, waiting).await;
        self.completion.state.lock().ack.clone().unwrap_or_else(|| {
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
        self.completion.acknowledged.notify_waiters();
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
        self.completion.acknowledged.notify_waiters();
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
    // Last field: Drop joins Core before the Tokio driver/pool can stop. App
    // separately retires IPC before releasing this lifecycle owner.
    _tasks: TaskRuntime,
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
        let tasks = TaskRuntime::start()?;
        let executor = tasks.handle.clone();
        let (sender, receiver) = mpsc::channel(64);
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
        let (revision_updates, revision_rx) = watch::channel(initial.revisions);
        let handle = CoreHandle {
            state: shared.clone(),
            analysis: engine.analysis.clone(),
            analysis_control: Arc::clone(&engine.analysis_control),
            waveform: Arc::clone(&engine.waveform),
            sender,
            executor: executor.clone(),
            revision_updates: revision_rx,
            wakeup: wakeup.clone(),
            subscribers: subscribers.clone(),
            query_timeout: None,
            #[cfg(test)]
            _test_tasks: None,
        };
        let (scan_tx, scan_rx) = mpsc::channel(3);
        let (relocation_tx, relocation_rx) = mpsc::channel(1);
        let core = Core {
            store,
            engine,
            logging,
            executor: executor.clone(),
            revision_updates,
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
            active_external_admission: None,
            active_optimization: None,
            processing: crate::processing::Service::new(data_dir.to_path_buf()),
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
            #[cfg(test)]
            _test_tasks: None,
        };
        let worker_control = control.clone();
        let worker = thread::Builder::new()
            .name("rivu-core".into())
            .spawn(move || {
                let _completion = CompletionGuard(worker_control.clone());
                // Local ownership ensures unwind drops Core before publishing
                // completion, on this thread rather than the shared reactor.
                let mut core = core;
                core.initialize();
                core.run(receiver, shutdown_receiver, &worker_control);
            })?;
        Ok(Self {
            handle,
            control,
            worker: Some(worker),
            _tasks: tasks,
        })
    }
    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(|worker| worker.is_finished())
    }
    pub fn join(&mut self) -> thread::Result<()> {
        self.worker.take().map_or(Ok(()), JoinHandle::join)
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
    other_tracks_check: bool,
}
enum ScanEvent {
    Progress {
        id: u64,
        progress: ScanProgress,
    },
    KnownSources {
        id: u64,
        paths: Vec<PathBuf>,
        reply: tokio::sync::oneshot::Sender<Result<Vec<Option<library::KnownFile>>>>,
    },
    Finished(ScanFinished),
    Saved(catalog::ScanSaved),
}
const MAX_RETAINED_SCAN_RESULTS: usize = 32;
struct ScanTask {
    status: ScanTaskStatus,
    owner: crate::projection::MediaTaskOwner,
    cancellation: library::ScanCancellation,
    worker: tokio::task::JoinHandle<()>,
    pending_save: Option<ScanFinished>,
}
struct RelocationTask {
    status: RelocationTaskStatus,
    prepared: crate::store::PreparedRelocation,
    cancellation: library::ScanCancellation,
    worker: Option<tokio::task::JoinHandle<()>>,
}
impl Drop for RelocationTask {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = futures::executor::block_on(worker);
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
    executor: tokio::runtime::Handle,
    revision_updates: watch::Sender<StateRevisions>,
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
    active_external_admission: Option<admission::ExternalAdmissionTask>,
    active_optimization: Option<maintenance::OptimizationTask>,
    processing: crate::processing::Service,
    processing_results: VecDeque<ProcessingTaskStatus>,
    scan_tx: mpsc::Sender<ScanEvent>,
    scan_rx: mpsc::Receiver<ScanEvent>,
    active_scan: Option<ScanTask>,
    scan_results: VecDeque<ScanTaskStatus>,
    next_scan_id: u64,
    relocation_tx: mpsc::Sender<RelocationFinished>,
    relocation_rx: mpsc::Receiver<RelocationFinished>,
    active_relocation: Option<RelocationTask>,
    relocation_results: VecDeque<RelocationTaskStatus>,
    next_relocation_id: u64,
    played: Vec<u64>,
    // Number of history entries through the current (or just removed) entry.
    played_cursor: usize,
    saved_config: Option<Config>,
    config_save_error: Option<String>,
    #[cfg(test)]
    _test_tasks: Option<TaskRuntime>,
}

impl Drop for Core {
    fn drop(&mut self) {
        // Runs on the dedicated Core owner (or a synchronous test owner),
        // including unwind. Cancellation is intent, never retirement.
        drop(self.active_export.take());
        drop(self.active_external_admission.take());
        if let Some(task) = self.active_scan.take() {
            if task.status.progress.phase != ScanPhase::Saving {
                task.cancellation.cancel();
            }
            self.scan_rx.close();
            while self.scan_rx.try_recv().is_ok() {}
            let _ = futures::executor::block_on(task.worker);
            while self.scan_rx.try_recv().is_ok() {}
        }
        self.relocation_rx.close();
        while self.relocation_rx.try_recv().is_ok() {}
        drop(self.active_relocation.take());
        while self.relocation_rx.try_recv().is_ok() {}
        drop(self.active_optimization.take());
        // AudioEngine::drop sends shutdown and physically joins its worker.
    }
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
mod admission;
mod catalog;
mod command;
mod dispatch;
mod export;
mod maintenance;
mod playback;
mod playlists;
mod processing;
mod queue;
mod relocation;
mod settings;
mod tracks;
mod write_gate;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests;
