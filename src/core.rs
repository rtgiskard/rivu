use crate::{
    analysis::{AnalysisControl, AnalysisFrame, DisplayDemand},
    audio::{self, AudioCommand, AudioEngine, AudioEvent},
    config::{Config, LibraryColumn},
    library::{self, M3uItem, ScanResult},
    logging,
    model::*,
    projection::{GuiSnapshot, MprisSnapshot, TraySnapshot},
    response::{
        Ack, PlaybackSnapshot, PlaylistEntryRevision, QueryError, QueryResponse, QueryRevisions,
        StateResponse, StateRevisions, StateSections, ViewResponse,
    },
    store::Store,
};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use parking_lot::{Condvar, Mutex, RwLock};
use rand::seq::SliceRandom;
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type Wakeup = Arc<dyn Fn() + Send + Sync>;
#[expect(
    clippy::large_enum_variant,
    reason = "Keep one-shot query replies inline instead of allocating a response box"
)]
enum CoreResponse {
    Query(QueryResponse),
    Ack(Ack),
}
enum Operation {
    Query(Query),
    Command(Command),
    GuiConfig(GuiConfigChange),
    Navigate(Navigation),
}

#[derive(Clone, Copy)]
pub(crate) enum Navigation {
    Next,
    Previous,
}

#[expect(
    clippy::large_enum_variant,
    reason = "Inline configuration transfer avoids another allocation in the bounded request channel"
)]
pub(crate) enum GuiConfigChange {
    ToggleStillMode,
    ToggleWaveformSeek,
    ToggleLibraryColumn(LibraryColumn),
    AddLibraryRoot(std::path::PathBuf),
    Settings(Config),
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
    pub(crate) system: SystemState,
    pub(crate) revisions: StateRevisions,
}

impl CoreState {
    pub(crate) fn current_track(&self) -> Option<&Track> {
        self.current_track.as_deref()
    }
    pub(crate) fn query_revisions(&self) -> QueryRevisions {
        QueryRevisions {
            library: self.library.revision,
            structure: self.library.structure_revision,
            playlist: self.library.playlist_revision,
            queue: self.queue.revision,
            playlist_entries: None,
        }
    }

