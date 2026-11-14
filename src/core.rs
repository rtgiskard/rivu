//! Core waits for events and applies state transitions on one dedicated owner.
//! An application-owned executor drives admission, replies, IPC and long jobs.
//! Caller timeout never cancels admitted work or authorizes replay. Real-time
//! audio stays independent; shutdown completion includes physical retirement.

#[cfg(any(test, feature = "test-support"))]
use crate::execution::TaskRuntime;
use crate::{
    config::{Config, ConfigPersistence},
    execution::BlockingExecutor,
    library::{self, M3uItem, ScanResult},
    projection::MprisSnapshot,
    store::Store,
};
use anyhow::{Context, Result};
use async_channel as mpsc;
use event_listener::Event;
use futures::FutureExt;
use futures::channel::oneshot;
use parking_lot::{Mutex, RwLock};
use rand::seq::SliceRandom;
use rivu_audio::capture::PcmControl;
use rivu_audio::{self as audio, AudioCommand, AudioEngine, AudioEvent, MediaInfo};
use rivu_format::decoder_hint::DecoderHint;
use rivu_protocol::query_tracking::QueryTicket;
use rivu_protocol::{
    model::*,
    response::{
        Ack, AckStatus, PlaybackSnapshot, QueryError, QueryResponse, QueryRevisions, StateResponse,
        StateRevisions, StateSections, TaskOutcome, ViewResponse,
    },
};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub mod state;
pub(crate) use state::CoreState;

mod runtime;
use catalog::{MAX_RETAINED_SCAN_RESULTS, ScanEvent, ScanTask};
#[cfg(test)]
use playback::PlaybackSession;
use playback::QueueHistory;
use relocation::{RelocationFinished, RelocationTask};
#[cfg(test)]
use runtime::CompletionGuard;
pub(crate) use runtime::CoreLifecycle;
pub use runtime::CoreRuntime;

mod query_worker;

mod task_wait;
use task_wait::TaskWaiters;

use rivu_protocol::{ADMISSION_TIMEOUT, REPLY_TIMEOUT, command_reply_timeout};

/// Retire an admitted async worker exactly once and convert panics to a task error.
/// Callers retain ownership of the task-specific completion and uncertainty policy.
pub(super) fn retire_worker(
    worker: async_executor::Task<()>,
    panic_message: &'static str,
) -> Result<()> {
    futures::executor::block_on(std::panic::AssertUnwindSafe(worker).catch_unwind())
        .map_err(|_| anyhow::anyhow!(panic_message))?;
    Ok(())
}

pub(super) fn retire_optional_worker(
    worker: &mut Option<async_executor::Task<()>>,
    panic_message: &'static str,
) -> Result<()> {
    worker
        .take()
        .map_or(Ok(()), |worker| retire_worker(worker, panic_message))
}
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
    MprisSeek {
        queue_id: u64,
        offset: i64,
    },
    MprisSetPosition {
        queue_id: u64,
        position: i64,
    },
    RenderSnapshot {
        source: u64,
        reply: oneshot::Sender<std::result::Result<Option<audio::AudioRenderSnapshot>, String>>,
    },
    WaitTask {
        target: TaskTarget,
        registration: u64,
    },
}

struct Request {
    operation: Operation,
    reply: Option<oneshot::Sender<CoreResponse>>,
}

/// Cloneable access to core business commands, queries, and published state.
#[derive(Clone)]
pub struct CoreHandle {
    state: Arc<RwLock<CoreState>>,
    pub(crate) pcm: Arc<PcmControl>,
    sender: mpsc::Sender<Request>,
    revision_updates: Arc<RwLock<Vec<RevisionSubscription>>>,
    task_waiters: Arc<Mutex<TaskWaiters>>,
    mpris_status: Arc<MprisStatusInbox>,
}

#[derive(Clone, Copy)]
pub(crate) enum RevisionInterest {
    Sections(StateSections),
    MprisService,
    Mpris,
    #[cfg(feature = "gui")]
    Tray,
}

struct RevisionSubscription {
    interest: RevisionInterest,
    sender: mpsc::Sender<()>,
}

pub(crate) struct MprisServiceState {
    pub enabled: bool,
    pub enable_generation: u64,
    pub shutting_down: bool,
}

struct MprisStatusInbox {
    latest: Mutex<Option<String>>,
    wake: mpsc::Sender<()>,
    ready: mpsc::Receiver<()>,
}

impl Default for MprisStatusInbox {
    fn default() -> Self {
        let (wake, ready) = mpsc::bounded(1);
        Self {
            latest: Mutex::new(None),
            wake,
            ready,
        }
    }
}

impl CoreHandle {
    /// Internal service facts bypass external command admission and coalesce.
    pub(crate) fn report_mpris_status(&self, status: String) {
        *self.mpris_status.latest.lock() = Some(status);
        let _ = self.mpris_status.wake.try_send(());
    }

