use crate::app::AppHandle;
use crate::core::CoreHandle;
use anyhow::{Context, Error, Result, bail};
use async_channel::{Receiver, Sender};
use async_io::Async;
use bytes::{Bytes, BytesMut};
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
    bridge_timeout, decode_request_frame, read_frame, timeout, timeout_at, write_error_frame,
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
    deadline: Option<Instant>,
}

impl RequestReader {
    async fn read(&mut self, stream: &mut (impl AsyncRead + Unpin)) -> Result<Bytes> {
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
        let deadline = self.deadline.expect("Started request frame has a deadline");
        let frame = timeout_at(deadline, read_frame(stream, MAX_REQUEST, &mut self.pending))
            .await
            .context("IPC request frame completion timed out")??;
        self.deadline = None;
        Ok(frame)
    }
}

enum WatchResult {
    Revisions(StateRevisions),
    Request(Bytes),
}

async fn wait_for_revisions(
    stream: &mut UnixStream,
    reader: &mut RequestReader,
    handle: &CoreHandle,
    expected: StateRevisions,
    updates: &Receiver<()>,
    shutdown: &Receiver<()>,
) -> Result<Option<WatchResult>> {
    loop {
        let current = handle.state_revisions();
        if current != expected {
            return Ok(Some(WatchResult::Revisions(current)));
        }
        futures::select! {
            changed = updates.recv().fuse() => { if changed.is_err() { return Ok(None); } }
            _ = shutdown.recv().fuse() => return Ok(None),
            frame = reader.read(stream).fuse() => return Ok(Some(WatchResult::Request(frame?))),
        }
    }
}

async fn serve_connection(
    mut stream: UnixStream,
    handle: CoreHandle,
    app: AppHandle,
    instance_id: u32,
    updates: Receiver<()>,
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
        let bytes = if let Some(frame) = pending_request.take() {
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
        let request = match decode_request_frame(&bytes) {
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
                let response = match run_query(&handle, query).await {
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
            WireRequest::Watch(expected) => {
                let Some(result) = wait_for_revisions(
                    &mut stream,
                    &mut reader,
                    &handle,
                    expected,
                    &updates,
                    &shutdown,
                )
                .await?
                else {
                    return Ok(());
                };
                let revisions = match result {
                    WatchResult::Request(frame) => {
                        pending_request = Some(frame);
                        continue;
                    }
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
                        handle.subscribe_revisions(),
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
    // Idle sessions observe shutdown; accepted operations drain their replies.
    // Dropping remaining owned Tasks after the deadline cancels their transport,
    // never the business operation already accepted by Core.
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
                writer.write_all(&[2, 0, 0, 0, 14, 1]).await.unwrap();
            };
            let (frame, _) = futures::join!(receive, send);
            assert_eq!(frame.unwrap().as_ref(), &[14, 1]);
        });
    }

    #[test]
    fn partial_request_deadline_survives_cancel_and_additional_bytes() {
        async_io::block_on(async {
            let (mut writer, mut stream) = socket_pair();
            let mut reader = RequestReader::default();
            writer.write_all(&[2]).await.unwrap();
            {
                let mut reading = std::pin::pin!(reader.read(&mut stream));
                let mut context = TaskContext::from_waker(Waker::noop());
                assert!(reading.as_mut().poll(&mut context).is_pending());
            }
            // Resume a partially read frame whose original deadline expired.
            // Additional bytes must not start a fresh completion interval.
            reader.deadline = Some(Instant::now() - FRAME_TIMEOUT);
            writer.write_all(&[0, 0, 0, 14]).await.unwrap();
            assert!(
                reader
                    .read(&mut stream)
                    .await
                    .unwrap_err()
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
            writer.write_all(&[2, 0, 0, 0, 14, 1]).await.unwrap();
            assert_eq!(reader.read(&mut stream).await.unwrap().as_ref(), &[14, 1]);
            async_io::Timer::after(FRAME_TIMEOUT * 2).await;
            writer.write_all(&[2, 0, 0, 0, 14, 2]).await.unwrap();
            assert_eq!(reader.read(&mut stream).await.unwrap().as_ref(), &[14, 2]);
        });
    }
}
