use crate::app::AppHandle;
use crate::core::CoreHandle;
use anyhow::{Context, Result, bail};
use async_channel::{Receiver, Sender};
use async_io::Async;
use bytes::BytesMut;
use futures::{
    FutureExt, StreamExt,
    io::{AsyncRead, AsyncReadExt},
    stream::FuturesUnordered,
};
use rand::RngExt;
use rivu_protocol::AppCommand;
use rivu_protocol::transport::{
    bridge_timeout, consume_request_frame, decode_frame_length, read_into_pending, timeout,
    timeout_at, write_error_frame, write_frame_buffered,
};
use rivu_protocol::wire::{
    CommandReceipt, CommandRefusal, CommandReply, HELLO_TIMEOUT, HelloResponse, MAX_CLIENTS,
    MAX_REQUEST, SessionState, UNKNOWN_INSTANCE, WireRequest, WireResponse, wire_state,
};
use rivu_protocol::{
    model::Query,
    query_tracking::QueryEpoch,
    response::{
        QueryError, QueryResponse, StateRequest, StateResponse, StateSections, StateWatch,
        TaskOutcome,
    },
};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
type UnixStream = Async<std::os::unix::net::UnixStream>;
type UnixListener = Async<std::os::unix::net::UnixListener>;
/// Holds the abstract listener before any database or audio startup side effects.
/// The kernel releases the address after the last listening descriptor closes.
pub struct SocketOwner {
    data_dir: PathBuf,
    instance_id: u32,
    listener: std::os::unix::net::UnixListener,
}

impl SocketOwner {
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir)?;
        let address = rivu_protocol::wire::address(data_dir)?;
        let listener = std::os::unix::net::UnixListener::bind_addr(&address)
            .context("Cannot acquire Rivu IPC address; another Rivu may own this data directory")?;
        listener.set_nonblocking(true)?;
        let instance_id = loop {
            let id = rand::rng().random();
            if id != UNKNOWN_INSTANCE {
                break id;
            }
        };
        Ok(Self {
            data_dir: data_dir.to_owned(),
            instance_id,
            listener,
        })
    }

    pub fn instance_id(&self) -> u32 {
        self.instance_id
    }

    pub(crate) fn migration_stdio(&self) -> std::io::Result<std::process::Stdio> {
        let fd: std::os::fd::OwnedFd = self.listener.try_clone()?.into();
        Ok(fd.into())
    }
}

/// Async transport owned by App; application execution stays alive until this
/// service and every connection retire, before SocketOwner releases authority.
pub struct Server {
    data_dir: PathBuf,
    instance_id: u32,
    shutdown: Sender<()>,
    worker: Option<async_executor::Task<()>>,
}

impl Server {
    pub(crate) fn start(
        owner: &SocketOwner,
        handle: CoreHandle,
        app: AppHandle,
        executor: &std::sync::Arc<async_executor::Executor<'static>>,
    ) -> Result<Self> {
        let data_dir = owner.data_dir.clone();
        let listener = Async::new(owner.listener.try_clone()?)?;
        let (shutdown, shutdown_rx) = async_channel::bounded(1);
        let instance_id = owner.instance_id();
        let executor_handle = executor.clone();
        let supervisor = app.clone();
        let worker = executor.spawn(async move {
            if std::panic::AssertUnwindSafe(run_server(
                listener,
                handle,
                app,
                shutdown_rx,
                executor_handle,
                instance_id,
            ))
            .catch_unwind()
            .await
            .is_err()
            {
                supervisor.fail_service(anyhow::anyhow!("Main IPC server panicked"));
            }
        });
        tracing::info!(path = %data_dir.display(), "ipc_server_started");
        Ok(Self {
            data_dir,
            instance_id,
            shutdown,
            worker: Some(worker),
        })
    }

    pub fn instance_id(&self) -> u32 {
        self.instance_id
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        tracing::info!(path = %self.data_dir.display(), "ipc_server_stopping");
        self.shutdown.close();
        if let Some(worker) = self.worker.take() {
            async_io::block_on(worker);
        }
    }
}
async fn run_command(app: &AppHandle, command: AppCommand) -> Result<CommandReceipt> {
    let duration = bridge_timeout(command.wait_timeout());
    timeout(duration, app.request_ack_async(command))
        .await
        .context("Reading IPC command receipt timed out")
}

const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const WAIT_INPUT_BUDGET: usize = 32;

#[derive(Default)]
struct RequestReader {
    pending: BytesMut,
    body_length: Option<usize>, // Validated when the four-byte prefix completes.
    deadline: Option<Instant>,
}

impl RequestReader {
    async fn read(&mut self, stream: &mut (impl AsyncRead + Unpin)) -> Result<Result<WireRequest>> {
        loop {
            if let Some(frame) = self.read_step(stream).await? {
                return Ok(frame);
            }
        }
    }

    // A valid control never owns a reply. Any other frame interrupts the
    // wait and must not be dispatched as a subsequent request.
    async fn read_wait_interrupt(
        &mut self,
        stream: &mut (impl AsyncRead + Unpin),
        mut on_cancel: impl FnMut(u64),
    ) -> Result<()> {
        loop {
            for _ in 0..WAIT_INPUT_BUDGET {
                match self.read_step(stream).await? {
                    Some(Ok(WireRequest::CancelRead(id))) => on_cancel(id),
                    Some(_) => return Ok(()),
                    None => {}
                }
            }
            // Even an always-ready socket (controls or a fragmented body) must yield.
            futures_lite::future::yield_now().await;
        }
    }

