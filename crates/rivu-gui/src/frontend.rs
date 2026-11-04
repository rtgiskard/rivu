//! Remote desktop state and bounded, instance-bound IPC workers.
//! Logical query deadlines notify the view without releasing its physical wait.
use crate::projection::GuiSnapshot;
use crate::transport::visual::VisualClient;
use crate::transport::waveform::WaveClient;
use anyhow::{Context, Result};
use async_channel::{Receiver, Sender};
use futures::channel::oneshot;
use parking_lot::{Mutex, RwLock};
use rivu_analysis::{AnalysisFrame, AnalysisSettings, DisplayDemand};
use rivu_client::{
    ClientSession, CommandFailure, InstanceChanged, SessionCancellation, client_session_with_cancel,
};
use rivu_format::config::Config;
use rivu_protocol::query_tracking::QueryTicket;
use rivu_protocol::waveform::WaveformFrame;
use rivu_protocol::{
    ADMISSION_TIMEOUT, AppCommand, REPLY_TIMEOUT,
    model::{ClientSnapshot, Command, ConfigMutation, ConfigPatch, Query},
    response::{
        Ack, AckStatus, QueryError, QueryResponse, StateResponse, StateRevisions, StateSections,
    },
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    thread::{self, JoinHandle},
};

type Wakeup = Arc<dyn Fn() + Send + Sync>;
type QueryTimeout = Arc<dyn Fn(QueryResponse) + Send + Sync>;
const QUERY_WORKERS: usize = 4;
const QUEUE_CAPACITY: usize = 32;

struct Published {
    snapshot: ClientSnapshot,
    revisions: StateRevisions,
    publication: u64,
    error: Option<String>,
}

impl Published {
    fn gui_snapshot(&self) -> GuiSnapshot {
        let mut snapshot = GuiSnapshot::from_client(&self.snapshot, self.revisions);
        if let Some(error) = &self.error {
            snapshot.system.last_error = Some(error.clone());
        }
        snapshot.system.revision = self.publication;
        snapshot
    }
}

#[derive(Default)]
struct ConnectionState {
    disconnected: bool,
    callback: Option<Wakeup>,
}

struct Shared {
    data_dir: PathBuf,
    instance_id: u32,
    published: RwLock<Published>,
    // Serialize the complete read-and-merge, not merely applying its response.
    // A delayed Watch read can therefore never overwrite an ACK refresh.
    refresh: Mutex<()>,
    stop: SessionCancellation,
    wakeup: RwLock<Option<Wakeup>>,
    disconnect: Mutex<ConnectionState>,
    visual: Arc<VisualClient>,
    wave: Arc<WaveClient>,
}

fn analysis_invalidated_by_config(previous: &Config, next: &Config) -> bool {
    !AnalysisSettings::from(previous).signal_compatible_with(AnalysisSettings::from(next))
        || previous.visualization_enabled != next.visualization_enabled
        || previous.output_device != next.output_device
        || previous.pipewire_auto_mix != next.pipewire_auto_mix
        || (next.visualization_signal == rivu_format::config::VisualizationSignal::After
            && previous.equalizer != next.equalizer)
}

impl Shared {
    fn notify(&self) {
        self.wake();
    }

    fn wake(&self) {
        let callback = self.wakeup.read().clone();
        if let Some(callback) = callback {
            callback();
        }
    }

    fn disconnected(&self) {
        let callback = {
            let mut connection = self.disconnect.lock();
            if connection.disconnected {
                return;
            }
            connection.disconnected = true;
            connection.callback.clone()
        };
        self.stop.cancel();
        if let Some(callback) = callback {
            callback();
        }
    }

    fn error(&self, message: String, fatal: bool) {
        if fatal {
            self.stop.cancel();
        }
        {
            let mut published = self.published.write();
            published.error = Some(message);
            published.snapshot.system.shutting_down |= fatal;
            published.publication = published.publication.wrapping_add(1);
        }
        if fatal {
            self.disconnected();
        }
        self.notify();
    }

