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
use rivu_protocol::protocol::{
    HELLO_TIMEOUT, HelloResponse, MAX_CLIENTS, MAX_REQUEST, SessionState, UNKNOWN_INSTANCE,
    WireRequest, WireResponse, wire_state,
};
use rivu_protocol::transport::{
    bridge_timeout, read_request_frame, timeout, timeout_at, write_error_frame,
    write_frame_buffered,
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
        let address = rivu_protocol::protocol::address(data_dir)?;
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
        if self.complete != 0 {
            let before = self.pending.len();
            let frame = read_request_frame(stream, &mut self.pending).await?;
            self.complete -= before - self.pending.len();
            return Ok(frame);
        }
        // Idle sessions have no expiry. Start one absolute deadline at the first
        // byte, retaining it when Watch updates cancel and resume this future.
        if self.pending.is_empty() {
            let mut prefix = [0; 4];
            let read = stream
                .read(&mut prefix)
                .await
                .context("Reading IPC frame")?;
            if read == 0 {
                bail!("IPC connection closed while reading frame");
            }
            self.deadline = Some(Instant::now() + FRAME_TIMEOUT);
            self.pending.extend_from_slice(&prefix[..read]);
        }
        let deadline = self
            .deadline
            .expect("started IPC frame must have a deadline");
        if Instant::now() >= deadline {
            bail!("IPC request frame completion timed out");
        }
        let frame = timeout_at(deadline, read_request_frame(stream, &mut self.pending))
            .await
            .context("IPC request frame completion timed out")??;
        self.deadline = None;
        Ok(frame)
    }

    async fn read_ahead(&mut self, stream: &mut (impl AsyncRead + Unpin)) -> Result<()> {
        // Keep pipelined bytes in the existing receive buffer without decoding or
        // executing the next request before the current query has replied. Once
        // full, apply backpressure; EOF behind a full buffer is observed later.
        let previous = self.pending.len();
        let available = 4 + MAX_REQUEST - previous;
        if available == 0 {
            return std::future::pending().await;
        }
        if previous == self.complete {
            // Most serial queries have no following frame. Observe EOF without
            // allocating receive storage or initializing an unused body buffer.
            let mut prefix = [0; 4];
            let count = available.min(prefix.len());
            let read = stream
                .read(&mut prefix[..count])
                .await
                .context("Reading IPC frame")?;
            assert!(
                read <= count,
                "AsyncRead returned more bytes than its buffer"
            );
            if read == 0 {
                bail!("IPC connection closed while reading frame");
            }
            self.pending.extend_from_slice(&prefix[..read]);
        } else {
            let count = available.min(8192);
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
            if read == 0 {
                bail!("IPC connection closed while reading frame");
            }
            // SAFETY: the count check bounds these initialized received bytes.
            unsafe { self.pending.advance_mut(read) };
        }
        let now = Instant::now();
        let next_deadline = now + FRAME_TIMEOUT;
        if previous == self.complete {
            self.deadline = Some(next_deadline);
        }
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            // Late bytes must not promote an expired tail to an untimed complete
            // frame. Earlier complete frames can still be consumed in order.
            return Ok(());
        }
        while self.pending.len() - self.complete >= 4 {
            let length = u32::from_le_bytes(
                self.pending[self.complete..self.complete + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            if length > MAX_REQUEST || self.pending.len() - self.complete < 4 + length {
                break;
            }
            self.complete += 4 + length;
            self.deadline = (self.complete < self.pending.len()).then_some(next_deadline);
        }
        Ok(())
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
                if let Err(error) = rivu_protocol::protocol::verify_peer(&stream) {
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
            while reader.pending.len() < encoded.len() * 3 {
                reader.read_ahead(&mut stream).await.unwrap();
            }
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
            let expired = Instant::now() - FRAME_TIMEOUT;
            reader.deadline = Some(expired);
            peer.write_all(&encoded[1..2]).await.unwrap();
            reader.read_ahead(&mut stream).await.unwrap();
            assert_eq!(reader.deadline, Some(expired));
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
