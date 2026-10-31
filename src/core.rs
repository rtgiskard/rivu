//! Core waits for events and applies state transitions on one dedicated owner.
//! An application-owned executor drives admission, replies, IPC and long jobs.
//! Caller timeout never cancels admitted work or authorizes replay. Real-time
//! audio stays independent; shutdown completion includes physical retirement.

#[cfg(test)]
use crate::execution::TaskRuntime;
use crate::{
    config::{Config, ConfigPatch, ConfigPersistence, LibraryColumn},
    execution::BlockingExecutor,
    library::{self, M3uItem, ScanResult},
    projection::{GuiSnapshot, MprisSnapshot, TraySnapshot},
    query_tracking::QueryTicket,
    store::Store,
};
use anyhow::{Context, Result};
use async_channel as mpsc;
use event_listener::Event;
use futures::FutureExt;
use futures::channel::oneshot;
use parking_lot::{Mutex, RwLock};
use rand::seq::SliceRandom;
use rivu_audio::analysis::{AnalysisControl, AnalysisFrame, DisplayDemand};
use rivu_audio::{self as audio, AudioCommand, AudioEngine, AudioEvent, MediaInfo};
use rivu_format::decoder_hint::DecoderHint;
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
use playback::{PlaybackSession, RetiredPlayback};
use relocation::{RelocationFinished, RelocationTask};
#[cfg(test)]
use runtime::CompletionGuard;
pub(crate) use runtime::CoreLifecycle;
pub use runtime::CoreRuntime;

mod query_worker;

mod task_wait;
use task_wait::TaskWaiters;

use rivu_protocol::{ADMISSION_TIMEOUT, REPLY_TIMEOUT, command_reply_timeout};