    // Perform at most one bounded read. Finish and consume this frame before
    // reading any byte of the next, even if the socket already holds both.
    async fn read_step(
        &mut self,
        stream: &mut (impl AsyncRead + Unpin),
    ) -> Result<Option<Result<WireRequest>>> {
        let receive = async {
            if let Some(length) = self.body_length {
                let need = 4 + length - self.pending.len();
                return read_into_pending(stream, &mut self.pending, need.min(8192)).await;
            }
            // An idle session needs no allocation to wait for its first byte.
            let prefix = &mut [0; 4][..4 - self.pending.len()];
            let read = stream.read(prefix).await?;
            self.pending.extend_from_slice(&prefix[..read]);
            Ok(read)
        };
        let read = if let Some(deadline) = self.deadline {
            if Instant::now() >= deadline {
                bail!("IPC request frame completion timed out");
            }
            timeout_at(deadline, receive)
                .await
                .context("IPC request frame completion timed out")?
        } else {
            receive.await
        }
        .context("Reading IPC frame")?;
        if read == 0 {
            bail!("IPC connection closed while reading frame");
        }
        let now = Instant::now();
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            bail!("IPC request frame completion timed out");
        }
        if self.body_length.is_none()
            && let Some(prefix) = self.pending.first_chunk::<4>()
        {
            self.body_length = Some(decode_frame_length(*prefix, MAX_REQUEST)?);
        }
        if let Some(length) = self.body_length
            && self.pending.len() == 4 + length
        {
            self.body_length = None;
            self.deadline = None;
            return Ok(Some(consume_request_frame(&mut self.pending, length)));
        }
        // The reader, not the waiting future, owns the first byte's deadline.
        self.deadline.get_or_insert_with(|| now + FRAME_TIMEOUT);
        Ok(None)
    }
}

async fn wait_for_query(
    stream: &mut (impl AsyncRead + Unpin),
    reader: &mut RequestReader,
    handle: &CoreHandle,
    query: Query,
    id: u64,
    shutdown: &Receiver<()>,
) -> Result<Option<Result<QueryResponse>>> {
    let mut owner = QueryEpoch::default();
    let ticket = owner.ticket();
    futures::select_biased! {
        _ = shutdown.recv().fuse() => Ok(None),
        frame = reader.read_wait_interrupt(stream, |cancelled_id| {
            if cancelled_id == id {
                owner.cancel();
            }
        }).fuse() => {
            frame?;
            Ok(Some(Err(anyhow::anyhow!("Concurrent IPC requests are not allowed during a query"))))
        },
        response = handle.query_retained_if_current(query, ticket).fuse() => Ok(Some(Ok(response))),
    }
}

#[derive(Default)]
struct ConnectionWatch {
    subscription: Option<(StateSections, Receiver<()>)>,
}

fn wait_for_state<'a>(
    stream: &'a mut UnixStream,
    reader: &'a mut RequestReader,
    write_buffer: &'a mut Vec<u8>,
    handle: &'a CoreHandle,
    connection: &'a mut ConnectionWatch,
    watch: StateWatch,
    shutdown: &'a Receiver<()>,
) -> impl Future<Output = Result<Option<StateResponse>>> + 'a {
    let request = StateRequest {
        sections: watch.sections,
        since: Some(watch.revisions),
    };
    if connection
        .subscription
        .as_ref()
        .is_some_and(|(sections, _)| *sections != watch.sections)
    {
        connection.subscription = None;
    }
    // Consume the coalesced wakeup before reading the state it announces. A
    // publication after that read must keep its wakeup for the next wait.
    if let Some((_, updates)) = &connection.subscription {
        let _ = updates.try_recv();
    }
    let initial = handle.read_state(request);
    let updates = if initial.sections().is_empty() {
        let registering = connection.subscription.is_none();
        let (_, receiver) = connection.subscription.get_or_insert_with(|| {
            (
                watch.sections,
                handle.subscribe_revisions(crate::core::RevisionInterest::Sections(watch.sections)),
            )
        });
        Some((receiver, registering))
    } else {
        None
    };
    async move {
        let Some((updates, registering)) = updates else {
            return Ok(Some(initial));
        };
        // Only a newly registered receiver can have missed the publication
        // between the initial read and registration. Reconcile it before waiting.
        if registering {
            let _ = updates.try_recv();
            let response = handle.read_state(request);
            if !response.sections().is_empty() {
                return Ok(Some(response));
            }
        }
        loop {
            futures::select_biased! {
                _ = shutdown.recv().fuse() => return Ok(None),
                interrupted = reader.read_wait_interrupt(stream, |_| {}).fuse() => {
                    interrupted?;
                    break;
                },
                changed = updates.recv().fuse() => { if changed.is_err() { return Ok(None); } }
            }
            let response = handle.read_state(request);
            if !response.sections().is_empty() {
                return Ok(Some(response));
            }
        }
        write_error_frame(
            stream,
            write_buffer,
            "Concurrent IPC requests are not allowed during a state wait",
        )
        .await?;
        Ok(None)
    }
}

// True if the task replied and the session can accept another request.
async fn serve_task_wait(
    stream: &mut UnixStream,
    reader: &mut RequestReader,
    write_buffer: &mut Vec<u8>,
    task: impl Future<Output = Result<TaskOutcome, QueryError>>,
    shutdown: &Receiver<()>,
) -> Result<bool> {
    let outcome = futures::select_biased! {
        _ = shutdown.recv().fuse() => return Ok(false),
        frame = reader.read_wait_interrupt(stream, |_| {}).fuse() => {
            frame?;
            None
        }
        outcome = task.fuse() => Some(outcome),
    };
    if let Some(outcome) = outcome {
        write_frame_buffered(stream, &WireResponse::Task(outcome), write_buffer).await?;
        Ok(true)
    } else {
        write_error_frame(
            stream,
            write_buffer,
            "Concurrent IPC requests are not allowed during a task wait",
        )
        .await?;
        Ok(false)
    }
}