    pub(crate) fn playlist_query_revisions(&self, playlist_id: i64) -> QueryRevisions {
        QueryRevisions {
            playlist_entries: Some(PlaylistEntryRevision {
                playlist_id,
                revision: self
                    .library
                    .playlist_entry_revisions
                    .get(&playlist_id)
                    .copied()
                    .unwrap_or(self.library.playlist_revision),
            }),
            ..self.query_revisions()
        }
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
            playback: self.playback != previous.playback || current_track_changed,
            queue: self.queue != previous.queue,
            library: self.library.revision != previous.library.revision
                || self.library.structure_revision != previous.library.structure_revision
                || self.library.playlist_revision != previous.library.playlist_revision
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
    pub analysis: Arc<RwLock<AnalysisFrame>>,
    analysis_control: Arc<AnalysisControl>,
    pub waveform: Arc<RwLock<audio::WaveformFrame>>,
    sender: Sender<Request>,
    wakeup: Arc<RwLock<Option<Wakeup>>>,
    subscribers: Arc<RwLock<Vec<Sender<()>>>>,
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

    pub(crate) fn update_gui_config(&self, change: GuiConfigChange) -> Result<()> {
        self.sender
            .try_send(Request {
                operation: Operation::GuiConfig(change),
                reply: None,
            })
            .context("Rivu is busy or stopped")
    }

    pub(crate) fn apply_gui_settings(&self, config: Config) -> Ack {
        self.operation_ack(
            Operation::GuiConfig(GuiConfigChange::Settings(config)),
            Duration::from_secs(12),
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
        let (tx, rx) = bounded(1);
        let request = Request {
            operation: Operation::Query(query),
            reply: Some(tx),
        };
        let response = self
            .sender
            .send_timeout(request, Duration::from_secs(2))
            .map_err(|error| format!("Core unavailable: {error}"))
            .and_then(|()| {
                rx.recv_timeout(Duration::from_secs(12))
                    .map_err(|error| format!("Core response unavailable: {error}"))
            });
        match response {
            Ok(CoreResponse::Query(response)) => response,
            Ok(CoreResponse::Ack(_)) => unreachable!("query request returned Ack"),
            Err(error) => QueryResponse {
                revisions: self.state.read().query_revisions(),
                result: Err(QueryError::Message(error)),
            },
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
        let duration = if matches!(command, Command::OptimizeDatabase) {
            Duration::from_secs(120)
        } else {
            Duration::from_secs(12)
        };
        self.operation_ack(Operation::Command(command), duration)
    }

    /// Navigate in one Core dispatch, preserving its status at execution time.
    pub(crate) fn navigate(&self, direction: Navigation) -> Ack {
        self.operation_ack(Operation::Navigate(direction), Duration::from_secs(12))
    }

    fn operation_ack(&self, operation: Operation, duration: Duration) -> Ack {
        let (tx, rx) = bounded(1);
        let request = Request {
            operation,
            reply: Some(tx),
        };
        let response = self
            .sender
            .send_timeout(request, Duration::from_secs(2))
            .map_err(|error| format!("Core unavailable: {error}"))
            .and_then(|()| {
                rx.recv_timeout(duration)
                    .map_err(|error| format!("Core response unavailable: {error}"))
            });
        match response {
            Ok(CoreResponse::Ack(ack)) => ack,
            Ok(CoreResponse::Query(_)) => unreachable!("command request returned query"),
            Err(error) => Ack {
                ok: false,
                error: Some(error),
                revision: self.revision(),
                scan_id: None,
                relocation_id: None,
            },
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

    pub(crate) fn shutdown_ack(&self, revision: impl Fn() -> u64) -> Ack {
        let requested = self.request_shutdown();
        let mut state = self.completion.state.lock();
        let deadline = Instant::now() + Duration::from_secs(12);
        while requested.is_ok() && state.ack.is_none() && !state.finished {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            self.completion.acknowledged.wait_for(&mut state, remaining);
        }
        state.ack.clone().unwrap_or_else(|| Ack {
            ok: false,
            error: Some(
                if state.finished {
                    "Core stopped before shutdown acknowledgement"
                } else {
                    "Core shutdown acknowledgement unavailable"
                }
                .into(),
            ),
            revision: revision(),
            scan_id: None,
            relocation_id: None,
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
    /// Start a standalone core without installing or owning process logging.
    pub fn start(data_dir: &Path, config_path: &Path) -> Result<Self> {
        Self::start_inner(data_dir, config_path, Config::load(config_path)?, None)
    }

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
        config.save(config_path)?;
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
        };
        let worker_control = control.clone();
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
                    queue_ids: Vec::new(),
                    playback: None,
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
                    config_last_attempt: Instant::now(),
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
                let _ = core
                    .engine
                    .commands
                    .send(AudioCommand::StillMode(core.state.system.config.still_mode));
                let _ = core
                    .engine
                    .commands
                    .send(AudioCommand::Volume(core.state.playback.volume));
                let _ = core.engine.commands.send(AudioCommand::FfmpegEnabled(
                    core.state.system.config.ffmpeg_enabled,
                ));
                let _ = core.engine.commands.send(AudioCommand::OutputSettings {
                    device: core.state.system.selected_device.clone(),
                    auto_mix: core.state.system.config.pipewire_auto_mix,
                });
                let _ = core.engine.commands.send(AudioCommand::AnalysisSettings(
                    core.state.system.config.as_ref().into(),
                ));
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
}
const MAX_RETAINED_SCAN_RESULTS: usize = 32;
struct ScanTask {
    status: ScanTaskStatus,
    cancellation: library::ScanCancellation,
    worker: JoinHandle<()>,
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
    result: Result<engine::relocation::VerifiedTarget>,
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
    // The only full queue projection: compact occurrence IDs, never shared with clients.
    queue_ids: Vec<u64>,
    playback: Option<PlaybackStats>,
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
    config_last_attempt: Instant,
    config_save_error: Option<String>,
}
impl Core {
    fn load_initial_state(&mut self) -> Result<()> {
        self.state.library.track_total = self.store.track_count()?;
        self.state.library.playlist_total = self.store.playlist_summary_page(0, 0)?.total;
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
mod engine;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::ScanRecord;
    use std::sync::atomic::Ordering;

    #[test]
    fn runtime_completion_notifies_on_panic_and_reconciles_late_registration() {
        let (control, _shutdown) = RuntimeControl::new();
        let (wake, wakes) = bounded(1);
        control.set_completion_callback(move || {
            let _ = wake.try_send(());
        });
        let worker_control = control.clone();
        assert!(
            thread::spawn(move || {
                let _completion = CompletionGuard(worker_control);
                panic!("core worker failure");
            })
            .join()
            .is_err()
        );
        assert_eq!(wakes.try_recv(), Ok(()));
        let (late, late_wakes) = bounded(1);
        control.set_completion_callback(move || {
            let _ = late.try_send(());
        });
        assert_eq!(late_wakes.try_recv(), Ok(()));
        let ack = control.shutdown_ack(|| 17);
        assert!(!ack.ok);
        assert_eq!(ack.revision, 17);
    }

    #[test]
    fn lifecycle_shutdown_coalesces_without_business_queue_capacity() {
        let (control, shutdown) = RuntimeControl::new();
        for _ in 0..1000 {
            control.request_shutdown().unwrap();
        }
        assert_eq!(shutdown.try_recv(), Ok(()));
        assert_eq!(
            shutdown.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        );
    }

    // An idle real audio worker handles stop/snapshot, but these tests never load
    // audio. Missing-file errors expose the selected entry without opening output.
    fn fixture() -> (tempfile::TempDir, Core) {
        fixture_with_duration(Some(10.0))
    }

    fn fixture_with_duration(duration: Option<f64>) -> (tempfile::TempDir, Core) {
        fixture_with_database(duration, false)
    }

    fn fixture_with_database(duration: Option<f64>, persistent: bool) -> (tempfile::TempDir, Core) {
        let directory = tempfile::tempdir().unwrap();
        let database_path = if persistent {
            directory.path().join("library.db")
        } else {
            Path::new(":memory:").to_path_buf()
        };
        let store = Store::open(&database_path).unwrap();
        store
            .apply_scan(&ScanResult {
                roots: vec![directory.path().to_path_buf()],
                suppressed_sources: Vec::new(),
                skipped_directories: Vec::new(),
                skipped_directories_truncated: false,
                scan_incomplete: false,
                uncertainties: Vec::new(),
                records: (1..=4)
                    .map(|id| ScanRecord {
                        path: directory.path().join(format!("{id}.wav")),
                        cue: None,
                        size: 1,
                        modified_ns: 1,
                        fingerprint: None,
                        media: MediaInfo {
                            title: format!("Track {id}"),
                            artist: String::new(),
                            album: String::new(),
                            duration,
                            codec: "pcm".into(),
                            channels: 2,
                            sample_rate: 48000,
                            bitrate_bps: None,
                            bitrate_exact: false,
                            track_number: None,
                            disc_number: None,
                            bits_per_sample: None,
                            release_date: None,
                        },
                    })
                    .collect(),
                errors: Vec::new(),
            })
            .unwrap();
        let mut state = CoreState::default();
        state.library.track_total = store.track_count().unwrap();
        state.system.config_path = directory.path().join("config.toml");
        Arc::make_mut(&mut state.system.config)
            .library_roots
            .push(directory.path().to_path_buf());
        let (scan_tx, scan_rx) = bounded(1);
        let (relocation_tx, relocation_rx) = bounded(1);
        let mut core = Core {
            store,
            engine: AudioEngine::new(1).unwrap(),
            logging: None,
            shared: Arc::new(RwLock::new(state.clone())),
            state,
            wakeup: Arc::new(RwLock::new(None)),
            subscribers: Arc::new(RwLock::new(Vec::new())),
            generation: 1,
            queue_ids: Vec::new(),
            playback: None,
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
            config_last_attempt: Instant::now(),
            config_save_error: None,
        };
        core.enqueue(&[1, 2, 2, 3, 4]).unwrap();
        (directory, core)
    }

    fn relocation_fixture() -> (tempfile::TempDir, Core, i64, i64) {
        let (directory, mut core) = fixture();
        let paths =
            ["old.wav", "target.wav", "other-copy.wav"].map(|name| directory.path().join(name));
        let media = core
            .store
            .stored_scan_files(&[directory.path().to_path_buf()])
            .unwrap()[0]
            .media
            .clone()
            .unwrap();
        let records = paths
            .iter()
            .map(|path| {
                std::fs::write(path, b"same complete content").unwrap();
                let metadata = std::fs::metadata(path).unwrap();
                ScanRecord {
                    path: path.clone(),
                    cue: None,
                    size: metadata.len(),
                    modified_ns: i64::try_from(
                        metadata
                            .modified()
                            .unwrap()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_nanos(),
                    )
                    .unwrap(),
                    fingerprint: Some(blake3::hash(b"same complete content").to_hex().to_string()),
                    media: media.clone(),
                }
            })
            .collect();
        core.store
            .apply_scan(&ScanResult {
                records,
                ..ScanResult::default()
            })
            .unwrap();
        let known = core
            .store
            .stored_scan_files(&[directory.path().to_path_buf()])
            .unwrap();
        let source = known
            .iter()
            .find(|file| file.path == paths[0])
            .unwrap()
            .track_id;
        let target = known
            .iter()
            .find(|file| file.path == paths[1])
            .unwrap()
            .track_id;
        std::fs::remove_file(&paths[0]).unwrap();
        core.store
            .apply_scan(&ScanResult {
                roots: vec![paths[0].clone()],
                ..ScanResult::default()
            })
            .unwrap();
        core.reload(true).unwrap();
        assert!(core.track(source).unwrap().missing);
        (directory, core, source, target)
    }

    fn settle_relocation(core: &mut Core) -> RelocationTaskStatus {
        let event = core
            .relocation_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        core.relocation_event(event);
        assert!(core.active_relocation.is_none());
        core.state.system.relocation_task.clone().unwrap()
    }

    #[test]
    fn manual_relocation_preserves_source_identity_state_and_references() {
        let (_directory, mut core, source, target) = relocation_fixture();
        let target_path = core.track(target).unwrap().path;
        core.command(Command::SetFavorite {
            track_ids: vec![source],
            favorite: true,
        })
        .unwrap();
        core.command(Command::EditTrack {
            track_id: source,
            title: Some("User title".into()),
            artist: None,
            album: None,
            decoder_hint: Some(DecoderHint::Native),
        })
        .unwrap();
        core.store.mark_played(source, 123).unwrap();
        core.store.increment_play_count(source).unwrap();
        core.enqueue(&[source]).unwrap();
        core.command(Command::CreatePlaylistWithTracks {
            name: "Preserved".into(),
            track_ids: vec![source],
        })
        .unwrap();
        let queue_before = core.queue_ids.clone();
        let revisions = core.state.query_revisions();
        core.command(Command::RelocateTrack {
            source_id: source,
            target_id: target,
        })
        .unwrap();
        let id = core.active_relocation.as_ref().unwrap().status.id;
        assert_ne!(core.track(source).unwrap().path, target_path);
        let status = settle_relocation(&mut core);
        assert_eq!(status.phase, RelocationPhase::Completed);
        let retained = core.track(source).unwrap();
        assert_eq!(retained.path, target_path);
        assert_eq!(retained.title, "User title");
        assert_eq!(retained.decoder_hint, DecoderHint::Native);
        assert!(retained.favorite && !retained.missing);
        assert_eq!(retained.play_count, 1);
        assert_eq!(retained.last_played, Some(123));
        assert!(core.store.track(target).unwrap().is_none());
        assert_eq!(core.queue_ids, queue_before);
        assert!(core.store.queued_track(source).unwrap().is_some());
        assert!(core.state.query_revisions().library > revisions.library);
        assert!(core.state.query_revisions().structure > revisions.structure);
        assert!(
            matches!(core.view(&Query::RelocationTask { id }), Ok(ViewResponse::RelocationTask(task)) if task == status)
        );
    }

    #[test]
    fn relocation_cancel_and_shutdown_never_commit_completed_worker_output() {
        for shutdown in [false, true] {
            let (_directory, mut core, source, target) = relocation_fixture();
            core.relocate_track(source, target).unwrap();
            let id = core.active_relocation.as_ref().unwrap().status.id;
            if shutdown {
                core.shutdown_relocation();
            } else {
                core.cancel_relocation(id).unwrap();
                settle_relocation(&mut core);
            }
            assert_eq!(
                core.state.system.relocation_task.as_ref().unwrap().phase,
                RelocationPhase::Cancelled
            );
            assert!(core.track(source).unwrap().missing);
            assert!(core.store.track(target).unwrap().is_some());
            // A retained completed cancellation is harmless and never retries.
            core.cancel_relocation(id).unwrap();
            if let Ok(event) = core.relocation_rx.try_recv() {
                core.relocation_event(event);
            }
            assert!(core.track(source).unwrap().missing);
        }
    }

    #[test]
    fn relocation_rechecks_target_conflicts_and_source_presence_at_commit() {
        for source_returns in [false, true] {
            let (_directory, mut core, source, target) = relocation_fixture();
            core.relocate_track(source, target).unwrap();
            // Fence hashing completion without allowing Core's commit dispatch.
            let event = core
                .relocation_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            if source_returns {
                std::fs::write(core.track(source).unwrap().path, b"returned").unwrap();
            } else {
                core.command(Command::SetFavorite {
                    track_ids: vec![target],
                    favorite: true,
                })
                .unwrap();
            }
            core.relocation_event(event);
            let status = core.state.system.relocation_task.as_ref().unwrap();
            assert_eq!(status.phase, RelocationPhase::Failed);
            assert!(status.error.is_some());
            assert!(core.track(source).unwrap().missing);
            assert!(core.store.track(target).unwrap().is_some());
        }
    }

    #[test]
    fn relocation_rejects_target_file_drift_after_background_hash() {
        let (_directory, mut core, source, target) = relocation_fixture();
        core.relocate_track(source, target).unwrap();
        let event = core
            .relocation_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        std::fs::write(core.track(target).unwrap().path, b"new bytes").unwrap();
        core.relocation_event(event);
        assert_eq!(
            core.state.system.relocation_task.as_ref().unwrap().phase,
            RelocationPhase::Failed
        );
        assert!(core.track(source).unwrap().missing);
    }

    #[test]
    fn relocation_rejects_active_affected_rows_at_start_and_during_hash() {
        for during_hash in [false, true] {
            for playing_target in [false, true] {
                let (_directory, mut core, source, target) = relocation_fixture();
                if during_hash {
                    core.relocate_track(source, target).unwrap();
                }
                core.enqueue(&[if playing_target { target } else { source }])
                    .unwrap();
                let queue_id = *core.queue_ids.last().unwrap();
                pending(&mut core, queue_id);
                if during_hash {
                    assert_eq!(settle_relocation(&mut core).phase, RelocationPhase::Failed);
                } else {
                    assert!(core.relocate_track(source, target).is_err());
                    assert!(core.active_relocation.is_none());
                }
                assert!(core.track(source).unwrap().missing);
                assert!(core.store.track(target).unwrap().is_some());
            }
        }
    }

    #[test]
    fn relocation_rejects_source_marked_independent_during_hash() {
        let (_directory, mut core, source, target) = relocation_fixture();
        core.relocate_track(source, target).unwrap();
        let event = core
            .relocation_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        core.command(Command::KeepTrackIndependent { track_id: source })
            .unwrap();
        core.relocation_event(event);
        assert_eq!(
            core.state.system.relocation_task.as_ref().unwrap().phase,
            RelocationPhase::Failed
        );
        assert!(core.track(source).unwrap().missing);
        assert!(core.store.track(target).unwrap().is_some());
    }

    #[test]
    fn relocation_rejects_database_identity_drift_after_fresh_verification() {
        for change_source in [false, true] {
            let (directory, mut core, source, target) = relocation_fixture();
            core.relocate_track(source, target).unwrap();
            let event = core
                .relocation_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            let changed_id = if change_source { source } else { target };
            let known = core
                .store
                .stored_scan_files(&[directory.path().to_path_buf()])
                .unwrap()
                .into_iter()
                .find(|file| file.track_id == changed_id)
                .unwrap();
            let changed_path = known.path.clone();
            core.store
                .apply_scan(&ScanResult {
                    records: vec![ScanRecord {
                        path: known.path,
                        cue: None,
                        size: known.size,
                        modified_ns: known.modified_ns,
                        fingerprint: Some(
                            blake3::hash(b"different historical identity")
                                .to_hex()
                                .to_string(),
                        ),
                        media: known.media.unwrap(),
                    }],
                    ..ScanResult::default()
                })
                .unwrap();
            if change_source {
                core.store
                    .apply_scan(&ScanResult {
                        roots: vec![changed_path],
                        ..ScanResult::default()
                    })
                    .unwrap();
            }
            core.relocation_event(event);
            assert_eq!(
                core.state.system.relocation_task.as_ref().unwrap().phase,
                RelocationPhase::Failed
            );
            assert!(core.track(source).unwrap().missing);
            assert!(core.store.track(target).unwrap().is_some());
        }
    }

    // Capture the commands Core really sends without opening a platform device.
    // The existing idle worker remains alive for its event/analysis ownership.
    fn recording_audio(core: &mut Core) -> Receiver<AudioCommand> {
        let (commands, receive) = bounded(64);
        let (record, recorded) = crossbeam_channel::unbounded();
        let idle_commands = std::mem::replace(&mut core.engine.commands, commands);
        thread::spawn(move || {
            while let Ok(command) = receive.recv() {
                let snapshot = match &command {
                    AudioCommand::StopAndSnapshot(reply) => Some(reply.clone()),
                    _ => None,
                };
                let _ = record.send(command);
                if let Some(reply) = snapshot {
                    let _ = reply.send((0, 0.0));
                }
            }
            drop(idle_commands);
        });
        recorded
    }

    fn navigation_fixture(status: PlaybackStatus) -> (tempfile::TempDir, Core) {
        let (directory, mut core) = fixture();
        for id in 1..=4 {
            std::fs::write(directory.path().join(format!("{id}.wav")), []).unwrap();
        }
        let id = core.queue_ids[1];
        pending(&mut core, id);
        core.state.playback.status = status;
        if status == PlaybackStatus::Stopped {
            core.playback = None;
        }
        (directory, core)
    }

    fn navigation_runtime(
        mut core: Core,
    ) -> (
        CoreHandle,
        RuntimeControl,
        JoinHandle<Core>,
        Arc<Mutex<Vec<MprisSnapshot>>>,
    ) {
        core.publish();
        let (sender, requests) = bounded(64);
        let handle = CoreHandle {
            state: core.shared.clone(),
            analysis: core.engine.analysis.clone(),
            analysis_control: core.engine.analysis_control.clone(),
            waveform: core.engine.waveform.clone(),
            sender,
            wakeup: core.wakeup.clone(),
            subscribers: core.subscribers.clone(),
        };
        let publications = Arc::new(Mutex::new(Vec::new()));
        let observed = publications.clone();
        let state = core.shared.clone();
        handle.set_wakeup(move || {
            observed
                .lock()
                .push(MprisSnapshot::from_core(&state.read()));
        });
        let (control, shutdown) = RuntimeControl::new();
        let worker_control = control.clone();
        let worker = thread::spawn(move || {
            core.run(requests, shutdown, &worker_control);
            core
        });
        (handle, control, worker, publications)
    }

    fn assert_navigation_audio(
        commands: &Receiver<AudioCommand>,
        status: PlaybackStatus,
        path: &Path,
    ) {
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioCommand::StopAndSnapshot(_)
        ));
        if status != PlaybackStatus::Stopped {
            match commands.recv_timeout(Duration::from_secs(2)).unwrap() {
                AudioCommand::Load {
                    path: loaded,
                    paused,
                    start_seconds,
                    ..
                } => {
                    assert_eq!(loaded, path);
                    assert_eq!(paused, status == PlaybackStatus::Paused);
                    assert_eq!(start_seconds, 0.0);
                }
                _ => panic!("navigation must load exactly once with the requested status"),
            }
        }
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn navigation_endpoint_preserves_execution_status_and_occurrence_selection() {
        for status in [
            PlaybackStatus::Paused,
            PlaybackStatus::Stopped,
            PlaybackStatus::Playing,
        ] {
            for direction in [Navigation::Next, Navigation::Previous] {
                let (directory, mut core) = navigation_fixture(status);
                let expected = core.queue_ids[match direction {
                    Navigation::Next => 2,
                    Navigation::Previous => 0,
                }];
                let path = core
                    .track(core.store.queue_track_id(expected).unwrap())
                    .unwrap()
                    .path;
                let commands = recording_audio(&mut core);
                let (handle, control, worker, publications) = navigation_runtime(core);
                let ack = handle.navigate(direction);
                assert!(ack.ok, "{:?}", ack.error);
                let snapshot = handle.mpris_snapshot();
                assert_eq!(snapshot.playback.status, status);
                assert_eq!(snapshot.queue.current_id, Some(expected));
                assert_eq!(snapshot.playback.position, 0.0);
                assert_eq!(snapshot.current_track().unwrap().path, path);
                assert_eq!(snapshot.current_track().unwrap().play_count, 0);
                assert!(snapshot.current_track().unwrap().last_played.is_none());
                assert!(!publications.lock().is_empty());
                assert!(
                    publications
                        .lock()
                        .iter()
                        .all(|state| state.playback.status == status)
                );
                assert_navigation_audio(&commands, status, &path);
                control.request_shutdown().unwrap();
                let core = worker.join().unwrap();
                assert_eq!(
                    core.store
                        .queue_projection(core.queue_ids.len())
                        .unwrap()
                        .current_id,
                    Some(expected)
                );
                assert!(core.store.history(MAX_QUERY_ROWS).unwrap().is_empty());
                drop(directory);
            }
        }
    }

    #[test]
    fn pause_accepted_before_queued_eof_keeps_next_and_repeat_one_paused() {
        for repeat in [RepeatMode::Off, RepeatMode::One] {
            let (_directory, mut core) = navigation_fixture(PlaybackStatus::Playing);
            core.state.playback.repeat = repeat;
            let expected = core.queue_ids[if repeat == RepeatMode::One { 1 } else { 2 }];
            let generation = core.generation;
            let commands = recording_audio(&mut core);
            core.command(Command::Pause).unwrap();
            core.audio_event(AudioEvent::Ended {
                generation,
                bitrate_bps: None,
            })
            .unwrap();
            assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
            assert_eq!(core.state.queue.current_id, Some(expected));
            // Core sends Load synchronously, but the recorder forwards it on
            // another thread. Its snapshot reply follows recording every earlier
            // FIFO command, so draining cannot race the final paused Load.
            let (snapshot, recorded_through_snapshot) = bounded(1);
            core.audio(AudioCommand::StopAndSnapshot(snapshot)).unwrap();
            recorded_through_snapshot
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            let loads: Vec<_> = commands
                .try_iter()
                .filter_map(|command| match command {
                    AudioCommand::Load { paused, .. } => Some(paused),
                    _ => None,
                })
                .collect();
            assert_eq!(loads, [true]);
            core.audio_event(AudioEvent::Ended {
                generation,
                bitrate_bps: None,
            })
            .unwrap();
            assert_eq!(core.state.queue.current_id, Some(expected));
            assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
        }
    }

    #[test]
    fn identical_shuffle_setter_preserves_previous_occurrence_history() {
        let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
        core.state.playback.shuffle = true;
        let previous = core.queue_ids[3];
        let current = core.state.queue.current_id.unwrap();
        core.played = vec![previous, current];
        core.played_cursor = 2;
        let _commands = recording_audio(&mut core);
        core.command(Command::Shuffle { enabled: true }).unwrap();
        core.navigate(Navigation::Previous).unwrap();
        assert_eq!(core.state.queue.current_id, Some(previous));
        assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
        assert_eq!(core.played, [previous, current]);
        assert_eq!(core.played_cursor, 1);
    }

    #[test]
    fn seek_uses_explicit_cue_end_not_estimated_whole_track_duration() {
        let (_directory, mut core) = navigation_fixture(PlaybackStatus::Paused);
        let _commands = recording_audio(&mut core);
        core.seek(25.0).unwrap();
        assert_eq!(core.state.playback.position, 25.0);
        Arc::make_mut(core.state.current_track.as_mut().unwrap()).cue = Some(CueSegment {
            sheet: "album.cue".into(),
            number: 1,
            start_frame: 75,
            end_frame: Some(300),
        });
        core.seek(25.0).unwrap();
        assert_eq!(core.state.playback.position, 3.0);
    }

    #[test]
    fn queued_pause_is_authoritative_for_navigation_not_the_callers_snapshot() {
        let (_directory, mut core) = navigation_fixture(PlaybackStatus::Playing);
        let expected = core.queue_ids[2];
        let commands = recording_audio(&mut core);
        let (handle, control, worker, publications) = navigation_runtime(core);
        assert_eq!(
            handle.mpris_snapshot().playback.status,
            PlaybackStatus::Playing
        );
        handle.send(Command::Pause).unwrap();
        assert!(handle.navigate(Navigation::Next).ok);
        assert_eq!(
            handle.mpris_snapshot().playback.status,
            PlaybackStatus::Paused
        );
        assert_eq!(handle.mpris_snapshot().queue.current_id, Some(expected));
        assert!(
            publications
                .lock()
                .iter()
                .all(|state| state.playback.status == PlaybackStatus::Paused)
        );
        assert!(matches!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioCommand::Pause(true)
        ));
        let path = handle
            .mpris_snapshot()
            .current_track()
            .unwrap()
            .path
            .clone();
        assert_navigation_audio(&commands, PlaybackStatus::Paused, &path);
        control.request_shutdown().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn previous_resets_position_without_loading_or_changing_status() {
        for status in [PlaybackStatus::Paused, PlaybackStatus::Playing] {
            let (_directory, mut core) = navigation_fixture(status);
            let current = core.state.queue.current_id;
            core.state.playback.position = 4.0;
            let seek_revision = core.state.playback.seek_revision;
            let commands = recording_audio(&mut core);
            let (handle, control, worker, publications) = navigation_runtime(core);
            assert!(handle.navigate(Navigation::Previous).ok);
            let snapshot = handle.mpris_snapshot();
            assert_eq!(snapshot.playback.status, status);
            assert_eq!(snapshot.queue.current_id, current);
            assert_eq!(snapshot.playback.position, 0.0);
            assert_eq!(
                snapshot.playback.seek_revision,
                seek_revision.wrapping_add(1)
            );
            assert!(
                publications
                    .lock()
                    .iter()
                    .all(|state| state.playback.status == status)
            );
            assert!(matches!(
                commands.recv_timeout(Duration::from_secs(2)).unwrap(),
                AudioCommand::Seek(0.0)
            ));
            assert!(commands.try_recv().is_err());
            control.request_shutdown().unwrap();
            worker.join().unwrap();
        }
    }

    #[test]
    fn shuffle_previous_preserves_status_and_follows_occurrence_history() {
        for status in [PlaybackStatus::Paused, PlaybackStatus::Stopped] {
            let (_directory, mut core) = navigation_fixture(status);
            let ids = core.queue_ids.clone();
            pending(&mut core, ids[3]);
            core.state.playback.status = status;
            if status == PlaybackStatus::Stopped {
                core.playback = None;
            }
            core.state.playback.shuffle = true;
            core.played = vec![ids[0], ids[2], ids[3]];
            core.played_cursor = 3;
            let commands = recording_audio(&mut core);
            let (handle, control, worker, publications) = navigation_runtime(core);
            for expected in [ids[2], ids[0]] {
                assert!(handle.navigate(Navigation::Previous).ok);
                let snapshot = handle.mpris_snapshot();
                assert_eq!(snapshot.queue.current_id, Some(expected));
                assert_eq!(snapshot.playback.status, status);
                assert_navigation_audio(&commands, status, &snapshot.current_track().unwrap().path);
            }
            assert!(
                publications
                    .lock()
                    .iter()
                    .all(|state| state.playback.status == status)
            );
            control.request_shutdown().unwrap();
            let core = worker.join().unwrap();
            assert_eq!(core.played, [ids[0], ids[2], ids[3]]);
            assert_eq!(core.played_cursor, 1);
        }
    }

    #[test]
    fn existing_wire_navigation_still_starts_paused_and_stopped_tracks() {
        for status in [PlaybackStatus::Paused, PlaybackStatus::Stopped] {
            for command in [Command::Next, Command::Previous] {
                let (_directory, mut core) = navigation_fixture(status);
                let commands = recording_audio(&mut core);
                let (handle, control, worker, _) = navigation_runtime(core);
                assert!(handle.request_ack(command).ok);
                let snapshot = handle.mpris_snapshot();
                assert_eq!(snapshot.playback.status, PlaybackStatus::Playing);
                assert_navigation_audio(
                    &commands,
                    PlaybackStatus::Playing,
                    &snapshot.current_track().unwrap().path,
                );
                control.request_shutdown().unwrap();
                worker.join().unwrap();
            }
        }
    }

    #[test]
    fn preserving_navigation_reuses_repeat_and_single_occurrence_shuffle_selection() {
        for status in [PlaybackStatus::Paused, PlaybackStatus::Stopped] {
            for (shuffle, repeat) in [
                (false, RepeatMode::All),
                (false, RepeatMode::One),
                (true, RepeatMode::Off),
            ] {
                let (_directory, mut core) = navigation_fixture(status);
                let expected = if shuffle {
                    core.store.clear_queue().unwrap();
                    core.reload_queue().unwrap();
                    core.enqueue(&[1]).unwrap();
                    core.queue_ids[0]
                } else if repeat == RepeatMode::All {
                    core.queue_ids[0]
                } else {
                    // Manual Next ignores Repeat One, just like the wire command.
                    core.queue_ids[2]
                };
                let current = if shuffle {
                    expected
                } else if repeat == RepeatMode::All {
                    *core.queue_ids.last().unwrap()
                } else {
                    core.queue_ids[1]
                };
                pending(&mut core, current);
                core.state.playback.status = status;
                if status == PlaybackStatus::Stopped {
                    core.playback = None;
                }
                core.state.playback.shuffle = shuffle;
                core.state.playback.repeat = repeat;
                let commands = recording_audio(&mut core);
                let (handle, control, worker, publications) = navigation_runtime(core);
                assert!(handle.navigate(Navigation::Next).ok);
                let snapshot = handle.mpris_snapshot();
                assert_eq!(snapshot.queue.current_id, Some(expected));
                assert_eq!(snapshot.playback.status, status);
                assert!(
                    publications
                        .lock()
                        .iter()
                        .all(|state| state.playback.status == status)
                );
                assert_navigation_audio(&commands, status, &snapshot.current_track().unwrap().path);
                control.request_shutdown().unwrap();
                worker.join().unwrap();
            }
        }
    }

    #[test]
    fn stopped_shuffle_previous_without_history_at_first_occurrence_is_noop() {
        let (_directory, mut core) = navigation_fixture(PlaybackStatus::Stopped);
        let current = core.queue_ids[0];
        pending(&mut core, current);
        core.playback = None;
        core.state.playback.status = PlaybackStatus::Stopped;
        core.state.playback.shuffle = true;
        let commands = recording_audio(&mut core);
        let (handle, control, worker, _) = navigation_runtime(core);
        assert!(handle.navigate(Navigation::Previous).ok);
        assert_eq!(handle.mpris_snapshot().queue.current_id, Some(current));
        assert_eq!(
            handle.mpris_snapshot().playback.status,
            PlaybackStatus::Stopped
        );
        assert!(commands.try_recv().is_err());
        control.request_shutdown().unwrap();
        worker.join().unwrap();
    }

    fn run_until_disconnected(core: &mut Core, requests: Receiver<Request>) {
        let (control, shutdown) = RuntimeControl::new();
        core.run(requests, shutdown, &control);
    }

    fn display_fixture() -> (
        CoreHandle,
        crate::analysis::AnalysisWorker,
        Receiver<Request>,
    ) {
        let analysis = Arc::new(RwLock::new(AnalysisFrame::default()));
        let worker = crate::analysis::AnalysisWorker::new(analysis.clone()).unwrap();
        worker.configure(crate::analysis::AnalysisSettings {
            fps: 60,
            fft_size: 512,
            window: crate::config::AnalysisWindow::None,
        });
        worker.set_playback_active(true);
        let (sender, receiver) = bounded(1);
        let handle = CoreHandle {
            state: Arc::new(RwLock::new(CoreState::default())),
            analysis,
            analysis_control: worker.control.clone(),
            waveform: Arc::new(RwLock::new(audio::WaveformFrame::default())),
            sender,
            wakeup: Arc::new(RwLock::new(None)),
            subscribers: Arc::new(RwLock::new(Vec::new())),
        };
        (handle, worker, receiver)
    }

    fn display_wait(ready: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ready() {
            assert!(Instant::now() < deadline, "display transition timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn display_samples(
        producer: &mut ringbuf::HeapProd<crate::analysis::TapFrame>,
        worker: &crate::analysis::AnalysisWorker,
        demand_token: u64,
        count: usize,
        samples: [f32; 2],
    ) {
        use ringbuf::traits::Producer;
        for _ in 0..count {
            assert!(
                producer
                    .try_push(crate::analysis::TapFrame {
                        samples,
                        epoch: worker.control.epoch.load(Ordering::Acquire),
                        demand_token,
                        gap: false,
                    })
                    .is_ok()
            );
        }
    }

    #[test]
    fn display_off_clears_results_with_a_saturated_business_queue_and_reopens() {
        let (handle, worker, _receiver) = display_fixture();
        let demand = DisplayDemand {
            enabled: true,
            visualizer: crate::analysis::VisualizerDemand {
                stereo_density: true,
                ..Default::default()
            },
        };
        handle.set_display_demand(demand);
        let mut producer = worker.producer();
        display_samples(
            &mut producer,
            &worker,
            worker.control.display_token(),
            512,
            [0.5; 2],
        );
        handle.request_analysis_frame();
        display_wait(|| handle.analysis.read().settings.is_some());
        handle.send(Command::DismissError).unwrap();
        // This is the pre-fix close path: its off request is rejected at capacity.
        assert!(handle.send(Command::Analysis { enabled: false }).is_err());
        handle.set_display_demand(DisplayDemand::default());
        display_wait(|| handle.analysis.read().settings.is_none());
        {
            let frame = handle.analysis.read();
            assert!(frame.spectrum_db.is_empty());
            assert_eq!(frame.visualizer.density.capacity(), 0);
            assert_eq!(frame.visualizer.demand, Default::default());
        }
        handle.set_display_demand(demand);
        display_samples(
            &mut producer,
            &worker,
            worker.control.display_token(),
            512,
            [0.0; 2],
        );
        handle.request_analysis_frame();
        display_wait(|| handle.analysis.read().settings.is_some());
        let frame = handle.analysis.read();
        assert!(handle.analysis_frame_is_current(&frame));
        assert_eq!(frame.visualizer.demand, demand.visualizer);
        assert_eq!([frame.rms_left, frame.rms_right], [0.0; 2]);
    }

    #[test]
    fn display_quick_off_on_rejects_in_flight_and_queued_old_spectrum() {
        use ringbuf::traits::Observer;
        let (handle, worker, _receiver) = display_fixture();
        let demand = DisplayDemand {
            enabled: true,
            ..Default::default()
        };
        handle.set_display_demand(demand);
        let old_token = worker.control.display_token();
        let mut producer = worker.producer();
        display_samples(&mut producer, &worker, old_token, 512, [0.25; 2]);
        handle.request_analysis_frame();
        display_wait(|| handle.analysis.read().settings.is_some());
        let held_frame = handle.analysis.write();
        let sequence = held_frame.sequence;
        display_samples(&mut producer, &worker, old_token, 512, [1.0; 2]);
        handle.request_analysis_frame();
        display_wait(|| producer.occupied_len() == 0);
        handle.set_display_demand(DisplayDemand::default());
        handle.set_display_demand(demand);
        assert!(!handle.analysis_frame_is_current(&held_frame));
        drop(held_frame);
        display_wait(|| handle.analysis.read().sequence > sequence);
        assert_eq!(handle.analysis.read().settings, None);
        display_samples(&mut producer, &worker, old_token, 512, [1.0; 2]);
        display_samples(
            &mut producer,
            &worker,
            worker.control.display_token(),
            511,
            [0.0; 2],
        );
        handle.request_analysis_frame();
        display_wait(|| producer.occupied_len() == 0);
        assert_eq!(handle.analysis.read().settings, None);
        display_samples(
            &mut producer,
            &worker,
            worker.control.display_token(),
            1,
            [0.0; 2],
        );
        handle.request_analysis_frame();
        display_wait(|| handle.analysis.read().settings.is_some());
        let frame = handle.analysis.read();
        assert!(handle.analysis_frame_is_current(&frame));
        assert_eq!([frame.rms_left, frame.rms_right], [0.0; 2]);
    }

    #[test]
    fn metadata_intents_merge_and_pages_invalidate_only_for_queued_changes() {
        for title_first in [true, false] {
            let (_directory, mut core) = fixture();
            core.store.set_queue_current(Some(1)).unwrap();
            core.refresh_queue_tracks().unwrap();
            let title = Command::EditTrack {
                track_id: 1,
                title: Some("Title".into()),
                artist: None,
                album: None,
                decoder_hint: Some(crate::model::DecoderHint::Ffmpeg),
            };
            let artist = Command::EditTrack {
                track_id: 1,
                title: None,
                artist: Some("Artist".into()),
                album: None,
                decoder_hint: None,
            };
            let revision = core.state.queue.revision;
            for command in if title_first {
                [title, artist]
            } else {
                [artist, title]
            } {
                core.command(command).unwrap();
            }
            assert_ne!(core.state.queue.revision, revision);
            let current = core.state.current_track().unwrap();
            assert_eq!((&*current.title, &*current.artist), ("Title", "Artist"));
            assert_eq!(current.decoder_hint, crate::model::DecoderHint::Ffmpeg);
            let page = core.store.queue_page(0, MAX_QUERY_ROWS).unwrap();
            assert_eq!(
                (&*page.rows[0].track.title, &*page.rows[0].track.artist),
                ("Title", "Artist")
            );
            core.remove_queue_entries(&[5]).unwrap();
            core.publish();
            let before = core.state.revisions;
            let queue = core.state.queue.clone();
            core.command(Command::EditTrack {
                track_id: 4,
                title: Some("Unqueued".into()),
                artist: None,
                album: None,
                decoder_hint: None,
            })
            .unwrap();
            core.publish();
            assert_eq!(core.state.queue, queue);
            assert_eq!(
                core.state.revisions.changed_since(before),
                StateSections {
                    library: true,
                    ..StateSections::default()
                }
            );
            core.command(Command::SetFavorite {
                track_ids: vec![2, 2, 3],
                favorite: true,
            })
            .unwrap();
            let rows = core.store.queue_page(0, MAX_QUERY_ROWS).unwrap().rows;
            assert!(rows[1].track.favorite && rows[2].track.favorite && rows[3].track.favorite);
            assert!(!rows[0].track.favorite);
        }
    }

    #[test]
    fn unchanged_metadata_favorite_and_playlist_intents_preserve_revisions() {
        let (_directory, mut core) = fixture();
        core.store.set_queue_current(Some(1)).unwrap();
        core.refresh_queue_tracks().unwrap();
        core.command(Command::CreatePlaylist { name: "Mix".into() })
            .unwrap();
        // Creating an override is persistent intent even when its display value matches.
        let before = core.state.query_revisions();
        core.command(Command::EditTrack {
            track_id: 1,
            title: Some("Track 1".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .unwrap();
        assert_ne!(core.state.library.revision, before.library);
        assert_eq!(core.state.queue.revision, before.queue);
        core.publish();
        let revisions = core.state.query_revisions();
        let sections = core.state.revisions;
        let current = Arc::clone(core.state.current_track.as_ref().unwrap());
        for command in [
            Command::EditTrack {
                track_id: 1,
                title: None,
                artist: None,
                album: None,
                decoder_hint: None,
            },
            Command::EditTrack {
                track_id: 1,
                title: Some("Track 1".into()),
                artist: None,
                album: None,
                decoder_hint: Some(DecoderHint::Auto),
            },
            Command::SetFavorite {
                track_ids: vec![1, 1, 2],
                favorite: false,
            },
            Command::SetFavorite {
                track_ids: vec![],
                favorite: true,
            },
            Command::RenamePlaylist {
                playlist_id: 1,
                name: "Mix".into(),
            },
        ] {
            core.command(command).unwrap();
            core.publish();
            assert_eq!(core.state.query_revisions(), revisions);
            assert_eq!(core.state.revisions, sections);
            assert!(Arc::ptr_eq(
                core.state.current_track.as_ref().unwrap(),
                &current
            ));
        }
        for command in [
            Command::EditTrack {
                track_id: 999,
                title: None,
                artist: None,
                album: None,
                decoder_hint: None,
            },
            Command::SetFavorite {
                track_ids: vec![1, 999],
                favorite: true,
            },
            Command::RenamePlaylist {
                playlist_id: 999,
                name: "Mix".into(),
            },
        ] {
            assert!(core.command(command).is_err());
            assert_eq!(core.state.query_revisions(), revisions);
            assert!(!core.track(1).unwrap().favorite);
        }
    }

    #[test]
    fn decoder_hint_intent_refreshes_current_without_invalidating_queue_or_restarting_audio() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 3);
        core.publish();
        let revisions = core.state.query_revisions();
        let sections = core.state.revisions;
        let playback = core.state.playback.clone();
        let generation = core.generation;
        for hint in [DecoderHint::Ffmpeg, DecoderHint::Native, DecoderHint::Auto] {
            core.command(Command::EditTrack {
                track_id: 2,
                title: None,
                artist: None,
                album: None,
                decoder_hint: Some(hint),
            })
            .unwrap();
            core.publish();
            assert_eq!(core.state.current_track().unwrap().decoder_hint, hint);
            assert_eq!(core.track(2).unwrap().decoder_hint, hint);
            assert_eq!(core.state.queue.current_id, Some(3));
            assert_eq!(core.state.queue.revision, revisions.queue);
            assert_eq!(core.state.playback, playback);
            assert_eq!(core.generation, generation);
            assert!(core.playback.is_some());
            let query = Query::QueueBatch {
                after: None,
                limit: 5,
                expected: Some(revisions),
            };
            assert!(matches!(core.view(&query), Ok(ViewResponse::QueueBatch(_))));
            let before_repeat = core.state.revisions;
            core.command(Command::EditTrack {
                track_id: 2,
                title: None,
                artist: None,
                album: None,
                decoder_hint: Some(hint),
            })
            .unwrap();
            core.publish();
            assert_eq!(core.state.revisions, before_repeat);
        }
        assert_ne!(core.state.library.revision, revisions.library);
        assert_ne!(core.state.revisions.playback, sections.playback);
        assert_eq!(core.state.revisions.queue, sections.queue);
    }

    #[test]
    fn activity_updates_playback_but_only_display_statistics_invalidate_queue_pages() {
        let (_directory, mut core) = fixture();
        core.store.set_queue_current(Some(1)).unwrap();
        core.refresh_queue_tracks().unwrap();
        let revision = core.state.queue.revision;
        let stats = core.store.mark_played(1, 7).unwrap();
        core.update_track_stats(1, stats).unwrap();
        assert_eq!(core.state.current_track().unwrap().last_played, Some(7));
        assert_eq!(core.state.queue.revision, revision);
        let stats = core.store.increment_play_count(1).unwrap();
        core.update_track_stats(1, stats).unwrap();
        assert_ne!(core.state.queue.revision, revision);
        assert_eq!(
            core.store.queue_page(0, 1).unwrap().rows[0]
                .track
                .play_count,
            1
        );
    }

    #[test]
    fn startup_and_catalog_refresh_retain_the_same_recent_history() {
        let (directory, mut core) = fixture();
        let media = core
            .store
            .stored_scan_files(&[directory.path().to_path_buf()])
            .unwrap()[0]
            .media
            .clone()
            .unwrap();
        let result = ScanResult {
            roots: vec![directory.path().to_path_buf()],
            records: (1..=MAX_QUERY_ROWS + 1)
                .map(|id| ScanRecord {
                    path: directory.path().join(format!("history-{id}.wav")),
                    cue: None,
                    size: 1,
                    modified_ns: 1,
                    fingerprint: None,
                    media: media.clone(),
                })
                .collect(),
            ..ScanResult::default()
        };
        core.store.apply_scan(&result).unwrap();
        let ids: Vec<_> = core
            .store
            .stored_scan_files(&[directory.path().to_path_buf()])
            .unwrap()
            .into_iter()
            .filter(|known| {
                known
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("history-")
            })
            .map(|known| known.track_id)
            .collect();
        for (index, &id) in ids.iter().enumerate() {
            core.store.mark_played(id, index as i64 + 1).unwrap();
        }
        let expected: Vec<_> = ids.iter().rev().take(MAX_QUERY_ROWS).copied().collect();
        core.load_initial_state().unwrap();
        let initial: Vec<_> = core
            .state
            .library
            .history
            .iter()
            .map(|entry| entry.track_id)
            .collect();
        assert_eq!(initial, expected);
        core.reload_catalog(false).unwrap();
        let refreshed: Vec<_> = core
            .state
            .library
            .history
            .iter()
            .map(|entry| entry.track_id)
            .collect();
        assert_eq!(refreshed, expected);
    }

    #[test]
    fn sql_queue_restores_order_duplicates_current_and_clear_does_not_reuse_ids() {
        let (_directory, mut core) = fixture();
        core.state.queue.current_id = Some(3);
        core.save(false).unwrap();
        core.queue_ids.clear();
        core.state.queue = QueueState::default();
        core.load_initial_state().unwrap();
        assert_eq!(
            queue_entries(&core),
            [(1, 1), (2, 2), (3, 2), (4, 3), (5, 4)]
        );
        assert_eq!(core.state.queue.current_id, Some(3));
        assert_eq!(core.state.queue.current_position, Some(2));
        assert_eq!(core.state.current_track().unwrap().id, 2);
        core.command(Command::ClearQueue).unwrap();
        core.enqueue(&[1]).unwrap();
        assert!(core.queue_ids[0] > 5);
        core.load_initial_state().unwrap();
        assert_eq!(core.state.queue.total, 1);
        assert!(core.state.current_track.is_none());
    }

    #[test]
    fn playlist_mutations_preserve_track_traversal_and_playback_sections() {
        let (_directory, mut core) = fixture();
        core.publish();
        let track_revision = core.state.query_revisions();
        let sections = core.state.revisions;
        let operations = [
            (
                Command::CreatePlaylist {
                    name: "empty".into(),
                },
                1,
            ),
            (
                Command::CreatePlaylistWithTracks {
                    name: "mix".into(),
                    track_ids: vec![1, 2],
                },
                2,
            ),
            (
                Command::RenamePlaylist {
                    playlist_id: 2,
                    name: "renamed".into(),
                },
                2,
            ),
            (
                Command::AddPlaylist {
                    playlist_id: 2,
                    track_ids: vec![3],
                },
                2,
            ),
            (
                Command::AddPlaylistSources {
                    playlist_id: 2,
                    directories: vec![],
                    track_ids: vec![4],
                },
                2,
            ),
            (Command::RemovePlaylistEntry { entry_id: 1 }, 2),
            (Command::DeletePlaylist { playlist_id: 1 }, 1),
            (Command::DeletePlaylist { playlist_id: 2 }, 0),
        ];
        for (command, total) in operations {
            let before = core.state.query_revisions();
            core.command(command).unwrap();
            core.publish();
            assert_eq!(core.state.library.playlist_total, total);
            assert_eq!(
                core.state.query_revisions().structure,
                track_revision.structure
            );
            assert_eq!(core.state.revisions.queue, sections.queue);
            assert_eq!(core.state.revisions.playback, sections.playback);
            let ViewResponse::TrackBatch(batch) = core
                .view(&Query::TrackBatch {
                    query: None,
                    favorite: None,
                    missing: None,
                    after: Some(1),
                    limit: 2,
                    expected: Some(track_revision),
                })
                .unwrap()
            else {
                panic!("expected track batch")
            };
            assert_eq!(
                batch.rows.iter().map(|track| track.id).collect::<Vec<_>>(),
                [2, 3]
            );
            assert!(
                core.view(&Query::PlaylistSummaryBatch {
                    after: None,
                    limit: 2,
                    expected: Some(before),
                })
                .is_err()
            );
        }
    }

    #[test]
    fn unchanged_scans_keep_query_results_valid_and_still_import_playlists() {
        let (directory, mut core) = fixture();
        core.store.set_queue_current(Some(1)).unwrap();
        core.refresh_queue_tracks().unwrap();
        let result = ScanResult {
            roots: vec![directory.path().to_path_buf()],
            records: core
                .store
                .stored_scan_files(&[directory.path().to_path_buf()])
                .unwrap()
                .into_iter()
                .map(|known| ScanRecord {
                    path: known.path,
                    cue: known.cue,
                    size: known.size,
                    modified_ns: known.modified_ns,
                    fingerprint: known.fingerprint,
                    media: known.media.unwrap(),
                })
                .collect(),
            ..ScanResult::default()
        };
        let before = core.state.query_revisions();
        let current = core.state.current_track.clone().unwrap();
        for import in [false, true] {
            let scan = ScanFinished {
                id: 0,
                result: Ok(result.clone()),
                import: import.then(|| {
                    (
                        "unchanged import".into(),
                        vec![M3uItem {
                            path: directory.path().join("1.wav"),
                            name: None,
                            cue_track: None,
                        }],
                    )
                }),
                progress: ScanProgress {
                    phase: ScanPhase::Completed,
                    processed: 4,
                    total: Some(4),
                    errors: 0,
                    path: None,
                },
            };
            assert!(core.finish_scan(scan).unwrap());
            let after = core.state.query_revisions();
            assert_eq!(after.library, before.library);
            assert_eq!(after.structure, before.structure);
            assert_eq!(after.queue, before.queue);
            if import {
                assert_ne!(after.playlist, before.playlist);
                let playlist = core.store.playlist_summary_page(0, 1).unwrap().rows[0].id;
                assert_eq!(core.store.playlist_track_ids(playlist, 10).unwrap(), [1]);
            } else {
                assert_eq!(after, before);
            }
            assert_eq!(core.state.current_track.as_deref(), Some(current.as_ref()));
            assert!(
                core.view(&Query::TrackBatch {
                    query: None,
                    favorite: None,
                    missing: None,
                    after: None,
                    limit: 2,
                    expected: Some(before),
                })
                .is_ok()
            );
        }
    }

    #[test]
    fn failed_playlist_import_keeps_committed_scan_visible() {
        let (directory, mut core) = fixture();
        let before = core.state.query_revisions();
        let mut media = core
            .store
            .stored_scan_files(&[directory.path().to_path_buf()])
            .unwrap()
            .into_iter()
            .find(|known| known.track_id == 1)
            .unwrap()
            .media
            .unwrap();
        media.title = "Updated by scan".into();
        let scan = ScanFinished {
            id: 0,
            result: Ok(ScanResult {
                records: vec![ScanRecord {
                    path: directory.path().join("1.wav"),
                    size: 1,
                    modified_ns: 2,
                    fingerprint: None,
                    cue: None,
                    media,
                }],
                ..ScanResult::default()
            }),
            import: Some(("empty".into(), vec![])),
            progress: ScanProgress {
                phase: ScanPhase::Completed,
                processed: 1,
                total: Some(1),
                errors: 0,
                path: None,
            },
        };
        assert!(core.finish_scan(scan).is_err());
        assert_eq!(
            core.store.queue_page(0, 1).unwrap().rows[0].track.title,
            "Updated by scan"
        );
        assert!(
            core.view(&Query::TrackBatch {
                query: None,
                favorite: None,
                missing: None,
                after: Some(1),
                limit: 2,
                expected: Some(before),
            })
            .is_err()
        );
        assert_eq!(core.state.library.track_total, 4);
        assert_eq!(core.state.library.playlist_total, 0);
    }

    #[test]
    fn failed_cue_import_never_reuses_old_entries_but_keeps_readable_items() {
        for readable in [false, true] {
            let (directory, mut core) = fixture();
            let sheet = directory.path().join("album.cue");
            let media = core
                .store
                .stored_scan_files(&[directory.path().to_path_buf()])
                .unwrap()
                .into_iter()
                .find(|file| file.track_id == 1)
                .unwrap()
                .media
                .unwrap();
            let old = ScanRecord {
                path: directory.path().join("1.wav"),
                size: 1,
                modified_ns: 1,
                fingerprint: None,
                cue: Some(crate::model::CueSegment {
                    sheet: sheet.clone(),
                    number: 1,
                    start_frame: 0,
                    end_frame: Some(75),
                }),
                media: media.clone(),
            };
            core.store
                .apply_scan(&ScanResult {
                    records: vec![old],
                    ..ScanResult::default()
                })
                .unwrap();
            let old_id = core
                .store
                .track_id_for_source(&sheet, Some(1))
                .unwrap()
                .unwrap();
            let path = directory.path().join("2.wav");
            let scan = ScanFinished {
                id: 0,
                result: Ok(ScanResult {
                    records: if readable {
                        vec![ScanRecord {
                            path: path.clone(),
                            size: 1,
                            modified_ns: 1,
                            fingerprint: None,
                            cue: None,
                            media,
                        }]
                    } else {
                        vec![]
                    },
                    errors: vec!["CUE sheet could not be read".into()],
                    ..ScanResult::default()
                }),
                import: Some((
                    "partial".into(),
                    vec![
                        M3uItem {
                            path: sheet,
                            name: None,
                            cue_track: Some(1),
                        },
                        M3uItem {
                            path,
                            name: None,
                            cue_track: None,
                        },
                    ],
                )),
                progress: ScanProgress {
                    phase: ScanPhase::Completed,
                    processed: 2,
                    total: Some(2),
                    errors: 1,
                    path: None,
                },
            };
            let result = core.finish_scan(scan);
            assert!(core.store.track(old_id).unwrap().is_some());
            if readable {
                assert!(!result.unwrap());
                let rows = core
                    .store
                    .playlist_entries_page(1, 0, MAX_QUERY_ROWS)
                    .unwrap()
                    .rows;
                assert_eq!(rows.iter().map(|row| row.track_id).collect::<Vec<_>>(), [2]);
            } else {
                assert!(result.is_err());
                assert_eq!(core.state.library.playlist_total, 0);
            }
        }
    }

    #[test]
    fn deferred_config_failure_retains_values_and_recovers_without_clearing_other_errors() {
        for unrelated in [false, true] {
            let (directory, mut core) = fixture();
            let path = core.state.system.config_path.clone();
            core.state.system.config.save(&path).unwrap();
            let backup = directory.path().join("config.good");
            std::fs::rename(&path, &backup).unwrap();
            std::fs::create_dir(&path).unwrap();
            core.command(Command::Volume { value: 0.25 }).unwrap();
            core.config_last_attempt -= Duration::from_millis(250);
            assert!(core.flush_config(false).is_err());
            core.publish();
            assert!(core.shared.read().system.last_error.is_some());
            assert_eq!(core.state.playback.volume, 0.25);
            assert_ne!(core.state.system.config.volume, 0.25);
            assert_ne!(Config::load(&backup).unwrap().volume, 0.25);
            if unrelated {
                core.state.system.last_error = Some("Unrelated failure".into());
            } else {
                core.command(Command::DismissError).unwrap();
            }
            // Retrying the same failure must not replace another error or a dismissal.
            let expected = core.state.system.last_error.clone();
            assert!(core.flush_config(true).is_err());
            assert_eq!(core.state.system.last_error, expected);
            std::fs::remove_dir(&path).unwrap();
            std::fs::rename(&backup, &path).unwrap();
            assert!(core.flush_config(true).unwrap());
            assert_eq!(Config::load(&path).unwrap().volume, 0.25);
            assert_eq!(core.state.system.last_error, expected);
            assert!(!core.flush_config(true).unwrap());
        }
    }

    #[test]
    fn successful_config_retry_clears_only_its_own_failure() {
        let (_directory, mut core) = fixture();
        let path = core.state.system.config_path.clone();
        std::fs::create_dir(&path).unwrap();
        core.command(Command::Shuffle { enabled: true }).unwrap();
        assert!(core.flush_config(true).is_err());
        assert!(core.state.system.last_error.is_some());
        std::fs::remove_dir(&path).unwrap();
        assert!(core.flush_config(true).unwrap());
        assert!(core.state.system.last_error.is_none());
        assert!(Config::load(&path).unwrap().shuffle);
    }

    #[test]
    fn shutdown_flush_reports_failure_and_does_not_wait_for_debounce() {
        let (_directory, mut core) = fixture();
        let path = core.state.system.config_path.clone();
        std::fs::create_dir(&path).unwrap();
        core.command(Command::Repeat {
            mode: RepeatMode::All,
        })
        .unwrap();
        let (sender, receiver) = bounded(1);
        drop(sender);
        run_until_disconnected(&mut core, receiver);
        let state = core.shared.read();
        assert!(state.system.shutting_down);
        assert!(state.system.last_error.is_some());
        assert_eq!(state.playback.repeat, RepeatMode::All);
        assert_eq!(state.system.config.repeat, RepeatMode::Off);
    }

    #[test]
    fn still_mode_toggle_preserves_debounced_playback_and_device_settings() {
        let (_directory, mut core) = fixture();
        core.command(Command::Volume { value: 0.25 }).unwrap();
        core.command(Command::Shuffle { enabled: true }).unwrap();
        core.command(Command::Repeat {
            mode: RepeatMode::All,
        })
        .unwrap();
        // Model a selected output whose persisted snapshot is still debounced.
        core.state.system.selected_device = Some("pending-output".into());
        for enabled in [true, false] {
            core.update_gui_config(GuiConfigChange::ToggleStillMode)
                .unwrap();
            assert_eq!(core.state.system.config.still_mode, enabled);
            assert_eq!(core.state.playback.volume, 0.25);
            assert!(core.state.playback.shuffle);
            assert_eq!(core.state.playback.repeat, RepeatMode::All);
            assert_eq!(
                core.state.system.selected_device.as_deref(),
                Some("pending-output")
            );
            let saved = Config::load(&core.state.system.config_path).unwrap();
            assert_eq!(saved.volume, 0.25);
            assert!(saved.shuffle);
            assert_eq!(saved.repeat, RepeatMode::All);
            assert_eq!(saved.output_device.as_deref(), Some("pending-output"));
            assert_eq!(saved.still_mode, enabled);
        }
    }

    #[test]
    fn library_column_toggles_merge_latest_config_without_audio_side_effects() {
        let (_directory, mut core) = fixture();
        core.command(Command::Volume { value: 0.25 }).unwrap();
        core.command(Command::Shuffle { enabled: true }).unwrap();
        core.command(Command::Repeat {
            mode: RepeatMode::All,
        })
        .unwrap();
        core.state.system.selected_device = Some("pending-output".into());
        let config = Arc::make_mut(&mut core.state.system.config);
        config.ui_scale = 1.25;
        config.waveform_seek_enabled = false;
        let audio = recording_audio(&mut core);
        let mut expected = core.state.system.config.as_ref().clone();
        expected.volume = 0.25;
        expected.shuffle = true;
        expected.repeat = RepeatMode::All;
        expected.output_device = Some("pending-output".into());
        for (column, columns) in [
            (
                LibraryColumn::Duration,
                crate::config::LibraryColumns {
                    artist: false,
                    album: false,
                    duration: false,
                    codec: false,
                    bitrate: false,
                },
            ),
            (
                LibraryColumn::Artist,
                crate::config::LibraryColumns {
                    artist: true,
                    album: false,
                    duration: false,
                    codec: false,
                    bitrate: false,
                },
            ),
            (
                LibraryColumn::Album,
                crate::config::LibraryColumns {
                    artist: true,
                    album: true,
                    duration: false,
                    codec: false,
                    bitrate: false,
                },
            ),
            (
                LibraryColumn::Codec,
                crate::config::LibraryColumns {
                    artist: true,
                    album: true,
                    duration: false,
                    codec: true,
                    bitrate: false,
                },
            ),
            (
                LibraryColumn::Bitrate,
                crate::config::LibraryColumns {
                    artist: true,
                    album: true,
                    duration: false,
                    codec: true,
                    bitrate: true,
                },
            ),
            (
                LibraryColumn::Artist,
                crate::config::LibraryColumns {
                    artist: false,
                    album: true,
                    duration: false,
                    codec: true,
                    bitrate: true,
                },
            ),
        ] {
            core.update_gui_config(GuiConfigChange::ToggleLibraryColumn(column))
                .unwrap();
            expected.library_columns = columns;
            assert_eq!(*core.state.system.config, expected);
            assert_eq!(
                Config::load(&core.state.system.config_path).unwrap(),
                expected
            );
            assert_eq!(core.state.playback.volume, expected.volume);
            assert_eq!(core.state.playback.shuffle, expected.shuffle);
            assert_eq!(core.state.playback.repeat, expected.repeat);
            assert_eq!(core.state.system.selected_device, expected.output_device);
            assert!(!core.flush_config(true).unwrap());
            core.publish();
            assert_eq!(*core.shared.read().system.config, expected);
            assert!(matches!(
                audio.try_recv(),
                Err(crossbeam_channel::TryRecvError::Empty)
            ));
        }
    }

    #[test]
    fn library_column_failed_save_does_not_publish_selection_or_deliver_audio() {
        let (_directory, mut core) = fixture();
        let audio = recording_audio(&mut core);
        let previous = core.state.system.config.clone();
        std::fs::create_dir(&core.state.system.config_path).unwrap();
        core.update_gui_config(GuiConfigChange::ToggleLibraryColumn(
            LibraryColumn::Duration,
        ))
        .unwrap_err();
        assert_eq!(*core.state.system.config, *previous);
        core.publish();
        assert_eq!(*core.shared.read().system.config, *previous);
        assert!(core.state.system.last_error.is_some());
        assert!(matches!(
            audio.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        ));
        std::fs::remove_dir(&core.state.system.config_path).unwrap();
        core.update_gui_config(GuiConfigChange::ToggleLibraryColumn(
            LibraryColumn::Duration,
        ))
        .unwrap();
        assert!(!core.state.system.config.library_columns.duration);
        assert_eq!(
            Config::load(&core.state.system.config_path).unwrap(),
            *core.state.system.config
        );
    }

    #[test]
    fn desktop_config_edits_preserve_live_playback_and_merge_roots() {
        let (directory, mut core) = fixture();
        core.command(Command::Volume { value: 0.25 }).unwrap();
        core.command(Command::Shuffle { enabled: true }).unwrap();
        core.command(Command::Repeat {
            mode: RepeatMode::All,
        })
        .unwrap();
        core.state.system.selected_device = Some("pending-output".into());
        let root = directory.path().join("added");
        let seek = core.state.system.config.waveform_seek_enabled;
        core.update_gui_config(GuiConfigChange::ToggleWaveformSeek)
            .unwrap();
        core.update_gui_config(GuiConfigChange::AddLibraryRoot(root.clone()))
            .unwrap();
        core.update_gui_config(GuiConfigChange::AddLibraryRoot(root))
            .unwrap();
        assert_eq!(core.state.system.config.library_roots.len(), 2);
        assert_eq!(core.state.system.config.waveform_seek_enabled, !seek);
        assert_eq!(core.state.playback.volume, 0.25);
        assert!(core.state.playback.shuffle);
        assert_eq!(core.state.playback.repeat, RepeatMode::All);
        assert_eq!(
            core.state.system.selected_device.as_deref(),
            Some("pending-output")
        );
        let saved = Config::load(&core.state.system.config_path).unwrap();
        assert_eq!(saved.volume, 0.25);
        assert!(saved.shuffle);
        assert_eq!(saved.repeat, RepeatMode::All);
        assert_eq!(saved.output_device.as_deref(), Some("pending-output"));
        assert_eq!(saved.library_roots.len(), 2);
    }

    #[test]
    fn settings_apply_resolves_unedited_playback_and_library_columns_at_execution() {
        let (_directory, mut core) = fixture();
        let mut draft = core.state.system.config.as_ref().clone();
        draft.volume = 0.4;
        core.command(Command::Shuffle { enabled: true }).unwrap();
        core.command(Command::Repeat {
            mode: RepeatMode::One,
        })
        .unwrap();
        for column in LibraryColumn::ALL {
            core.update_gui_config(GuiConfigChange::ToggleLibraryColumn(column))
                .unwrap();
        }
        let columns = core.state.system.config.library_columns;
        core.update_gui_config(GuiConfigChange::Settings(draft))
            .unwrap();
        assert!(core.state.playback.shuffle);
        assert_eq!(core.state.playback.repeat, RepeatMode::One);
        assert_eq!(core.state.playback.volume, 0.4);
        assert_eq!(core.state.system.config.library_columns, columns);
        let saved = Config::load(&core.state.system.config_path).unwrap();
        assert!(saved.shuffle);
        assert_eq!(saved.repeat, RepeatMode::One);
        assert_eq!(saved.volume, 0.4);
        assert_eq!(saved.library_columns, columns);
    }

    #[test]
    fn playback_publication_does_not_invalidate_unchanged_sections() {
        let (_directory, mut core) = fixture();
        core.publish();
        let before = core.state.revisions;
        let wakeup_revision = core.shared.read().system.revision;
        core.state.playback.position = 2.0;
        core.publish();
        assert_eq!(
            core.state.revisions.changed_since(before),
            StateSections {
                playback: true,
                ..StateSections::default()
            }
        );
        {
            let published = core.shared.read();
            assert_eq!(published.revisions.system, before.system);
            assert_ne!(published.system.revision, wakeup_revision);
        }
        let before = core.state.revisions;
        core.state.system.last_error = Some("failed scan".into());
        core.publish();
        assert_eq!(
            core.state.revisions.changed_since(before),
            StateSections {
                system: true,
                ..StateSections::default()
            }
        );
    }

    #[test]
    fn scan_result_dismissal_preserves_errors_and_active_scans() {
        let (_directory, mut core) = fixture();
        for phase in [
            ScanPhase::Completed,
            ScanPhase::Failed,
            ScanPhase::Cancelled,
        ] {
            core.state.system.scan_progress = Some(ScanProgress {
                phase,
                processed: 2,
                total: Some(2),
                errors: usize::from(phase == ScanPhase::Failed),
                path: None,
            });
            core.state.system.scan_message = "Scan result".into();
            core.state.system.last_error = Some("Unreadable source".into());
            core.state.system.scanning = true;
            let progress = core.state.system.scan_progress.clone();
            core.command(Command::DismissScanResult).unwrap();
            assert_eq!(core.state.system.scan_progress, progress);
            assert_eq!(core.state.system.scan_message, "Scan result");

            core.state.system.scanning = false;
            core.command(Command::DismissScanResult).unwrap();
            assert!(core.state.system.scan_progress.is_none());
            assert!(core.state.system.scan_message.is_empty());
            assert_eq!(
                core.state.system.last_error.as_deref(),
                Some("Unreadable source")
            );
        }
    }

    #[test]
    fn playlist_group_commands_reject_stale_intents_and_preserve_noop_revisions() {
        let (_directory, mut core) = fixture();
        for name in ["one", "two"] {
            core.command(Command::CreatePlaylistWithTracks {
                name: name.into(),
                track_ids: vec![1, 2, 3, 4],
            })
            .unwrap();
        }
        let entries = core.store.playlist_entries_page(1, 0, 4).unwrap().rows;
        let ids: Vec<_> = entries.iter().map(|row| row.id).collect();
        let baseline = core.state.playlist_query_revisions(1);
        let token = baseline.playlist_entries.unwrap().revision;
        for command in [
            Command::RemovePlaylistEntries {
                playlist_id: 1,
                entry_ids: vec![],
                expected_revision: token,
            },
            Command::MovePlaylistEntries {
                playlist_id: 1,
                entry_ids: vec![ids[0], ids[1]],
                before_entry_id: Some(ids[2]),
                expected_revision: token,
            },
            Command::MovePlaylistEntries {
                playlist_id: 1,
                entry_ids: vec![ids[0], ids[2]],
                before_entry_id: Some(ids[2]),
                expected_revision: token,
            },
        ] {
            core.command(command).unwrap();
            assert_eq!(core.state.playlist_query_revisions(1), baseline);
        }
        for command in [
            Command::RemovePlaylistEntries {
                playlist_id: 1,
                entry_ids: vec![ids[0], i64::MAX],
                expected_revision: token,
            },
            Command::MovePlaylistEntries {
                playlist_id: 1,
                entry_ids: vec![ids[0]],
                before_entry_id: Some(i64::MAX),
                expected_revision: token,
            },
            Command::MovePlaylistEntries {
                playlist_id: 999,
                entry_ids: vec![],
                before_entry_id: None,
                expected_revision: token,
            },
        ] {
            assert!(core.command(command).is_err());
            assert_eq!(core.state.playlist_query_revisions(1), baseline);
            assert_eq!(
                core.store.playlist_entries_page(1, 0, 4).unwrap().rows,
                entries
            );
        }
        let other = core.store.playlist_entries_page(2, 0, 1).unwrap().rows[0].id;
        core.command(Command::MovePlaylistEntry {
            entry_id: other,
            index: 3,
        })
        .unwrap();
        assert_eq!(core.state.playlist_query_revisions(1), baseline);
        core.command(Command::MovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[2], ids[0]],
            before_entry_id: None,
            expected_revision: token,
        })
        .unwrap();
        let reordered = core.state.playlist_query_revisions(1);
        assert_eq!(reordered.playlist, baseline.playlist);
        assert_ne!(reordered.playlist_entries, baseline.playlist_entries);
        assert_eq!(core.store.playlist_track_ids(1, 4).unwrap(), [2, 4, 1, 3]);
        for command in [
            Command::RemovePlaylistEntries {
                playlist_id: 1,
                entry_ids: vec![ids[0]],
                expected_revision: token,
            },
            Command::MovePlaylistEntries {
                playlist_id: 1,
                entry_ids: vec![],
                before_entry_id: None,
                expected_revision: token,
            },
        ] {
            assert!(core.command(command).is_err());
            assert_eq!(core.state.playlist_query_revisions(1), reordered);
            assert_eq!(core.store.playlist_track_ids(1, 4).unwrap(), [2, 4, 1, 3]);
        }
        core.command(Command::RemovePlaylistEntries {
            playlist_id: 1,
            entry_ids: vec![ids[2], ids[0]],
            expected_revision: reordered.playlist_entries.unwrap().revision,
        })
        .unwrap();
        let removed = core.state.playlist_query_revisions(1);
        assert_ne!(removed.playlist, reordered.playlist);
        assert_ne!(removed.playlist_entries, reordered.playlist_entries);
        assert_eq!(core.store.playlist_track_ids(1, 4).unwrap(), [2, 4]);
        assert!(
            core.command(Command::RemovePlaylistEntries {
                playlist_id: 1,
                entry_ids: vec![ids[1]],
                expected_revision: token,
            })
            .is_err()
        );
        assert!(
            core.command(Command::MovePlaylistEntries {
                playlist_id: 1,
                entry_ids: vec![ids[1]],
                before_entry_id: None,
                expected_revision: reordered.playlist_entries.unwrap().revision,
            })
            .is_err()
        );
        assert_eq!(core.store.playlist_track_ids(1, 4).unwrap(), [2, 4]);
        assert_eq!(core.state.playlist_query_revisions(1), removed);
    }

    #[test]
    fn playlist_location_batches_publish_scoped_tokens_even_for_empty_results() {
        let (_directory, mut core) = fixture();
        core.command(Command::CreatePlaylistWithTracks {
            name: "mix".into(),
            track_ids: vec![1, 2],
        })
        .unwrap();
        let entry = core.store.playlist_entries_page(1, 0, 1).unwrap().rows[0].id;
        let query = Query::PlaylistEntryLocations {
            playlist_id: 1,
            entry_ids: vec![entry, i64::MAX],
        };
        let before = core.query_response(&query);
        assert_eq!(before.revisions, core.state.playlist_query_revisions(1));
        assert!(
            matches!(before.result, Ok(ViewResponse::PlaylistEntryLocations(rows)) if rows.len() == 1 && rows[0].position == 0)
        );
        let empty = core.query_response(&Query::PlaylistEntryLocations {
            playlist_id: 1,
            entry_ids: vec![i64::MAX],
        });
        assert_eq!(empty.revisions, before.revisions);
        assert!(
            matches!(empty.result, Ok(ViewResponse::PlaylistEntryLocations(rows)) if rows.is_empty())
        );
        let oversized = core.query_response(&Query::PlaylistEntryLocations {
            playlist_id: 1,
            entry_ids: vec![entry; MAX_QUERY_ROWS + 1],
        });
        assert_eq!(oversized.revisions, before.revisions);
        assert!(matches!(oversized.result, Err(QueryError::Message(_))));
        core.command(Command::AddPlaylist {
            playlist_id: 1,
            track_ids: vec![3],
        })
        .unwrap();
        assert!(
            core.command(Command::MovePlaylistEntries {
                playlist_id: 1,
                entry_ids: vec![entry],
                before_entry_id: None,
                expected_revision: before.revisions.playlist_entries.unwrap().revision,
            })
            .is_err()
        );
        assert_eq!(core.store.playlist_track_ids(1, 4).unwrap(), [1, 2, 3]);
        let current = core.query_response(&query);
        assert_ne!(
            current.revisions.playlist_entries,
            before.revisions.playlist_entries
        );
        assert!(
            matches!(current.result, Ok(ViewResponse::PlaylistEntryLocations(rows)) if rows[0].total == 3)
        );
    }

    #[test]
    fn playlist_append_and_move_only_invalidate_changed_pages() {
        let (_directory, mut core) = fixture();
        core.command(Command::CreatePlaylistWithTracks {
            name: "mix".into(),
            track_ids: vec![1, 2],
        })
        .unwrap();
        let entry_id = core.store.playlist_entries_page(1, 0, 1).unwrap().rows[0].id;
        let expected = core.state.playlist_query_revisions(1);
        let query = Query::PlaylistEntryBatch {
            playlist_id: 1,
            after: None,
            limit: 1,
            expected: Some(expected),
        };
        for command in [
            Command::AddPlaylist {
                playlist_id: 1,
                track_ids: vec![],
            },
            Command::AddPlaylist {
                playlist_id: 1,
                track_ids: vec![2, 1, 2],
            },
            Command::AddPlaylistSources {
                playlist_id: 1,
                directories: vec![],
                track_ids: vec![],
            },
            Command::AddPlaylistSources {
                playlist_id: 1,
                directories: vec![],
                track_ids: vec![1, 2],
            },
            Command::MovePlaylistEntry { entry_id, index: 0 },
        ] {
            core.command(command).unwrap();
            assert_eq!(core.state.playlist_query_revisions(1), expected);
            assert!(core.view(&query).is_ok());
        }
        for command in [
            Command::AddPlaylist {
                playlist_id: 999,
                track_ids: vec![],
            },
            Command::AddPlaylist {
                playlist_id: 1,
                track_ids: vec![3, 999],
            },
            Command::AddPlaylistSources {
                playlist_id: 999,
                directories: vec![],
                track_ids: vec![],
            },
            Command::AddPlaylistSources {
                playlist_id: 1,
                directories: vec![],
                track_ids: vec![3, 999],
            },
            Command::MovePlaylistEntry {
                entry_id,
                index: usize::MAX,
            },
            Command::MovePlaylistEntry {
                entry_id: i64::MAX,
                index: 0,
            },
        ] {
            assert!(core.command(command).is_err());
            assert_eq!(core.state.playlist_query_revisions(1), expected);
            assert_eq!(core.store.playlist_track_ids(1, 10).unwrap(), [1, 2]);
        }
        for command in [
            Command::AddPlaylist {
                playlist_id: 1,
                track_ids: vec![3],
            },
            Command::AddPlaylistSources {
                playlist_id: 1,
                directories: vec![],
                track_ids: vec![4],
            },
            Command::MovePlaylistEntry { entry_id, index: 3 },
        ] {
            let reordered = matches!(command, Command::MovePlaylistEntry { .. });
            let before = core.state.playlist_query_revisions(1);
            let query = Query::PlaylistEntryBatch {
                playlist_id: 1,
                after: None,
                limit: 1,
                expected: Some(before),
            };
            core.command(command).unwrap();
            let after = core.state.playlist_query_revisions(1);
            if reordered {
                assert_eq!(after.playlist, before.playlist);
                assert_ne!(after.playlist_entries, before.playlist_entries);
            } else {
                assert_ne!(after.playlist, before.playlist);
            }
            assert_eq!(after.library, before.library);
            assert_eq!(after.queue, before.queue);
            assert_eq!(core.view(&query).unwrap_err(), QueryError::RevisionChanged);
        }
        assert_eq!(core.store.playlist_track_ids(1, 10).unwrap(), [2, 3, 4, 1]);
    }

    #[test]
    fn directory_missing_query_composes_filters_ancestors_and_exact_pages() {
        use crate::model::DirectoryRow;
        let (directory, core) = fixture_with_database(Some(10.0), true);
        let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
        connection
            .execute_batch(
                "UPDATE tracks SET path='/music/a/deep/missing.wav', raw_title='Needle Nested', favorite=1, missing=1 WHERE id=1;
                 UPDATE tracks SET path='/music/a/deep/present.wav', raw_title='Needle Present', favorite=1, missing=0 WHERE id=2;
                 UPDATE tracks SET path='/music/b/deep/missing.wav', raw_title='Needle Other', favorite=0, missing=1 WHERE id=3;
                 UPDATE tracks SET path='/music/direct.wav', raw_title='Needle Direct', favorite=1, missing=1 WHERE id=4;
                 INSERT INTO tracks(id,path,raw_title,favorite,missing) VALUES
                 (5,'/music/present/only.wav','Other',0,0),
                 (6,'/music/hidden/deep/missing.wav','Other',1,1);",
            )
            .unwrap();
        let page = |path: &str, query: Option<&str>, favorite, missing, offset, limit| {
            let response = core.query_response(&Query::DirectoryPage {
                path: path.into(),
                query: query.map(str::to_owned),
                favorite,
                missing,
                offset,
                limit,
            });
            let Ok(ViewResponse::DirectoryPage(page)) = response.result else {
                panic!("expected directory page");
            };
            page
        };
        let directory_row = |path: &str| DirectoryRow::Directory { path: path.into() };
        for (missing, paths, track_ids) in [
            (true, vec!["/music/a", "/music/b", "/music/hidden"], vec![4]),
            (false, vec!["/music/a", "/music/present"], vec![]),
        ] {
            let all = page("/music", None, None, Some(missing), 0, 10);
            assert_eq!(all.total, paths.len() + track_ids.len());
            assert_eq!(
                &all.rows[..paths.len()],
                paths
                    .iter()
                    .map(|path| directory_row(path))
                    .collect::<Vec<_>>()
            );
            for (row, id) in all.rows[paths.len()..].iter().zip(track_ids) {
                assert!(
                    matches!(row, DirectoryRow::Track(row) if row.id == id && row.missing == missing)
                );
            }
            for offset in 0..all.total {
                let single = page("/music", None, None, Some(missing), offset, 1);
                assert_eq!(single.total, all.total);
                assert_eq!(single.rows, all.rows[offset..offset + 1]);
            }
            for (offset, limit) in [(0, 0), (all.total, 10), (usize::MAX, 10)] {
                let empty = page("/music", None, None, Some(missing), offset, limit);
                assert_eq!(empty.total, all.total);
                assert!(empty.rows.is_empty());
            }
        }
        for missing in [true, false] {
            let selected = page("/music", Some("NEEDLE"), Some(true), Some(missing), 0, 10);
            assert_eq!(selected.total, if missing { 2 } else { 1 });
            assert_eq!(selected.rows[0], directory_row("/music/a"));
            if missing {
                assert!(
                    matches!(&selected.rows[1], DirectoryRow::Track(row) if row.id == 4 && row.favorite && row.missing)
                );
            }
            let ancestor = page("/music/a", Some("needle"), Some(true), Some(missing), 0, 10);
            assert_eq!(ancestor.total, 1);
            assert_eq!(ancestor.rows, [directory_row("/music/a/deep")]);
            let leaf = page(
                "/music/a/deep",
                Some("needle"),
                Some(true),
                Some(missing),
                0,
                10,
            );
            assert_eq!(leaf.total, 1);
            let expected_id = if missing { 1 } else { 2 };
            assert!(
                matches!(&leaf.rows[..], [DirectoryRow::Track(row)] if row.id == expected_id && row.favorite && row.missing == missing)
            );
            let count = page("/music", Some("needle"), Some(true), Some(missing), 0, 0);
            assert_eq!(count.total, selected.total);
            assert!(count.rows.is_empty());
            let absent = page("/music", Some("absent"), Some(true), Some(missing), 0, 10);
            assert_eq!(absent.total, 0);
            assert!(absent.rows.is_empty());
        }
        let non_favorite = page("/music", Some("needle"), Some(false), Some(true), 0, 10);
        assert_eq!(non_favorite.total, 1);
        assert_eq!(non_favorite.rows, [directory_row("/music/b")]);
        let absent = page("/music/hidden", None, None, Some(false), 0, 10);
        assert_eq!(absent.total, 0);
        assert!(absent.rows.is_empty());
    }

    #[test]
    fn playlist_reorders_preserve_catalog_and_other_playlist_queries() {
        let (_directory, mut core) = fixture();
        for (name, tracks) in [("one", vec![1, 2, 3]), ("two", vec![2, 3, 4])] {
            core.command(Command::CreatePlaylistWithTracks {
                name: name.into(),
                track_ids: tracks,
            })
            .unwrap();
        }
        core.publish();
        let before = core.state.revisions;
        let summaries = Query::PlaylistSummaries {
            offset: 0,
            limit: 2,
        };
        let catalog = core.query_response(&summaries);
        let first_entry = core.store.playlist_entries_page(1, 0, 1).unwrap().rows[0].id;
        let old_lookup = core.query_response(&Query::PlaylistEntry {
            entry_id: first_entry,
        });
        let batch = |playlist_id, after, expected| Query::PlaylistEntryBatch {
            playlist_id,
            after,
            limit: 1,
            expected,
        };
        let first = core.query_response(&batch(1, None, Some(catalog.revisions)));
        let other = core.query_response(&batch(2, None, Some(catalog.revisions)));
        let Ok(ViewResponse::PlaylistEntryBatch(entries)) = &first.result else {
            panic!("expected first entry batch");
        };
        let continuation = entries.next;
        let old_snapshot = GuiSnapshot::from_core(&core.state);
        core.command(Command::MovePlaylistEntry {
            entry_id: first_entry,
            index: 2,
        })
        .unwrap();
        core.publish();
        assert_eq!(
            core.state.revisions.changed_since(before),
            StateSections {
                library: true,
                ..StateSections::default()
            }
        );
        assert_eq!(
            serde_json::to_value(core.query_response(&summaries)).unwrap(),
            serde_json::to_value(&catalog).unwrap(),
        );
        assert!(old_snapshot.library.playlist_entry_revisions.is_empty());
        assert_eq!(core.state.library.playlist_entry_revisions.len(), 1);
        assert_eq!(core.state.query_revisions(), catalog.revisions);
        assert_eq!(
            core.view(&batch(1, continuation, Some(first.revisions)))
                .unwrap_err(),
            QueryError::RevisionChanged
        );
        assert!(core.view(&batch(2, None, Some(other.revisions))).is_ok());
        // A summary baseline cannot silently continue an entry traversal.
        assert_eq!(
            core.view(&batch(1, continuation, Some(catalog.revisions)))
                .unwrap_err(),
            QueryError::RevisionChanged
        );
        let fresh = core.query_response(&Query::PlaylistEntry {
            entry_id: first_entry,
        });
        assert_ne!(
            fresh.revisions.playlist_entries,
            old_lookup.revisions.playlist_entries
        );
        assert!(
            matches!(fresh.result, Ok(ViewResponse::PlaylistEntry(Some(entry)))
            if entry.id == first_entry && entry.position == 2)
        );
        assert!(
            core.view(&Query::PlaylistSummaryBatch {
                after: Some(1),
                limit: 1,
                expected: Some(catalog.revisions),
            })
            .is_ok()
        );
        assert!(
            core.view(&Query::TrackBatch {
                query: None,
                favorite: None,
                missing: None,
                after: None,
                limit: 1,
                expected: Some(catalog.revisions),
            })
            .is_ok()
        );

        let current = core.state.playlist_query_revisions(1);
        let other_entry = core.store.playlist_entries_page(2, 0, 1).unwrap().rows[0].id;
        // Two mutations may coalesce into one snapshot; both scoped tokens survive.
        for index in [2, 1] {
            core.command(Command::MovePlaylistEntry {
                entry_id: other_entry,
                index,
            })
            .unwrap();
        }
        core.publish();
        assert!(core.view(&batch(1, None, Some(current))).is_ok());
        assert_eq!(
            core.view(&batch(2, None, Some(other.revisions)))
                .unwrap_err(),
            QueryError::RevisionChanged
        );
        assert_eq!(core.state.library.playlist_entry_revisions.len(), 2);
        assert_eq!(core.state.query_revisions(), catalog.revisions);
        // A token for another playlist is never an entry-order baseline.
        assert_eq!(
            core.view(&batch(2, None, Some(current))).unwrap_err(),
            QueryError::RevisionChanged
        );
    }

    #[test]
    fn playlist_catalog_epochs_and_metadata_reject_old_entry_results() {
        let (_directory, mut core) = fixture();
        core.command(Command::CreatePlaylistWithTracks {
            name: "one".into(),
            track_ids: vec![1, 2, 3],
        })
        .unwrap();
        let removed = core.store.playlist_entries_page(1, 0, 1).unwrap().rows[0].id;
        for command in [
            Command::CreatePlaylist { name: "two".into() },
            Command::RenamePlaylist {
                playlist_id: 1,
                name: "renamed".into(),
            },
            Command::AddPlaylist {
                playlist_id: 1,
                track_ids: vec![4],
            },
            Command::RemovePlaylistEntry { entry_id: removed },
            Command::DeletePlaylist { playlist_id: 1 },
        ] {
            let page = core.store.playlist_entries_page(1, 0, 1).unwrap();
            core.command(Command::MovePlaylistEntry {
                entry_id: page.rows[0].id,
                index: page.total - 1,
            })
            .unwrap();
            let expected = core.state.playlist_query_revisions(1);
            core.command(command).unwrap();
            assert_ne!(core.state.library.playlist_revision, expected.playlist);
            assert_eq!(core.state.library.revision, expected.library);
            assert!(core.state.library.playlist_entry_revisions.is_empty());
            assert_eq!(
                core.view(&Query::PlaylistEntryBatch {
                    playlist_id: 1,
                    after: None,
                    limit: 1,
                    expected: Some(expected),
                })
                .unwrap_err(),
                QueryError::RevisionChanged
            );
        }
        core.command(Command::AddPlaylist {
            playlist_id: 2,
            track_ids: vec![1, 2],
        })
        .unwrap();
        let expected = core.state.playlist_query_revisions(2);
        core.command(Command::EditTrack {
            track_id: 1,
            title: Some("edited".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .unwrap();
        assert_ne!(core.state.library.revision, expected.library);
        assert_eq!(
            core.view(&Query::PlaylistEntryBatch {
                playlist_id: 2,
                after: None,
                limit: 1,
                expected: Some(expected),
            })
            .unwrap_err(),
            QueryError::RevisionChanged
        );
        let response = core.query_response(&Query::PlaylistEntries {
            playlist_id: 2,
            offset: 0,
            limit: 1,
        });
        assert!(
            matches!(response.result, Ok(ViewResponse::PlaylistEntries(page))
            if page.rows[0].title == "edited")
        );
    }

    #[test]
    fn batch_continuation_rejects_library_change_but_not_playback_change() {
        let (_directory, mut core) = fixture();
        let expected = core.state.query_revisions();
        let ViewResponse::TrackBatch(first) = core
            .view(&Query::TrackBatch {
                query: None,
                favorite: None,
                missing: None,
                after: None,
                limit: 2,
                expected: Some(expected),
            })
            .unwrap()
        else {
            panic!("expected track batch");
        };
        let query = Query::TrackBatch {
            query: None,
            favorite: None,
            missing: None,
            after: first.next,
            limit: 2,
            expected: Some(expected),
        };
        core.state.playback.position = 2.0;
        core.publish();
        let ViewResponse::TrackBatch(last) = core.view(&query).unwrap() else {
            panic!("expected track batch");
        };
        assert_eq!(
            last.rows.iter().map(|track| track.id).collect::<Vec<_>>(),
            [3, 4]
        );
        assert_eq!(last.next, None);
        core.command(Command::SetFavorite {
            track_ids: vec![1],
            favorite: true,
        })
        .unwrap();
        assert_eq!(core.view(&query).unwrap_err(), QueryError::RevisionChanged);
    }

    #[test]
    fn batch_errors_distinguish_revision_changes_from_invalid_requests() {
        let (_directory, mut core) = fixture();
        core.command(Command::CreatePlaylistWithTracks {
            name: "mix".into(),
            track_ids: vec![1, 2],
        })
        .unwrap();
        let expected = core.state.query_revisions();
        let queries = [
            Query::TrackBatch {
                query: None,
                favorite: None,
                missing: None,
                after: None,
                limit: 0,
                expected: Some(expected),
            },
            Query::PlaylistSummaryBatch {
                after: None,
                limit: 0,
                expected: Some(expected),
            },
            Query::PlaylistEntryBatch {
                playlist_id: 1,
                after: None,
                limit: 0,
                expected: Some(expected),
            },
        ];
        for query in &queries {
            assert!(matches!(core.view(query), Err(QueryError::Message(_))));
        }
        core.command(Command::RenamePlaylist {
            playlist_id: 1,
            name: "renamed".into(),
        })
        .unwrap();
        // Track traversal ignores playlist-only changes; its invalid limit is
        // still a normal request error. Both playlist lanes reject stale data.
        assert!(matches!(
            core.view(&queries[0]),
            Err(QueryError::Message(_))
        ));
        for query in &queries[1..] {
            assert_eq!(core.view(query).unwrap_err(), QueryError::RevisionChanged);
        }
        core.command(Command::SetFavorite {
            track_ids: vec![1],
            favorite: true,
        })
        .unwrap();
        for query in &queries {
            assert_eq!(core.view(query).unwrap_err(), QueryError::RevisionChanged);
        }
    }

    fn settle_scan(core: &mut Core) -> bool {
        while core.active_scan.is_some() {
            let event = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            core.scan_event(event);
        }
        core.scan_results.back().unwrap().progress.phase == ScanPhase::Completed
    }

    #[test]
    fn scan_start_ack_binds_the_accepted_task() {
        let (directory, mut core) = fixture();
        let path = directory.path().join("1.wav");
        write_pcm(&path);
        let (requests, receiver) = bounded(3);
        let (reply, ack) = bounded(1);
        requests
            .send(Request {
                operation: Operation::Command(Command::Scan {
                    paths: vec![path],
                    force: false,
                }),
                reply: Some(reply),
            })
            .unwrap();
        drop(requests);
        run_until_disconnected(&mut core, receiver);
        let CoreResponse::Ack(ack) = ack.recv().unwrap() else {
            panic!("Expected ack");
        };
        assert!(ack.ok);
        let id = ack
            .scan_id
            .expect("Accepted scans must expose their identity");
        let ViewResponse::ScanTask(task) = core.view(&Query::ScanTask { id }).unwrap() else {
            panic!("Expected task");
        };
        assert_eq!(task.id, id);
    }

    #[test]
    fn completed_scan_wait_is_independent_of_later_scans_and_global_errors() {
        let (directory, mut core) = fixture();
        let path = directory.path().join("1.wav");
        write_pcm(&path);
        core.scan(vec![path.clone()], None, false).unwrap();
        let id = core.active_scan.as_ref().unwrap().status.id;
        assert!(settle_scan(&mut core));
        let ViewResponse::ScanTask(before) = core.view(&Query::ScanTask { id }).unwrap() else {
            panic!("Expected task");
        };
        core.state.system.last_error = Some("Unrelated audio failure".into());
        core.scan(vec![path], None, false).unwrap();
        let ViewResponse::ScanTask(after) = core.view(&Query::ScanTask { id }).unwrap() else {
            panic!("Expected task");
        };
        assert_eq!(before, after);
        assert_ne!(core.active_scan.as_ref().unwrap().status.id, id);
        assert!(settle_scan(&mut core));
    }

    #[test]
    fn scan_lookup_wait_observes_cancel_and_shutdown_without_committing() {
        for shutdown in [false, true] {
            let (directory, mut core) = fixture();
            let path = directory.path().join("new.wav");
            write_pcm(&path);
            let total = core.state.library.track_total;
            core.scan(vec![path], None, false).unwrap();
            let id = core.active_scan.as_ref().unwrap().status.id;
            let lookup = loop {
                let event = core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                if matches!(event, ScanEvent::KnownSource { .. }) {
                    break event;
                }
                core.scan_event(event);
            };
            // Keep the request alive without replying: cancellation must wake the
            // worker even when owner disconnection cannot release its reply wait.
            if shutdown {
                core.shutdown_scan();
            } else {
                core.cancel_scan(id).unwrap();
                assert!(!settle_scan(&mut core));
            }
            drop(lookup);
            assert!(core.active_scan.is_none());
            assert_eq!(core.state.library.track_total, total);
            assert_eq!(
                core.scan_results.back().unwrap().progress.phase,
                ScanPhase::Cancelled
            );
        }
    }

    #[test]
    fn cancelling_finished_but_uncommitted_scan_discards_its_result() {
        let (directory, mut core) = fixture();
        let path = directory.path().join("new.wav");
        write_pcm(&path);
        let total = core.state.library.track_total;
        core.scan(vec![path], None, false).unwrap();
        let id = core.active_scan.as_ref().unwrap().status.id;
        let finished = loop {
            match core.scan_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                ScanEvent::Progress { .. } => {}
                event @ ScanEvent::KnownSource { .. } => core.scan_event(event),
                ScanEvent::Finished(scan) => break scan,
            }
        };
        core.command(Command::CancelScan { id }).unwrap();
        core.scan_event(ScanEvent::Finished(finished));
        assert_eq!(core.state.library.track_total, total);
        assert_eq!(
            core.scan_results.back().unwrap().progress.phase,
            ScanPhase::Cancelled
        );
        assert!(core.active_scan.is_none());
    }
    #[test]
    fn stale_scan_events_cannot_replace_the_active_task() {
        let (directory, mut core) = fixture();
        let path = directory.path().join("1.wav");
        write_pcm(&path);
        core.scan(vec![path], None, false).unwrap();
        let before = core.active_scan.as_ref().unwrap().status.clone();
        let progress = ScanProgress {
            phase: ScanPhase::Failed,
            processed: 999,
            total: None,
            errors: 1,
            path: None,
        };
        core.scan_event(ScanEvent::Progress {
            id: 0,
            progress: progress.clone(),
        });
        core.scan_event(ScanEvent::Finished(ScanFinished {
            id: 0,
            result: Ok(ScanResult::default()),
            import: None,
            progress,
        }));
        let (reply, receive) = bounded(1);
        core.scan_event(ScanEvent::KnownSource {
            id: 0,
            path: directory.path().join("1.wav"),
            reply,
        });
        assert!(receive.recv().unwrap().is_err());
        assert_eq!(core.active_scan.as_ref().unwrap().status, before);
        assert!(core.scan_results.is_empty());
        assert!(settle_scan(&mut core));
    }

    #[test]
    fn scan_terminal_retention_is_bounded_and_unknown_ids_fail() {
        let (directory, mut core) = fixture();
        let path = directory.path().join("1.wav");
        write_pcm(&path);
        for _ in 0..MAX_RETAINED_SCAN_RESULTS + 1 {
            core.scan(vec![path.clone()], None, false).unwrap();
            assert!(settle_scan(&mut core));
        }
        assert_eq!(core.scan_results.len(), MAX_RETAINED_SCAN_RESULTS);
        assert!(core.view(&Query::ScanTask { id: 1 }).is_err());
        assert!(core.view(&Query::ScanTask { id: u64::MAX }).is_err());
        assert!(
            core.view(&Query::ScanTask {
                id: MAX_RETAINED_SCAN_RESULTS as u64 + 1
            })
            .is_ok()
        );
    }

    fn write_pcm(path: &Path) {
        use std::io::Write;
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(b"RIFF").unwrap();
        file.write_all(&236_u32.to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt ").unwrap();
        file.write_all(&16_u32.to_le_bytes()).unwrap();
        file.write_all(&1_u16.to_le_bytes()).unwrap();
        file.write_all(&1_u16.to_le_bytes()).unwrap();
        file.write_all(&7500_u32.to_le_bytes()).unwrap();
        file.write_all(&15000_u32.to_le_bytes()).unwrap();
        file.write_all(&2_u16.to_le_bytes()).unwrap();
        file.write_all(&16_u16.to_le_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&200_u32.to_le_bytes()).unwrap();
        file.write_all(&[0; 200]).unwrap();
    }

    #[test]
    fn lowering_scan_depth_preserves_excluded_tracks_until_they_are_scanned() {
        let (directory, mut core) = fixture();
        let allowed = directory.path().join("allowed");
        let excluded = allowed.join("excluded");
        std::fs::create_dir_all(&excluded).unwrap();
        let visible = allowed.join("visible.wav");
        let hidden = excluded.join("hidden.wav");
        write_pcm(&visible);
        write_pcm(&hidden);
        let mut config = core.state.system.config.as_ref().clone();
        config.scan_max_depth = 2;
        core.command(Command::Configure {
            config: std::sync::Arc::new(config),
        })
        .unwrap();
        core.command(Command::Scan {
            paths: vec![directory.path().to_path_buf()],
            force: false,
        })
        .unwrap();
        assert!(settle_scan(&mut core));
        let hidden_id = core
            .store
            .track_id_for_source(&hidden, None)
            .unwrap()
            .unwrap();
        core.store.set_favorite(&[hidden_id], true).unwrap();
        core.store
            .edit_track(
                hidden_id,
                Some("Kept title"),
                Some("Artist"),
                Some("Album"),
                None,
            )
            .unwrap();
        std::fs::remove_file(&hidden).unwrap();
        let mut config = core.state.system.config.as_ref().clone();
        config.scan_max_depth = 1;
        core.command(Command::Configure {
            config: std::sync::Arc::new(config),
        })
        .unwrap();
        core.command(Command::Scan {
            paths: vec![directory.path().to_path_buf()],
            force: false,
        })
        .unwrap();
        assert!(!settle_scan(&mut core));
        assert!(
            core.state
                .system
                .last_error
                .as_ref()
                .unwrap()
                .contains(excluded.to_str().unwrap())
        );
        let preserved = core.track(hidden_id).unwrap();
        assert!(!preserved.missing);
        assert!(preserved.favorite);
        assert_eq!(preserved.title, "Kept title");
        let visible_id = core
            .store
            .track_id_for_source(&visible, None)
            .unwrap()
            .unwrap();
        assert!(!core.track(visible_id).unwrap().missing);
        let mut config = core.state.system.config.as_ref().clone();
        config.scan_max_depth = 2;
        core.command(Command::Configure {
            config: std::sync::Arc::new(config),
        })
        .unwrap();
        core.command(Command::Scan {
            paths: vec![directory.path().to_path_buf()],
            force: false,
        })
        .unwrap();
        assert!(settle_scan(&mut core));
        let removed = core.track(hidden_id).unwrap();
        assert!(removed.missing);
        assert!(removed.favorite);
        assert_eq!(removed.title, "Kept title");
    }

    #[test]
    fn scan_rejects_when_no_library_roots_are_configured() {
        let (directory, mut core) = fixture();
        Arc::make_mut(&mut core.state.system.config)
            .library_roots
            .clear();
        assert!(
            core.command(Command::Scan {
                paths: vec![directory.path().to_path_buf()],
                force: false,
            })
            .is_err()
        );
        assert!(!core.state.system.scanning);
        assert!(core.active_scan.is_none());
    }

    #[test]
    fn scan_accepts_a_child_of_a_configured_root() {
        let (directory, mut core) = fixture();
        let child = directory.path().join("nested");
        std::fs::create_dir(&child).unwrap();
        core.command(Command::Scan {
            paths: vec![child],
            force: false,
        })
        .unwrap();
        assert!(settle_scan(&mut core));
        assert_eq!(core.store.library_stats().unwrap().total, 4);
    }

    #[test]
    fn scan_rejects_prefix_sibling_of_a_configured_root() {
        let (_directory, mut core) = fixture();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("Music");
        let sibling = parent.path().join("Music-other");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&sibling).unwrap();
        Arc::make_mut(&mut core.state.system.config).library_roots = vec![root];
        assert!(
            core.command(Command::Scan {
                paths: vec![sibling],
                force: false,
            })
            .is_err()
        );
        assert!(core.active_scan.is_none());
    }

    #[test]
    fn scan_accepts_symlinks_to_outside_a_configured_root() {
        let (directory, mut core) = fixture();
        let outside = tempfile::tempdir().unwrap();
        let source = outside.path().join("source.wav");
        write_pcm(&source);
        let link = directory.path().join("outside-link");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let logical = link.join("source.wav");
        core.command(Command::Scan {
            paths: vec![link.clone()],
            force: false,
        })
        .unwrap();
        assert!(settle_scan(&mut core));
        let id = core
            .store
            .track_id_for_source(&logical, None)
            .unwrap()
            .unwrap();
        assert_eq!(core.track(id).unwrap().path, logical);
        assert!(
            core.store
                .track_id_for_source(&source, None)
                .unwrap()
                .is_none()
        );
        let page = core
            .store
            .directory_page(&link, None, None, None, 0, MAX_QUERY_ROWS)
            .unwrap();
        assert_eq!(page.total, 1);
        assert!(matches!(&page.rows[0], DirectoryRow::Track(row) if row.id == id));
    }

    #[test]
    fn scan_keeps_distinct_configured_aliases_of_the_same_directory() {
        let (directory, mut core) = fixture();
        let outside = tempfile::tempdir().unwrap();
        write_pcm(&outside.path().join("source.wav"));
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        for alias in [&first, &second] {
            std::os::unix::fs::symlink(outside.path(), alias).unwrap();
        }
        let mut config = core.state.system.config.as_ref().clone();
        config.library_roots = vec![first.join("."), second.join("unused/..")];
        core.command(Command::Configure {
            config: std::sync::Arc::new(config),
        })
        .unwrap();
        core.command(Command::Scan {
            paths: vec![first.join("unused/.."), second.join(".")],
            force: false,
        })
        .unwrap();
        assert!(settle_scan(&mut core));
        let ids = [&first, &second].map(|alias| {
            let path = alias.join("source.wav");
            let id = core
                .store
                .track_id_for_source(&path, None)
                .unwrap()
                .unwrap();
            assert_eq!(core.track(id).unwrap().path, path);
            assert_eq!(
                core.store
                    .directory_page(alias, None, None, None, 0, MAX_QUERY_ROWS)
                    .unwrap()
                    .total,
                1
            );
            id
        });
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn scan_of_one_root_ignores_an_unavailable_configured_root() {
        let (directory, mut core) = fixture();
        let source = directory.path().join("1.wav");
        write_pcm(&source);
        Arc::make_mut(&mut core.state.system.config)
            .library_roots
            .push(directory.path().join("offline/Music"));
        core.command(Command::Scan {
            paths: vec![source.clone()],
            force: false,
        })
        .unwrap();
        assert!(settle_scan(&mut core));
        assert_eq!(core.track(1).unwrap().path, source);
        assert!(!core.track(1).unwrap().missing);
    }

    #[test]
    fn cue_scan_can_reference_another_configured_root() {
        let (directory, mut core) = fixture();
        let sheets = directory.path().join("sheets");
        let sources = directory.path().join("sources");
        std::fs::create_dir(&sheets).unwrap();
        std::fs::create_dir(&sources).unwrap();
        let source = sources.join("source.wav");
        write_pcm(&source);
        let sheet = sheets.join("album.cue");
        std::fs::write(
            &sheet,
            "FILE ../sources/source.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        )
        .unwrap();
        Arc::make_mut(&mut core.state.system.config).library_roots = vec![sheets, sources];
        core.command(Command::Scan {
            paths: vec![sheet.clone()],
            force: false,
        })
        .unwrap();
        assert!(settle_scan(&mut core));
        let id = core
            .store
            .track_id_for_source(&sheet, Some(1))
            .unwrap()
            .unwrap();
        let track = core.track(id).unwrap();
        assert_eq!(track.path, source);
        assert_eq!(track.cue.as_ref().unwrap().sheet, sheet);
    }

    #[test]
    fn scan_rejects_a_logical_parent_escape_even_when_the_path_exists() {
        let (_directory, mut core) = fixture();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("Music");
        std::fs::create_dir(&root).unwrap();
        Arc::make_mut(&mut core.state.system.config).library_roots = vec![root.clone()];
        assert!(
            core.command(Command::Scan {
                paths: vec![root.join("..")],
                force: false,
            })
            .is_err()
        );
        assert!(core.active_scan.is_none());
    }

    #[test]
    fn missing_and_broken_requests_do_not_prevent_other_roots_from_scanning() {
        let (directory, mut core) = fixture();
        let outside = tempfile::tempdir().unwrap();
        let missing = outside.path().join("offline");
        let broken = directory.path().join("broken-link");
        std::os::unix::fs::symlink(&missing, &broken).unwrap();
        let source = directory.path().join("1.wav");
        write_pcm(&source);
        Arc::make_mut(&mut core.state.system.config)
            .library_roots
            .push(missing.clone());
        core.command(Command::Scan {
            paths: vec![missing, broken, source.clone()],
            force: false,
        })
        .unwrap();
        assert!(!settle_scan(&mut core));
        assert!(core.state.system.last_error.is_some());
        let track = core.track(1).unwrap();
        assert_eq!(track.path, source);
        assert!(!track.missing);
        assert!(track.duration.unwrap() < 1.0);
    }

    #[test]
    fn playlist_import_and_directory_queries_preserve_configured_aliases() {
        let (directory, mut core) = fixture();
        let outside = tempfile::tempdir().unwrap();
        let source = outside.path().join("source.wav");
        write_pcm(&source);
        std::fs::write(
            outside.path().join("playlist.m3u"),
            "./unused/../source.wav\n",
        )
        .unwrap();
        let alias = directory.path().join("Music");
        std::os::unix::fs::symlink(outside.path(), &alias).unwrap();
        let mut config = core.state.system.config.as_ref().clone();
        config.library_roots = vec![alias.join("unused/.."), outside.path().to_path_buf()];
        core.command(Command::Configure {
            config: std::sync::Arc::new(config),
        })
        .unwrap();
        assert_eq!(
            core.state.system.config.library_roots,
            [alias.clone(), outside.path().to_path_buf()]
        );
        core.command(Command::Scan {
            paths: vec![source.clone()],
            force: false,
        })
        .unwrap();
        assert!(settle_scan(&mut core));
        let physical_id = core
            .store
            .track_id_for_source(&source, None)
            .unwrap()
            .unwrap();
        core.command(Command::ImportPlaylist {
            path: alias.join("playlist.m3u"),
            name: Some("Alias playlist".into()),
        })
        .unwrap();
        assert!(settle_scan(&mut core));
        let logical = alias.join("source.wav");
        let logical_id = core
            .store
            .track_id_for_source(&logical, None)
            .unwrap()
            .unwrap();
        assert_ne!(logical_id, physical_id);
        let playlist_id = core.store.playlist_summary_page(0, 1).unwrap().rows[0].id;
        let entries = core
            .store
            .playlist_entries_page(playlist_id, 0, MAX_QUERY_ROWS)
            .unwrap();
        assert_eq!(entries.rows.len(), 1);
        assert_eq!(entries.rows[0].track_id, logical_id);
        let page = core
            .store
            .directory_page(
                &alias.join("unused/.."),
                None,
                None,
                None,
                0,
                MAX_QUERY_ROWS,
            )
            .unwrap();
        assert_eq!(page.total, 1);
        assert!(matches!(&page.rows[0], DirectoryRow::Track(row) if row.id == logical_id));
        core.enqueue(&[logical_id]).unwrap();
        let entries = core.store.queue_page(0, MAX_QUERY_ROWS).unwrap().rows;
        let entry = entries.last().unwrap();
        assert_eq!(entry.track_id, logical_id);
        assert_eq!(core.track(entry.track_id).unwrap().path, logical);
    }

    #[test]
    fn mixed_inside_and_outside_scan_is_rejected_atomically() {
        let (directory, mut core) = fixture();
        let inside = directory.path().join("inside");
        std::fs::create_dir(&inside).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let total = core.state.library.track_total;
        assert!(
            core.command(Command::Scan {
                paths: vec![inside, outside.path().to_path_buf()],
                force: false,
            })
            .is_err()
        );
        assert_eq!(core.state.library.track_total, total);
        assert!(!core.state.system.scanning);
        assert!(core.active_scan.is_none());
    }

    fn pending(core: &mut Core, queue_id: u64) {
        core.store.set_queue_current(Some(queue_id)).unwrap();

        core.refresh_queue_tracks().unwrap();
        core.state.playback.status = PlaybackStatus::Playing;
        let (track_id, duration) = {
            let track = core.state.current_track().unwrap();
            (track.id, track.duration)
        };
        core.state.playback.duration = duration;
        core.playback = Some(PlaybackStats {
            track_id,
            source: core
                .playback_source(core.state.current_track().unwrap())
                .unwrap(),
            heard: 0.0,
            position: None,
            started: false,
            counted: false,
            last_heard_at: None,
            activity_updated: false,
            paused_at: None,
        });
    }

    fn started_event(core: &Core, duration: Option<f64>) -> AudioEvent {
        let track = core.state.current_track().unwrap();
        AudioEvent::Started {
            generation: core.generation,
            info: MediaInfo {
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                duration,
                codec: track.codec.clone(),
                channels: track.channels,
                sample_rate: track.sample_rate,
                bitrate_bps: None,
                bitrate_exact: false,
                track_number: None,
                disc_number: None,
                bits_per_sample: None,
                release_date: None,
            },
        }
    }

    fn started(core: &mut Core, duration: Option<f64>) -> Result<()> {
        core.audio_event(started_event(core, duration))
    }

    fn playing(core: &mut Core, queue_id: u64) {
        pending(core, queue_id);
        started(core, Some(10.0)).unwrap();
    }

    fn progress(core: &mut Core, position_seconds: f64, listened_seconds: f64) {
        core.audio_event(AudioEvent::Progress {
            generation: core.generation,
            position_seconds,
            listened_seconds,
        })
        .unwrap();
    }

    fn play_count(core: &Core, track_id: i64) -> u64 {
        core.store.track(track_id).unwrap().unwrap().play_count
    }

    fn missing(core: &mut Core, command: Command, track_id: i64) {
        let path = core.track(track_id).unwrap().path.clone();
        let error = core.command(command).unwrap_err();
        assert!(format!("{error:#}").contains(&path.display().to_string()));
    }

    fn precision_pending(core: &mut Core) {
        let track = core.store.track(1).unwrap().unwrap();
        std::fs::write(&track.path, b"encoded source").unwrap();
        let metadata = std::fs::metadata(&track.path).unwrap();
        let mut media = core
            .store
            .known_source(&track.path)
            .unwrap()
            .unwrap()
            .media
            .unwrap();
        media.bitrate_bps = Some(128_000);
        media.bitrate_exact = false;
        core.store
            .apply_scan(&ScanResult {
                records: vec![ScanRecord {
                    path: track.path,
                    cue: None,
                    size: metadata.len(),
                    modified_ns: metadata
                        .modified()
                        .unwrap()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_nanos() as i64,
                    fingerprint: None,
                    media,
                }],
                ..ScanResult::default()
            })
            .unwrap();
        core.store.clear_queue().unwrap();
        core.reload_queue().unwrap();
        core.enqueue(&[1]).unwrap();
        let queue_id = core.queue_ids[0];
        pending(core, queue_id);
        assert!(core.playback.as_ref().unwrap().source.is_some());
    }

    #[test]
    fn playback_started_upgrades_estimate_and_duplicate_cannot_downgrade() {
        let (_directory, mut core) = fixture();
        precision_pending(&mut core);
        let mut event = started_event(&core, Some(10.0));
        if let AudioEvent::Started { info, .. } = &mut event {
            info.bitrate_bps = Some(128_000);
            info.bitrate_exact = true;
        }
        let before = core.state.library.revision;
        core.audio_event(event).unwrap();
        assert!(core.store.track(1).unwrap().unwrap().bitrate_exact);
        assert!(core.state.current_track().unwrap().bitrate_exact);
        assert!(
            core.store.queue_page(0, 10).unwrap().rows[0]
                .track
                .bitrate_exact
        );
        // The precision update shares Started's existing content revision.
        assert_eq!(core.state.library.revision, before.wrapping_add(1));
        let revision = core.state.library.revision;
        let mut duplicate = started_event(&core, Some(10.0));
        if let AudioEvent::Started { info, .. } = &mut duplicate {
            info.bitrate_bps = Some(64_000);
        }
        core.audio_event(duplicate).unwrap();
        assert_eq!(core.state.library.revision, revision);
        assert_eq!(
            core.store.track(1).unwrap().unwrap().bitrate_bps,
            Some(128_000)
        );
    }

    #[test]
    fn stale_playback_start_cannot_upgrade_precision() {
        let (_directory, mut core) = fixture();
        precision_pending(&mut core);
        let revision = core.state.library.revision;
        let mut event = started_event(&core, Some(10.0));
        if let AudioEvent::Started { generation, info } = &mut event {
            *generation = generation.wrapping_sub(1);
            info.bitrate_bps = Some(192_000);
            info.bitrate_exact = true;
        }
        core.audio_event(event).unwrap();
        assert_eq!(core.state.library.revision, revision);
        assert!(!core.playback.as_ref().unwrap().started);
        assert!(!core.store.track(1).unwrap().unwrap().bitrate_exact);
        assert_eq!(
            core.store.track(1).unwrap().unwrap().bitrate_bps,
            Some(128_000)
        );
    }

    #[test]
    fn full_eof_upgrades_estimate_but_stale_and_stopped_events_do_not() {
        for drain in [false, true] {
            let (_directory, mut core) = fixture();
            precision_pending(&mut core);
            started(&mut core, Some(10.0)).unwrap();
            let generation = core.generation;
            core.audio_event(AudioEvent::Ended {
                generation: generation.wrapping_sub(1),
                bitrate_bps: Some(192_000),
            })
            .unwrap();
            assert!(!core.store.track(1).unwrap().unwrap().bitrate_exact);
            if drain {
                let (send, receive) = bounded(1);
                core.engine.events = receive;
                send.send(AudioEvent::Ended {
                    generation,
                    bitrate_bps: Some(192_000),
                })
                .unwrap();
                core.finish_playback(Ok(None), false, Ok(())).unwrap();
            } else {
                core.audio_event(AudioEvent::Ended {
                    generation,
                    bitrate_bps: Some(192_000),
                })
                .unwrap();
            }
            let exact = core.store.track(1).unwrap().unwrap();
            assert_eq!(exact.bitrate_bps, Some(192_000));
            assert!(exact.bitrate_exact);
            let revision = core.state.library.revision;
            core.audio_event(AudioEvent::Ended {
                generation,
                bitrate_bps: Some(256_000),
            })
            .unwrap();
            assert_eq!(core.state.library.revision, revision);
            assert_eq!(core.store.track(1).unwrap().unwrap(), exact);
        }
        let (_directory, mut core) = fixture();
        precision_pending(&mut core);
        started(&mut core, Some(10.0)).unwrap();
        let generation = core.generation;
        core.finish_playback(Ok(None), false, Ok(())).unwrap();
        core.audio_event(AudioEvent::Ended {
            generation,
            bitrate_bps: Some(192_000),
        })
        .unwrap();
        assert!(!core.store.track(1).unwrap().unwrap().bitrate_exact);
    }

    #[test]
    fn same_track_restart_retains_queued_started_and_eof_bitrate_upgrades() {
        for eof in [false, true] {
            let (_directory, mut core) = fixture();
            precision_pending(&mut core);
            let queue_id = core.queue_ids[0];
            let generation = core.generation;
            let event = if eof {
                started(&mut core, Some(10.0)).unwrap();
                AudioEvent::Ended {
                    generation,
                    bitrate_bps: Some(192_000),
                }
            } else {
                let mut event = started_event(&core, Some(10.0));
                if let AudioEvent::Started { info, .. } = &mut event {
                    info.bitrate_bps = Some(192_000);
                    info.bitrate_exact = true;
                }
                event
            };
            let _commands = recording_audio(&mut core);
            let (sender, events) = bounded(1);
            core.engine.events = events;
            sender.send(event).unwrap();

            core.start(queue_id, false).unwrap();
            assert_eq!(core.generation, generation.wrapping_add(1));
            assert!(!core.playback.as_ref().unwrap().started);
            assert!(core.playback.as_ref().unwrap().source.is_none());

            // A new decoder estimate must not replace the retiring worker's upgrade.
            let mut event = started_event(&core, Some(10.0));
            if let AudioEvent::Started { info, .. } = &mut event {
                info.bitrate_bps = Some(64_000);
            }
            for event in [None, Some(event)] {
                if let Some(event) = event {
                    core.audio_event(event).unwrap();
                }
                core.publish();
                let stored = core.store.track(1).unwrap().unwrap();
                assert_eq!(stored.bitrate_bps, Some(192_000));
                assert!(stored.bitrate_exact);
                let state = core.shared.read();
                assert_eq!(state.playback.status, PlaybackStatus::Playing);
                assert_eq!(state.queue.current_id, Some(queue_id));
                assert_eq!(state.current_track().unwrap(), &stored);
                let page = core.store.queue_page(0, 1).unwrap();
                assert_eq!(page.rows[0].track.bitrate_bps, stored.bitrate_bps);
                assert_eq!(page.rows[0].track.bitrate_exact, stored.bitrate_exact);
            }
        }
    }

    #[test]
    fn playback_bitrate_runtime_replacement_and_optional_failure_do_not_interrupt_start() {
        for replace in [false, true] {
            let (_directory, mut core) = fixture();
            precision_pending(&mut core);
            if replace {
                let path = core.state.current_track().unwrap().path.clone();
                std::fs::write(path, b"different encoded source with different size").unwrap();
            }
            let mut event = started_event(&core, Some(10.0));
            if let AudioEvent::Started { info, .. } = &mut event {
                info.bitrate_bps = Some(if replace { 192_000 } else { u64::MAX });
                info.bitrate_exact = true;
            }
            core.audio_event(event).unwrap();
            assert!(core.playback.as_ref().unwrap().started);
            assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
            let track = core.store.track(1).unwrap().unwrap();
            assert_eq!(track.bitrate_bps, Some(128_000));
            assert!(!track.bitrate_exact);
        }
    }

    #[test]
    fn successful_start_records_recent_once_and_twenty_percent_is_strict() {
        let (_directory, mut core) = fixture();
        pending(&mut core, 1);
        assert!(core.store.history(200).unwrap().is_empty());
        started(&mut core, Some(10.0)).unwrap();
        assert_eq!(core.store.history(200).unwrap()[0].track_id, 1);
        assert_eq!(play_count(&core, 1), 0);

        // A duplicate Started must not refresh last-played a second time.
        core.store.mark_played(1, 7).unwrap();
        started(&mut core, Some(10.0)).unwrap();
        assert_eq!(core.store.history(200).unwrap()[0].played_at, 7);
        progress(&mut core, 9.0, 2.0);
        progress(&mut core, 9.0, f64::NAN);
        progress(&mut core, 9.0, f64::INFINITY);
        assert_eq!(play_count(&core, 1), 0);
        progress(&mut core, 9.0, 2.000_001);
        assert_eq!(play_count(&core, 1), 1);
        assert_eq!(core.state.current_track().unwrap().play_count, 1);
        assert_eq!(
            core.store.queue_page(0, 1).unwrap().rows[0]
                .track
                .play_count,
            1
        );
        progress(&mut core, 9.5, 3.0);
        assert_eq!(play_count(&core, 1), 1);
        assert_eq!(core.state.current_track().unwrap().play_count, 1);
        assert_eq!(
            core.store.queue_page(0, 1).unwrap().rows[0]
                .track
                .play_count,
            1
        );
    }
    #[test]
    fn reload_refreshes_current_track_activity_and_probe_metadata() {
        let (directory, mut core) = fixture();
        core.store.set_queue_current(Some(1)).unwrap();
        core.refresh_queue_tracks().unwrap();
        core.store.mark_played(1, 7).unwrap();
        core.reload(false).unwrap();
        assert_eq!(core.state.current_track().unwrap().last_played, Some(7));

        let known = core
            .store
            .stored_scan_files(&[directory.path().to_path_buf()])
            .unwrap()
            .into_iter()
            .find(|file| file.track_id == 1)
            .unwrap();
        let mut media = known.media.unwrap();
        media.codec = "updated-codec".into();
        media.sample_rate = 96_000;
        core.store
            .apply_scan(&ScanResult {
                roots: vec![directory.path().to_path_buf()],
                suppressed_sources: Vec::new(),
                skipped_directories: Vec::new(),
                skipped_directories_truncated: false,
                scan_incomplete: true,
                uncertainties: Vec::new(),
                records: vec![ScanRecord {
                    path: known.path,
                    cue: known.cue,
                    size: known.size,
                    modified_ns: known.modified_ns + 1,
                    fingerprint: known.fingerprint,
                    media,
                }],
                errors: Vec::new(),
            })
            .unwrap();
        core.reload(true).unwrap();
        let current = core.state.current_track().unwrap();
        assert_eq!(current.codec, "updated-codec");
        assert_eq!(current.sample_rate, 96_000);
    }

    #[test]
    fn configured_threshold_controls_counting_including_zero_percent() {
        for percent in [0.0, 50.0, 99.0] {
            let (_directory, mut core) = fixture();
            Arc::make_mut(&mut core.state.system.config).play_count_threshold_percent = percent;
            playing(&mut core, 1);
            let boundary = 10.0 * percent / 100.0;
            progress(&mut core, 10.0, boundary);
            assert_eq!(play_count(&core, 1), 0);
            progress(&mut core, 10.0, boundary + 0.000_001);
            assert_eq!(play_count(&core, 1), 1);
        }
    }

    #[test]
    fn repeated_progress_pause_seek_and_resume_count_only_once() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        core.store.mark_played(1, 7).unwrap();
        progress(&mut core, 1.0, 1.0);
        core.command(Command::Pause).unwrap();
        progress(&mut core, 1.0, 1.0);
        core.command(Command::Seek { seconds: 9.0 }).unwrap();
        assert_eq!(play_count(&core, 1), 0);
        core.command(Command::Resume).unwrap();
        progress(&mut core, 9.5, 1.5);
        core.command(Command::Seek { seconds: 0.0 }).unwrap();
        progress(&mut core, 0.5, 2.0);
        assert_eq!(play_count(&core, 1), 0);
        progress(&mut core, 1.0, 2.5);
        for _ in 0..3 {
            core.command(Command::Pause).unwrap();
            core.command(Command::Resume).unwrap();
            core.command(Command::Seek { seconds: 5.0 }).unwrap();
            progress(&mut core, 5.0, 2.0);
            progress(&mut core, 8.0, 8.0);
        }
        let final_activity = core.state.playback.last_heard_at.unwrap();
        core.stop().unwrap();
        assert_eq!(play_count(&core, 1), 1);
        assert_eq!(
            core.store.history(200).unwrap()[0].played_at,
            final_activity
        );
    }

    #[test]
    fn paused_confirmations_do_not_advance_activity_at_stop() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        progress(&mut core, 2.0, 2.0);
        let heard_at = core.state.playback.last_heard_at;
        core.command(Command::Pause).unwrap();
        let pause_bound = core.playback.as_ref().unwrap().paused_at.unwrap();
        progress(&mut core, 20.0, 20.0);
        assert!(core.state.playback.last_heard_at.unwrap() <= pause_bound);
        assert_eq!(core.playback.as_ref().unwrap().heard, 20.0);
        assert_eq!(play_count(&core, 1), 1);
        let generation = core.generation;
        core.finish_playback(Ok(Some((generation, 20.0))), false, Ok(()))
            .unwrap();
        assert_eq!(
            core.store.history(200).unwrap()[0].played_at,
            core.state
                .playback
                .last_heard_at
                .unwrap_or(heard_at.unwrap())
        );
    }

    #[test]
    fn failed_load_and_stale_events_neither_count_nor_record_recent() {
        let (_directory, mut core) = fixture();
        pending(&mut core, 1);
        let generation = core.generation;
        let track = core.state.current_track().unwrap();
        core.audio_event(AudioEvent::Started {
            generation: generation.wrapping_sub(1),
            info: MediaInfo {
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                duration: Some(10.0),
                codec: track.codec.clone(),
                channels: track.channels,
                sample_rate: track.sample_rate,
                bitrate_bps: None,
                bitrate_exact: false,
                track_number: None,
                disc_number: None,
                bits_per_sample: None,
                release_date: None,
            },
        })
        .unwrap();
        progress(&mut core, 10.0, 10.0);
        core.audio_event(AudioEvent::Failed {
            generation,
            message: "Decoder failed".into(),
        })
        .unwrap();
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert_eq!(
            core.state.system.last_error.as_deref(),
            Some("Decoder failed")
        );
        assert_ne!(core.generation, generation);
        core.audio_event(AudioEvent::Progress {
            generation,
            position_seconds: 10.0,
            listened_seconds: 10.0,
        })
        .unwrap();
        assert_eq!(core.state.playback.position, 0.0);
        assert_eq!(play_count(&core, 1), 0);
        assert!(core.store.history(200).unwrap().is_empty());
    }

    #[test]
    fn final_stop_snapshot_counts_only_matching_generation_above_boundary() {
        for (heard, stale, expected) in [(2.0, false, 0), (2.01, false, 1), (10.0, true, 0)] {
            let (_directory, mut core) = fixture();
            playing(&mut core, 1);
            progress(&mut core, 2.0, 2.0);
            let generation = core.generation;
            let snapshot_generation = if stale { generation - 1 } else { generation };
            core.finish_playback(Ok(Some((snapshot_generation, heard))), false, Ok(()))
                .unwrap();
            assert_eq!(play_count(&core, 1), expected);
            assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
            assert_ne!(core.generation, generation);
            assert!(core.playback.is_none());
            core.audio_event(AudioEvent::Progress {
                generation,
                position_seconds: 10.0,
                listened_seconds: 10.0,
            })
            .unwrap();
            assert_eq!(play_count(&core, 1), expected);
        }
    }

    #[test]
    fn stop_ack_records_queued_start_and_snapshot_without_terminal_navigation() {
        for (heard, expected) in [(2.0, 0), (2.01, 1)] {
            let (_directory, mut core) = fixture();
            pending(&mut core, 1);
            let generation = core.generation;
            let (sender, events) = bounded(8);
            core.engine.events = events;
            sender.send(started_event(&core, Some(10.0))).unwrap();
            sender.send(started_event(&core, Some(10.0))).unwrap();
            sender
                .send(AudioEvent::Progress {
                    generation,
                    position_seconds: 9.0,
                    listened_seconds: 2.0,
                })
                .unwrap();
            sender
                .send(AudioEvent::Progress {
                    generation: generation - 1,
                    position_seconds: 10.0,
                    listened_seconds: 10.0,
                })
                .unwrap();
            sender
                .send(AudioEvent::Ended {
                    generation,
                    bitrate_bps: None,
                })
                .unwrap();
            sender
                .send(AudioEvent::Failed {
                    generation,
                    message: "Retired failure".into(),
                })
                .unwrap();
            sender
                .send(AudioEvent::DecoderStopped { generation })
                .unwrap();
            core.finish_playback(Ok(Some((generation, heard))), false, Ok(()))
                .unwrap();
            assert_eq!(play_count(&core, 1), expected);
            assert_eq!(core.store.history(200).unwrap().len(), 1);
            assert_eq!(core.store.history(200).unwrap()[0].track_id, 1);
            assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
            assert_eq!(core.state.queue.current_id, Some(1));
            assert_eq!(core.state.playback.position, 0.0);
            assert!(core.state.system.last_error.is_none());
            assert_eq!(core.generation, generation.wrapping_add(1));
            assert!(core.playback.is_none());
        }
    }

    #[test]
    fn queued_start_rejection_fences_once_without_recursive_stop_or_count() {
        let (_directory, mut core) = fixture();
        pending(&mut core, 1);
        let generation = core.generation;
        let (sender, events) = bounded(4);
        core.engine.events = events;
        sender.send(started_event(&core, Some(10.0))).unwrap();
        sender
            .send(AudioEvent::Progress {
                generation,
                position_seconds: 10.0,
                listened_seconds: 10.0,
            })
            .unwrap();
        sender
            .send(AudioEvent::Ended {
                generation,
                bitrate_bps: None,
            })
            .unwrap();
        core.store.remove_tracks(&[1]).unwrap();
        assert!(
            core.finish_playback(Ok(Some((generation, 10.0))), false, Ok(()))
                .is_err()
        );
        assert_eq!(core.generation, generation.wrapping_add(1));
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert!(core.playback.is_none());
        while let Ok(event) = core.engine.events.try_recv() {
            core.audio_event(event).unwrap();
        }
        assert!(core.store.history(200).unwrap().is_empty());
        assert_eq!(play_count(&core, 2), 0);
    }

    #[test]
    fn database_optimization_requires_stopped_playback_and_no_scan() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        core.state.system.database_optimization = Some(DatabaseOptimization {
            database_bytes_before: 1,
            database_bytes_after: 1,
            wal_bytes_before: 1,
            wal_bytes_after: 0,
        });
        assert!(core.command(Command::OptimizeDatabase).is_err());
        assert!(core.state.system.database_optimization.is_none());
        core.command(Command::Pause).unwrap();
        assert!(core.command(Command::OptimizeDatabase).is_err());
        core.stop().unwrap();
        core.state.system.scanning = true;
        assert!(core.command(Command::OptimizeDatabase).is_err());
        core.state.system.scanning = false;
        core.enqueue(&[4]).unwrap();
        core.command(Command::OptimizeDatabase).unwrap();
        assert!(core.state.system.database_optimization.is_some());
        assert_eq!(core.store.history(200).unwrap()[0].track_id, 1);
        assert_eq!(play_count(&core, 1), 0);
    }

    #[test]
    fn favorite_updates_preserve_counts_and_recent_history() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        progress(&mut core, 3.0, 3.0);
        let played_at = core.store.history(200).unwrap()[0].played_at;
        core.command(Command::SetFavorite {
            track_ids: vec![1, 2],
            favorite: true,
        })
        .unwrap();
        assert!(core.track(1).unwrap().favorite);
        assert!(core.track(2).unwrap().favorite);
        assert!(!core.track(3).unwrap().favorite);
        core.command(Command::SetFavorite {
            track_ids: vec![1],
            favorite: false,
        })
        .unwrap();
        assert!(!core.track(1).unwrap().favorite);
        assert!(core.track(2).unwrap().favorite);
        assert_eq!(play_count(&core, 1), 1);
        assert_eq!(core.store.history(200).unwrap()[0].played_at, played_at);
        assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    }

    #[test]
    fn removing_missing_tracks_reuses_queue_playlist_and_recent_cleanup_without_disk_deletes() {
        let (_directory, mut core) = fixture();
        let kept_path = core.track(1).unwrap().path.clone();
        std::fs::write(&kept_path, b"Do not delete").unwrap();
        let playlist_id = core.store.create_playlist("Mixed availability").unwrap();
        core.store.add_playlist(playlist_id, &[1, 2]).unwrap();
        core.store
            .apply_scan(&ScanResult {
                roots: vec![core.track(2).unwrap().path.clone()],
                ..ScanResult::default()
            })
            .unwrap();
        core.reload(true).unwrap();
        playing(&mut core, 2);
        core.played = vec![1, 2, 3];
        core.played_cursor = 2;
        core.command(Command::RemoveMissingTracks).unwrap();
        assert_eq!(
            core.store
                .library_view_page(None, None, None, LibrarySort::Id, 0, MAX_QUERY_ROWS)
                .unwrap()
                .rows
                .iter()
                .map(|track| track.id)
                .collect::<Vec<_>>(),
            [1, 3, 4]
        );
        assert_eq!(core.queue_ids, [1, 4, 5]);
        assert_eq!(
            core.store
                .playlist_entries_page(playlist_id, 0, MAX_QUERY_ROWS)
                .unwrap()
                .rows
                .iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            [1]
        );
        assert_eq!(core.played, [1]);
        assert_eq!(core.played_cursor, 1);
        assert_eq!(core.state.queue.current_id, None);
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert!(core.state.library.history.is_empty());
        assert_eq!(std::fs::read(&kept_path).unwrap(), b"Do not delete");
    }

    #[test]
    fn forced_scan_reprobes_unchanged_files_instead_of_reusing_cached_metadata() {
        let (_directory, mut core) = fixture();
        let path = core.track(1).unwrap().path.clone();
        write_pcm(&path);
        let mut scanned = library::scan_paths(
            std::slice::from_ref(&path),
            |_| Ok(None),
            false,
            &[_directory.path().to_path_buf()],
            core.state.system.config.scan_max_depth,
            &library::ScanCancellation::default(),
            |_, _, _, _, _| {},
        )
        .unwrap();
        let probed_title = scanned.records[0].media.title.clone();
        scanned.records[0].media.title = "Cached metadata".into();
        core.store.apply_scan(&scanned).unwrap();
        core.reload(true).unwrap();
        for (force, expected) in [(false, "Cached metadata"), (true, probed_title.as_str())] {
            core.command(Command::Scan {
                paths: vec![path.clone()],
                force,
            })
            .unwrap();
            assert!(settle_scan(&mut core));
            assert_eq!(core.track(1).unwrap().title, expected);
        }
    }

    #[test]
    fn stale_events_cannot_count_or_stop_new_playback() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        let generation = core.generation;
        core.stop().unwrap();
        playing(&mut core, 2);
        core.audio_event(AudioEvent::Progress {
            generation,
            position_seconds: 10.0,
            listened_seconds: 10.0,
        })
        .unwrap();
        core.audio_event(AudioEvent::Ended {
            generation,
            bitrate_bps: None,
        })
        .unwrap();
        core.audio_event(AudioEvent::Failed {
            generation,
            message: "Old failure".into(),
        })
        .unwrap();
        assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
        assert_eq!(core.state.playback.position, 0.0);
        assert!(core.state.system.last_error.is_none());
        assert_eq!(play_count(&core, 1), 0);
        assert_eq!(play_count(&core, 2), 0);
        progress(&mut core, 3.0, 3.0);
        assert_eq!(play_count(&core, 2), 1);
    }

    #[test]
    fn started_uses_valid_decoder_duration_or_keeps_known_metadata_duration() {
        for duration in [
            None,
            Some(f64::NAN),
            Some(0.0),
            Some(f64::INFINITY),
            Some(5.0),
        ] {
            let (_directory, mut core) = fixture();
            pending(&mut core, 1);
            started(&mut core, duration).unwrap();
            let expected_duration = if duration == Some(5.0) { 5.0 } else { 10.0 };
            assert_eq!(core.state.playback.duration, Some(expected_duration));
            let boundary = expected_duration * 20.0 / 100.0;
            progress(&mut core, expected_duration, boundary);
            assert_eq!(play_count(&core, 1), 0);
            progress(&mut core, expected_duration, boundary + 0.000_001);
            assert_eq!(play_count(&core, 1), 1);
        }
    }

    #[test]
    fn rejected_recent_update_stops_without_counting_the_start() {
        let (_directory, mut core) = fixture();
        pending(&mut core, 1);
        let generation = core.generation;
        core.store.remove_tracks(&[1]).unwrap();
        assert!(started(&mut core, Some(10.0)).is_err());
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert_ne!(core.generation, generation);
        assert!(core.playback.is_none());
        assert!(core.store.history(200).unwrap().is_empty());
        assert_eq!(play_count(&core, 2), 0);
    }

    #[test]
    fn unknown_duration_waits_for_natural_eof_and_uses_confirmed_position() {
        for (heard, expected) in [(2.0, 0), (2.01, 1), (8.0, 1)] {
            let (_directory, mut core) = fixture_with_duration(None);
            pending(&mut core, 1);
            started(&mut core, None).unwrap();
            progress(&mut core, 10.0, heard);
            assert_eq!(play_count(&core, 1), 0);
            let generation = core.generation;
            assert!(
                core.audio_event(AudioEvent::Ended {
                    generation,
                    bitrate_bps: None
                })
                .is_err()
            );
            assert_eq!(play_count(&core, 1), expected);
            assert_eq!(core.state.playback.duration, Some(10.0));
            assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        }
    }

    #[test]
    fn unknown_duration_never_uses_seek_targets_heard_time_or_manual_stop() {
        let (_directory, mut core) = fixture_with_duration(None);
        pending(&mut core, 1);
        started(&mut core, None).unwrap();
        core.command(Command::Seek { seconds: 10.0 }).unwrap();
        progress(&mut core, f64::NAN, 8.0);
        let generation = core.generation;
        assert!(
            core.audio_event(AudioEvent::Ended {
                generation,
                bitrate_bps: None
            })
            .is_err()
        );
        assert_eq!(play_count(&core, 1), 0);
        assert_eq!(core.state.playback.duration, None);

        pending(&mut core, 1);
        started(&mut core, None).unwrap();
        progress(&mut core, 10.0, 8.0);
        core.stop().unwrap();
        assert_eq!(play_count(&core, 1), 0);
    }

    #[test]
    fn stopped_decoder_final_progress_counts_once_and_fences_late_events() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        progress(&mut core, 2.0, 2.0);
        progress(&mut core, 2.1, 2.1);
        let generation = core.generation;
        core.audio_event(AudioEvent::DecoderStopped { generation })
            .unwrap();
        core.audio_event(AudioEvent::Progress {
            generation,
            position_seconds: 10.0,
            listened_seconds: 10.0,
        })
        .unwrap();
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert_eq!(core.state.playback.position, 0.0);
        assert!(core.playback.is_none());
        assert_eq!(play_count(&core, 1), 1);
    }

    #[test]
    fn m3u_playlist_deduplicates_in_first_occurrence_order_without_deduplicating_queue() {
        let (directory, mut core) = fixture();
        for track_id in [1, 2, 3] {
            write_pcm(&core.track(track_id).unwrap().path);
        }
        let path = directory.path().join("duplicates.m3u");
        std::fs::write(&path, "2.wav\n1.wav\n2.wav\n3.wav\n1.wav\n").unwrap();
        core.command(Command::ImportPlaylist {
            path,
            name: Some("First occurrences".into()),
        })
        .unwrap();
        assert!(settle_scan(&mut core));
        let playlist_id = core.store.playlist_summary_page(0, 1).unwrap().rows[0].id;
        assert_eq!(
            core.store
                .playlist_entries_page(playlist_id, 0, MAX_QUERY_ROWS)
                .unwrap()
                .rows
                .iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            [2, 1, 3]
        );
        core.command(Command::Enqueue {
            track_ids: vec![2, 1, 2, 3, 1],
        })
        .unwrap();
        assert_eq!(
            core.store
                .queue_page(5, MAX_QUERY_ROWS)
                .unwrap()
                .rows
                .iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            [2, 1, 2, 3, 1]
        );
    }

    #[test]
    fn cue_playlist_import_deduplicates_segments_in_first_occurrence_order() {
        let (directory, mut core) = fixture();
        let sheet = directory.path().join("album.cue");
        std::fs::write(&sheet, "FILE album.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:05:00\n").unwrap();
        let items = library::import_playlist(&sheet).unwrap();
        let records = (1..=2)
            .map(|number| ScanRecord {
                path: directory.path().join("album.wav"),
                cue: Some(CueSegment {
                    sheet: sheet.clone(),
                    number,
                    start_frame: u64::from(number - 1) * 375,
                    end_frame: if number == 1 { Some(375) } else { None },
                }),
                size: 1,
                modified_ns: 1,
                fingerprint: None,
                media: MediaInfo {
                    title: format!("Part {number}"),
                    artist: String::new(),
                    album: "Album".into(),
                    duration: Some(5.0),
                    codec: "pcm".into(),
                    channels: 2,
                    sample_rate: 48_000,
                    bitrate_bps: None,
                    bitrate_exact: false,
                    track_number: None,
                    disc_number: None,
                    bits_per_sample: None,
                    release_date: None,
                },
            })
            .collect();
        core.finish_scan(ScanFinished {
            id: 0,
            result: Ok(ScanResult {
                roots: vec![sheet],
                records,
                ..ScanResult::default()
            }),
            import: Some((
                "CUE occurrences".into(),
                vec![items[1].clone(), items[0].clone(), items[1].clone()],
            )),
            progress: ScanProgress {
                phase: ScanPhase::ReadingMetadata,
                processed: 1,
                total: Some(1),
                errors: 0,
                path: None,
            },
        })
        .unwrap();
        let playlist_id = core.store.playlist_summary_page(0, 1).unwrap().rows[0].id;
        let entries = core
            .store
            .playlist_entries_page(playlist_id, 0, MAX_QUERY_ROWS)
            .unwrap()
            .rows;
        let tracks: Vec<_> = entries
            .iter()
            .map(|entry| core.track(entry.track_id).unwrap())
            .collect();
        assert_eq!(
            tracks
                .iter()
                .map(|track| track.title.as_str())
                .collect::<Vec<_>>(),
            ["Part 2", "Part 1"]
        );
        assert_ne!(tracks[0].id, tracks[1].id);
        assert_ne!(entries[0].id, entries[1].id);
    }

    #[test]
    fn ended_missing_next_stops_and_resume_attempts_a_load() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        core.state.playback.position = 10.0;
        let generation = core.generation;
        let error = core
            .audio_event(AudioEvent::Ended {
                generation,
                bitrate_bps: None,
            })
            .unwrap_err();
        assert!(error.to_string().contains("2.wav"));
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert_eq!(core.state.playback.position, 0.0);
        assert!(core.playback.is_none());
        assert_eq!(core.store.history(1).unwrap()[0].track_id, 1);
        assert_ne!(core.generation, generation);
        core.audio_event(AudioEvent::Progress {
            generation,
            position_seconds: 10.0,
            listened_seconds: 10.0,
        })
        .unwrap();
        assert_eq!(core.state.playback.position, 0.0);
        // Resume must try to reopen the selected track, not send Pause(false)
        // to an empty worker and falsely report playing.
        missing(&mut core, Command::Resume, 1);
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
    }

    #[test]
    fn missing_manual_next_preserves_current_playback() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        core.state.playback.position = 4.0;
        let generation = core.generation;
        let played_at = core.store.history(1).unwrap()[0].played_at;
        missing(&mut core, Command::Next, 2);
        assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
        assert_eq!(core.state.queue.current_id, Some(1));
        assert_eq!(core.state.playback.position, 4.0);
        assert_eq!(core.generation, generation);
        assert_eq!(core.playback.as_ref().unwrap().track_id, 1);
        assert_eq!(core.store.history(1).unwrap()[0].played_at, played_at);
    }

    #[test]
    fn count_save_failure_still_stops_and_preserves_audio_error() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        let playback = core.playback.as_mut().unwrap();
        playback.track_id = i64::MAX;
        playback.heard = 3.0;
        let generation = core.generation;
        let error = core
            .audio_event(AudioEvent::Failed {
                generation,
                message: "Decoder failed".into(),
            })
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("Decoder failed"));
        assert!(message.contains("does not exist"));
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert_ne!(core.generation, generation);
    }

    fn queue_entries(core: &Core) -> Vec<(u64, i64)> {
        core.store
            .queue_page(0, MAX_QUERY_ROWS)
            .unwrap()
            .rows
            .into_iter()
            .map(|row| (row.id, row.track_id))
            .collect()
    }

    fn assert_queue_saved(core: &Core) {
        assert_eq!(core.store.queue_ids().unwrap(), core.queue_ids);
        assert_eq!(core.store.queue_state().unwrap(), core.state.queue);
    }

    #[test]
    fn single_queue_removal_survives_restore() {
        let (_directory, mut core) = fixture();
        core.state.queue.current_id = Some(2);
        core.save(false).unwrap();
        core.command(Command::RemoveQueue { queue_id: 2 }).unwrap();
        core.load_initial_state().unwrap();
        assert_eq!(queue_entries(&core), [(1, 1), (3, 2), (4, 3), (5, 4)]);
        assert_eq!(core.state.queue.current_id, None);
    }

    #[test]
    fn playlist_creation_failure_leaves_catalog_and_versions_unchanged() {
        let (_directory, mut core) = fixture();
        core.publish();
        let before = core.state.query_revisions();
        assert!(
            core.command(Command::CreatePlaylistWithTracks {
                name: "invalid".into(),
                track_ids: vec![1, 999],
            })
            .is_err()
        );
        core.publish();
        assert_eq!(core.store.playlist_summary_page(0, 1).unwrap().total, 0);
        assert_eq!(core.state.library.playlist_total, 0);
        assert_eq!(core.state.query_revisions(), before);
        core.command(Command::CreatePlaylistWithTracks {
            name: "valid".into(),
            track_ids: vec![2, 1, 2],
        })
        .unwrap();
        let playlist = core.store.playlist_summary_page(0, 1).unwrap().rows[0].id;
        assert_eq!(core.store.playlist_track_ids(playlist, 10).unwrap(), [2, 1]);
        assert_eq!(core.state.library.playlist_total, 1);
    }

    #[test]
    fn track_removal_validates_the_set_before_stopping_playback() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        let generation = core.generation;
        assert!(
            core.command(Command::RemoveTracks {
                track_ids: vec![1, 1, 999],
            })
            .is_err()
        );
        assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
        assert_eq!(core.generation, generation);
        assert!(core.store.track(1).unwrap().is_some());
        core.command(Command::RemoveTracks {
            track_ids: vec![1, 1],
        })
        .unwrap();
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert!(core.store.track(1).unwrap().is_none());
        assert!(
            core.store
                .queue_page(0, MAX_QUERY_ROWS)
                .unwrap()
                .rows
                .iter()
                .all(|entry| entry.track_id != 1)
        );
        assert_queue_saved(&core);
    }

    #[test]
    fn failed_track_deletion_retains_persisted_current_occurrence_and_fences_audio() {
        for missing in [false, true] {
            for status in [
                PlaybackStatus::Stopped,
                PlaybackStatus::Paused,
                PlaybackStatus::Playing,
            ] {
                let (directory, mut core) = fixture_with_database(Some(10.0), true);
                if missing {
                    let path = core.track(2).unwrap().path;
                    core.store
                        .apply_scan(&ScanResult {
                            roots: vec![path],
                            ..ScanResult::default()
                        })
                        .unwrap();
                }
                playing(&mut core, 3);
                match status {
                    PlaybackStatus::Stopped => core.stop().unwrap(),
                    PlaybackStatus::Paused => core.command(Command::Pause).unwrap(),
                    PlaybackStatus::Playing => {}
                }
                core.save(false).unwrap();
                let playlist = core
                    .store
                    .create_playlist_with_tracks("Keep", &[1, 2])
                    .unwrap();
                let connection =
                    rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
                connection
                    .execute_batch(
                        "CREATE TRIGGER reject_track_delete BEFORE DELETE ON tracks
                     BEGIN SELECT RAISE(ABORT,'rejected track deletion'); END;",
                    )
                    .unwrap();
                let current = Arc::clone(core.state.current_track.as_ref().unwrap());
                let generation = core.generation;
                let late_started = started_event(&core, Some(10.0));
                let command = if missing {
                    Command::RemoveMissingTracks
                } else {
                    Command::RemoveTracks {
                        track_ids: vec![2, 2],
                    }
                };
                let error = core.command(command).unwrap_err();
                assert!(format!("{error:#}").contains("rejected track deletion"));
                assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
                assert_ne!(core.generation, generation);
                assert!(core.playback.is_none());
                assert_eq!(core.state.queue.current_id, Some(3));
                assert!(Arc::ptr_eq(
                    core.state.current_track.as_ref().unwrap(),
                    &current
                ));
                assert_eq!(
                    queue_entries(&core),
                    [(1, 1), (2, 2), (3, 2), (4, 3), (5, 4)]
                );
                assert_eq!(core.store.playlist_track_ids(playlist, 10).unwrap(), [1, 2]);
                assert_queue_saved(&core);
                core.audio_event(late_started).unwrap();
                core.audio_event(AudioEvent::Ended {
                    generation,
                    bitrate_bps: None,
                })
                .unwrap();
                assert_eq!(core.state.queue.current_id, Some(3));
                assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
                core.load_initial_state().unwrap();
                assert_eq!(core.state.queue.current_id, Some(3));
                assert_eq!(core.state.current_track().unwrap().id, 2);
                connection
                    .execute_batch("DROP TRIGGER reject_track_delete")
                    .unwrap();
                let command = if missing {
                    Command::RemoveMissingTracks
                } else {
                    Command::RemoveTracks {
                        track_ids: vec![2, 2],
                    }
                };
                core.command(command).unwrap();
                assert!(core.state.current_track.is_none());
                assert_eq!(core.state.queue.current_id, None);
                assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
                assert_eq!(queue_entries(&core), [(1, 1), (4, 3), (5, 4)]);
                assert_queue_saved(&core);
            }
        }
    }

    #[test]
    fn unrelated_track_deletion_preserves_current_occurrence_and_playback_status() {
        for missing in [false, true] {
            for status in [
                PlaybackStatus::Stopped,
                PlaybackStatus::Paused,
                PlaybackStatus::Playing,
            ] {
                let (_directory, mut core) = fixture();
                if missing {
                    let path = core.track(1).unwrap().path;
                    core.store
                        .apply_scan(&ScanResult {
                            roots: vec![path],
                            ..ScanResult::default()
                        })
                        .unwrap();
                }
                playing(&mut core, 3);
                match status {
                    PlaybackStatus::Stopped => core.stop().unwrap(),
                    PlaybackStatus::Paused => core.command(Command::Pause).unwrap(),
                    PlaybackStatus::Playing => {}
                }
                let generation = core.generation;
                let command = if missing {
                    Command::RemoveMissingTracks
                } else {
                    Command::RemoveTracks { track_ids: vec![1] }
                };
                core.command(command).unwrap();
                assert_eq!(core.state.playback.status, status);
                assert_eq!(core.generation, generation);
                assert_eq!(core.state.queue.current_id, Some(3));
                assert_eq!(core.state.queue.current_position, Some(1));
                assert_eq!(core.state.current_track().unwrap().id, 2);
                assert_queue_saved(&core);
            }
        }
    }

    #[test]
    fn interval_queue_moves_match_stable_selection_order_and_preserve_current() {
        let (_directory, mut core) = fixture();
        let original = core.queue_ids.clone();
        let current = original[2];
        core.store.set_queue_current(Some(current)).unwrap();
        for mask in 0..(1usize << original.len()) {
            for target in (0..=original.len()).chain(std::iter::once(usize::MAX)) {
                core.store.reorder_queue(&original).unwrap();
                core.queue_ids.clone_from(&original);
                core.refresh_queue_tracks().unwrap();
                let selected: Vec<_> = original
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| mask & (1 << index) != 0)
                    .map(|(_, &id)| id)
                    .collect();
                let mut expected: Vec<_> = original
                    .iter()
                    .copied()
                    .filter(|id| !selected.contains(id))
                    .collect();
                let insertion = target.min(expected.len());
                expected.splice(insertion..insertion, selected.iter().copied());
                let mut requested: Vec<_> = selected.iter().rev().copied().collect();
                if let Some(&id) = selected.first() {
                    requested.push(id);
                }
                core.command(Command::MoveQueueEntries {
                    queue_ids: requested,
                    index: target,
                })
                .unwrap();
                assert_eq!(core.queue_ids, expected, "mask={mask} target={target}");
                assert_queue_saved(&core);
                assert_eq!(core.state.queue.current_id, Some(current));
                assert_eq!(
                    core.state.queue.current_position,
                    expected.iter().position(|&id| id == current)
                );
                let ViewResponse::QueuePage(page) = core
                    .view(&Query::QueuePage {
                        offset: 0,
                        limit: MAX_QUERY_ROWS,
                    })
                    .unwrap()
                else {
                    panic!("expected queue page")
                };
                assert_eq!(page.total, original.len());
                assert_eq!(
                    page.rows
                        .iter()
                        .map(|row| (row.id, row.position))
                        .collect::<Vec<_>>(),
                    expected
                        .iter()
                        .enumerate()
                        .map(|(position, &id)| (id, position))
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn bulk_queue_edits_preserve_current_order_history_and_reject_invalid_ids_atomically() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 3);
        progress(&mut core, 1.5, 1.5);
        core.played = vec![1, 3, 5];
        core.played_cursor = 2;
        let generation = core.generation;
        let before = queue_entries(&core);
        for command in [
            Command::MoveQueueEntries {
                queue_ids: vec![3, 99],
                index: 0,
            },
            Command::RemoveQueueEntries {
                queue_ids: vec![3, 99],
            },
            Command::MoveQueue {
                queue_id: 3,
                index: usize::MAX,
            },
        ] {
            assert!(core.command(command).is_err());
            assert_eq!(queue_entries(&core), before);
            assert_eq!(core.generation, generation);
            assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
        }
        core.command(Command::MoveQueueEntries {
            queue_ids: vec![5, 1, 3, 1],
            index: 1,
        })
        .unwrap();
        assert_eq!(
            queue_entries(&core),
            [(2, 2), (1, 1), (3, 2), (5, 4), (4, 3)]
        );
        assert_eq!(core.state.queue.current_id, Some(3));
        assert_eq!(core.state.queue.current_position, Some(2));
        assert_eq!(core.generation, generation);
        assert_eq!(core.played, [1, 3, 5]);
        core.command(Command::MoveQueueEntries {
            queue_ids: vec![1, 3, 5],
            index: usize::MAX,
        })
        .unwrap();
        assert_eq!(
            queue_entries(&core),
            [(2, 2), (4, 3), (1, 1), (3, 2), (5, 4)]
        );
        core.command(Command::RemoveQueueEntries {
            queue_ids: vec![1, 1],
        })
        .unwrap();
        assert_eq!(core.generation, generation);
        assert_eq!(core.played, [3, 5]);
        assert_eq!(core.played_cursor, 1);
        core.command(Command::RemoveQueueEntries {
            queue_ids: vec![3, 3],
        })
        .unwrap();
        assert_eq!(core.state.queue.current_id, None);
        assert!(core.state.current_track().is_none());
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert_queue_saved(&core);
    }

    #[test]
    fn dedup_preserves_playing_duplicate_identity_at_first_position_and_prunes_history() {
        let (_directory, mut core) = fixture();
        core.command(Command::MoveQueue {
            queue_id: 3,
            index: 4,
        })
        .unwrap();
        playing(&mut core, 3);
        core.played = vec![1, 2, 4, 3, 2, 5];
        core.played_cursor = 4;
        let generation = core.generation;
        core.command(Command::DeduplicateQueue).unwrap();
        assert_eq!(queue_entries(&core), [(1, 1), (3, 2), (4, 3), (5, 4)]);
        assert_eq!(core.state.queue.current_id, Some(3));
        assert_eq!(core.state.queue.current_position, Some(1));
        assert_eq!(core.played, [1, 4, 3, 5]);
        assert_eq!(core.played_cursor, 3);
        assert_eq!(core.generation, generation);
        assert_queue_saved(&core);
    }

    #[test]
    fn randomize_changes_order_without_changing_current_or_history() {
        use rand::SeedableRng;
        let (_directory, mut core) = fixture();
        playing(&mut core, 3);
        core.played = vec![1, 3, 5];
        core.played_cursor = 2;
        let generation = core.generation;
        let mut expected = core.queue_ids.clone();
        core.randomize_queue(&mut rand::rngs::StdRng::seed_from_u64(42))
            .unwrap();
        let mut actual = core.queue_ids.clone();
        actual.sort_unstable();
        expected.sort_unstable();
        assert_eq!(actual, expected);
        assert_eq!(core.state.queue.current_id, Some(3));
        assert_eq!(core.played, [1, 3, 5]);
        assert_eq!(core.played_cursor, 2);
        assert_eq!(core.generation, generation);
    }

    #[test]
    fn empty_and_singleton_queue_edits_are_valid_and_empty_bulk_requests_are_no_ops() {
        let (_directory, mut core) = fixture();
        let before = core.state.queue.clone();
        core.command(Command::RemoveQueueEntries { queue_ids: vec![] })
            .unwrap();
        core.command(Command::MoveQueueEntries {
            queue_ids: vec![],
            index: usize::MAX,
        })
        .unwrap();
        assert_eq!(core.state.queue, before);
        for singleton in [false, true] {
            core.command(Command::ClearQueue).unwrap();
            if singleton {
                core.enqueue(&[2]).unwrap();
            }
            let before = core.queue_ids.clone();
            core.command(Command::RandomizeQueue).unwrap();
            core.command(Command::DeduplicateQueue).unwrap();
            assert_eq!(core.queue_ids, before);
        }
    }

    #[test]
    fn removing_tracks_prunes_history_and_previous_retains_the_remaining_gap() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 4);
        core.state.playback.shuffle = true;
        core.played = vec![1, 2, 3, 4, 5];
        core.played_cursor = 4;
        core.command(Command::RemoveTracks { track_ids: vec![2] })
            .unwrap();
        assert_eq!(core.played, [1, 4, 5]);
        assert_eq!(core.played_cursor, 2);
        missing(&mut core, Command::Previous, 1);
        assert_eq!(core.played_cursor, 2);
        assert_queue_saved(&core);
    }