    fn dismiss_error(&self) {
        let changed = {
            let mut published = self.published.write();
            let changed = published.error.take().is_some();
            if changed {
                published.publication = published.publication.wrapping_add(1);
            }
            changed
        };
        if changed {
            self.notify();
        }
    }

    fn merge(&self, response: StateResponse) {
        let (shutdown, refresh_waveform) = {
            let mut published = self.published.write();
            if self.stop.is_cancelled() {
                return;
            }
            let device_changed = response.system.as_ref().is_some_and(|next| {
                next.selected_device != published.snapshot.system.selected_device
            });
            let refresh_waveform =
                response.playback.is_some() || response.configuration.is_some() || device_changed;
            let invalidate_analysis = response.playback.as_ref().is_some_and(|next| {
                next.source_generation != published.snapshot.source_generation
                    || next.playback.seek_revision != published.snapshot.playback.seek_revision
                    || next.playback.status != published.snapshot.playback.status
            }) || response.configuration.as_ref().is_some_and(|next| {
                analysis_invalidated_by_config(
                    &published.snapshot.configuration.config,
                    &next.config,
                )
            }) || device_changed;
            if invalidate_analysis {
                self.visual.invalidate();
            }
            let sections = response.sections();
            published
                .revisions
                .apply_sections(response.revisions, sections);
            response.apply_to(&mut published.snapshot);
            self.visual
                .configure(&published.snapshot.configuration.config);
            published.publication = published.publication.wrapping_add(1);
            (published.snapshot.system.shutting_down, refresh_waveform)
        };
        if refresh_waveform {
            self.wave.refresh();
        }
        if shutdown {
            self.disconnected();
        }
        self.notify();
    }

    fn refresh_after_operation(&self, session: &mut ClientSession) -> Result<()> {
        let _owner = self.refresh.lock();
        let announced = session.get_state(StateSections::default())?.revisions;
        let current = self.published.read().revisions;
        let sections = announced.changed_since(current);
        if !sections.is_empty() {
            self.merge(session.get_state(sections)?);
        }
        Ok(())
    }

    fn query_error(&self, query: &Query, error: QueryError) -> QueryResponse {
        let published = self.published.read();
        QueryResponse {
            revisions: published
                .snapshot
                .library
                .query_revisions_for(query, published.snapshot.queue.revision),
            result: Err(error),
        }
    }

    fn ack_error(&self, status: AckStatus, message: String) -> Ack {
        Ack::new(
            status,
            Some(message),
            self.published.read().snapshot.system.revision,
        )
    }
}

struct CommandRequest {
    command: AppCommand,
    reply: Option<oneshot::Sender<Ack>>,
}
struct QueryRequest {
    query: Query,
    ticket: Option<QueryTicket>,
    reply: oneshot::Sender<QueryResponse>,
}

/// Owns remote workers only. Dropping the desktop never sends Core Shutdown.
pub(crate) struct FrontendRuntime {
    pub(crate) handle: FrontendHandle,
    workers: Vec<JoinHandle<()>>,
}

#[derive(Clone)]
pub(crate) struct FrontendHandle {
    shared: Arc<Shared>,
    commands: Sender<CommandRequest>,
    queries: Sender<QueryRequest>,
    query_timeout: Option<QueryTimeout>,
    pub(crate) analysis: Arc<RwLock<AnalysisFrame>>,
    pub(crate) waveform: Arc<RwLock<WaveformFrame>>,
}

