use crate::app::AppHandle;
use crate::core::CoreHandle;
use anyhow::{Context, Error, Result, bail};
use async_channel::{Receiver, Sender};
use async_io::Async;
use bytes::{BufMut, BytesMut};
use futures::{
    FutureExt, StreamExt,
    io::{AsyncRead, AsyncReadExt},
    stream::FuturesUnordered,
};
use rand::RngExt;
use rivu_protocol::transport::{
    bridge_timeout, read_request_frame, timeout, timeout_at, write_error_frame,
    write_frame_buffered,
};
use rivu_protocol::wire::{
    HELLO_TIMEOUT, HelloResponse, MAX_CLIENTS, MAX_REQUEST, SessionState, UNKNOWN_INSTANCE,
    WireRequest, WireResponse, wire_state,
};
use rivu_protocol::{AppCommand, query_wait_timeout};
use rivu_protocol::{
    model::Query,
    response::{Ack, QueryResponse, StateRevisions},
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
    listener: std::os::unix::net::UnixListener,
}

impl SocketOwner {
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir)?;
        let address = rivu_protocol::wire::address(data_dir)?;
        let listener = std::os::unix::net::UnixListener::bind_addr(&address)
            .context("Cannot acquire Rivu IPC address; another Rivu may own this data directory")?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            data_dir: data_dir.to_owned(),
            listener,
        })
    }
}

/// Async transport owned by App; Core's runtime stays alive until this service
/// and every connection have retired, before SocketOwner releases authority.
pub struct Server {
    data_dir: PathBuf,
    shutdown: Sender<()>,
    worker: Option<async_executor::Task<()>>,
}

