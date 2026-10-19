use super::protocol::{
    HELLO_TIMEOUT, HelloResponse, MAX_CLIENTS, MAX_REQUEST, SessionState, UNKNOWN_INSTANCE,
    WireRequest, WireResponse, wire_state,
};
use super::transport::{
    bridge_timeout, decode_request_frame, read_frame, write_error_frame, write_frame_buffered,
};
use crate::core::CoreHandle;
use crate::{
    app::{AppCommand, AppHandle},
    model::Query,
    response::{Ack, QueryResponse, StateRevisions},
};
use anyhow::{Context, Error, Result, bail};
use bytes::{BufMut, Bytes, BytesMut};
use rand::RngExt;
use std::{
    fs,
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    net::{UnixListener, UnixStream},
    runtime::Builder,
    sync::watch,
    time::{Instant, timeout, timeout_at},
};
/// Holds the abstract listener before any database or audio startup side effects.
/// The kernel releases the address after the last listening descriptor closes.
pub struct SocketOwner {
    data_dir: PathBuf,
    listener: std::os::unix::net::UnixListener,
}

impl SocketOwner {
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir)?;
        let address = super::endpoint::address(data_dir)?;
        let listener = std::os::unix::net::UnixListener::bind_addr(&address)
            .context("Cannot acquire Rivu IPC address; another Rivu may own this data directory")?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            data_dir: data_dir.to_owned(),
            listener,
        })
    }
}

/// Transport worker using a cloned listener.
/// The application must retain SocketOwner through core and server teardown.
pub struct Server {
    data_dir: PathBuf,
    shutdown: watch::Sender<bool>,
    bridge_stop: crossbeam_channel::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    pub(crate) fn start(owner: &SocketOwner, handle: CoreHandle, app: AppHandle) -> Result<Self> {
        let data_dir = owner.data_dir.clone();
        let worker_listener = owner.listener.try_clone()?;
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (bridge_stop, bridge_stop_rx) = crossbeam_channel::bounded(1);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let worker_data_dir = data_dir.clone();
        let worker = thread::Builder::new().name("rivu-ipc".into()).spawn(move || {
            let runtime = match Builder::new_current_thread().enable_io().enable_time().build() {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(error = %error, "ipc_runtime_start_failed");
                    let _ = ready_tx.send(Err(anyhow::Error::from(error)));
                    return;
                }
            };
            // App retains a listening descriptor through runtime shutdown;
            // stopping this worker must not release directory ownership.
            let listener = {
                let _guard = runtime.enter();
                UnixListener::from_std(worker_listener)
            };
            let listener = match listener {
                Ok(listener) => listener,
                Err(error) => {
                    let _ = ready_tx.send(Err(anyhow::Error::from(error)));
                    return;
                }
            };
            let (revision_tx, revision_rx) = watch::channel(handle.state_revisions());
            let updates = handle.subscribe();
            let bridge_handle = handle.clone();
            let bridge = thread::Builder::new().name("rivu-ipc-revisions".into()).spawn(move || {
                loop {
                    crossbeam_channel::select! {
                        recv(updates) -> message => {
                            if message.is_ok() { let _ = revision_tx.send(bridge_handle.state_revisions()); } else { break; }
                        }
                        recv(bridge_stop_rx) -> _ => break,
                    }
                }
            });
            let bridge = match bridge {
                Ok(bridge) => bridge,
                Err(error) => {
                    tracing::error!(path = %worker_data_dir.display(), error = %error, "ipc_revision_bridge_start_failed");
                    let _ = ready_tx.send(Err(anyhow::anyhow!("starting IPC revision bridge: {error}")));
                    return;
                }
            };
            tracing::info!(path = %worker_data_dir.display(), "ipc_server_started");
            let _ = ready_tx.send(Ok(()));
            runtime.block_on(run_server(listener, handle, app, revision_rx, shutdown_rx));
            let _ = bridge.join();
        })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                data_dir,
                shutdown,
                bridge_stop,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                tracing::error!(path = %data_dir.display(), error = %error, "ipc_server_start_failed");
                let _ = worker.join();
                Err(error)
            }
            Err(error) => {
                tracing::error!(path = %data_dir.display(), error = %error, "ipc_server_start_failed");
                let _ = worker.join();
                Err(anyhow::Error::from(error))
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        tracing::info!(path = %self.data_dir.display(), "ipc_server_stopping");
        let _ = self.shutdown.send(true);
        let _ = self.bridge_stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
async fn run_command(app: &AppHandle, command: AppCommand) -> Result<Ack> {
    let duration = bridge_timeout(command.wait_timeout());
    timeout(
        duration,
        tokio::task::spawn_blocking({
            let app = app.clone();
            move || app.request_ack(command)
        }),
    )
    .await
    .context("Reading IPC acknowledgement timed out")?
    .map_err(|error| anyhow::anyhow!("Core command task failed: {error}"))
}

async fn run_query(handle: &CoreHandle, query: Query) -> Result<QueryResponse> {
    timeout(
        bridge_timeout(CoreHandle::query_wait_timeout()),
        tokio::task::spawn_blocking({
            let handle = handle.clone();
            move || handle.query(query)
        }),
    )
    .await
    .context("Reading IPC query timed out")?
    .map_err(|error| anyhow::anyhow!("Core query task failed: {error}"))
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
            let read = stream
                .read_buf(&mut (&mut self.pending).limit(4))
                .await
                .context("Reading IPC frame")?;
            if read == 0 {
                bail!("IPC connection closed while reading frame");
            }
            self.deadline = Some(Instant::now() + FRAME_TIMEOUT);
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
    updates: &mut watch::Receiver<StateRevisions>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<WatchResult>> {
    loop {
        let current = handle.state_revisions();
        if current != expected {
            return Ok(Some(WatchResult::Revisions(current)));
        }
        tokio::select! {
            changed = updates.changed() => { if changed.is_err() { return Ok(None); } }
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return Ok(None); } }
            frame = reader.read(stream) => return Ok(Some(WatchResult::Request(frame?))),
        }
    }
}