async fn serve_connection(
    mut stream: UnixStream,
    handle: CoreHandle,
    app: AppHandle,
    instance_id: u32,
    shutdown: Receiver<()>,
) -> Result<()> {
    let mut reader = RequestReader::default();
    let mut write_buffer = Vec::new();
    let mut session = SessionState::Unbound;
    let mut state_watch = ConnectionWatch::default();
    let mut desktop_registration = None;
    loop {
        // Socket readiness alone does not impose a cooperative execution budget.
        futures_lite::future::yield_now().await;
        if shutdown.is_closed() {
            return Ok(());
        }
        let decoded = if session.is_bound() {
            futures::select! {
                frame = reader.read(&mut stream).fuse() => frame?,
                _ = shutdown.recv().fuse() => return Ok(()),
            }
        } else {
            futures::select! {
                frame = timeout(HELLO_TIMEOUT, reader.read(&mut stream)).fuse() => {
                    match frame {
                        Ok(frame) => frame?,
                        Err(_) => {
                            tracing::warn!("ipc_hello_timeout");
                            return Ok(());
                        }
                    }
                }
                _ = shutdown.recv().fuse() => return Ok(()),
            }
        };
        let request = match decoded {
            Ok(request) => request,
            Err(error) => {
                if session.is_bound()
                    && let Some(refusal) = error.downcast_ref::<CommandRefusal>()
                {
                    tracing::warn!(error = %refusal, "ipc_invalid_command");
                    let response = WireResponse::Command(CommandReply::Refused(refusal.clone()));
                    write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
                    return Ok(());
                }
                tracing::warn!(error = %error, "ipc_invalid_request");
                write_error_frame(
                    &mut stream,
                    &mut write_buffer,
                    format!("Invalid IPC request: {error}"),
                )
                .await?;
                return Ok(());
            }
        };
        if !session.is_bound() && !matches!(request, WireRequest::Hello(_)) {
            tracing::warn!("ipc_hello_required");
            write_error_frame(
                &mut stream,
                &mut write_buffer,
                "IPC session requires a Hello handshake",
            )
            .await?;
            continue;
        }
        match request {
            WireRequest::Hello(sections) => {
                if session.is_bound() {
                    tracing::warn!("ipc_duplicate_hello");
                    write_error_frame(
                        &mut stream,
                        &mut write_buffer,
                        "IPC session is already handshaken",
                    )
                    .await?;
                    continue;
                }
                let response = WireResponse::Hello(HelloResponse {
                    instance_id: instance_id.to_le_bytes(),
                    state: wire_state(handle.state(sections)),
                });
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
                session = SessionState::Bound(instance_id);
            }
            WireRequest::ReadState(request) => {
                let response = WireResponse::State(wire_state(handle.read_state(request)));
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
            }
            WireRequest::Query(read) => {
                let query = match Query::try_from(read.query) {
                    Ok(query) => query,
                    Err(error) => {
                        tracing::warn!(error = %error, "ipc_invalid_query");
                        write_error_frame(
                            &mut stream,
                            &mut write_buffer,
                            format!("Invalid IPC query: {error}"),
                        )
                        .await?;
                        continue;
                    }
                };
                let Some(response) =
                    wait_for_query(&mut stream, &mut reader, &handle, query, read.id, &shutdown)
                        .await?
                else {
                    return Ok(());
                };
                let response = match response {
                    Ok(response) => WireResponse::Query(response),
                    Err(error) => {
                        write_error_frame(&mut stream, &mut write_buffer, error.to_string())
                            .await?;
                        return Ok(());
                    }
                };
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
            }
            // Cancellation arriving after its reply is deliberately inert.
            WireRequest::CancelRead(_) => {}
            WireRequest::RegisterDesktop => {
                let result = if desktop_registration.is_some() {
                    Err(anyhow::anyhow!(
                        "Desktop already registered on this session"
                    ))
                } else {
                    desktop_registration = Some(app.register_desktop());
                    Ok(())
                };
                let ack = CommandReceipt::from_result(result, handle.publication_revision());
                let response = WireResponse::Command(ack.into());
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
            }
            WireRequest::Command(command) => {
                let command = match AppCommand::try_from(command) {
                    Ok(command) => command,
                    Err(error) => {
                        tracing::warn!(error = %error, "ipc_invalid_command");
                        let response = WireResponse::Command(CommandReply::Refused(
                            CommandRefusal::InvalidCommand(format!("Invalid IPC command: {error}")),
                        ));
                        write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
                        continue;
                    }
                };
                let response = match run_command(&app, command).await {
                    Ok(ack) => WireResponse::Command(ack.into()),
                    Err(error) => {
                        tracing::warn!(error = %error, "ipc_command_failed");
                        write_error_frame(
                            &mut stream,
                            &mut write_buffer,
                            format!("IPC command failed: {error:#}"),
                        )
                        .await?;
                        continue;
                    }
                };
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
            }
            WireRequest::LookupTask(target) => {
                let response = match handle.lookup_task(target).await {
                    Ok(status) => WireResponse::TaskLookup(status),
                    Err(error) => WireResponse::Error(format!("Task lookup failed: {error:#}")),
                };
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
            }
            WireRequest::WaitTask(target) => {
                if !serve_task_wait(
                    &mut stream,
                    &mut reader,
                    &mut write_buffer,
                    handle.wait_task(target),
                    &shutdown,
                )
                .await?
                {
                    return Ok(());
                }
            }
            WireRequest::WaitState(watch) => {
                let Some(state) = wait_for_state(
                    &mut stream,
                    &mut reader,
                    &mut write_buffer,
                    &handle,
                    &mut state_watch,
                    watch,
                    &shutdown,
                )
                .await?
                else {
                    return Ok(());
                };
                let response = WireResponse::State(wire_state(state));
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
            }
        }
    }
}