impl Server {
    pub(crate) fn start(owner: &SocketOwner, handle: CoreHandle, app: AppHandle) -> Result<Self> {
        let data_dir = owner.data_dir.clone();
        let listener = Async::new(owner.listener.try_clone()?)?;
        let (shutdown, shutdown_rx) = async_channel::bounded(1);
        let executor = handle.executor().clone();
        let worker = executor.spawn(run_server(listener, handle, app, shutdown_rx));
        tracing::info!(path = %data_dir.display(), "ipc_server_started");
        Ok(Self {
            data_dir,
            shutdown,
            worker: Some(worker),
        })
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
async fn run_command(app: &AppHandle, command: AppCommand) -> Result<Ack> {
    let duration = bridge_timeout(command.wait_timeout());
    timeout(duration, app.request_ack_async(command))
        .await
        .context("Reading IPC acknowledgement timed out")
}

async fn run_query(handle: &CoreHandle, query: Query) -> Result<QueryResponse> {
    timeout(
        bridge_timeout(query_wait_timeout()),
        handle.query_async(query),
    )
    .await
    .context("Reading IPC query timed out")
}

const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct RequestReader {
    pending: BytesMut,
    complete: usize,
    deadline: Option<Instant>,
}

impl RequestReader {
    async fn read(&mut self, stream: &mut (impl AsyncRead + Unpin)) -> Result<Result<WireRequest>> {
        while self.complete == 0 {
            self.read_ahead(stream).await?;
        }
        let before = self.pending.len();
        let frame = read_request_frame(stream, &mut self.pending).await?;
        self.complete -= before - self.pending.len();
        Ok(frame)
    }

    async fn read_ahead(&mut self, stream: &mut (impl AsyncRead + Unpin)) -> Result<()> {
        // Complete frames wait untimed for earlier replies. Only the trailing
        // incomplete frame owns a deadline, even when the receive buffer is full.
        let deadline = self.deadline;
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            bail!("IPC request frame completion timed out");
        }
        let tail = self.pending.len() - self.complete;
        let need = if tail < 4 {
            4 - tail
        } else {
            let length = u32::from_le_bytes(
                self.pending[self.complete..self.complete + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            if length > MAX_REQUEST {
                bail!("IPC frame exceeds maximum size");
            }
            4 + length - tail
        };
        let available = 4 + MAX_REQUEST - self.pending.len();
        // Never read past the missing prefix or this frame's body boundary.
        // A fragmented oversized prefix is rejected before any body is read.
        let count = available.min(need).min(8192);
        let receive = async {
            if count == 0 {
                // Full buffers apply backpressure, but a partial tail must still
                // expire without socket activity. Complete-only buffers are idle.
                return std::future::pending().await;
            }
            let read = if tail < 4 {
                // Serial queries without a following frame need no allocation
                // merely to observe EOF or wait for the first request byte.
                let mut prefix = [0; 4];
                let read = stream
                    .read(&mut prefix[..count])
                    .await
                    .context("Reading IPC frame")?;
                assert!(
                    read <= count,
                    "AsyncRead returned more bytes than its buffer"
                );
                self.pending.extend_from_slice(&prefix[..read]);
                read
            } else {
                self.pending.reserve(count);
                let spare = self.pending.chunk_mut();
                // SAFETY: reserve supplies count writable bytes. AsyncRead requires
                // initialized memory; Pending reads do not advance visible length.
                let bytes = unsafe {
                    std::ptr::write_bytes(spare.as_mut_ptr(), 0, count);
                    std::slice::from_raw_parts_mut(spare.as_mut_ptr(), count)
                };
                let read = stream.read(bytes).await.context("Reading IPC frame")?;
                assert!(
                    read <= count,
                    "AsyncRead returned more bytes than its buffer"
                );
                // SAFETY: the count check bounds these initialized received bytes.
                unsafe { self.pending.advance_mut(read) };
                read
            };
            if read == 0 {
                bail!("IPC connection closed while reading frame");
            }
            let now = Instant::now();
            // Start at the first byte, never renew on bytes or cancelled waits.
            let deadline = *self.deadline.get_or_insert_with(|| now + FRAME_TIMEOUT);
            if now >= deadline {
                bail!("IPC request frame completion timed out");
            }
            let tail = self.pending.len() - self.complete;
            if tail >= 4 {
                let length = u32::from_le_bytes(
                    self.pending[self.complete..self.complete + 4]
                        .try_into()
                        .unwrap(),
                ) as usize;
                if length > MAX_REQUEST {
                    bail!("IPC frame exceeds maximum size");
                }
                if tail == 4 + length {
                    self.complete = self.pending.len();
                    self.deadline = None;
                }
            }
            Ok(())
        };
        if let Some(deadline) = deadline {
            timeout_at(deadline, receive)
                .await
                .context("IPC request frame completion timed out")?
        } else {
            receive.await
        }
    }
}

async fn wait_for_query(
    stream: &mut UnixStream,
    reader: &mut RequestReader,
    handle: &CoreHandle,
    query: Query,
    shutdown: &Receiver<()>,
) -> Result<Option<Result<QueryResponse>>> {
    let query = run_query(handle, query).fuse();
    futures::pin_mut!(query);
    loop {
        futures::select! {
            response = query => return Ok(Some(response)),
            _ = shutdown.recv().fuse() => return Ok(None),
            read = reader.read_ahead(stream).fuse() => read?,
        }
    }
}

enum WatchResult {
    Revisions(StateRevisions),
    Request,
}

fn wait_for_revisions<'a>(
    stream: &'a mut UnixStream,
    reader: &'a mut RequestReader,
    pending_request: &'a mut Option<Result<WireRequest>>,
    handle: &'a CoreHandle,
    expected: StateRevisions,
    shutdown: &'a Receiver<()>,
) -> impl Future<Output = Result<Option<WatchResult>>> + 'a {
    let current = handle.state_revisions();
    // Register only a live Watch. The future rechecks after registration so a
    // publish before subscription cannot leave this wait asleep. On exit the
    // next subscription/publication prunes the dropped receiver's sender.
    let updates = (current == expected).then(|| handle.subscribe_revisions());
    async move {
        let Some(updates) = updates else {
            return Ok(Some(WatchResult::Revisions(current)));
        };
        loop {
            let current = handle.state_revisions();
            if current != expected {
                return Ok(Some(WatchResult::Revisions(current)));
            }
            futures::select! {
                changed = updates.recv().fuse() => { if changed.is_err() { return Ok(None); } }
                _ = shutdown.recv().fuse() => return Ok(None),
                frame = reader.read(stream).fuse() => {
                    *pending_request = Some(frame?);
                    return Ok(Some(WatchResult::Request));
                },
            }
        }
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
    let mut pending_request = None;
    let mut write_buffer = Vec::new();
    let mut session = SessionState::Unbound;
    loop {
        // Socket readiness alone does not impose a cooperative execution budget.
        futures_lite::future::yield_now().await;
        if shutdown.is_closed() {
            return Ok(());
        }
        let decoded = if let Some(frame) = pending_request.take() {
            frame
        } else if session.is_bound() {
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
            WireRequest::State(sections) => {
                let response = WireResponse::State(wire_state(handle.state(sections)));
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
            }
            WireRequest::Query(query) => {
                let query = match Query::try_from(query) {
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
                    wait_for_query(&mut stream, &mut reader, &handle, query, &shutdown).await?
                else {
                    return Ok(());
                };
                let response = match response {
                    Ok(response) => WireResponse::Query(response),
                    Err(error) => {
                        tracing::warn!(error = %error, "ipc_query_failed");
                        write_error_frame(
                            &mut stream,
                            &mut write_buffer,
                            format!("IPC query failed: {error:#}"),
                        )
                        .await?;
                        continue;
                    }
                };
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
            }
            WireRequest::Command(command) => {
                let command = match AppCommand::try_from(command) {
                    Ok(command) => command,
                    Err(error) => {
                        tracing::warn!(error = %error, "ipc_invalid_command");
                        write_error_frame(
                            &mut stream,
                            &mut write_buffer,
                            format!("Invalid IPC command: {error}"),
                        )
                        .await?;
                        continue;
                    }
                };
                let response = match run_command(&app, command).await {
                    Ok(response) => WireResponse::Ack(response),
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
            WireRequest::WaitTask(target) => {
                let outcome = futures::select! {
                    outcome = handle.wait_task(target).fuse() => Some(outcome),
                    _ = shutdown.recv().fuse() => return Ok(()),
                    frame = reader.read(&mut stream).fuse() => {
                        let _ = frame?;
                        None
                    }
                };
                let Some(outcome) = outcome else {
                    write_error_frame(
                        &mut stream,
                        &mut write_buffer,
                        "Concurrent IPC requests are not allowed during a task wait",
                    )
                    .await?;
                    return Ok(());
                };
                write_frame_buffered(&mut stream, &WireResponse::Task(outcome), &mut write_buffer)
                    .await?;
            }
            WireRequest::Watch(expected) => {
                let Some(result) = wait_for_revisions(
                    &mut stream,
                    &mut reader,
                    &mut pending_request,
                    &handle,
                    expected,
                    &shutdown,
                )
                .await?
                else {
                    return Ok(());
                };
                let revisions = match result {
                    WatchResult::Request => continue,
                    WatchResult::Revisions(revisions) => revisions,
                };
                let response = WireResponse::Watch(revisions);
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
) {
    let instance_id = {
        let mut rng = rand::rng();
        loop {
            let id = rng.random();
            if id != UNKNOWN_INSTANCE {
                break id;
            }
        }
    };
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
                    clients.push(handle.executor().spawn(async move {
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

pub fn is_no_instance(error: &Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::AsyncWriteExt;
    use rivu_protocol::{response::StateSections, transport::encode_request_frame_into};
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
            // Resume a partially read frame whose original deadline expired.
            // Additional bytes must not start a fresh completion interval.
            reader.deadline = Some(Instant::now() - FRAME_TIMEOUT);
            writer
                .write_all(&encoded[1..encoded.len() - 1])
                .await
                .unwrap();
            assert!(
                reader
                    .read(&mut stream)
                    .await
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("completion timed out")
            );
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
            encode_request_frame_into(&WireRequest::State(StateSections::ALL), &mut encoded)
                .unwrap();
            writer.write_all(&encoded).await.unwrap();
            assert!(matches!(
                reader.read(&mut stream).await.unwrap().unwrap(),
                WireRequest::State(sections) if sections == StateSections::ALL
            ));
        });
    }

    #[test]
    fn query_wait_eof_and_shutdown_cancel_the_core_reply_receiver() {
        for (shutdown_connection, pipeline) in [(false, false), (true, false), (false, true)] {
            async_io::block_on(async {
                let (handle, queries) = CoreHandle::test_query_port();
                let (mut peer, mut stream) = socket_pair();
                let mut reader = RequestReader::default();
                let (stop, shutdown) = async_channel::bounded(1);
                let mut waiting = Box::pin(wait_for_query(
                    &mut stream,
                    &mut reader,
                    &handle,
                    Query::LibraryStats,
                    &shutdown,
                ));
                assert!(waiting.as_mut().now_or_never().is_none());
                let reply = timeout(Duration::from_secs(2), queries.receive())
                    .await
                    .unwrap();
                assert!(!reply.is_cancelled());
                if pipeline {
                    let mut encoded = Vec::new();
                    encode_request_frame_into(
                        &WireRequest::State(StateSections::ALL),
                        &mut encoded,
                    )
                    .unwrap();
                    encoded.push(1); // A partial frame after the complete request.
                    peer.write_all(&encoded).await.unwrap();
                    assert!(waiting.as_mut().now_or_never().is_none());
                    assert!(!reply.is_cancelled());
                }
                if shutdown_connection {
                    stop.close();
                } else {
                    drop(peer);
                }
                let result = timeout(Duration::from_secs(2), waiting).await.unwrap();
                if shutdown_connection {
                    assert!(result.unwrap().is_none());
                } else {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("connection closed")
                    );
                }
                assert!(reply.is_cancelled());
            });
        }
    }

    #[test]
    fn query_wait_keeps_complete_and_partial_following_requests_in_order() {
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
                &shutdown,
            ));
            assert!(waiting.as_mut().now_or_never().is_none());
            let reply = timeout(Duration::from_secs(2), queries.receive())
                .await
                .unwrap();
            let mut encoded = Vec::new();
            encode_request_frame_into(&WireRequest::State(StateSections::ALL), &mut encoded)
                .unwrap();
            peer.write_all(&encoded).await.unwrap();
            encode_request_frame_into(&WireRequest::Watch(StateRevisions::default()), &mut encoded)
                .unwrap();
            peer.write_all(&encoded).await.unwrap();
            encode_request_frame_into(&WireRequest::Hello(StateSections::ALL), &mut encoded)
                .unwrap();
            peer.write_all(&encoded[..1]).await.unwrap();
            assert!(waiting.as_mut().now_or_never().is_none());
            assert!(!reply.is_cancelled());
            reply.respond(QueryResponse {
                revisions: Default::default(),
                result: Ok(rivu_protocol::response::ViewResponse::Track(None)),
            });
            assert!(waiting.await.unwrap().unwrap().is_ok());
            let deadline = reader.deadline.unwrap();
            assert!(matches!(
                reader.read(&mut stream).await.unwrap().unwrap(),
                WireRequest::State(sections) if sections == StateSections::ALL
            ));
            assert!(matches!(
                reader.read(&mut stream).await.unwrap().unwrap(),
                WireRequest::Watch(revisions) if revisions == StateRevisions::default()
            ));
            assert_eq!(reader.deadline, Some(deadline));
            peer.write_all(&encoded[1..]).await.unwrap();
            assert!(matches!(
                reader.read(&mut stream).await.unwrap().unwrap(),
                WireRequest::Hello(sections) if sections == StateSections::ALL
            ));
            assert!(reader.deadline.is_none());
        });
    }

    #[test]
    fn query_read_ahead_applies_backpressure_at_the_existing_frame_limit() {
        async_io::block_on(async {
            let mut bytes = vec![0; 4 + MAX_REQUEST + 1];
            bytes[..4].copy_from_slice(&(MAX_REQUEST as u32).to_le_bytes());
            let mut input = bytes.as_slice();
            let mut reader = RequestReader::default();
            while reader.pending.len() < 4 + MAX_REQUEST {
                reader.read_ahead(&mut input).await.unwrap();
            }
            assert_eq!(input.len(), 1);
            assert!(reader.read_ahead(&mut input).now_or_never().is_none());
            assert_eq!(input.len(), 1);
        });
    }

    #[test]
    fn query_read_ahead_never_crosses_a_prefix_or_body_boundary() {
        async_io::block_on(async {
            let mut first = Vec::new();
            encode_request_frame_into(&WireRequest::State(StateSections::ALL), &mut first).unwrap();
            let mut second = Vec::new();
            encode_request_frame_into(&WireRequest::Watch(StateRevisions::default()), &mut second)
                .unwrap();
            let mut pipeline = first.clone();
            pipeline.extend_from_slice(&second);
            let mut input = pipeline.as_slice();
            let mut reader = RequestReader::default();
            reader.read_ahead(&mut input).await.unwrap();
            assert_eq!(reader.pending.len(), 4);
            let deadline = reader.deadline.unwrap();
            reader.read_ahead(&mut input).await.unwrap();
            assert_eq!(reader.pending.as_ref(), first.as_slice());
            assert_eq!(input, second.as_slice());
            assert_eq!(reader.complete, first.len());
            assert!(reader.deadline.is_none());
            // A complete first frame is untimed, including while the next prefix
            // is received, and decoding it cannot consume that partial prefix.
            reader.read_ahead(&mut input).await.unwrap();
            assert_eq!(reader.pending.len(), first.len() + 4);
            assert!(reader.deadline.unwrap() >= deadline);
            assert!(matches!(
                reader.read(&mut input).await.unwrap().unwrap(),
                WireRequest::State(_)
            ));
            assert_eq!(reader.pending.as_ref(), &second[..4]);
            assert!(matches!(
                reader.read(&mut input).await.unwrap().unwrap(),
                WireRequest::Watch(_)
            ));
            assert!(reader.deadline.is_none());
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
                        .unwrap_err()
                        .to_string()
                        .contains("exceeds maximum size")
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
    fn query_wait_silent_partial_expiry_releases_reply_even_with_a_full_buffer() {
        for full in [false, true] {
            async_io::block_on(async {
                let (handle, queries) = CoreHandle::test_query_port();
                let (_peer, mut stream) = socket_pair();
                let mut reader = RequestReader::default();
                let mut complete = Vec::new();
                encode_request_frame_into(&WireRequest::State(StateSections::ALL), &mut complete)
                    .unwrap();
                let bytes = if full {
                    let mut bytes = complete.clone();
                    bytes.extend_from_slice(&(MAX_REQUEST as u32).to_le_bytes());
                    bytes.resize(4 + MAX_REQUEST, 0);
                    bytes
                } else {
                    vec![1]
                };
                let mut input = bytes.as_slice();
                while !input.is_empty() {
                    reader.read_ahead(&mut input).await.unwrap();
                }
                assert_eq!(reader.pending.len(), bytes.len());
                assert_eq!(reader.complete, if full { complete.len() } else { 0 });
                // Shorten only the test's first-byte deadline. Keep the peer open
                // and silent: no new bytes or EOF may be required to wake expiry.
                reader.deadline = Some(Instant::now() + Duration::from_millis(100));
                let deadline = reader.deadline;
                let (_stop, shutdown) = async_channel::bounded(1);
                let mut waiting = Box::pin(wait_for_query(
                    &mut stream,
                    &mut reader,
                    &handle,
                    Query::LibraryStats,
                    &shutdown,
                ));
                assert!(waiting.as_mut().now_or_never().is_none());
                let reply = timeout(Duration::from_secs(2), queries.receive())
                    .await
                    .unwrap();
                assert!(
                    timeout(Duration::from_secs(2), waiting)
                        .await
                        .unwrap()
                        .unwrap_err()
                        .to_string()
                        .contains("completion timed out")
                );
                assert!(reply.is_cancelled());
                assert_eq!(reader.deadline, deadline);
                assert_eq!(reader.pending.len(), bytes.len());
                if full {
                    // Expiry never reclassifies or decodes the complete prefix.
                    assert!(matches!(
                        reader.read(&mut stream).await.unwrap().unwrap(),
                        WireRequest::State(_)
                    ));
                }
                assert!(
                    reader
                        .read(&mut stream)
                        .await
                        .err()
                        .expect("an expired partial frame must fail")
                        .to_string()
                        .contains("completion timed out")
                );
            });
        }
    }

    #[test]
    fn late_completed_pipelined_frame_cannot_erase_its_deadline() {
        async_io::block_on(async {
            let (mut peer, mut stream) = socket_pair();
            let mut reader = RequestReader::default();
            let mut encoded = Vec::new();
            encode_request_frame_into(&WireRequest::State(StateSections::ALL), &mut encoded)
                .unwrap();
            peer.write_all(&encoded).await.unwrap();
            while reader.pending.len() < encoded.len() {
                reader.read_ahead(&mut stream).await.unwrap();
            }
            peer.write_all(&encoded[..1]).await.unwrap();
            reader.read_ahead(&mut stream).await.unwrap();
            reader.deadline = Some(Instant::now() - FRAME_TIMEOUT);
            peer.write_all(&encoded[1..]).await.unwrap();
            peer.write_all(&encoded).await.unwrap();
            assert!(
                reader
                    .read_ahead(&mut stream)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("completion timed out")
            );
            assert_eq!(reader.pending.len(), encoded.len() + 1);
            assert!(matches!(
                reader.read(&mut stream).await.unwrap().unwrap(),
                WireRequest::State(_)
            ));
            assert!(
                reader
                    .read(&mut stream)
                    .await
                    .err()
                    .expect("a late complete frame must retain its timeout")
                    .to_string()
                    .contains("completion timed out")
            );
        });
    }

    #[test]
    fn read_ahead_assigns_and_retains_the_trailing_partial_frame_deadline() {
        async_io::block_on(async {
            let (mut peer, mut stream) = socket_pair();
            let mut reader = RequestReader::default();
            let mut encoded = Vec::new();
            encode_request_frame_into(&WireRequest::State(StateSections::ALL), &mut encoded)
                .unwrap();
            peer.write_all(&encoded).await.unwrap();
            while reader.pending.len() < encoded.len() {
                reader.read_ahead(&mut stream).await.unwrap();
            }
            assert!(reader.deadline.is_none());
            peer.write_all(&encoded[..1]).await.unwrap();
            reader.read_ahead(&mut stream).await.unwrap();
            assert!(reader.deadline.is_some());
            let deadline = Instant::now() + FRAME_TIMEOUT;
            reader.deadline = Some(deadline);
            peer.write_all(&encoded[1..2]).await.unwrap();
            reader.read_ahead(&mut stream).await.unwrap();
            assert_eq!(reader.deadline, Some(deadline));
            reader.deadline = Some(Instant::now() - FRAME_TIMEOUT);
            // The earlier complete frame is still usable; only the unfinished
            // frame expires, and additional read-ahead bytes cannot renew it.
            assert!(matches!(
                reader.read(&mut stream).await.unwrap().unwrap(),
                WireRequest::State(_)
            ));
            assert!(
                reader
                    .read(&mut stream)
                    .await
                    .err()
                    .expect("an expired partial frame must fail")
                    .to_string()
                    .contains("completion timed out")
            );
        });
    }

    #[test]
    fn watch_rechecks_revision_after_registration_without_a_notification() {
        async_io::block_on(async {
            let (handle, _queries) = CoreHandle::test_query_port();
            let (_peer, mut stream) = socket_pair();
            let mut reader = RequestReader::default();
            let mut pending = None;
            let (_stop, shutdown) = async_channel::bounded(1);
            let baseline = handle.state_revisions();
            let waiting = wait_for_revisions(
                &mut stream,
                &mut reader,
                &mut pending,
                &handle,
                baseline,
                &shutdown,
            );
            // Simulate publication whose wakeup was missed at registration. The
            // state recheck must complete even though no channel item exists.
            let current = handle.test_advance_revision(false);
            assert!(matches!(
                timeout(Duration::from_secs(2), waiting).await.unwrap().unwrap(),
                Some(WatchResult::Revisions(revisions)) if revisions == current
            ));
            assert_eq!(handle.test_revision_subscriptions(), (1, 0));
            // An already changed baseline replies without another subscription.
            assert!(matches!(
                wait_for_revisions(
                    &mut stream, &mut reader, &mut pending, &handle, baseline, &shutdown,
                ).await.unwrap(),
                Some(WatchResult::Revisions(revisions)) if revisions == current
            ));
            assert_eq!(handle.test_revision_subscriptions(), (1, 0));
            handle.test_advance_revision(true);
            assert_eq!(handle.test_revision_subscriptions(), (0, 0));
        });
    }

    #[test]
    fn watch_coalesces_notifications_and_releases_on_each_exit() {
        for exit in 0..4 {
            async_io::block_on(async {
                let (handle, _queries) = CoreHandle::test_query_port();
                let (mut peer, mut stream) = socket_pair();
                let mut reader = RequestReader::default();
                let mut pending = None;
                let (stop, shutdown) = async_channel::bounded(1);
                let baseline = handle.state_revisions();
                let mut waiting = Box::pin(wait_for_revisions(
                    &mut stream,
                    &mut reader,
                    &mut pending,
                    &handle,
                    baseline,
                    &shutdown,
                ));
                assert!(waiting.as_mut().now_or_never().is_none());
                match exit {
                    0 => {
                        handle.test_advance_revision(true);
                        let current = handle.test_advance_revision(true);
                        assert!(matches!(
                            timeout(Duration::from_secs(2), waiting).await.unwrap().unwrap(),
                            Some(WatchResult::Revisions(revisions)) if revisions == current
                        ));
                    }
                    1 => {
                        let mut encoded = Vec::new();
                        encode_request_frame_into(
                            &WireRequest::State(StateSections::ALL),
                            &mut encoded,
                        )
                        .unwrap();
                        peer.write_all(&encoded).await.unwrap();
                        assert!(matches!(
                            timeout(Duration::from_secs(2), waiting)
                                .await
                                .unwrap()
                                .unwrap(),
                            Some(WatchResult::Request)
                        ));
                        assert!(matches!(pending, Some(Ok(WireRequest::State(_)))));
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
                assert_eq!(handle.test_revision_subscriptions(), (1, 0));
                // A subsequent active Watch prunes the old closed sender and
                // still receives the next real revision, without registry growth.
                let baseline = handle.state_revisions();
                let mut pending = None;
                let (_peer, mut stream) = socket_pair();
                let (_stop, shutdown) = async_channel::bounded(1);
                let waiting = wait_for_revisions(
                    &mut stream,
                    &mut reader,
                    &mut pending,
                    &handle,
                    baseline,
                    &shutdown,
                );
                assert_eq!(handle.test_revision_subscriptions(), (1, 1));
                let current = handle.test_advance_revision(true);
                assert!(matches!(
                    timeout(Duration::from_secs(2), waiting).await.unwrap().unwrap(),
                    Some(WatchResult::Revisions(revisions)) if revisions == current
                ));
                handle.test_advance_revision(true);
                assert_eq!(handle.test_revision_subscriptions(), (0, 0));
            });
        }
    }
}