    /// Capture the active audio source and processing state without waiting on the GUI thread.
    pub(crate) fn render_snapshot(
        &self,
        source: u64,
    ) -> Result<oneshot::Receiver<std::result::Result<Option<audio::AudioRenderSnapshot>, String>>>
    {
        let (reply, receiver) = oneshot::channel();
        self.sender
            .try_send(Request {
                operation: Operation::RenderSnapshot { source, reply },
                reply: None,
            })
            .context("Rivu is busy or stopped")?;
        Ok(receiver)
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
    #[cfg(test)]
    pub fn config_snapshot(&self) -> Arc<Config> {
        Arc::clone(&self.state.read().configuration.config)
    }
    pub(crate) fn mpris_service_state(&self) -> MprisServiceState {
        let state = self.state.read();
        MprisServiceState {
            enabled: state.configuration.config.mpris_enabled,
            enable_generation: state.mpris_enable_generation,
            shutting_down: state.system.shutting_down,
        }
    }
    /// In-process publication sequence; not a query-domain revision.
    pub fn publication_revision(&self) -> u64 {
        self.state.read().system.revision
    }
    pub fn mpris_snapshot(&self) -> MprisSnapshot {
        MprisSnapshot::from_core(&self.state.read())
    }
    #[cfg(feature = "gui")]
    pub(crate) fn tray_snapshot(&self) -> crate::projection::TraySnapshot {
        let state = self.state.read();
        crate::projection::TraySnapshot::from_core(&state)
    }
    #[cfg(test)]
    pub(crate) fn state_snapshot(&self) -> CoreState {
        self.state.read().clone()
    }

    /// Subscribe before reading a baseline, then reconcile after async setup.
    /// Notifications coalesce; consumers always re-read their published state.
    pub(crate) fn subscribe_revisions(&self, interest: RevisionInterest) -> mpsc::Receiver<()> {
        let (sender, receiver) = mpsc::bounded(1);
        let mut subscribers = self.revision_updates.write();
        subscribers.retain(|subscriber| !subscriber.sender.is_closed());
        subscribers.push(RevisionSubscription { interest, sender });
        receiver
    }

    pub(crate) async fn query_async(&self, query: Query) -> QueryResponse {
        self.query_with_timeout(query, REPLY_TIMEOUT).await
    }

    /// IPC retained reads keep their physical slot until the owner replies.
    pub(crate) async fn query_retained(&self, query: Query) -> QueryResponse {
        self.query_retained_with_ticket(query, None).await
    }

    pub(crate) async fn query_retained_if_current(
        &self,
        query: Query,
        ticket: QueryTicket,
    ) -> QueryResponse {
        self.query_retained_with_ticket(query, Some(ticket)).await
    }

    async fn query_retained_with_ticket(
        &self,
        query: Query,
        ticket: Option<QueryTicket>,
    ) -> QueryResponse {
        let (tx, rx) = oneshot::channel();
        let request = Request {
            operation: Operation::Query { query, ticket },
            reply: Some(tx),
        };
        let response = match self.admit(request).await {
            Ok(()) => rx
                .await
                .map_err(|error| format!("Core response unavailable: {error}")),
            Err(error) => Err(format!("Core unavailable: {error}")),
        };
        match response {
            Ok(CoreResponse::Query(response)) => response,
            Ok(CoreResponse::Ack(_)) => unreachable!("query operation returned acknowledgement"),
            Err(message) => QueryResponse {
                revisions: self.state.read().query_revisions(),
                result: Err(QueryError::Message(message)),
            },
        }
    }
    #[cfg(test)]
    /// Drive the runtime-neutral query port for synchronous test owners.
    pub fn query(&self, query: Query) -> QueryResponse {
        futures::executor::block_on(self.query_async(query))
    }

    async fn query_with_timeout(&self, query: Query, duration: Duration) -> QueryResponse {
        let (tx, rx) = oneshot::channel();
        let request = Request {
            operation: Operation::Query {
                query,
                ticket: None,
            },
            reply: Some(tx),
        };
        let response = match self.admit(request).await {
            Err(error) => Err(format!("Core unavailable: {error}")),
            Ok(()) => match rivu_protocol::transport::timeout(duration, rx).await {
                Ok(result) => result.map_err(|error| format!("Core response unavailable: {error}")),
                Err(error) => Err(format!("Core response unavailable: {error}")),
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
        rivu_protocol::transport::timeout(ADMISSION_TIMEOUT, self.sender.send(request))
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
                source_generation: state.source_generation,
                processing_audition: state.processing_audition,
                processing_selection: state.processing_selection,
            }),
            queue: sections.queue.then(|| state.queue.clone()),
            library: sections.library.then(|| state.library.clone()),
            system: sections.system.then(|| {
                let mut system = state.system.clone();
                system.scan_owner = state.scan_owner;
                system
            }),
            configuration: sections.configuration.then(|| state.configuration.clone()),
            processing: sections
                .processing
                .then(|| rivu_protocol::response::ProcessingState {
                    task: state.processing.clone(),
                }),
        }
    }