async fn run_server(
    listener: UnixListener,
    handle: CoreHandle,
    app: AppHandle,
    shutdown: Receiver<()>,
    executor: std::sync::Arc<async_executor::Executor<'static>>,
    instance_id: u32,
) {
    let mut clients = FuturesUnordered::new();
    enum Event {
        Accepted(std::io::Result<(UnixStream, std::os::unix::net::SocketAddr)>),
        Completed(Option<Result<()>>),
        Shutdown,
    }
    loop {
        futures_lite::future::yield_now().await;
        let event = {
            let completed = async {
                if clients.is_empty() {
                    std::future::pending().await
                } else {
                    clients.next().await
                }
            };
            futures::select! {
                accepted = listener.accept().fuse() => Event::Accepted(accepted),
                _ = shutdown.recv().fuse() => Event::Shutdown,
                result = completed.fuse() => Event::Completed(result),
            }
        };
        match event {
            Event::Accepted(accepted) => {
                let (mut stream, _) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => {
                        tracing::warn!(error = %error, "ipc_accept_failed");
                        continue;
                    }
                };
                if let Err(error) = rivu_protocol::wire::verify_peer(&stream) {
                    tracing::warn!(error = %error, "ipc_peer_rejected");
                    continue;
                }
                // Reclaim completed sessions without polling idle sockets.
                for _ in 0..MAX_CLIENTS {
                    match clients.next().now_or_never() {
                        Some(Some(result)) => log_connection_result(result),
                        _ => break,
                    }
                }
                if clients.len() < MAX_CLIENTS {
                    let connection = serve_connection(
                        stream,
                        handle.clone(),
                        app.clone(),
                        instance_id,
                        shutdown.clone(),
                    );
                    clients.push(executor.spawn(async move {
                        std::panic::AssertUnwindSafe(connection)
                            .catch_unwind()
                            .await
                            .unwrap_or_else(|_| {
                                Err(anyhow::anyhow!("IPC connection task panicked"))
                            })
                    }));
                } else {
                    tracing::warn!(
                        clients = clients.len(),
                        max_clients = MAX_CLIENTS,
                        "ipc_client_limit_reached"
                    );
                    let mut buffer = Vec::new();
                    if let Err(error) =
                        write_error_frame(&mut stream, &mut buffer, "IPC server is busy").await
                    {
                        tracing::debug!(error = %error, "ipc_busy_response_failed");
                    }
                }
            }
            Event::Shutdown => break,
            Event::Completed(result) => {
                if let Some(result) = result {
                    log_connection_result(result);
                }
            }
        }
    }
    // Idle sessions and query waits observe shutdown. Accepted commands drain
    // their replies; cancelling query transport never interrupts its SQL owner.
    // The deadline bounds transport retirement, not physical Core ownership.
    let _ = timeout(Duration::from_secs(120), async {
        while let Some(result) = clients.next().await {
            log_connection_result(result);
        }
    })
    .await;
}