type Wakeup = Arc<dyn Fn() + Send + Sync>;
type QueryTimeout = Arc<dyn Fn(QueryResponse) + Send + Sync>;

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
    GuiConfig(GuiConfigChange),
    Navigate(Navigation),
    MprisSeek {
        queue_id: u64,
        offset: i64,
    },
    WaveformSnapshot(
        oneshot::Sender<std::result::Result<Option<audio::WaveformScanSnapshot>, String>>,
    ),
    WaitTask {
        target: TaskTarget,
        registration: u64,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum Navigation {
    Next,
    Previous,
}

pub(crate) enum GuiConfigChange {
    ToggleVisualization,
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

/// Cloneable access to core business commands, queries, and published state.
#[derive(Clone)]
pub struct CoreHandle {
    state: Arc<RwLock<CoreState>>,
    pub(crate) analysis: Arc<RwLock<AnalysisFrame>>,
    analysis_control: Arc<AnalysisControl>,
    pub(crate) waveform: Arc<RwLock<audio::WaveformFrame>>,
    sender: mpsc::Sender<Request>,
    revision_updates: Arc<RwLock<Vec<mpsc::Sender<()>>>>,
    wakeup: Arc<RwLock<Option<Wakeup>>>,
    task_waiters: Arc<Mutex<TaskWaiters>>,
    // Window-scoped: cloning a GUI handle must not change IPC/CLI query policy.
    query_timeout: Option<QueryTimeout>,
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
        self.analysis_control.frame_is_current(frame)
    }

    /// Coalesce a GUI consumer frame into at most one pending analysis job.
    pub(crate) fn request_analysis_frame(&self) {
        self.analysis_control.request_frame();
    }

    /// Capture the active audio source and processing state without waiting on the GUI thread.
    pub(crate) fn waveform_snapshot(
        &self,
    ) -> Result<oneshot::Receiver<std::result::Result<Option<audio::WaveformScanSnapshot>, String>>>
    {
        let (reply, receiver) = oneshot::channel();
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

    pub(crate) async fn apply_gui_settings(&self, patch: ConfigPatch) -> Ack {
        self.operation_ack_async(
            Operation::GuiConfig(GuiConfigChange::Settings(Box::new(patch))),
            REPLY_TIMEOUT,
        )
        .await
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
        Arc::clone(&self.state.read().configuration.config)
    }
    pub(crate) fn library_revision(&self) -> u64 {
        self.state.read().library.revision
    }
    pub(crate) fn mpris_service_state(&self) -> MprisServiceState {
        let state = self.state.read();
        MprisServiceState {
            enabled: state.configuration.config.mpris_enabled,
            shutting_down: state.system.shutting_down,
        }
    }
    /// In-process publication/wakeup sequence; not a query-domain revision.
    pub fn publication_revision(&self) -> u64 {
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
        self.notify_revisions();
    }
    pub fn clear_wakeup(&self) {
        *self.wakeup.write() = None;
        self.notify_revisions();
    }
    fn notify_revisions(&self) {
        self.revision_updates
            .write()
            .retain(|sender| !matches!(sender.try_send(()), Err(mpsc::TrySendError::Closed(_))));
    }

    pub(crate) fn subscribe_revisions(&self) -> mpsc::Receiver<()> {
        let (sender, receiver) = mpsc::bounded(1);
        let mut subscribers = self.revision_updates.write();
        subscribers.retain(|sender| !sender.is_closed());
        subscribers.push(sender);
        receiver
    }

    pub(crate) async fn query_async(&self, query: Query) -> QueryResponse {
        self.query_with_ticket_async(query, None, false, REPLY_TIMEOUT)
            .await
    }
    #[cfg(test)]
    /// Drive the runtime-neutral query port for synchronous test owners.
    pub fn query(&self, query: Query) -> QueryResponse {
        futures::executor::block_on(self.query_async(query))
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
    pub(crate) async fn query_for_gui(&self, query: Query) -> QueryResponse {
        self.query_with_ticket_async(query, None, true, REPLY_TIMEOUT)
            .await
    }

    /// Cancel stale queued reads without interrupting an already executing read.
    /// A timeout notifies the GUI, then drains and discards the actual reply.
    pub(crate) async fn query_if_current(
        &self,
        query: Query,
        ticket: QueryTicket,
    ) -> QueryResponse {
        self.query_with_ticket_async(query, Some(ticket), true, REPLY_TIMEOUT)
            .await
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
            Ok(()) => match rivu_protocol::transport::timeout(duration, &mut rx).await {
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
            }),
            queue: sections.queue.then(|| state.queue.clone()),
            library: sections.library.then(|| state.library.clone()),
            system: sections.system.then(|| state.system.clone()),
            configuration: sections.configuration.then(|| state.configuration.clone()),
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

    /// Navigate in one Core dispatch, preserving its status at execution time.
    pub(crate) async fn navigate(&self, direction: Navigation) -> Ack {
        self.operation_ack_async(Operation::Navigate(direction), REPLY_TIMEOUT)
            .await
    }

    /// Keep the relative intent and occurrence bound until Core executes it.
    pub(crate) async fn mpris_seek(&self, queue_id: u64, offset: i64) -> Ack {
        self.operation_ack_async(Operation::MprisSeek { queue_id, offset }, REPLY_TIMEOUT)
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
    revision_updates: Arc<RwLock<Vec<mpsc::Sender<()>>>>,
    state: CoreState,
    shared: Arc<RwLock<CoreState>>,
    wakeup: Arc<RwLock<Option<Wakeup>>>,
    task_waiters: Arc<Mutex<TaskWaiters>>,
    task_completions_pending: bool,
    generation: u64,
    output_settings_request: u64,
    next_playlist_token: u64,
    database_unavailable: bool,
    // Failed scalar/current-track refresh gates dependent controls until recovery.
    queue_projection_stale: bool,
    // A runtime selection must reach the database before accepting its projection.
    queue_current_pending: bool,
    playback: Option<PlaybackSession>,
    // At most one playback can retire while database writes are excluded.
    retired_playback: Option<RetiredPlayback>,
    playback_write_deferred: bool,
    playback_history_pending: bool,
    pending_bitrate: Option<(u64, bool)>,
    pending_navigation: Option<PlaybackStatus>,
    active_export: Option<export::ExportTask>,
    active_external_admission: Option<admission::ExternalAdmissionTask>,
    active_optimization: Option<maintenance::OptimizationTask>,
    processing: crate::processing::Service,
    pending_processing_audition: Option<(u64, u64)>,
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
        self.close_task_waiters();
        if let Some(reads) = &self.read_queries {
            reads.fence.transition().finish(true);
        }
        drop(self.read_queries.take());
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
        // Any earlier startup read can fail before the queue projection is reached.
        self.queue_projection_stale = true;
        self.state.library.track_total = self.store.track_count()?;
        self.state.library.playlist_total = self.store.playlist_summary_page(None, 0, 0)?.total;
        for playlist_id in self.store.playlist_ids()? {
            self.advance_playlist_entry_revision(playlist_id);
        }
        self.replace_history(self.store.history(MAX_QUERY_ROWS)?);
        self.state.system.ffmpeg_status = if self.state.configuration.config.ffmpeg_enabled {
            audio::ffmpeg_status().unwrap_or_else(|error| format!("unavailable: {error:#}"))
        } else {
            "disabled".to_owned()
        };
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