impl FrontendRuntime {
    pub(crate) fn connect(data_dir: &Path, expected_instance: u32) -> Result<Self> {
        let stop = SessionCancellation::default();
        let mut watcher = client_session_with_cancel(data_dir, stop.clone())?;
        watcher.bind_instance(expected_instance)?;
        let initial = watcher.get_state(StateSections::ALL)?;
        anyhow::ensure!(
            initial.sections() == StateSections::ALL,
            "Incomplete initial GUI state"
        );
        let instance_id = watcher
            .instance_id()
            .context("GUI Hello did not bind an instance")?;
        watcher.bind_instance(instance_id)?;
        let visual = Arc::new(VisualClient::connect(data_dir, instance_id)?);
        let wave = Arc::new(WaveClient::connect(data_dir, instance_id)?);
        let revisions = initial.revisions;
        let mut snapshot = ClientSnapshot::default();
        initial.apply_to(&mut snapshot);
        visual.configure(&snapshot.configuration.config);
        let shared = Arc::new(Shared {
            data_dir: data_dir.to_owned(),
            instance_id,
            published: RwLock::new(Published {
                error: None,
                publication: snapshot.system.revision,
                snapshot,
                revisions,
            }),
            refresh: Mutex::new(()),
            stop,
            wakeup: RwLock::new(None),
            disconnect: Mutex::new(ConnectionState::default()),
            visual: Arc::clone(&visual),
            wave: Arc::clone(&wave),
        });
        let weak = Arc::downgrade(&shared);
        visual.set_wakeup(Arc::new(move || {
            if let Some(shared) = weak.upgrade() {
                shared.wake();
            }
        }));
        let weak = Arc::downgrade(&shared);
        wave.set_wakeup(Arc::new(move || {
            if let Some(shared) = weak.upgrade() {
                shared.wake();
            }
        }));
        let weak = Arc::downgrade(&shared);
        visual.set_error_callback(Arc::new(move |message| {
            if let Some(shared) = weak.upgrade() {
                shared.error(message, true);
            }
        }));
        let weak = Arc::downgrade(&shared);
        wave.set_error_callback(Arc::new(move |message| {
            if let Some(shared) = weak.upgrade() {
                shared.error(message, true);
            }
        }));
        let (commands, command_rx) = async_channel::bounded(QUEUE_CAPACITY);
        let (queries, query_rx) = async_channel::bounded(QUEUE_CAPACITY);
        let mut runtime = Self {
            handle: FrontendHandle {
                shared: Arc::clone(&shared),
                commands,
                queries,
                query_timeout: None,
                analysis: Arc::clone(&visual.analysis),
                waveform: Arc::clone(&wave.waveform),
            },
            workers: Vec::with_capacity(QUERY_WORKERS + 2),
        };
        let session = runtime.handle.session()?;
        runtime.spawn("rivu-gui-command", move |shared| {
            command_worker(shared, session, command_rx)
        })?;
        for _ in 0..QUERY_WORKERS {
            let session = runtime.handle.session()?;
            let receiver = query_rx.clone();
            runtime.spawn("rivu-gui-query", move |shared| {
                query_worker(shared, session, receiver)
            })?;
        }
        runtime.spawn("rivu-gui-watch", move |shared| {
            while !shared.stop.is_cancelled() {
                let current = shared.published.read().revisions;
                let announced = match watcher.watch_until(current) {
                    Ok(Some(revisions)) => revisions,
                    Ok(None) => break,
                    Err(error) => {
                        shared.error(format!("GUI state watcher disconnected: {error:#}"), true);
                        break;
                    }
                };
                let _owner = shared.refresh.lock();
                let sections = announced.changed_since(shared.published.read().revisions);
                if sections.is_empty() {
                    continue;
                }
                match watcher.get_state(sections) {
                    Ok(response) => shared.merge(response),
                    Err(error) => {
                        if !shared.stop.is_cancelled() {
                            shared.error(format!("GUI state refresh failed: {error:#}"), true);
                        }
                        break;
                    }
                }
            }
        })?;
        Ok(runtime)
    }

    fn spawn(
        &mut self,
        name: &str,
        operation: impl FnOnce(Arc<Shared>) + Send + 'static,
    ) -> Result<()> {
        let shared = Arc::clone(&self.handle.shared);
        self.workers
            .push(thread::Builder::new().name(name.into()).spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    operation(Arc::clone(&shared))
                }));
                if result.is_err() {
                    shared.error("GUI IPC worker panicked".into(), true);
                }
            })?);
        Ok(())
    }
}