    #[test]
    fn repeat_one_only_repeats_natural_end_and_repeat_all_revisits_singleton() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        core.state.playback.repeat = RepeatMode::One;
        missing(&mut core, Command::Next, 2);
        let generation = core.generation;
        let error = core
            .audio_event(AudioEvent::Ended {
                generation,
                bitrate_bps: None,
            })
            .unwrap_err();
        assert!(error.to_string().contains("1.wav"));
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        core.command(Command::RemoveTracks {
            track_ids: vec![2, 3, 4],
        })
        .unwrap();
        core.state.playback.shuffle = true;
        core.state.playback.repeat = RepeatMode::All;
        core.played = vec![1];
        core.played_cursor = 1;
        missing(&mut core, Command::Next, 1);
    }

    #[test]
    fn duplicate_queue_entries_count_as_separate_plays_but_share_recent_history() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 2);
        progress(&mut core, 3.0, 3.0);
        assert_eq!(play_count(&core, 2), 1);
        core.stop().unwrap();
        playing(&mut core, 3);
        progress(&mut core, 3.0, 3.0);
        progress(&mut core, 8.0, 8.0);
        assert_eq!(play_count(&core, 2), 2);
        let history = core.store.history(200).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].track_id, 2);
        assert_eq!(
            core.store.queue_track_id(2).unwrap(),
            core.store.queue_track_id(3).unwrap()
        );
        assert_ne!(core.queue_ids[1], core.queue_ids[2]);
    }

    #[test]
    fn failed_playlist_start_does_not_keep_an_obsolete_queue_selection() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        let playlist_id = core.store.create_playlist("missing").unwrap();
        core.store.add_playlist(playlist_id, &[2]).unwrap();
        core.reload(true).unwrap();
        let error = core
            .command(Command::PlayPlaylist { playlist_id })
            .unwrap_err();
        assert!(error.to_string().contains("2.wav"));
        assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);
        assert_eq!(core.state.queue.current_id, None);
        missing(&mut core, Command::Resume, 2);
    }
    #[test]
    fn shutdown_cancels_and_disconnects_a_backpressured_scan_worker() {
        let (_directory, mut core) = fixture();
        let cancellation = library::ScanCancellation::default();
        let observed = cancellation.clone();
        let sender = core.scan_tx.clone();
        let progress = ScanProgress {
            phase: ScanPhase::Discovering,
            processed: 0,
            total: None,
            errors: 0,
            path: None,
        };
        sender
            .send(ScanEvent::Progress {
                id: 1,
                progress: progress.clone(),
            })
            .unwrap();
        let (result_tx, result_rx) = bounded(1);
        core.active_scan = Some(ScanTask {
            status: ScanTaskStatus {
                id: 1,
                progress: progress.clone(),
                message: String::new(),
                error: None,
            },
            cancellation,
            worker: thread::spawn(move || {
                let disconnected = sender
                    .send(ScanEvent::Finished(ScanFinished {
                        id: 1,
                        result: Ok(ScanResult::default()),
                        import: None,
                        progress,
                    }))
                    .is_err();
                result_tx
                    .send((disconnected, observed.is_cancelled()))
                    .unwrap();
            }),
        });
        core.shutdown_scan();
        assert_eq!(result_rx.recv().unwrap(), (true, true));
        assert!(core.active_scan.is_none());
        assert_eq!(
            core.scan_results.back().unwrap().progress.phase,
            ScanPhase::Cancelled
        );
    }

    #[test]
    fn random_draws_include_current_weight_duplicates_and_ignore_forward_history() {
        use rand::SeedableRng;
        let (_directory, mut core) = fixture();
        core.state.queue.current_id = Some(3);
        core.played = vec![1, 3, 5];
        core.played_cursor = 1;
        let mut rng = rand::rngs::StdRng::seed_from_u64(9);
        let mut counts = [0usize; 5];
        let mut immediate_repeat = false;
        let mut previous = 3;
        for _ in 0..30_000 {
            let id = core.random_queue_id(&mut rng).unwrap();
            immediate_repeat |= previous == id;
            counts[id as usize - 1] += 1;
            previous = id;
        }
        assert!(immediate_repeat);
        assert!(counts.iter().all(|count| (5_500..6_500).contains(count)));
        assert!(counts[1] + counts[2] > counts[0] * 18 / 10);
        core.command(Command::ClearQueue).unwrap();
        core.enqueue(&[1]).unwrap();
        let id = core.queue_ids[0];
        assert_eq!(core.random_queue_id(&mut rng), Some(id));
        assert_eq!(core.random_queue_id(&mut rng), Some(id));
        for _ in 0..5000 {
            core.record_queue_history(id);
        }
        assert_eq!(core.played.len(), 4096);
        assert_eq!(core.played_cursor, 4096);
    }
    #[test]
    fn queue_batch_revisions_ignore_current_only_changes_and_reject_metadata_edits() {
        let (_directory, mut core) = fixture();
        let expected = core.state.query_revisions();
        core.state.queue.current_id = Some(3);
        core.save(false).unwrap();
        let query = Query::QueueBatch {
            after: None,
            limit: 2,
            expected: Some(expected),
        };
        assert!(matches!(core.view(&query), Ok(ViewResponse::QueueBatch(_))));
        core.command(Command::EditTrack {
            track_id: 2,
            title: Some("Changed".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .unwrap();
        assert!(matches!(
            core.view(&query),
            Err(QueryError::RevisionChanged)
        ));
        let state = core.state.query_revisions();
        core.state.playback.position = 5.;
        assert_eq!(core.state.query_revisions().queue, state.queue);
    }

    #[test]
    fn configure_failed_save_keeps_desired_settings_and_does_not_deliver_audio() {
        let (_directory, mut core) = fixture();
        let audio = recording_audio(&mut core);
        let previous = core.state.system.config.clone();
        std::fs::create_dir(&core.state.system.config_path).unwrap();
        let mut config = previous.as_ref().clone();
        config.volume = 0.25;
        config.shuffle = true;
        config.repeat = RepeatMode::All;
        config.still_mode = true;
        config.ffmpeg_enabled = false;
        config.page_size = 32;
        config.output_device = Some("configured-output".into());
        Arc::make_mut(&mut core.state.system.devices).push("configured-output".into());
        core.played = vec![1, 2];
        core.played_cursor = 2;
        let playback = core.state.playback.clone();
        let device = core.state.system.selected_device.clone();
        let ffmpeg_status = core.state.system.ffmpeg_status.clone();

        core.command(Command::Configure {
            config: std::sync::Arc::new(config),
        })
        .unwrap_err();

        assert_eq!(*core.state.system.config, *previous);
        assert_eq!(core.state.playback.volume, playback.volume);
        assert_eq!(core.state.playback.shuffle, playback.shuffle);
        assert_eq!(core.state.playback.repeat, playback.repeat);
        assert_eq!(core.state.system.selected_device, device);
        assert_eq!(core.state.system.ffmpeg_status, ffmpeg_status);
        assert_eq!(core.played, [1, 2]);
        assert_eq!(core.played_cursor, 2);
        assert!(matches!(
            audio.recv_timeout(Duration::from_millis(50)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout)
        ));
    }

    #[test]
    fn configure_rejected_preparation_keeps_saved_and_desired_settings() {
        let (_directory, mut core) = fixture();
        let audio = recording_audio(&mut core);
        let previous = core.state.system.config.clone();
        previous.save(&core.state.system.config_path).unwrap();
        let mut config = previous.as_ref().clone();
        config.volume = 0.25;
        config.still_mode = true;
        config.page_size = 0;

        core.command(Command::Configure {
            config: std::sync::Arc::new(config),
        })
        .unwrap_err();

        assert_eq!(*core.state.system.config, *previous);
        assert_eq!(core.state.playback.volume, previous.volume);
        assert_eq!(
            Config::load(&core.state.system.config_path).unwrap(),
            *previous
        );
        assert!(matches!(
            audio.recv_timeout(Duration::from_millis(50)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout)
        ));
    }

    #[test]
    fn configure_saved_but_disconnected_audio_retains_desired_settings() {
        let (_directory, mut core) = fixture();
        let (commands, receiver) = bounded(1);
        drop(receiver);
        core.engine.commands = commands;
        let mut config = core.state.system.config.as_ref().clone();
        config.volume = 0.25;
        config.shuffle = true;
        config.repeat = RepeatMode::All;
        config.still_mode = true;
        config.ffmpeg_enabled = false;
        config.page_size = 32;
        config.output_device = Some("configured-output".into());
        Arc::make_mut(&mut core.state.system.devices).push("configured-output".into());
        core.played = vec![1, 2];
        core.played_cursor = 2;
        let expected = config.clone();

        core.command(Command::Configure {
            config: std::sync::Arc::new(config),
        })
        .unwrap_err();

        assert_eq!(*core.state.system.config, expected);
        assert_eq!(core.state.playback.volume, expected.volume);
        assert_eq!(core.state.playback.shuffle, expected.shuffle);
        assert_eq!(core.state.playback.repeat, expected.repeat);
        assert_eq!(core.state.system.selected_device, expected.output_device);
        assert_eq!(core.state.system.ffmpeg_status, "disabled");
        assert!(core.played.is_empty());
        assert_eq!(core.played_cursor, 0);
        assert_eq!(
            Config::load(&core.state.system.config_path).unwrap(),
            expected
        );
        assert!(!core.flush_config(true).unwrap());
        core.publish();
        assert_eq!(*core.shared.read().system.config, expected);
    }

    #[test]
    fn still_mode_failed_save_keeps_desired_settings_and_does_not_deliver_audio() {
        let (_directory, mut core) = fixture();
        let audio = recording_audio(&mut core);
        let previous = core.state.system.config.clone();
        std::fs::create_dir(&core.state.system.config_path).unwrap();

        core.update_gui_config(GuiConfigChange::ToggleStillMode)
            .unwrap_err();

        assert_eq!(*core.state.system.config, *previous);
        assert!(matches!(
            audio.recv_timeout(Duration::from_millis(50)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout)
        ));
    }

    #[test]
    fn still_mode_saved_but_disconnected_audio_retains_desired_settings() {
        let (_directory, mut core) = fixture();
        let (commands, receiver) = bounded(1);
        drop(receiver);
        core.engine.commands = commands;
        // Pending playback settings must be incorporated in the same saved snapshot.
        core.state.playback.volume = 0.25;
        core.state.playback.shuffle = true;
        core.state.playback.repeat = RepeatMode::All;
        core.state.system.selected_device = Some("pending-output".into());
        let mut expected = core.state.system.config.as_ref().clone();
        expected.volume = 0.25;
        expected.shuffle = true;
        expected.repeat = RepeatMode::All;
        expected.output_device = Some("pending-output".into());

        for enabled in [true, false] {
            expected.still_mode = enabled;
            core.update_gui_config(GuiConfigChange::ToggleStillMode)
                .unwrap_err();

            assert_eq!(*core.state.system.config, expected);
            assert_eq!(core.state.playback.volume, expected.volume);
            assert_eq!(core.state.playback.shuffle, expected.shuffle);
            assert_eq!(core.state.playback.repeat, expected.repeat);
            assert_eq!(core.state.system.selected_device, expected.output_device);
            assert_eq!(
                Config::load(&core.state.system.config_path).unwrap(),
                expected
            );
            assert!(!core.flush_config(true).unwrap());
            core.publish();
            assert_eq!(*core.shared.read().system.config, expected);
        }
    }

    #[test]
    fn metadata_edit_refreshes_published_history_without_rewriting_old_snapshots() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        playing(&mut core, 2);
        core.publish();
        let before = core.shared.read().library.history.clone();
        let before_ids = before.iter().map(|row| row.track_id).collect::<Vec<_>>();
        let before_played_at = before
            .iter()
            .find(|row| row.track_id == 1)
            .unwrap()
            .played_at;
        core.command(Command::EditTrack {
            track_id: 1,
            title: Some("Renamed history".to_owned()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .unwrap();
        core.publish();
        let published = core.shared.read().library.history.clone();
        let row = published.iter().find(|row| row.track_id == 1).unwrap();
        assert_eq!(row.title, "Renamed history");
        assert_eq!(row.played_at, before_played_at);
        assert_eq!(
            published.iter().map(|row| row.track_id).collect::<Vec<_>>(),
            before_ids
        );
        assert_eq!(
            published
                .iter()
                .find(|row| row.track_id == 2)
                .unwrap()
                .title,
            "Track 2"
        );
        assert_eq!(
            before.iter().find(|row| row.track_id == 1).unwrap().title,
            "Track 1"
        );
    }

    #[test]
    fn crossing_play_threshold_refreshes_published_history_count_during_playback() {
        let (_directory, mut core) = fixture();
        playing(&mut core, 1);
        core.publish();
        let before = core.shared.read().library.history.clone();
        progress(&mut core, 2.0, 2.0);
        core.publish();
        assert_eq!(core.shared.read().library.history[0].play_count, 0);
        progress(&mut core, 3.0, 3.0);
        core.publish();
        let published = core.shared.read().library.history.clone();
        assert_eq!(published[0].play_count, 1);
        assert_eq!(published[0].track_id, 1);
        assert_eq!(published[0].played_at, before[0].played_at);
        assert_eq!(before[0].play_count, 0);
        assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    }
}
