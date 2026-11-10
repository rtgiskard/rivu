//! Remote desktop state and bounded, instance-bound IPC workers.
//! Logical query deadlines notify the view without releasing its physical wait.
use crate::projection::GuiSnapshot;
use crate::transport::visual::VisualClient;
use anyhow::{Context, Result};
use async_channel::{Receiver, Sender};
use futures::channel::oneshot;
use parking_lot::{Mutex, RwLock};
use rivu_analysis::{AnalysisFrame, AnalysisSettings, DisplayDemand};
use rivu_client::{
    ClientSession, CommandFailure, InstanceChanged, SessionCancellation, client_session_with_cancel,
};
use rivu_format::config::Config;
use rivu_protocol::query_tracking::{QueryEpoch, QueryTicket};
use rivu_protocol::{
    ADMISSION_TIMEOUT, AppCommand, REPLY_TIMEOUT,
    model::{Command, ConfigMutation, ConfigPatch, Query},
    response::{
        Ack, AckStatus, ObservedState, QueryError, QueryResponse, QueryRevisionScope,
        StateResponse, StateSections,
    },
};
use rivu_visuals::WaveformFrame;
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
    observed: ObservedState,
    publication: u64,
    error: Option<String>,
    disconnected: bool,
}

impl Published {
    fn gui_snapshot(&self) -> GuiSnapshot {
        let mut snapshot =
            GuiSnapshot::from_client(self.observed.snapshot(), self.observed.applied());
        if let Some(error) = &self.error {
            snapshot.system.last_error = Some(error.clone());
        }
        snapshot.system.revision = self.publication;
        snapshot.system.shutting_down |= self.disconnected;
        snapshot
    }
}

#[derive(Default)]
struct ConnectionState {
    disconnected: bool,
    fatal_error: Option<String>,
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
    shutdown_admitted: Mutex<bool>,
}