impl Drop for FrontendRuntime {
    fn drop(&mut self) {
        self.handle.commands.close();
        self.handle.queries.close();
        self.handle.shared.disconnected();
        self.handle.shared.visual.stop();
        self.handle.shared.wave.stop();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn command_worker(
    shared: Arc<Shared>,
    mut session: ClientSession,
    receiver: Receiver<CommandRequest>,
) {
    while let Ok(request) = receiver.recv_blocking() {
        if shared.stop.is_cancelled() {
            break;
        }
        let shutdown = matches!(request.command, AppCommand::Shutdown);
        if matches!(request.command, AppCommand::Core(Command::DismissError)) {
            shared.dismiss_error();
        }
        let mut ack = match session.request_ack(request.command) {
            Ok(ack) => ack,
            Err(error) => {
                let status = error
                    .downcast_ref::<CommandFailure>()
                    .map_or(AckStatus::Unknown, |failure| failure.status);
                if error.downcast_ref::<InstanceChanged>().is_some() {
                    shared.error(format!("GUI instance changed: {error:#}"), true);
                }
                shared.ack_error(
                    status,
                    format!("GUI command failed: {error:#}; do not automatically retry"),
                )
            }
        };
        if !shutdown
            && !shared.stop.is_cancelled()
            && let Err(error) = shared.refresh_after_operation(&mut session)
        {
            shared.error(format!("Authoritative GUI refresh failed: {error:#}"), true);
            if ack.is_success() {
                ack.status = AckStatus::Failed;
                ack.error = Some(format!(
                    "Command succeeded but authoritative state refresh failed: {error:#}; do not retry"
                ));
            }
        }
        if !shutdown && shared.stop.is_cancelled() && ack.is_success() {
            ack.status = AckStatus::Failed;
            ack.error = Some(
                "Command succeeded but GUI disconnected before authoritative refresh; do not retry"
                    .into(),
            );
        }
        if let Some(reply) = request.reply {
            let _ = reply.send(ack);
        } else if !ack.is_success() {
            shared.error(
                ack.error.unwrap_or_else(|| "GUI command failed".into()),
                false,
            );
        }
    }
}

fn query_worker(shared: Arc<Shared>, mut session: ClientSession, receiver: Receiver<QueryRequest>) {
    while let Ok(request) = receiver.recv_blocking() {
        if shared.stop.is_cancelled() {
            break;
        }
        if request.reply.is_canceled()
            || request
                .ticket
                .as_ref()
                .is_some_and(|ticket| !ticket.is_current())
        {
            let _ = request
                .reply
                .send(shared.query_error(&request.query, QueryError::Cancelled));
            continue;
        }
        let response = match session.query_retained(request.query.clone()) {
            Ok(response) => response,
            Err(error) => {
                if error.downcast_ref::<InstanceChanged>().is_some() {
                    shared.error(format!("GUI instance changed: {error:#}"), true);
                }
                shared.query_error(&request.query, QueryError::from(error))
            }
        };
        let response = if request
            .ticket
            .as_ref()
            .is_some_and(|ticket| !ticket.is_current())
        {
            shared.query_error(&request.query, QueryError::Cancelled)
        } else {
            response
        };
        let _ = request.reply.send(response);
    }
}

impl FrontendHandle {
    fn session(&self) -> Result<ClientSession> {
        let mut session =
            client_session_with_cancel(&self.shared.data_dir, self.shared.stop.clone())?;
        session.bind_instance(self.shared.instance_id)?;
        Ok(session)
    }

    pub(crate) fn data_dir(&self) -> &Path {
        &self.shared.data_dir
    }
    pub(crate) fn instance_id(&self) -> u32 {
        self.shared.instance_id
    }
    pub(crate) fn is_connected(&self) -> bool {
        !self.shared.stop.is_cancelled()
    }
    pub(crate) fn set_disconnect_callback(&self, callback: impl Fn() + Send + Sync + 'static) {
        let callback: Wakeup = Arc::new(callback);
        let disconnected = {
            let mut connection = self.shared.disconnect.lock();
            connection.callback = Some(Arc::clone(&callback));
            connection.disconnected
        };
        if disconnected {
            callback();
        }
    }
    pub(crate) fn send(&self, command: Command) -> Result<()> {
        self.send_app(command.into())
    }
    pub(crate) fn request_shutdown(&self) -> Result<()> {
        self.send_app(AppCommand::Shutdown)
    }
    fn send_app(&self, command: AppCommand) -> Result<()> {
        anyhow::ensure!(self.is_connected(), "GUI is disconnected from Core");
        self.commands
            .try_send(CommandRequest {
                command,
                reply: None,
            })
            .context("GUI command queue is busy or stopped")
    }
    pub(crate) fn update_config(&self, change: ConfigMutation) -> Result<()> {
        self.send(Command::MutateConfig { change })
    }
    pub(crate) async fn apply_settings(&self, patch: ConfigPatch) -> Ack {
        self.request_ack(Command::MutateConfig {
            change: ConfigMutation::Settings(Box::new(patch)),
        })
        .await
    }
    pub(crate) async fn request_ack(&self, command: Command) -> Ack {
        let (reply, receiver) = oneshot::channel();
        if !self.is_connected() {
            return self.shared.ack_error(
                AckStatus::NotAdmitted,
                "GUI is disconnected from Core".into(),
            );
        }
        match rivu_protocol::transport::timeout(
            ADMISSION_TIMEOUT,
            self.commands.send(CommandRequest {
                command: command.into(),
                reply: Some(reply),
            }),
        )
        .await
        {
            Ok(Ok(())) => {}
            _ => {
                return self.shared.ack_error(
                    AckStatus::NotAdmitted,
                    "GUI command queue is busy or stopped; command not sent".into(),
                );
            }
        }
        receiver.await.unwrap_or_else(|_| {
            self.shared.ack_error(
                AckStatus::Unknown,
                "GUI command outcome unknown; do not automatically retry".into(),
            )
        })
    }
    pub(crate) fn with_query_timeout(
        mut self,
        notify: impl Fn(QueryResponse) + Send + Sync + 'static,
    ) -> Self {
        self.query_timeout = Some(Arc::new(notify));
        self
    }
    pub(crate) async fn query_for_gui(&self, query: Query) -> QueryResponse {
        self.query_with_ticket(query, None, REPLY_TIMEOUT).await
    }
    pub(crate) async fn query_if_current(
        &self,
        query: Query,
        ticket: QueryTicket,
    ) -> QueryResponse {
        self.query_with_ticket(query, Some(ticket), REPLY_TIMEOUT)
            .await
    }
    async fn query_with_ticket(
        &self,
        query: Query,
        ticket: Option<QueryTicket>,
        duration: std::time::Duration,
    ) -> QueryResponse {
        if ticket.as_ref().is_some_and(|ticket| !ticket.is_current()) {
            return self.shared.query_error(&query, QueryError::Cancelled);
        }
        let (reply, mut receiver) = oneshot::channel();
        let request = QueryRequest {
            query: query.clone(),
            ticket: ticket.clone(),
            reply,
        };
        if !self.is_connected()
            || !matches!(
                rivu_protocol::transport::timeout(ADMISSION_TIMEOUT, self.queries.send(request))
                    .await,
                Ok(Ok(()))
            )
        {
            return self.shared.query_error(
                &query,
                QueryError::Message("GUI query queue is busy or stopped".into()),
            );
        }
        match rivu_protocol::transport::timeout(duration, &mut receiver).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => self.shared.query_error(
                &query,
                QueryError::Message(format!("GUI query disconnected: {error}")),
            ),
            Err(error) => {
                let response = self.shared.query_error(
                    &query,
                    QueryError::Message(format!("GUI query response timed out: {error}")),
                );
                if ticket.as_ref().is_none_or(QueryTicket::is_current)
                    && let Some(notify) = &self.query_timeout
                {
                    notify(response.clone());
                }
                // Keep the real transport request and GUI worker slot alive.
                // A late success cannot resurrect a logically timed-out read.
                let _ = receiver.await;
                response
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn query(&self, query: Query) -> QueryResponse {
        futures::executor::block_on(self.query_for_gui(query))
    }
    pub(crate) fn gui_snapshot(&self) -> GuiSnapshot {
        let state = self.shared.published.read();
        state.gui_snapshot()
    }
    pub(crate) fn gui_snapshot_if_changed(&self, revision: u64) -> Option<GuiSnapshot> {
        let state = self.shared.published.read();
        (state.publication != revision).then(|| state.gui_snapshot())
    }
    pub(crate) fn config_snapshot(&self) -> Arc<Config> {
        Arc::clone(&self.shared.published.read().snapshot.configuration.config)
    }
    pub(crate) fn library_revision(&self) -> u64 {
        self.shared.published.read().snapshot.library.revision
    }
    pub(crate) fn set_wakeup(&self, callback: impl Fn() + Send + Sync + 'static) {
        *self.shared.wakeup.write() = Some(Arc::new(callback));
        self.shared.notify();
    }
    pub(crate) fn clear_wakeup(&self) {
        *self.shared.wakeup.write() = None;
    }
    pub(crate) fn set_display_demand(&self, demand: DisplayDemand) {
        self.shared.visual.set_display_demand(demand);
    }
    pub(crate) fn set_waveform_demand(&self, enabled: bool) {
        self.shared.wave.set_enabled(enabled);
    }
    pub(crate) fn analysis_frame_is_current(&self, frame: &AnalysisFrame) -> bool {
        self.is_connected() && self.shared.visual.analysis_frame_is_current(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ConfigFixture;
    use std::time::Duration;

    fn fixture() -> (tempfile::TempDir, crate::test_support::CoreRuntime) {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        Config {
            media_read_buffer_mib: 1,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            tray_enabled: false,
            ..Default::default()
        }
        .save_fixture(&config_path)
        .unwrap();
        let runtime =
            crate::test_support::CoreRuntime::start(directory.path(), &config_path).unwrap();
        (directory, runtime)
    }

    #[test]
    fn analysis_config_preserves_presentation_changes_but_fences_signal_changes() {
        let previous = Config {
            visualization_signal: rivu_format::config::VisualizationSignal::After,
            ..Default::default()
        };
        let mut next = previous.clone();
        next.analysis_rate_hz = if previous.analysis_rate_hz == 60 {
            5
        } else {
            60
        };
        next.volume = 0.0;
        next.waveform_seek_enabled = !previous.waveform_seek_enabled;
        next.visualization_fps = if previous.visualization_fps == 60 {
            30
        } else {
            60
        };
        assert!(!analysis_invalidated_by_config(&previous, &next));

        for change in 0..6 {
            let mut next = previous.clone();
            match change {
                0 => {
                    next.analysis_fft_size = if previous.analysis_fft_size == 512 {
                        1024
                    } else {
                        512
                    }
                }
                1 => {
                    next.analysis_window =
                        if previous.analysis_window == rivu_format::config::AnalysisWindow::Hann {
                            rivu_format::config::AnalysisWindow::BlackmanHarris
                        } else {
                            rivu_format::config::AnalysisWindow::Hann
                        }
                }
                2 => next.visualization_signal = rivu_format::config::VisualizationSignal::Before,
                3 => next.visualization_enabled = !previous.visualization_enabled,
                4 => next.output_device = Some("another-device".into()),
                _ => next.pipewire_auto_mix = !previous.pipewire_auto_mix,
            }
            assert!(
                analysis_invalidated_by_config(&previous, &next),
                "change {change}"
            );
        }

        for signal in [
            rivu_format::config::VisualizationSignal::Before,
            rivu_format::config::VisualizationSignal::After,
        ] {
            let previous = Config {
                visualization_signal: signal,
                ..previous.clone()
            };
            let mut next = previous.clone();
            next.equalizer.enabled = !previous.equalizer.enabled;
            assert_eq!(
                analysis_invalidated_by_config(&previous, &next),
                signal == rivu_format::config::VisualizationSignal::After
            );
        }
    }

    #[test]
    fn local_failure_can_be_dismissed_without_a_core_error() {
        let (_directory, runtime) = fixture();
        let shared = &runtime.handle.shared;
        shared.error("Command not admitted".into(), false);
        assert!(runtime.handle.gui_snapshot().system.last_error.is_some());
        let before = rivu_client::get_state(runtime.handle.data_dir(), StateSections::ALL).unwrap();
        assert!(before.system.as_ref().unwrap().last_error.is_none());
        let ack = futures::executor::block_on(runtime.handle.request_ack(Command::DismissError));
        assert!(ack.is_success());
        assert!(runtime.handle.gui_snapshot().system.last_error.is_none());
        let after = rivu_client::get_state(runtime.handle.data_dir(), StateSections::ALL).unwrap();
        assert!(after.system.unwrap().last_error.is_none());
    }

    /// Pause only real IPC scheduling; no Core operation or response is replaced.
    struct ExecutorPause {
        release: async_channel::Sender<()>,
        worker: Option<async_executor::Task<()>>,
    }

    impl ExecutorPause {
        fn new(runtime: &crate::test_support::CoreRuntime) -> Self {
            let (release, blocked) = async_channel::bounded(1);
            let (entered, started) = std::sync::mpsc::sync_channel(1);
            let worker = runtime.executor().spawn(async move {
                entered.send(()).unwrap();
                let _ = blocked.recv_blocking();
            });
            let pause = Self {
                release,
                worker: Some(worker),
            };
            started
                .recv_timeout(Duration::from_secs(5))
                .expect("real IPC executor did not pause");
            pause
        }
    }

    impl Drop for ExecutorPause {
        fn drop(&mut self) {
            let _ = self.release.try_send(());
            if let Some(worker) = self.worker.take() {
                async_io::block_on(worker);
            }
        }
    }

    #[test]
    fn logical_timeout_retains_real_ipc_wait_until_reply_and_discards_late_success() {
        let (_directory, runtime) = fixture();
        let pause = ExecutorPause::new(&runtime);
        let (notice, notices) = std::sync::mpsc::sync_channel(1);
        let handle = runtime.handle.clone().with_query_timeout(move |response| {
            let _ = notice.try_send(response);
        });
        let (finished, completion) = std::sync::mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let response = futures::executor::block_on(handle.query_with_ticket(
                Query::Track { track_id: i64::MAX },
                None,
                Duration::from_millis(10),
            ));
            finished.send(response).unwrap();
        });
        let notice = notices
            .recv_timeout(Duration::from_secs(5))
            .expect("logical timeout was not reported");
        assert!(matches!(notice.result, Err(QueryError::Message(_))));
        assert!(matches!(
            completion.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        drop(pause);
        let response = completion
            .recv_timeout(Duration::from_secs(5))
            .expect("retained real IPC wait did not retire");
        worker.join().unwrap();
        assert_eq!(response.result.err(), notice.result.err());
        assert!(matches!(
            runtime
                .handle
                .query(Query::Track { track_id: i64::MAX })
                .result,
            Ok(rivu_protocol::response::ViewResponse::Track(None))
        ));
    }

    #[test]
    fn runtime_drop_cancels_a_retained_ipc_wait_while_core_remains_alive() {
        let (directory, mut runtime) = fixture();
        let pause = ExecutorPause::new(&runtime);
        let (notice, notices) = std::sync::mpsc::sync_channel(1);
        let handle = runtime.handle.clone().with_query_timeout(move |response| {
            let _ = notice.try_send(response);
        });
        let (finished, completion) = std::sync::mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let response = futures::executor::block_on(handle.query_with_ticket(
                Query::Track { track_id: i64::MAX },
                None,
                Duration::from_millis(10),
            ));
            finished.send(response).unwrap();
        });
        let timed_out = notices
            .recv_timeout(Duration::from_secs(5))
            .expect("query did not enter retained waiting");
        assert!(matches!(
            completion.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        runtime.disconnect_frontend();
        let response = completion
            .recv_timeout(Duration::from_secs(5))
            .expect("runtime drop did not retire retained wait");
        worker.join().unwrap();
        assert!(matches!(timed_out.result, Err(QueryError::Message(_))));
        assert_eq!(response.result.err(), timed_out.result.err());
        drop(pause);
        let state = rivu_client::get_state(directory.path(), StateSections::ALL).unwrap();
        assert!(!state.system.unwrap().shutting_down);
    }
}