async fn serve_connection(
    mut stream: UnixStream,
    handle: CoreHandle,
    app: AppHandle,
    instance_id: u32,
    mut updates: watch::Receiver<StateRevisions>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut reader = RequestReader::default();
    let mut pending_request = None;
    let mut write_buffer = Vec::new();
    let mut session = SessionState::Unbound;
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let bytes = if let Some(frame) = pending_request.take() {
            frame
        } else if session.is_bound() {
            tokio::select! {
                frame = reader.read(&mut stream) => frame?,
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return Ok(()); }
                    continue;
                }
            }
        } else {
            tokio::select! {
                frame = timeout(HELLO_TIMEOUT, reader.read(&mut stream)) => {
                    match frame {
                        Ok(frame) => frame?,
                        Err(_) => {
                            tracing::warn!("ipc_hello_timeout");
                            return Ok(());
                        }
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return Ok(()); }
                    continue;
                }
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
                    &mut updates,
                    &mut shutdown,
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
    updates: watch::Receiver<StateRevisions>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut rng = rand::rng();
    let mut instance_id = UNKNOWN_INSTANCE;
    while instance_id == UNKNOWN_INSTANCE {
        instance_id = rng.random();
    }
    let mut clients = Vec::new();
    let revisions = updates;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, _) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => {
                        tracing::warn!(error = %error, "ipc_accept_failed");
                        continue;
                    }
                };
                if let Err(error) = super::endpoint::verify_peer(&stream) {
                    tracing::warn!(error = %error, "ipc_peer_rejected");
                    continue;
                }
                clients.retain(|task: &tokio::task::JoinHandle<Result<()>>| !task.is_finished());
                if clients.len() < MAX_CLIENTS {
                    clients.push(tokio::spawn(serve_connection(
                        stream,
                        handle.clone(),
                        app.clone(),
                        instance_id,
                        revisions.clone(),
                        shutdown.clone(),
                    )));
                } else {
                    tracing::warn!(clients = clients.len(), max_clients = MAX_CLIENTS, "ipc_client_limit_reached");
                    let mut buffer = Vec::new();
                    if let Err(error) = write_error_frame(&mut stream, &mut buffer, "IPC server is busy").await {
                        tracing::debug!(error = %error, "ipc_busy_response_failed");
                    }
                }
            }
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { break; } }
        }
    }
    // Idle sessions observe shutdown; accepted operations finish their reply
    // before App retires the socket and releases instance authority.
    let _ = timeout(Duration::from_secs(120), async {
        for task in &mut clients {
            let _ = task.await;
        }
    })
    .await;
    for task in clients {
        task.abort();
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
    use std::{
        future::Future,
        task::{Context as TaskContext, Waker},
    };
    use tokio::io::AsyncWriteExt;

    #[tokio::test(start_paused = true)]
    async fn idle_request_wait_accepts_a_frame_after_the_completion_interval() {
        let (mut writer, mut stream) = tokio::io::duplex(32);
        let reading = tokio::spawn(async move { RequestReader::default().read(&mut stream).await });
        tokio::task::yield_now().await;
        tokio::time::advance(FRAME_TIMEOUT * 2).await;
        writer.write_all(&[2, 0, 0, 0, 14, 1]).await.unwrap();
        assert_eq!(reading.await.unwrap().unwrap().as_ref(), &[14, 1]);
    }

    #[tokio::test(start_paused = true)]
    async fn partial_request_deadline_survives_cancel_and_additional_bytes() {
        let (mut writer, mut stream) = tokio::io::duplex(32);
        let mut reader = RequestReader::default();
        writer.write_all(&[2]).await.unwrap();
        {
            let mut reading = std::pin::pin!(reader.read(&mut stream));
            let mut context = TaskContext::from_waker(Waker::noop());
            assert!(reading.as_mut().poll(&mut context).is_pending());
        }
        tokio::time::advance(FRAME_TIMEOUT - Duration::from_secs(1)).await;
        writer.write_all(&[0, 0, 0, 14]).await.unwrap();
        {
            let mut reading = std::pin::pin!(reader.read(&mut stream));
            let mut context = TaskContext::from_waker(Waker::noop());
            assert!(reading.as_mut().poll(&mut context).is_pending());
        }
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(
            reader
                .read(&mut stream)
                .await
                .unwrap_err()
                .to_string()
                .contains("completion timed out")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn completed_request_resets_the_deadline_for_the_next_frame() {
        let (mut writer, mut stream) = tokio::io::duplex(32);
        let mut reader = RequestReader::default();
        writer.write_all(&[2, 0, 0, 0, 14, 1]).await.unwrap();
        assert_eq!(reader.read(&mut stream).await.unwrap().as_ref(), &[14, 1]);
        tokio::time::advance(FRAME_TIMEOUT * 2).await;
        writer.write_all(&[2, 0, 0, 0, 14, 2]).await.unwrap();
        assert_eq!(reader.read(&mut stream).await.unwrap().as_ref(), &[14, 2]);
    }
}