fn analysis_invalidated_by_config(previous: &Config, next: &Config) -> bool {
    !AnalysisSettings::from(previous).signal_compatible_with(AnalysisSettings::from(next))
        || previous.visualization_enabled != next.visualization_enabled
        || previous.output_device != next.output_device
        || previous.pipewire_auto_mix != next.pipewire_auto_mix
        || (next.visualization_signal == rivu_format::config::VisualizationSignal::After
            && (previous.equalizer != next.equalizer
                || previous.processing_bypassed != next.processing_bypassed))
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
        self.visual.stop();
        if let Some(callback) = callback {
            callback();
        }
    }

    fn error(&self, message: String, fatal: bool) {
        if fatal {
            let mut connection = self.disconnect.lock();
            if connection.disconnected {
                return;
            }
            connection
                .fatal_error
                .get_or_insert_with(|| message.clone());
            drop(connection);
            self.stop.cancel();
        }
        {
            let mut published = self.published.write();
            published.error = Some(message);
            published.disconnected |= fatal;
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

    fn merge(&self, response: StateResponse) -> bool {
        let shutdown = {
            let mut published = self.published.write();
            if self.stop.is_cancelled() {
                return false;
            }
            let device_changed = response.system.as_ref().is_some_and(|next| {
                next.selected_device != published.observed.snapshot().system.selected_device
            });
            let invalidate_analysis = response.playback.as_ref().is_some_and(|next| {
                next.source_generation != published.observed.snapshot().source_generation
                    || next.playback.seek_revision
                        != published.observed.snapshot().playback.seek_revision
                    || next.playback.status != published.observed.snapshot().playback.status
            }) || response.configuration.as_ref().is_some_and(|next| {
                analysis_invalidated_by_config(
                    &published.observed.snapshot().configuration.config,
                    &next.config,
                )
            }) || device_changed;
            if invalidate_analysis {
                self.visual.invalidate();
            }
            published.observed.apply(self.instance_id, response);
            self.visual
                .configure(&published.observed.snapshot().configuration.config);
            let state = published.observed.snapshot();
            self.visual.configure_source(
                state.current_track.as_deref(),
                state.source_generation,
                state.playback.duration,
            );
            published.publication = published.publication.wrapping_add(1);
            published.observed.snapshot().system.shutting_down
        };
        if shutdown {
            self.disconnected();
        }
        self.notify();
        true
    }

    fn refresh_after_operation(&self, session: &mut ClientSession) -> Result<()> {
        let _owner = self.refresh.lock();
        let announced = session.get_state(StateSections::default())?.revisions;
        let current = self.published.read().observed.applied();
        let sections = announced.changed_since(current);
        if !sections.is_empty() {
            let response = session.get_state(sections)?;
            let complete = response.sections().intersection(sections) == sections;
            anyhow::ensure!(
                self.merge(response),
                "GUI disconnected before state refresh"
            );
            anyhow::ensure!(complete, "Incomplete GUI state refresh");
        }
        anyhow::ensure!(
            !self.stop.is_cancelled(),
            "GUI disconnected before authoritative refresh"
        );
        Ok(())
    }

    fn refresh_announced(
        &self,
        session: &mut ClientSession,
        announced: rivu_protocol::response::StateRevisions,
    ) -> Result<()> {
        let _owner = self.refresh.lock();
        let sections = announced.changed_since(self.published.read().observed.applied());
        if !sections.is_empty() {
            // A Watch carries only tokens: even a delayed notification reads
            // the current payload here, inside the same mutex as ACK refreshes.
            let response = session.get_state(sections)?;
            let complete = response.sections().intersection(sections) == sections;
            anyhow::ensure!(
                self.merge(response),
                "GUI disconnected before Watch refresh"
            );
            anyhow::ensure!(complete, "Incomplete GUI Watch refresh");
        }
        Ok(())
    }

    fn query_error(&self, scope: QueryRevisionScope, error: QueryError) -> QueryResponse {
        let published = self.published.read();
        QueryResponse {
            revisions: published
                .observed
                .snapshot()
                .library
                .query_revisions_for_scope(scope, published.observed.snapshot().queue.revision),
            result: Err(error),
        }
    }

    fn ack_error(&self, status: AckStatus, message: String) -> Ack {
        Ack::new(
            status,
            Some(message),
            self.published.read().observed.snapshot().system.revision,
        )
    }
}

/// The owner's ACK survives independently of a later observation failure.
#[derive(Debug)]
pub(crate) struct CommandDelivery {
    pub(crate) ack: Ack,
    pub(crate) observation: CommandObservation,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CommandObservation {
    Refreshed,
    Unavailable(String),
    NotRequested,
}

impl CommandDelivery {
    fn observe(ack: Ack, shutdown: bool, refresh: impl FnOnce() -> Result<()>) -> Self {
        let observation = if shutdown {
            CommandObservation::NotRequested
        } else {
            match refresh() {
                Ok(()) => CommandObservation::Refreshed,
                Err(error) => CommandObservation::Unavailable(format!(
                    "Authoritative GUI refresh failed: {error:#}; do not retry"
                )),
            }
        };
        Self { ack, observation }
    }

    /// Draft rebases/writeback require both owner success and a fresh read.
    pub(crate) fn observed_result(&self) -> std::result::Result<(), String> {
        if !self.ack.is_success() {
            return Err(self
                .ack
                .error
                .clone()
                .unwrap_or_else(|| "Command failed".into()));
        }
        match &self.observation {
            CommandObservation::Refreshed => Ok(()),
            CommandObservation::Unavailable(error) => Err(error.clone()),
            CommandObservation::NotRequested => Err("Authoritative state was not requested".into()),
        }
    }
}

struct CommandRequest {
    command: AppCommand,
    reply: Option<oneshot::Sender<CommandDelivery>>,
}
struct QueryRequest {
    query: Query,
    ticket: Option<QueryTicket>,
    owner: QueryTicket,
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
    shutdown: Sender<CommandRequest>,
    queries: Sender<QueryRequest>,
    query_timeout: Option<QueryTimeout>,
    pub(crate) analysis: Arc<RwLock<AnalysisFrame>>,
    pub(crate) waveform: Arc<RwLock<WaveformFrame>>,
}

impl FrontendRuntime {
    pub(crate) fn connect(data_dir: &Path, expected_instance: u32) -> Result<Self> {
        let stop = SessionCancellation::default();
        let mut watcher = client_session_with_cancel(data_dir, stop.clone());
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
        let mut observed = ObservedState::new(instance_id);
        observed.apply(instance_id, initial);
        visual.configure(&observed.snapshot().configuration.config);
        visual.configure_source(
            observed.snapshot().current_track.as_deref(),
            observed.snapshot().source_generation,
            observed.snapshot().playback.duration,
        );
        let shared = Arc::new(Shared {
            data_dir: data_dir.to_owned(),
            instance_id,
            published: RwLock::new(Published {
                error: None,
                publication: observed.snapshot().system.revision,
                observed,
                disconnected: false,
            }),
            refresh: Mutex::new(()),
            stop,
            wakeup: RwLock::new(None),
            disconnect: Mutex::new(ConnectionState::default()),
            visual: Arc::clone(&visual),
            shutdown_admitted: Mutex::new(false),
        });
        let weak = Arc::downgrade(&shared);
        visual.set_wakeup(Arc::new(move || {
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
        let (commands, command_rx) = async_channel::bounded(QUEUE_CAPACITY);
        // Quit has its own single admission slot, independent of edit pressure.
        let (shutdown, shutdown_rx) = async_channel::bounded(1);
        let (queries, query_rx) = async_channel::bounded(QUEUE_CAPACITY);
        let mut runtime = Self {
            handle: FrontendHandle {
                shared: Arc::clone(&shared),
                commands,
                shutdown,
                queries,
                query_timeout: None,
                analysis: Arc::clone(&visual.analysis),
                waveform: Arc::clone(&visual.waveform),
            },
            workers: Vec::with_capacity(QUERY_WORKERS + 2),
        };
        let session = runtime.handle.session()?;
        runtime.spawn("rivu-gui-command", move |shared| {
            command_worker(shared, session, command_rx, shutdown_rx)
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
                let current = shared.published.read().observed.applied();
                let announced = match watcher.watch_until(current.into()) {
                    Ok(Some(revisions)) => revisions,
                    Ok(None) => break,
                    Err(error) => {
                        shared.error(format!("GUI state watcher disconnected: {error:#}"), true);
                        break;
                    }
                };
                if let Err(error) = shared.refresh_announced(&mut watcher, announced) {
                    if !shared.stop.is_cancelled() {
                        shared.error(format!("GUI state refresh failed: {error:#}"), true);
                    }
                    break;
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

    pub(crate) fn shutdown(&mut self) -> Result<()> {
        self.handle.commands.close();
        self.handle.shutdown.close();
        self.handle.queries.close();
        self.handle.shared.disconnected();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        match self.handle.shared.disconnect.lock().fatal_error.take() {
            Some(error) => Err(anyhow::anyhow!(error)),
            None => Ok(()),
        }
    }
}

impl Drop for FrontendRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn command_worker(
    shared: Arc<Shared>,
    mut session: ClientSession,
    receiver: Receiver<CommandRequest>,
    shutdown_receiver: Receiver<CommandRequest>,
) {
    while let Ok(request) = futures_lite::future::block_on(futures_lite::future::or(
        shutdown_receiver.recv(),
        receiver.recv(),
    )) {
        if shared.stop.is_cancelled() {
            break;
        }
        let shutdown = matches!(request.command, AppCommand::Shutdown);
        if matches!(request.command, AppCommand::Core(Command::DismissError)) {
            shared.dismiss_error();
        }
        let mut instance_error = None;
        let ack = match session.request_ack(request.command) {
            Ok(ack) => ack,
            Err(error) => {
                let status = error
                    .downcast_ref::<CommandFailure>()
                    .map_or(AckStatus::Unknown, |failure| failure.status);
                if error.downcast_ref::<InstanceChanged>().is_some() {
                    instance_error = Some(format!("GUI instance changed: {error:#}"));
                }
                shared.ack_error(
                    status,
                    format!("GUI command failed: {error:#}; do not automatically retry"),
                )
            }
        };
        let shutdown_complete = finish_command(
            &shared,
            shutdown,
            request.reply,
            ack,
            instance_error,
            || {
                anyhow::ensure!(
                    !shared.stop.is_cancelled(),
                    "GUI disconnected before authoritative refresh"
                );
                shared.refresh_after_operation(&mut session)
            },
        );
        if shutdown_complete {
            break;
        }
    }
}

fn finish_command(
    shared: &Shared,
    shutdown: bool,
    reply: Option<oneshot::Sender<CommandDelivery>>,
    ack: Ack,
    instance_error: Option<String>,
    refresh: impl FnOnce() -> Result<()>,
) -> bool {
    if shutdown && ack.status == AckStatus::NotAdmitted {
        *shared.shutdown_admitted.lock() = false;
    }
    if let Some(message) = instance_error {
        shared.error(message, true);
    }
    let shutdown_complete = shutdown && ack.is_success();
    let delivery = CommandDelivery::observe(ack, shutdown, refresh);
    if let CommandObservation::Unavailable(message) = &delivery.observation
        && !shared.stop.is_cancelled()
    {
        shared.error(message.clone(), true);
    }
    if let Some(reply) = reply {
        let _ = reply.send(delivery);
    } else if !delivery.ack.is_success() {
        shared.error(
            delivery
                .ack
                .error
                .unwrap_or_else(|| "GUI command failed".into()),
            false,
        );
    }
    shutdown_complete
}

fn query_worker(shared: Arc<Shared>, mut session: ClientSession, receiver: Receiver<QueryRequest>) {
    while let Ok(mut request) = receiver.recv_blocking() {
        if shared.stop.is_cancelled() {
            break;
        }
        let scope = QueryRevisionScope::for_query(&request.query);
        if request.reply.is_canceled()
            || !request.owner.is_current()
            || request
                .ticket
                .as_ref()
                .is_some_and(|ticket| !ticket.is_current())
        {
            let _ = request
                .reply
                .send(shared.query_error(scope, QueryError::Cancelled));
            continue;
        }
        let cancelled = async {
            futures_lite::future::or(request.reply.cancellation(), async {
                futures_lite::future::or(request.owner.cancelled(), async {
                    if let Some(ticket) = &request.ticket {
                        ticket.cancelled().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                })
                .await;
            })
            .await;
        };
        let response = match session.query_retained_until(request.query, cancelled) {
            Ok(response) => response,
            Err(error) => {
                if error.downcast_ref::<InstanceChanged>().is_some() {
                    shared.error(format!("GUI instance changed: {error:#}"), true);
                }
                shared.query_error(scope, QueryError::from(error))
            }
        };
        let response = if !request.owner.is_current()
            || request
                .ticket
                .as_ref()
                .is_some_and(|ticket| !ticket.is_current())
        {
            shared.query_error(scope, QueryError::Cancelled)
        } else {
            response
        };
        let _ = request.reply.send(response);
    }
}

impl FrontendHandle {
    fn session(&self) -> Result<ClientSession> {
        let mut session =
            client_session_with_cancel(&self.shared.data_dir, self.shared.stop.clone());
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
        let result: Result<()> = (|| {
            anyhow::ensure!(self.is_connected(), "GUI is disconnected from Core");
            let mut admitted = self.shared.shutdown_admitted.lock();
            if !*admitted {
                self.shutdown
                    .try_send(CommandRequest {
                        command: AppCommand::Shutdown,
                        reply: None,
                    })
                    .context("GUI shutdown worker stopped; shutdown not sent")?;
                *admitted = true;
            }
            Ok(())
        })();
        if let Err(error) = &result {
            self.shared
                .error(format!("GUI quit failed: {error:#}"), false);
        }
        result
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
    /// Admit immediate preference edits synchronously, preserving click order.
    pub(crate) fn enqueue_config(
        &self,
        change: ConfigMutation,
    ) -> Result<impl Future<Output = CommandDelivery> + use<>> {
        anyhow::ensure!(self.is_connected(), "GUI is disconnected from Core");
        let (reply, receiver) = oneshot::channel();
        self.commands
            .try_send(CommandRequest {
                command: Command::MutateConfig { change }.into(),
                reply: Some(reply),
            })
            .context("GUI command queue is busy or stopped")?;
        let shared = Arc::clone(&self.shared);
        Ok(async move {
            receiver.await.unwrap_or_else(|_| CommandDelivery {
                ack: shared.ack_error(
                    AckStatus::Unknown,
                    "GUI command outcome unknown; do not automatically retry".into(),
                ),
                observation: CommandObservation::Unavailable(
                    "GUI command worker stopped before observation".into(),
                ),
            })
        })
    }
    pub(crate) async fn apply_settings(&self, patch: ConfigPatch) -> CommandDelivery {
        self.request_ack(Command::MutateConfig {
            change: ConfigMutation::Settings(Box::new(patch)),
        })
        .await
    }
    pub(crate) async fn request_ack(&self, command: Command) -> CommandDelivery {
        let (reply, receiver) = oneshot::channel();
        if !self.is_connected() {
            return CommandDelivery {
                ack: self.shared.ack_error(
                    AckStatus::NotAdmitted,
                    "GUI is disconnected from Core".into(),
                ),
                observation: CommandObservation::NotRequested,
            };
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
                return CommandDelivery {
                    ack: self.shared.ack_error(
                        AckStatus::NotAdmitted,
                        "GUI command queue is busy or stopped; command not sent".into(),
                    ),
                    observation: CommandObservation::NotRequested,
                };
            }
        }
        receiver.await.unwrap_or_else(|_| CommandDelivery {
            ack: self.shared.ack_error(
                AckStatus::Unknown,
                "GUI command outcome unknown; do not automatically retry".into(),
            ),
            observation: CommandObservation::Unavailable(
                "GUI command worker stopped before observation".into(),
            ),
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
        let scope = QueryRevisionScope::for_query(&query);
        if ticket.as_ref().is_some_and(|ticket| !ticket.is_current()) {
            return self.shared.query_error(scope, QueryError::Cancelled);
        }
        let (reply, mut receiver) = oneshot::channel();
        let mut owner = QueryEpoch::default();
        let request = QueryRequest {
            query,
            ticket: ticket.clone(),
            owner: owner.ticket(),
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
                scope,
                QueryError::Message("GUI query queue is busy or stopped".into()),
            );
        }
        match rivu_protocol::transport::timeout(duration, &mut receiver).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => self.shared.query_error(
                scope,
                QueryError::Message(format!("GUI query disconnected: {error}")),
            ),
            Err(error) => {
                owner.cancel();
                let response = self.shared.query_error(
                    scope,
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
    #[cfg(test)]
    pub(crate) fn publish_local_error(&self, message: Option<String>) {
        match message {
            Some(message) => self.shared.error(message, false),
            None => self.shared.dismiss_error(),
        }
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
        Arc::clone(
            &self
                .shared
                .published
                .read()
                .observed
                .snapshot()
                .configuration
                .config,
        )
    }
    pub(crate) fn library_revision(&self) -> u64 {
        self.shared
            .published
            .read()
            .observed
            .snapshot()
            .library
            .revision
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
        self.shared.visual.set_waveform_demand(enabled);
    }
    pub(crate) fn set_waveform_presentation_visible(&self, visible: bool) {
        self.shared
            .visual
            .set_waveform_presentation_visible(visible);
    }
    pub(crate) fn consume_current_analysis(
        &self,
        consume: impl FnOnce(&AnalysisFrame) -> bool,
    ) -> bool {
        self.is_connected() && self.shared.visual.consume_current_analysis(consume)
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
    fn rejected_shutdown_allows_explicit_retry_without_automatic_retry() {
        let (_directory, runtime) = fixture();
        let mut handle = runtime.handle.clone();
        let (shutdown, requests) = async_channel::bounded(1);
        handle.shutdown = shutdown;
        handle.request_shutdown().unwrap();
        let request = requests.try_recv().unwrap();
        let shared = Arc::clone(&handle.shared);
        let (notice, notices) = std::sync::mpsc::sync_channel(1);
        let test_thread = thread::current().id();
        *handle.shared.wakeup.write() = Some(Arc::new(move || {
            if thread::current().id() != test_thread {
                return;
            }
            let admitted = shared
                .shutdown_admitted
                .try_lock()
                .expect("shutdown admission mutex held during notification");
            notice.try_send(*admitted).unwrap();
        }));
        assert!(!finish_command(
            &handle.shared,
            matches!(request.command, AppCommand::Shutdown),
            request.reply,
            Ack::new(AckStatus::NotAdmitted, Some("rejected".into()), 1),
            None,
            || panic!("shutdown must not refresh"),
        ));
        assert!(!notices.try_recv().unwrap());
        assert!(
            requests.is_empty(),
            "rejected shutdown automatically retried"
        );
        handle.clear_wakeup();
        handle.request_shutdown().unwrap();
        handle.request_shutdown().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            requests.try_recv().unwrap().command,
            AppCommand::Shutdown
        ));
    }

    #[test]
    fn shutdown_retains_admission_after_failed_unknown_or_successful_ack() {
        let (_directory, runtime) = fixture();
        for status in [AckStatus::Failed, AckStatus::Unknown, AckStatus::Succeeded] {
            let mut handle = runtime.handle.clone();
            let (shutdown, requests) = async_channel::bounded(1);
            handle.shutdown = shutdown;
            handle.request_shutdown().unwrap();
            let request = requests.try_recv().unwrap();
            let (reply, delivery) = oneshot::channel();
            assert_eq!(
                finish_command(
                    &handle.shared,
                    matches!(request.command, AppCommand::Shutdown),
                    Some(reply),
                    Ack::new(status, None, 1),
                    None,
                    || panic!("shutdown must not refresh"),
                ),
                status == AckStatus::Succeeded,
            );
            assert_eq!(
                futures::executor::block_on(delivery).unwrap().ack.status,
                status
            );
            assert!(*handle.shared.shutdown_admitted.lock());
            handle.request_shutdown().unwrap();
            assert!(requests.is_empty());
            *handle.shared.shutdown_admitted.lock() = false;
        }
    }

    #[test]
    fn shutdown_bypasses_full_command_queue_and_is_admitted_only_once() {
        let (_directory, runtime) = fixture();
        let mut handle = runtime.handle.clone();
        let (commands, receiver) = async_channel::bounded(QUEUE_CAPACITY);
        let (shutdown, shutdown_receiver) = async_channel::bounded(1);
        handle.commands = commands;
        handle.shutdown = shutdown;
        for _ in 0..QUEUE_CAPACITY {
            handle.send(Command::DismissError).unwrap();
        }
        assert!(handle.send(Command::DismissError).is_err());
        handle.request_shutdown().unwrap();
        handle.request_shutdown().unwrap();
        assert_eq!(handle.shutdown.len(), 1);
        let (finished, completion) = std::sync::mpsc::sync_channel(1);
        handle.set_disconnect_callback(move || {
            let _ = finished.send(());
        });
        let session = handle.session().unwrap();
        let shared = Arc::clone(&handle.shared);
        let pending = receiver.clone();
        let worker = thread::spawn(move || {
            command_worker(shared, session, receiver, shutdown_receiver);
        });
        completion
            .recv_timeout(Duration::from_secs(5))
            .expect("priority shutdown did not reach Core");
        assert_eq!(pending.len(), QUEUE_CAPACITY);
        assert!(handle.shutdown.is_empty());
        handle.commands.close();
        handle.shutdown.close();
        worker.join().unwrap();
    }

    #[test]
    fn unavailable_shutdown_worker_publishes_visible_failure() {
        let (_directory, runtime) = fixture();
        let mut handle = runtime.handle.clone();
        let (shutdown, receiver) = async_channel::bounded(1);
        handle.shutdown = shutdown;
        drop(receiver);
        assert!(handle.request_shutdown().is_err());
        assert!(
            handle
                .gui_snapshot()
                .system
                .last_error
                .unwrap()
                .contains("GUI quit failed")
        );
        assert!(!*handle.shared.shutdown_admitted.lock());
    }

    fn publish_track_metadata(shared: &Shared, track_id: i64) {
        let mut library = shared.published.read().observed.snapshot().library.clone();
        library.change_metadata(Some(&[track_id]), Some(&[]));
        assert!(shared.merge(StateResponse {
            library: Some(library),
            ..Default::default()
        }));
    }

    #[test]
    fn fatal_disconnect_survives_notification_dismissal_and_worker_retirement() {
        let (directory, runtime) = fixture();
        let mut frontend =
            FrontendRuntime::connect(directory.path(), runtime.handle.instance_id()).unwrap();
        frontend.handle.shared.error("connection lost".into(), true);
        frontend.handle.shared.dismiss_error();
        frontend.handle.shared.error("late failure".into(), true);
        assert_eq!(
            frontend.shutdown().unwrap_err().to_string(),
            "connection lost"
        );
    }

    #[test]
    fn local_and_core_shutdown_do_not_promote_notifications_to_exit_errors() {
        for core_shutdown in [false, true] {
            let (directory, runtime) = fixture();
            let mut frontend =
                FrontendRuntime::connect(directory.path(), runtime.handle.instance_id()).unwrap();
            frontend
                .handle
                .shared
                .error("command rejected".into(), false);
            if core_shutdown {
                let mut system = frontend
                    .handle
                    .shared
                    .published
                    .read()
                    .observed
                    .snapshot()
                    .system
                    .clone();
                system.shutting_down = true;
                assert!(frontend.handle.shared.merge(StateResponse {
                    system: Some(system),
                    ..Default::default()
                }));
                frontend.handle.shared.error("late disconnect".into(), true);
            }
            frontend.shutdown().unwrap();
        }
    }

    #[test]
    fn disconnected_query_observes_playlist_deletion_after_submission() {
        let (_directory, runtime) = fixture();
        let shared = &runtime.handle.shared;
        let mut library = shared.published.read().observed.snapshot().library.clone();
        Arc::make_mut(&mut library.playlist_entry_revisions).insert(7, 12);
        library.change_metadata(Some(&[]), Some(&[7]));
        assert!(shared.merge(StateResponse {
            library: Some(library),
            ..Default::default()
        }));
        let (sender, requests) = async_channel::bounded(1);
        let mut handle = runtime.handle.clone();
        handle.queries = sender;
        let query = || Query::PlaylistEntryLocations {
            playlist_id: 7,
            query: Some("needle".into()),
            entry_ids: vec![11, 12],
        };
        let submitted = query().stamp(handle.gui_snapshot().query_revisions_for(&query()));
        let response = futures::executor::block_on(async {
            let (response, ()) = futures::join!(handle.query_for_gui(query()), async {
                let request = requests.recv().await.unwrap();
                let mut library = shared.published.read().observed.snapshot().library.clone();
                Arc::make_mut(&mut library.playlist_entry_revisions).remove(&7);
                assert!(shared.merge(StateResponse {
                    library: Some(library),
                    ..Default::default()
                }));
                drop(request);
            });
            response
        });
        assert!(matches!(response.result, Err(QueryError::Message(_))));
        assert_ne!(query().stamp(response.revisions), submitted);
        assert_eq!(
            query().stamp(response.revisions),
            query().stamp(handle.gui_snapshot().query_revisions_for(&query()))
        );
    }

    #[test]
    fn successful_ack_survives_failed_refresh_without_replay() {
        let mut ack = Ack::new(AckStatus::Succeeded, Some("owner warning".into()), 42);
        ack.scan_id = Some(11);
        ack.relocation_id = Some(12);
        ack.processing_id = Some(13);
        let expected = serde_json::to_value(&ack).unwrap();
        let mut refreshes = 0;
        let delivery = CommandDelivery::observe(ack, false, || {
            refreshes += 1;
            anyhow::bail!("read failed")
        });
        assert_eq!(refreshes, 1);
        assert_eq!(serde_json::to_value(&delivery.ack).unwrap(), expected);
        assert!(matches!(
            delivery.observation,
            CommandObservation::Unavailable(_)
        ));
        assert!(delivery.observed_result().is_err());
    }

    #[test]
    fn failed_ack_and_successful_observation_remain_independent() {
        let mut ack = Ack::new(AckStatus::Failed, Some("partially committed".into()), 73);
        ack.scan_id = Some(19);
        let expected = serde_json::to_value(&ack).unwrap();
        let delivery = CommandDelivery::observe(ack, false, || Ok(()));
        assert_eq!(delivery.observation, CommandObservation::Refreshed);
        assert_eq!(serde_json::to_value(&delivery.ack).unwrap(), expected);
        assert_eq!(
            delivery.observed_result(),
            Err("partially committed".into())
        );
    }

    #[test]
    fn shutdown_ack_never_requires_a_post_acceptance_read() {
        let delivery =
            CommandDelivery::observe(Ack::new(AckStatus::Succeeded, None, 1), true, || {
                panic!("Shutdown must not refresh after acceptance")
            });
        assert!(delivery.ack.is_success());
        assert_eq!(delivery.observation, CommandObservation::NotRequested);
    }

    #[test]
    fn delayed_watch_announcement_cannot_restore_pre_ack_payload() {
        let (_directory, runtime) = fixture();
        let mut watch = runtime.handle.session().unwrap();
        let old_announcement = watch.get_state(StateSections::default()).unwrap().revisions;
        let delivery = futures::executor::block_on(
            runtime.handle.request_ack(Command::Volume { value: 0.31 }),
        );
        assert!(delivery.observed_result().is_ok());
        let refreshed = runtime.handle.shared.published.read().observed.applied();
        runtime
            .handle
            .shared
            .refresh_announced(&mut watch, old_announcement)
            .unwrap();
        let published = runtime.handle.shared.published.read();
        assert_eq!(published.observed.snapshot().playback.volume, 0.31);
        assert_eq!(published.observed.applied().playback, refreshed.playback);
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
        next.animation_fps = if previous.animation_fps == 60 { 30 } else { 60 };
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
            let mut next = previous.clone();
            next.processing_bypassed = !previous.processing_bypassed;
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
        let ack =
            futures::executor::block_on(runtime.handle.request_ack(Command::DismissError)).ack;
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

    #[test]
    fn logical_timeout_retires_only_the_request_owner_and_keeps_its_reply_receiver() {
        use futures::FutureExt;

        let (_directory, runtime) = fixture();
        let (sender, requests) = async_channel::bounded(1);
        let (notice, notices) = async_channel::bounded(1);
        let mut handle = runtime.handle.clone().with_query_timeout(move |response| {
            notice.try_send(response).unwrap();
        });
        handle.queries = sender;
        let mut source = QueryEpoch::default();
        let ticket = source.ticket();
        // Hold admission so the revision change deterministically follows
        // submission but precedes the logical deadline.
        let (occupied_reply, _occupied_receiver) = oneshot::channel();
        assert!(
            handle
                .queries
                .try_send(QueryRequest {
                    query: Query::LibraryStats,
                    ticket: None,
                    owner: QueryEpoch::default().ticket(),
                    reply: occupied_reply,
                })
                .is_ok()
        );
        let query = Query::Track { track_id: 42 };
        let submitted = query.stamp(handle.gui_snapshot().query_revisions_for(&query));
        let mut pending =
            Box::pin(handle.query_with_ticket(query, Some(ticket.clone()), Duration::ZERO));
        assert!(pending.as_mut().now_or_never().is_none());
        publish_track_metadata(&handle.shared, 42);
        drop(requests.try_recv().unwrap());
        let (response, timed_out) = futures::executor::block_on(async {
            futures::join!(pending, async {
                let request = requests.recv().await.unwrap();
                let timed_out = notices.recv().await.unwrap();
                assert!(!request.owner.is_current());
                assert!(
                    ticket.is_current(),
                    "one timed-out page cannot cancel its source owner"
                );
                assert!(
                    !request.reply.is_canceled(),
                    "timeout must retain physical completion"
                );
                request
                    .reply
                    .send(QueryResponse {
                        revisions: Default::default(),
                        result: Err(QueryError::Cancelled),
                    })
                    .unwrap();
                timed_out
            },)
        });
        let query = Query::Track { track_id: 42 };
        assert_ne!(query.stamp(response.revisions), submitted);
        assert_eq!(
            query.stamp(response.revisions),
            query.stamp(handle.gui_snapshot().query_revisions_for(&query))
        );
        assert!(matches!(response.result, Err(QueryError::Message(_))));
        assert_eq!(response.result.err(), timed_out.result.err());
        assert!(ticket.is_current());
    }

    #[test]
    fn query_worker_cancels_dropped_reply_and_retired_ticket_before_reusing_its_session() {
        use futures::FutureExt;
        use rivu::test_support::ipc::{
            complete_test_handshake, read_test_request, reply_test_query, socket_test_directory,
        };
        use rivu_protocol::{
            response::ViewResponse,
            wire::{WireQuery, WireRequest},
        };
        use std::os::unix::net::UnixListener;

        let (_fixture_directory, runtime) = fixture();
        let directory = socket_test_directory();
        let listener =
            UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap())
                .unwrap();
        let (started, active) = std::sync::mpsc::sync_channel(1);
        let (cancel_seen, cancelled) = std::sync::mpsc::sync_channel(1);
        let (release, resume) = std::sync::mpsc::sync_channel(1);
        let server = thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake(&mut stream);
            let mut previous_id = None;
            for track_id in 0..3 {
                let WireRequest::QueryRetained(query) = read_test_request(&mut stream) else {
                    panic!("expected retained GUI query");
                };
                assert_ne!(previous_id, Some(query.id));
                previous_id = Some(query.id);
                assert!(matches!(query.query, WireQuery::Track { track_id: id } if id == track_id));
                if track_id < 2 {
                    started.send(()).unwrap();
                    assert!(matches!(
                        read_test_request(&mut stream),
                        WireRequest::QueryCancel(id) if id == query.id
                    ));
                    cancel_seen.send(()).unwrap();
                    resume.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                reply_test_query(
                    &mut stream,
                    QueryResponse {
                        revisions: Default::default(),
                        result: if track_id < 2 {
                            Err(QueryError::Cancelled)
                        } else {
                            Ok(ViewResponse::Track(None))
                        },
                    },
                );
            }
        });
        let mut handle = runtime.handle.clone();
        let (sender, receiver) = async_channel::bounded(4);
        handle.queries = sender;
        let shared = Arc::clone(&handle.shared);
        let mut session = client_session_with_cancel(directory.path(), shared.stop.clone());
        session.bind_instance(1).unwrap();
        let worker = thread::spawn(move || query_worker(shared, session, receiver));

        let mut abandoned = Box::pin(handle.query_for_gui(Query::Track { track_id: 0 }));
        assert!(abandoned.as_mut().now_or_never().is_none());
        active.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(abandoned);
        cancelled.recv_timeout(Duration::from_secs(5)).unwrap();

        let mut owner = QueryEpoch::default();
        let query = Query::Track { track_id: 1 };
        let submitted = query.stamp(handle.gui_snapshot().query_revisions_for(&query));
        let mut replaced = Box::pin(handle.query_if_current(query, owner.ticket()));
        assert!(replaced.as_mut().now_or_never().is_none());
        release.send(()).unwrap();
        active.recv_timeout(Duration::from_secs(5)).unwrap();
        owner.cancel();
        cancelled.recv_timeout(Duration::from_secs(5)).unwrap();
        // Logical retirement must not free this worker before the server's
        // physical completion, even though its result is already obsolete.
        assert!(replaced.as_mut().now_or_never().is_none());
        publish_track_metadata(&handle.shared, 1);
        release.send(()).unwrap();
        let response = futures::executor::block_on(replaced);
        assert!(matches!(response.result, Err(QueryError::Cancelled)));
        let query = Query::Track { track_id: 1 };
        assert_ne!(query.stamp(response.revisions), submitted);
        assert_eq!(
            query.stamp(response.revisions),
            query.stamp(handle.gui_snapshot().query_revisions_for(&query))
        );
        assert!(matches!(
            futures::executor::block_on(handle.query_for_gui(Query::Track { track_id: 2 })).result,
            Ok(ViewResponse::Track(None))
        ));
        handle.queries.close();
        worker.join().unwrap();
        server.join().unwrap();
    }
}