    pub fn state_revisions(&self) -> StateRevisions {
        self.state.read().revisions
    }

    pub async fn request_ack(&self, command: Command) -> Ack {
        let duration = command_reply_timeout(&command);
        self.operation_ack_async(Operation::Command(command), duration)
            .await
    }

    /// Keep the relative intent and occurrence bound until Core executes it.
    pub(crate) async fn mpris_seek(&self, queue_id: u64, offset: i64) -> Ack {
        self.operation_ack_async(Operation::MprisSeek { queue_id, offset }, REPLY_TIMEOUT)
            .await
    }

    pub(crate) async fn mpris_set_position(&self, queue_id: u64, position: i64) -> Ack {
        self.operation_ack_async(
            Operation::MprisSetPosition { queue_id, position },
            REPLY_TIMEOUT,
        )
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
                self.publication_revision(),
            );
        }
        match rivu_protocol::transport::timeout(duration, rx).await {
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
                self.publication_revision(),
            ),
        }
    }
}

struct Core {
    store: Store,
    engine: AudioEngine,
    logging: Option<rivu_utils::logging::Control>,
    executor: Arc<async_executor::Executor<'static>>,
    workers: Arc<BlockingExecutor>,
    read_queries: Option<query_worker::ReadQueries>,
    initial_devices: Option<runtime::InitialDevices>,
    revision_updates: Arc<RwLock<Vec<RevisionSubscription>>>,
    mpris_status: Arc<MprisStatusInbox>,
    state: CoreState,
    shared: Arc<RwLock<CoreState>>,
    #[cfg(test)]
    publication_observer: Option<Arc<dyn Fn() + Send + Sync>>,
    task_waiters: Arc<Mutex<TaskWaiters>>,
    task_completions_pending: bool,
    transport: playback::PlaybackOwner,
    projections: projections::ProjectionRecovery,
    output_settings_request: u64,
    next_playlist_token: u64,
    database_unavailable: bool,
    active_export: Option<export::ExportTask>,
    active_external_admission: Option<admission::ExternalAdmissionTask>,
    active_optimization: Option<maintenance::OptimizationTask>,
    active_source_tags: Option<source_tags::SourceTagTask>,
    pending_written_source: Option<source_tags::PendingWrittenSource>,
    processing: crate::processing::Service,
    pending_processing_audition: Option<(u64, u64)>,
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
    preferences: settings::Preferences,
    #[cfg(test)]
    _test_tasks: Option<TaskRuntime>,
}

impl Drop for Core {
    fn drop(&mut self) {
        // Runs on the dedicated Core owner (or a synchronous test owner),
        // including unwind. Cancellation is intent, never retirement.
        self.close_task_waiters();
        if let Some(reads) = &self.read_queries {
            reads.fence.transition().finish(true);
        }
        drop(self.read_queries.take());
        self.shutdown_source_tags();
        drop(self.active_export.take());
        drop(self.active_external_admission.take());
        if let Some(task) = self.active_scan.take() {
            self.scan_rx.close();
            if task.status.progress.phase != ScanPhase::Saving {
                task.cancellation.cancel();
            }
            while self.scan_rx.try_recv().is_ok() {}
            let _ = futures::executor::block_on(
                std::panic::AssertUnwindSafe(task.worker).catch_unwind(),
            );
            while self.scan_rx.try_recv().is_ok() {}
        }
        self.relocation_rx.close();
        while self.relocation_rx.try_recv().is_ok() {}
        drop(self.active_relocation.take());
        while self.relocation_rx.try_recv().is_ok() {}
        drop(self.active_optimization.take());
        drop(self.initial_devices.take());
        // AudioEngine::drop sends shutdown and physically joins its worker.
    }
}
impl Core {
    fn load_initial_state(&mut self) -> Result<()> {
        self.transport.projection_stale = true;
        self.projections.track_total = true;
        self.projections.playlist_total = true;
        self.projections.playlist_tokens = true;
        self.transport.history_pending = true;
        self.state.system.ffmpeg_status = if self.preferences.settings().ffmpeg_enabled {
            audio::ffmpeg_status().unwrap_or_else(|error| format!("unavailable: {error:#}"))
        } else {
            "disabled".to_owned()
        };
        self.state.library.track_total = self.store.track_count()?;
        self.projections.track_total = false;
        self.state.library.playlist_total = self.store.playlist_summary_page(None, 0, 0)?.total;
        self.projections.playlist_total = false;
        self.recover_playlist_tokens()?;
        self.replace_history(self.store.history(MAX_QUERY_ROWS)?);
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
mod projections;
mod queue;
mod relocation;
mod settings;
mod source_tags;
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