fn log_connection_result(result: Result<()>) {
    if let Err(error) = result {
        tracing::debug!(error = %error, "ipc_connection_ended");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::AsyncWriteExt;
    use rivu_protocol::transport::encode_request_frame_into;
    use std::{
        future::Future,
        task::{Context as TaskContext, Waker},
    };

    fn socket_pair() -> (UnixStream, UnixStream) {
        let (write, read) = std::os::unix::net::UnixStream::pair().unwrap();
        (Async::new(write).unwrap(), Async::new(read).unwrap())
    }

    #[test]
    fn idle_request_wait_accepts_a_frame_after_the_completion_interval() {
        async_io::block_on(async {
            let (mut writer, mut stream) = socket_pair();
            let mut reader = RequestReader::default();
            let receive = reader.read(&mut stream);
            let send = async {
                async_io::Timer::after(FRAME_TIMEOUT * 2).await;
                let mut encoded = Vec::new();
                encode_request_frame_into(&WireRequest::Hello(StateSections::ALL), &mut encoded)
                    .unwrap();
                writer.write_all(&encoded).await.unwrap();
            };
            let (frame, _) = futures::join!(receive, send);
            assert!(matches!(
                frame.unwrap().unwrap(),
                WireRequest::Hello(sections) if sections == StateSections::ALL
            ));
        });
    }

    #[test]
    fn partial_request_deadline_survives_cancel_and_additional_bytes() {
        async_io::block_on(async {
            let (mut writer, mut stream) = socket_pair();
            let mut reader = RequestReader::default();
            let mut encoded = Vec::new();
            encode_request_frame_into(&WireRequest::Hello(StateSections::ALL), &mut encoded)
                .unwrap();
            writer.write_all(&encoded[..1]).await.unwrap();
            {
                let mut reading = std::pin::pin!(reader.read(&mut stream));
                let mut context = TaskContext::from_waker(Waker::noop());
                assert!(reading.as_mut().poll(&mut context).is_pending());
            }
            let deadline = reader.deadline.expect("first prefix byte starts the clock");
            writer
                .write_all(&encoded[1..encoded.len() - 1])
                .await
                .unwrap();
            assert!(
                reader
                    .read_wait_interrupt(&mut stream, |_| {})
                    .now_or_never()
                    .is_none()
            );
            assert_eq!(reader.deadline, Some(deadline));
            // The final byte is ready, but the original frame deadline expired
            // while its waiter was cancelled. A ready read must not win here.
            reader.deadline = Some(Instant::now() - FRAME_TIMEOUT);
            writer
                .write_all(&encoded[encoded.len() - 1..])
                .await
                .unwrap();
            assert!(matches!(
                reader.read(&mut stream).await,
                Err(error) if error.to_string().contains("frame completion timed out")
            ));
        });
    }

    #[test]
    fn query_reply_preserves_an_incomplete_next_request() {
        async_io::block_on(async {
            let mut frame = Vec::new();
            encode_request_frame_into(&WireRequest::RegisterDesktop, &mut frame).unwrap();
            for split in [1, frame.len() - 1] {
                let (handle, queries) = CoreHandle::test_query_port();
                let (mut peer, mut stream) = socket_pair();
                let mut reader = RequestReader::default();
                let (_stop, shutdown) = async_channel::bounded(1);
                peer.write_all(&frame[..split]).await.unwrap();
                let mut waiting = Box::pin(wait_for_query(
                    &mut stream,
                    &mut reader,
                    &handle,
                    Query::Track { track_id: 1 },
                    41,
                    &shutdown,
                ));
                assert!(waiting.as_mut().now_or_never().is_none());
                let reply = queries.receive().now_or_never().unwrap();
                reply.respond(QueryResponse {
                    revisions: Default::default(),
                    result: Ok(rivu_protocol::response::ViewResponse::Track(None)),
                });
                assert!(waiting.await.unwrap().unwrap().unwrap().result.is_ok());

                peer.write_all(&frame[split..]).await.unwrap();
                assert!(matches!(
                    reader.read(&mut stream).await.unwrap().unwrap(),
                    WireRequest::RegisterDesktop
                ));
            }
        });
    }

    #[test]
    fn completed_request_resets_the_deadline_for_the_next_frame() {
        async_io::block_on(async {
            let (mut writer, mut stream) = socket_pair();
            let mut reader = RequestReader::default();
            let mut encoded = Vec::new();
            encode_request_frame_into(&WireRequest::Hello(StateSections::ALL), &mut encoded)
                .unwrap();
            writer.write_all(&encoded).await.unwrap();
            assert!(matches!(
                reader.read(&mut stream).await.unwrap().unwrap(),
                WireRequest::Hello(sections) if sections == StateSections::ALL
            ));
            async_io::Timer::after(FRAME_TIMEOUT * 2).await;
            encode_request_frame_into(
                &WireRequest::ReadState(StateSections::ALL.into()),
                &mut encoded,
            )
            .unwrap();
            writer.write_all(&encoded).await.unwrap();
            assert!(matches!(
                reader.read(&mut stream).await.unwrap().unwrap(),
                WireRequest::ReadState(request) if request == StateSections::ALL.into()
            ));
        });
    }

    #[test]
    fn query_wait_eof_and_shutdown_cancel_the_core_reply_receiver() {
        if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(20)) {
            return;
        }
        for shutdown_connection in [false, true] {
            async_io::block_on(async {
                let (handle, queries) = CoreHandle::test_query_port();
                let (peer, mut stream) = socket_pair();
                let mut reader = RequestReader::default();
                let (stop, shutdown) = async_channel::bounded(1);
                let mut waiting = Box::pin(wait_for_query(
                    &mut stream,
                    &mut reader,
                    &handle,
                    Query::LibraryStats,
                    41,
                    &shutdown,
                ));
                assert!(waiting.as_mut().now_or_never().is_none());
                let reply = timeout(Duration::from_secs(2), queries.receive())
                    .await
                    .unwrap();
                assert!(!reply.is_cancelled());
                if shutdown_connection {
                    stop.close();
                } else {
                    drop(peer);
                }
                let result = timeout(Duration::from_secs(2), waiting).await.unwrap();
                if shutdown_connection {
                    assert!(result.unwrap().is_none());
                } else {
                    assert!(result.is_err());
                }
                assert!(reply.is_cancelled());
            });
        }
    }

    #[test]
    fn query_cancel_flood_yields_to_executor_peers_and_keeps_terminal_progress() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        struct CancelFlood {
            frame: Vec<u8>,
            consumed: Arc<AtomicUsize>,
            limit: usize,
        }

        impl AsyncRead for CancelFlood {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _context: &mut TaskContext<'_>,
                output: &mut [u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                let this = self.get_mut();
                let consumed = this.consumed.load(Ordering::Relaxed);
                // Always ready, but fail finitely if a missing yield monopolizes
                // one executor tick. No timer or competing OS thread is needed.
                if consumed >= this.limit {
                    return std::task::Poll::Ready(Err(std::io::Error::other(
                        "query cancellation flood exceeded its cooperative budget",
                    )));
                }
                let offset = consumed % this.frame.len();
                let count = output
                    .len()
                    .min(this.frame.len() - offset)
                    .min(this.limit - consumed);
                output[..count].copy_from_slice(&this.frame[offset..offset + count]);
                this.consumed.store(consumed + count, Ordering::Relaxed);
                std::task::Poll::Ready(Ok(count))
            }
        }

        for shutdown_connection in [false, true] {
            let executor = async_executor::Executor::new();
            let (handle, queries) = CoreHandle::test_query_port();
            let (stop, shutdown) = async_channel::bounded(1);
            let mut encoded = Vec::new();
            encode_request_frame_into(&WireRequest::CancelRead(40), &mut encoded).unwrap();
            let stale = encoded.repeat(WAIT_INPUT_BUDGET * 4);
            let mut reader = RequestReader::default();
            encode_request_frame_into(&WireRequest::CancelRead(41), &mut encoded).unwrap();
            let consumed = Arc::new(AtomicUsize::new(0));
            let flood = CancelFlood {
                limit: encoded.len() * WAIT_INPUT_BUDGET * 16,
                frame: encoded,
                consumed: consumed.clone(),
            };
            let mut input = futures::io::Cursor::new(stale).chain(flood);
            let waiting = executor.spawn(async move {
                wait_for_query(
                    &mut input,
                    &mut reader,
                    &handle,
                    Query::LibraryStats,
                    41,
                    &shutdown,
                )
                .await
            });

            assert!(executor.try_tick());
            assert!(!waiting.is_finished());
            assert_eq!(
                consumed.load(Ordering::Relaxed),
                0,
                "one tick must not drain the stale cancellation backlog"
            );
            let reply = queries
                .receive()
                .now_or_never()
                .expect("stale cancellations must not starve query admission");
            assert!(reply.ticket_is_current());
            for _ in 0..8 {
                assert!(executor.try_tick());
                if !reply.ticket_is_current() {
                    break;
                }
            }
            assert!(consumed.load(Ordering::Relaxed) > 0);
            assert!(!reply.ticket_is_current());

            // The cancellation source remains ready throughout these ticks.
            // A distinct task on the same executor must still finish.
            let sibling = executor.spawn(async {});
            for _ in 0..4 {
                assert!(executor.try_tick());
            }
            assert!(sibling.now_or_never().is_some());
            assert!(!waiting.is_finished());
            assert!(
                !reply.is_cancelled(),
                "logical cancellation must keep the physical reply slot"
            );

            let reply = if shutdown_connection {
                stop.close();
                Some(reply)
            } else {
                reply.respond(QueryResponse {
                    revisions: Default::default(),
                    result: Err(rivu_protocol::response::QueryError::Cancelled),
                });
                None
            };
            for _ in 0..4 {
                let _ = executor.try_tick();
            }
            let result = waiting
                .now_or_never()
                .expect("query reply and shutdown must progress during cancellation flood")
                .unwrap();
            if let Some(reply) = reply {
                assert!(result.is_none());
                assert!(reply.is_cancelled());
            } else {
                assert!(matches!(
                    result.unwrap().unwrap().result,
                    Err(rivu_protocol::response::QueryError::Cancelled)
                ));
            }
        }
    }

    #[test]
    fn identified_cancel_retires_only_its_owner_and_retains_the_physical_reply() {
        async_io::block_on(async {
            let (handle, queries) = CoreHandle::test_query_port();
            let (mut peer, mut stream) = socket_pair();
            let mut reader = RequestReader::default();
            let (_stop, shutdown) = async_channel::bounded(1);
            let mut encoded = Vec::new();
            for id in [41, 42] {
                // Include a delayed cancellation from the previous request before
                // admitting this one. A stale control must not poison its ticket.
                encode_request_frame_into(&WireRequest::CancelRead(id - 1), &mut encoded).unwrap();
                peer.write_all(&encoded).await.unwrap();
                let mut waiting = Box::pin(wait_for_query(
                    &mut stream,
                    &mut reader,
                    &handle,
                    Query::LibraryStats,
                    id,
                    &shutdown,
                ));
                assert!(waiting.as_mut().now_or_never().is_none());
                let reply = timeout(Duration::from_secs(2), queries.receive())
                    .await
                    .unwrap();
                assert!(reply.ticket_is_current());
                assert!(!reply.is_cancelled());

                encode_request_frame_into(&WireRequest::CancelRead(id), &mut encoded).unwrap();
                peer.write_all(&encoded).await.unwrap();
                timeout(
                    Duration::from_secs(2),
                    futures_lite::future::or(
                        async {
                            let _ = waiting.as_mut().await;
                            panic!("cancellation released the physical read before its reply");
                        },
                        reply.owner_cancelled(),
                    ),
                )
                .await
                .unwrap();
                assert!(!reply.ticket_is_current());
                assert!(
                    !reply.is_cancelled(),
                    "cancellation must retain the Core reply receiver"
                );
                assert!(waiting.as_mut().now_or_never().is_none());
                reply.respond(QueryResponse {
                    revisions: Default::default(),
                    result: Err(rivu_protocol::response::QueryError::Cancelled),
                });
                let response = waiting.await.unwrap().unwrap().unwrap();
                assert!(matches!(
                    response.result,
                    Err(rivu_protocol::response::QueryError::Cancelled)
                ));
            }
        });
    }

    #[test]
    fn query_wait_rejects_non_control_without_dispatching_it() {
        async_io::block_on(async {
            let (handle, queries) = CoreHandle::test_query_port();
            let (mut peer, mut stream) = socket_pair();
            let mut reader = RequestReader::default();
            let (_stop, shutdown) = async_channel::bounded(1);
            let mut waiting = Box::pin(wait_for_query(
                &mut stream,
                &mut reader,
                &handle,
                Query::LibraryStats,
                41,
                &shutdown,
            ));
            assert!(waiting.as_mut().now_or_never().is_none());
            let reply = timeout(Duration::from_secs(2), queries.receive())
                .await
                .unwrap();
            let mut encoded = Vec::new();
            encode_request_frame_into(&WireRequest::RegisterDesktop, &mut encoded).unwrap();
            peer.write_all(&encoded).await.unwrap();
            let error = waiting.await.unwrap().unwrap().unwrap_err();
            assert!(error.to_string().contains("Concurrent IPC requests"));
            assert!(reply.is_cancelled());
        });
    }

    #[test]
    fn maximum_request_consumes_no_byte_of_the_next_prefix() {
        async_io::block_on(async {
            let mut bytes = vec![0; 4 + MAX_REQUEST + 1];
            bytes[..4].copy_from_slice(&(MAX_REQUEST as u32).to_le_bytes());
            let mut input = bytes.as_slice();
            let mut reader = RequestReader::default();
            // The body isn't a request, but its legal frame must be consumed
            // in full before reporting the decode error, and no further.
            assert!(reader.read(&mut input).await.unwrap().is_err());
            assert_eq!(input.len(), 1);
        });
    }

    #[test]
    fn adjacent_requests_are_read_one_frame_at_a_time() {
        async_io::block_on(async {
            let mut first = Vec::new();
            encode_request_frame_into(
                &WireRequest::ReadState(StateSections::ALL.into()),
                &mut first,
            )
            .unwrap();
            let mut second = Vec::new();
            encode_request_frame_into(
                &WireRequest::WaitState(rivu_protocol::response::StateRevisions::default().into()),
                &mut second,
            )
            .unwrap();
            let mut pipeline = first.clone();
            pipeline.extend_from_slice(&second);
            let mut input = pipeline.as_slice();
            let mut reader = RequestReader::default();
            assert!(reader.read_step(&mut input).await.unwrap().is_none());
            assert_eq!(input, &pipeline[4..]);
            assert!(matches!(
                reader.read(&mut input).await.unwrap().unwrap(),
                WireRequest::ReadState(_)
            ));
            assert_eq!(input, second.as_slice());
            assert!(matches!(
                reader.read(&mut input).await.unwrap().unwrap(),
                WireRequest::WaitState(_)
            ));
            assert!(input.is_empty());
        });
    }

    #[test]
    fn query_wait_fragmented_oversized_prefix_rejects_without_reading_its_body() {
        for split in 1..4 {
            async_io::block_on(async {
                let (handle, queries) = CoreHandle::test_query_port();
                let (mut peer, mut stream) = socket_pair();
                let mut reader = RequestReader::default();
                let (_stop, shutdown) = async_channel::bounded(1);
                let mut waiting = Box::pin(wait_for_query(
                    &mut stream,
                    &mut reader,
                    &handle,
                    Query::LibraryStats,
                    41,
                    &shutdown,
                ));
                assert!(waiting.as_mut().now_or_never().is_none());
                let reply = timeout(Duration::from_secs(2), queries.receive())
                    .await
                    .unwrap();
                let prefix = (MAX_REQUEST as u32 + 1).to_le_bytes();
                peer.write_all(&prefix[..split]).await.unwrap();
                assert!(waiting.as_mut().now_or_never().is_none());
                assert!(!reply.is_cancelled());
                let body = [0x5a; 16];
                let mut remaining = prefix[split..].to_vec();
                remaining.extend_from_slice(&body);
                peer.write_all(&remaining).await.unwrap();
                assert!(
                    timeout(Duration::from_secs(2), waiting)
                        .await
                        .unwrap()
                        .is_err()
                );
                assert!(reply.is_cancelled());
                assert_eq!(reader.pending.as_ref(), prefix.as_slice());
                let mut unread = [0; 16];
                timeout(Duration::from_secs(2), stream.read_exact(&mut unread))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(unread, body);
            });
        }
    }

    #[test]
    fn query_wait_silent_partial_expiry_releases_reply() {
        async_io::block_on(async {
            let (handle, queries) = CoreHandle::test_query_port();
            let (mut peer, mut stream) = socket_pair();
            let mut reader = RequestReader::default();
            let (_stop, shutdown) = async_channel::bounded(1);
            let waiting = wait_for_query(
                &mut stream,
                &mut reader,
                &handle,
                Query::LibraryStats,
                41,
                &shutdown,
            );
            let send_partial = async {
                let reply = timeout(Duration::from_secs(2), queries.receive())
                    .await
                    .unwrap();
                // Start the frame deadline only after Core holds the reply slot.
                peer.write_all(&[1]).await.unwrap();
                reply
            };
            let (result, reply) = timeout(FRAME_TIMEOUT * 2, async {
                futures::join!(waiting, send_partial)
            })
            .await
            .unwrap();
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("frame completion timed out")
            );
            assert!(reply.is_cancelled());
        });
    }

    #[test]
    fn wait_interrupt_yields_without_losing_the_next_frame() {
        async_io::block_on(async {
            let mut frame = Vec::new();
            encode_request_frame_into(&WireRequest::CancelRead(42), &mut frame).unwrap();
            let mut bytes = frame.repeat(WAIT_INPUT_BUDGET * 2);
            encode_request_frame_into(&WireRequest::RegisterDesktop, &mut frame).unwrap();
            bytes.extend_from_slice(&frame);
            let mut input = bytes.as_slice();
            let mut count = 0;
            let mut reader = RequestReader::default();
            let mut waiting = Box::pin(reader.read_wait_interrupt(&mut input, |id| {
                assert_eq!(id, 42);
                count += 1;
            }));
            assert!(waiting.as_mut().now_or_never().is_none());
            waiting.await.unwrap();
            assert_eq!(count, WAIT_INPUT_BUDGET * 2);
            assert!(input.is_empty());
        });
    }

    #[test]
    fn late_query_cancel_does_not_retire_a_task_wait() {
        use rivu_protocol::{
            model::{RelocationPhase, RelocationTaskStatus},
            transport::read_response_frame,
        };
        async_io::block_on(async {
            for concurrent_request in [false, true] {
                let (mut peer, mut stream) = socket_pair();
                let mut reader = RequestReader::default();
                let mut write_buffer = Vec::new();
                let (_stop, shutdown) = async_channel::bounded(1);
                let (complete, result) = futures::channel::oneshot::channel();
                let mut waiting = Box::pin(serve_task_wait(
                    &mut stream,
                    &mut reader,
                    &mut write_buffer,
                    async { result.await.unwrap() },
                    &shutdown,
                ));
                let mut frame = Vec::new();
                encode_request_frame_into(&WireRequest::CancelRead(42), &mut frame).unwrap();
                peer.write_all(&frame).await.unwrap();
                assert!(waiting.as_mut().now_or_never().is_none());
                if concurrent_request {
                    encode_request_frame_into(&WireRequest::RegisterDesktop, &mut frame).unwrap();
                    peer.write_all(&frame).await.unwrap();
                } else {
                    complete
                        .send(Ok(TaskOutcome::Relocation(RelocationTaskStatus {
                            id: 7,
                            source_id: 1,
                            target_id: 2,
                            phase: RelocationPhase::Completed,
                            error: None,
                        })))
                        .unwrap();
                }
                let keep_session = timeout(Duration::from_secs(2), waiting)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(keep_session, !concurrent_request);
                drop(stream);
                let response = read_response_frame(&mut peer, &mut BytesMut::new())
                    .await
                    .unwrap();
                if concurrent_request {
                    assert!(
                        matches!(response, WireResponse::Error(message) if message.contains("Concurrent IPC requests"))
                    );
                } else {
                    assert!(
                        matches!(response, WireResponse::Task(Ok(TaskOutcome::Relocation(task))) if task.id == 7)
                    );
                }
            }
        });
    }

    #[test]
    fn state_wait_rechecks_after_registration_without_a_notification() {
        async_io::block_on(async {
            let (handle, _queries) = CoreHandle::test_query_port();
            let (_peer, mut stream) = socket_pair();
            let mut reader = RequestReader::default();
            let mut write_buffer = Vec::new();
            let (_stop, shutdown) = async_channel::bounded(1);
            let baseline = handle.state_revisions();
            let mut connection = ConnectionWatch::default();
            let waiting = wait_for_state(
                &mut stream,
                &mut reader,
                &mut write_buffer,
                &handle,
                &mut connection,
                baseline.into(),
                &shutdown,
            );
            assert_eq!(handle.test_revision_subscriptions().1, 1);
            // Simulate publication whose wakeup was missed at registration. The
            // state recheck must complete even though no channel item exists.
            let current = handle.test_advance_revision(false);
            assert!(matches!(
                timeout(Duration::from_secs(2), waiting).await.unwrap().unwrap(),
                Some(state) if state.revisions == current && state.sections().library
            ));
            assert_eq!(handle.test_revision_subscriptions(), (1, 1));
            let first = connection.subscription.as_ref().unwrap().1.clone();
            // An immediate delta consumes its coalesced notification and reuses the receiver.
            let current = handle.test_advance_revision(true);
            assert!(!first.is_empty());
            assert!(matches!(
                wait_for_state(
                    &mut stream, &mut reader, &mut write_buffer, &handle, &mut connection, baseline.into(), &shutdown,
                ).await.unwrap(),
                Some(state) if state.revisions == current && state.sections().library
            ));
            assert!(first.is_empty());
            let waiting = wait_for_state(
                &mut stream,
                &mut reader,
                &mut write_buffer,
                &handle,
                &mut connection,
                current.into(),
                &shutdown,
            );
            handle.test_advance_revision(true);
            assert!(waiting.await.unwrap().unwrap().sections().library);
            assert!(first.same_channel(&connection.subscription.as_ref().unwrap().1));
            drop(first);
            // Changing selection retires the old registration even if the new
            // wait finds an immediate delta and needs no receiver of its own.
            let library = StateWatch {
                revisions: baseline,
                sections: StateSections {
                    library: true,
                    ..Default::default()
                },
            };
            assert!(
                wait_for_state(
                    &mut stream,
                    &mut reader,
                    &mut write_buffer,
                    &handle,
                    &mut connection,
                    library,
                    &shutdown,
                )
                .await
                .unwrap()
                .unwrap()
                .sections()
                .library
            );
            assert!(connection.subscription.is_none());
            handle.test_advance_revision(true);
            assert_eq!(handle.test_revision_subscriptions(), (0, 0));
        });
    }

    #[test]
    fn state_watch_coalesces_notifications_and_releases_on_connection_exit() {
        use rivu_protocol::transport::read_response_frame;

        if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(20)) {
            return;
        }
        for exit in 0..5 {
            async_io::block_on(async {
                let (handle, _queries) = CoreHandle::test_query_port();
                let (mut peer, mut stream) = socket_pair();
                let mut reader = RequestReader::default();
                let mut write_buffer = Vec::new();
                let (stop, shutdown) = async_channel::bounded(1);
                let baseline = handle.state_revisions();
                let mut connection = ConnectionWatch::default();
                let mut waiting = Box::pin(wait_for_state(
                    &mut stream,
                    &mut reader,
                    &mut write_buffer,
                    &handle,
                    &mut connection,
                    baseline.into(),
                    &shutdown,
                ));
                assert!(waiting.as_mut().now_or_never().is_none());
                let mut cancel = Vec::new();
                encode_request_frame_into(&WireRequest::CancelRead(42), &mut cancel).unwrap();
                peer.write_all(&cancel).await.unwrap();
                assert!(waiting.as_mut().now_or_never().is_none());
                assert_eq!(handle.test_revision_subscriptions(), (1, 1));
                match exit {
                    0 => {
                        handle.test_advance_revision(true);
                        let current = handle.test_advance_revision(true);
                        assert!(matches!(
                            timeout(Duration::from_secs(2), waiting).await.unwrap().unwrap(),
                            Some(state) if state.revisions == current && state.sections().library
                        ));
                    }
                    1 | 4 => {
                        let mut encoded = Vec::new();
                        if exit == 1 {
                            encode_request_frame_into(
                                &WireRequest::ReadState(StateSections::ALL.into()),
                                &mut encoded,
                            )
                            .unwrap();
                        } else {
                            // A cancel tag without its ID is a decode error, not a control.
                            encoded.extend_from_slice(&[
                                2,
                                0,
                                0,
                                0,
                                rivu_protocol::wire::PROTOCOL_VERSION,
                                rivu_protocol::wire::RequestTag::CancelRead as u8,
                            ]);
                        }
                        peer.write_all(&encoded).await.unwrap();
                        assert!(
                            timeout(Duration::from_secs(2), waiting)
                                .await
                                .unwrap()
                                .unwrap()
                                .is_none()
                        );
                        assert!(matches!(
                            timeout(Duration::from_secs(2), read_response_frame(&mut peer, &mut BytesMut::new()))
                                .await.unwrap().unwrap(),
                            WireResponse::Error(message) if message.contains("Concurrent IPC requests")
                        ));
                    }
                    2 => {
                        stop.close();
                        assert!(
                            timeout(Duration::from_secs(2), waiting)
                                .await
                                .unwrap()
                                .unwrap()
                                .is_none()
                        );
                    }
                    _ => {
                        drop(peer);
                        assert!(
                            timeout(Duration::from_secs(2), waiting)
                                .await
                                .unwrap()
                                .is_err()
                        );
                    }
                }
                drop(connection);
                let (slots, live) = handle.test_revision_subscriptions();
                assert_eq!(live, 0);
                assert!(slots <= 1);
                // Starting the next wait must not grow the registry.
                let baseline = handle.state_revisions();
                let (_peer, mut stream) = socket_pair();
                let (_stop, shutdown) = async_channel::bounded(1);
                let mut connection = ConnectionWatch::default();
                let waiting = wait_for_state(
                    &mut stream,
                    &mut reader,
                    &mut write_buffer,
                    &handle,
                    &mut connection,
                    baseline.into(),
                    &shutdown,
                );
                assert_eq!(handle.test_revision_subscriptions(), (1, 1));
                let current = handle.test_advance_revision(true);
                assert!(matches!(
                    timeout(Duration::from_secs(2), waiting).await.unwrap().unwrap(),
                    Some(state) if state.revisions == current && state.sections().library
                ));
                drop(connection);
                let (slots, live) = handle.test_revision_subscriptions();
                assert_eq!(live, 0);
                assert!(slots <= 1);
            });
        }
    }
}
