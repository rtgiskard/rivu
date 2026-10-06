use crate::{
    core::{AppHandle, CoreState},
    model::{
        Command, DatabaseOptimization, LibrarySnapshot, PlaybackState, QueueState, ScanProgress,
        Track,
    },
    projection::ClientSnapshot,
    response::{Ack, StateResponse, ViewResponse},
};
use anyhow::{Context, Error, Result, bail};
use bincode::{
    config,
    serde::{decode_from_slice, encode_to_vec},
};
use bytes::{Buf, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    runtime::Builder,
    sync::watch,
    time::timeout,
};

const MAX_REQUEST: usize = 64 * 1024;
const MAX_RESPONSE: usize = 64 * 1024 * 1024;
// Clients do not know the server instance before the first response.
const CLIENT_INSTANCE_UNKNOWN: u16 = 0;

/// Length-prefixed bincode request envelope.
#[derive(Serialize, Deserialize)]
struct RequestFrame {
    instance_id: u16,
    request: RequestKind,
}

#[derive(Serialize, Deserialize)]
enum RequestKind {
    Command(String),
    Ack(String),
    Watch { revision: u16 },
}

enum DecodedRequest {
    Command(Command),
    Ack(Command),
    Watch { revision: u16 },
}

fn decode_request(frame: RequestFrame) -> Result<DecodedRequest> {
    Ok(match frame.request {
        RequestKind::Command(command) => {
            DecodedRequest::Command(serde_json::from_str(&command).context("Decoding command")?)
        }
        RequestKind::Ack(command) => {
            DecodedRequest::Ack(serde_json::from_str(&command).context("Decoding command")?)
        }
        RequestKind::Watch { revision } => DecodedRequest::Watch { revision },
    })
}

#[derive(Serialize, Deserialize)]
struct ResponseFrame {
    instance_id: u16,
    revision: u16,
    response: WireResponse,
}

// Wire revisions are equality tokens, not ordered counters. Truncation to u16
// permits wraparound; watchers only need to know whether the current token
// equals the token supplied by the client. Missing a full 16-bit cycle is
// outside the protocol's bounded-observation guarantee.
fn revision_matches(current: u64, expected: u16) -> bool {
    current as u16 == expected
}

// Config uses omitted fields in human-readable formats, which are not safe in
// bincode's positional structs. Keep only that field as JSON on the wire.
mod wire_config {
    use crate::config::Config;
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;

    pub fn serialize<S: serde::Serializer>(
        config: &Arc<Config>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serde_json::to_string(config.as_ref())
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Arc<Config>, D::Error> {
        let json = String::deserialize(deserializer)?;
        serde_json::from_str(&json)
            .map(Arc::new)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "crate::model::SystemState")]
struct WireSystemState {
    scanning: bool,
    scan_message: String,
    #[serde(default)]
    scan_progress: Option<ScanProgress>,
    last_error: Option<String>,
    devices: Arc<Vec<String>>,
    selected_device: Option<String>,
    revision: u64,
    #[serde(with = "wire_config")]
    config: Arc<crate::config::Config>,
    config_path: PathBuf,
    mpris_status: String,
    ffmpeg_status: String,
    database_optimization: Option<DatabaseOptimization>,
    shutting_down: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "ClientSnapshot")]
struct WireSnapshot {
    library: LibrarySnapshot,
    queue: QueueState,
    current_track: Option<Arc<Track>>,
    playback: PlaybackState,
    #[serde(with = "WireSystemState")]
    system: crate::model::SystemState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum WireState {
    Snapshot(#[serde(with = "WireSnapshot")] ClientSnapshot),
    Ack { revision: u64 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WireResponse {
    ok: bool,
    error: Option<String>,
    state: WireState,
    view: Option<ViewResponse>,
}

impl WireResponse {
    fn from_response(response: StateResponse) -> Self {
        Self {
            ok: response.ok,
            error: response.error,
            state: WireState::Snapshot(response.state),
            view: response.view,
        }
    }

    fn from_ack(ack: Ack) -> Self {
        Self {
            ok: ack.ok,
            error: ack.error,
            state: WireState::Ack {
                revision: ack.revision,
            },
            view: None,
        }
    }
}

fn unpack_ack(frame: ResponseFrame) -> Result<Ack> {
    let ok = frame.response.ok;
    let error = frame.response.error;
    match frame.response.state {
        WireState::Ack { revision } => Ok(Ack {
            ok,
            error,
            revision,
        }),
        _ => bail!("IPC response was not an acknowledgement"),
    }
}

fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let payload = encode_to_vec(value, config::standard()).context("Encoding IPC frame")?;
    if payload.len() > MAX_RESPONSE {
        bail!("IPC frame exceeds maximum size");
    }
    let len = u32::try_from(payload.len()).context("IPC frame is too large")?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn decode_frame<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T> {
    let (value, consumed) =
        decode_from_slice(bytes, config::standard()).context("Decoding IPC frame")?;
    if consumed != bytes.len() {
        bail!("Trailing bytes in IPC frame");
    }
    Ok(value)
}

pub struct Server {
    path: PathBuf,
    stopping: Arc<AtomicBool>,
    shutdown: watch::Sender<bool>,
    bridge_stop: crossbeam_channel::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

pub fn bind(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        return Ok(());
    }
    if !fs::symlink_metadata(path)?.file_type().is_socket() {
        bail!("Refusing to replace non-socket {}", path.display());
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => bail!(
            "Rivu is already running at {}. Use its CLI or TUI.",
            path.display()
        ),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            fs::remove_file(path)?;
            Ok(())
        }
        Err(error) => Err(error).context("Checking existing Rivu socket"),
    }
}

impl Server {
    pub fn start(path: PathBuf, handle: AppHandle) -> Result<Self> {
        let stopping = Arc::new(AtomicBool::new(false));
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (bridge_stop, bridge_stop_rx) = crossbeam_channel::bounded(1);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let worker_path = path.clone();
        let worker = thread::Builder::new()
            .name("rivu-ipc".into())
            .spawn(move || {
                let runtime = match Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(anyhow::Error::from(error)));
                        return;
                    }
                };
                let listener = {
                    let _guard = runtime.enter();
                    UnixListener::bind(&worker_path)
                };
                let listener = match listener {
                    Ok(listener) => listener,
                    Err(error) => {
                        let _ = ready_tx.send(Err(anyhow::Error::from(error)));
                        return;
                    }
                };
                if let Err(error) =
                    fs::set_permissions(&worker_path, fs::Permissions::from_mode(0o600))
                {
                    let _ = ready_tx.send(Err(anyhow::Error::from(error)));
                    return;
                }
                let (revision_tx, revision_rx) = watch::channel(handle.core_state().system.revision);
                let updates = handle.subscribe();
                let bridge_handle = handle.clone();
                let bridge = thread::Builder::new()
                    .name("rivu-ipc-revisions".into())
                    .spawn(move || {
                        loop {
                            crossbeam_channel::select! {
                                recv(updates) -> message => {
                                    if message.is_ok() { let _ = revision_tx.send(bridge_handle.core_state().system.revision); } else { break; }
                                }
                                recv(bridge_stop_rx) -> _ => break,
                            }
                        }
                    });
                let Ok(bridge) = bridge else {
                    let _ = ready_tx.send(Err(anyhow::anyhow!("starting IPC revision bridge")));
                    return;
                };
                let _ = ready_tx.send(Ok(()));
                runtime.block_on(run_server(listener, handle, revision_rx, shutdown_rx));
                let _ = bridge.join();
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                path,
                stopping,
                shutdown,
                bridge_stop,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(error) => {
                let _ = worker.join();
                Err(anyhow::Error::from(error))
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.shutdown.send(true);
        let _ = self.bridge_stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

async fn read_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    limit: usize,
    pending: &mut BytesMut,
) -> Result<Bytes> {
    loop {
        if pending.len() >= 4 {
            let length = u32::from_le_bytes(pending[..4].try_into().unwrap()) as usize;
            if length > limit {
                bail!("IPC frame exceeds maximum size");
            }
            if pending.len() >= 4 + length {
                pending.advance(4);
                return Ok(pending.split_to(length).freeze());
            }
        }
        let before = pending.len();
        stream
            .read_buf(pending)
            .await
            .context("Reading IPC frame")?;
        if pending.len() == before {
            bail!("IPC connection closed while reading frame");
        }
    }
}

async fn write_frame<S: AsyncWrite + Unpin>(stream: &mut S, value: &impl Serialize) -> Result<()> {
    stream
        .write_all(&encode_frame(value)?)
        .await
        .context("Writing IPC frame")?;
    stream.flush().await.context("Flushing IPC frame")?;
    Ok(())
}

fn response_frame(response: StateResponse, instance_id: u16) -> ResponseFrame {
    ResponseFrame {
        instance_id,
        revision: response.state.system.revision as u16,
        response: WireResponse::from_response(response),
    }
}

fn ack_frame(ack: Ack, instance_id: u16) -> ResponseFrame {
    ResponseFrame {
        instance_id,
        revision: ack.revision as u16,
        response: WireResponse::from_ack(ack),
    }
}
fn unpack_response(frame: ResponseFrame) -> Result<StateResponse> {
    let WireResponse {
        ok,
        error,
        state,
        view,
    } = frame.response;
    let state = match state {
        WireState::Snapshot(state) => state,
        WireState::Ack { .. } => bail!("IPC acknowledgement used where state was required"),
    };
    Ok(StateResponse {
        ok,
        error,
        state,
        view,
    })
}

fn overview_response(state: &CoreState) -> StateResponse {
    StateResponse {
        ok: true,
        error: None,
        state: ClientSnapshot::from_core(state),
        view: None,
    }
}

async fn command(handle: &AppHandle, command: Command) -> StateResponse {
    let fallback = ClientSnapshot::from_core(&handle.core_state());
    let duration = if matches!(command, Command::OptimizeDatabase) {
        Duration::from_secs(120)
    } else {
        Duration::from_secs(15)
    };
    match timeout(
        duration,
        tokio::task::spawn_blocking({
            let handle = handle.clone();
            move || handle.request(command)
        }),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => StateResponse {
            ok: false,
            error: Some(format!("Core command task failed: {error}")),
            state: fallback,
            view: None,
        },
        Err(_) => StateResponse {
            ok: false,
            error: Some("Core command timed out".into()),
            state: fallback,
            view: None,
        },
    }
}

async fn wait_for_revision(
    stream: &mut UnixStream,
    pending: &mut BytesMut,
    handle: &AppHandle,
    revision: u16,
    updates: &mut watch::Receiver<u64>,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<StateResponse> {
    loop {
        let state = handle.core_state();
        if !revision_matches(state.system.revision, revision) || state.system.shutting_down {
            return Some(overview_response(&state));
        }
        tokio::select! {
            changed = updates.changed() => { if changed.is_err() { return None; } }
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return None; } }
            ready = stream.readable() => {
                if ready.is_err() { return None; }
                match stream.try_read_buf(pending) {
                    Ok(0) => return None,
                    Ok(_) => {
                        if pending.len() >= 4 {
                            let length =
                                u32::from_le_bytes(pending[..4].try_into().unwrap()) as usize;
                            if length > MAX_REQUEST {
                                return None;
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => return None,
                }
            }
        }
    }
}

async fn serve_connection(
    mut stream: UnixStream,
    handle: AppHandle,
    instance_id: u16,
    mut updates: watch::Receiver<u64>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut pending = BytesMut::new();
    loop {
        let bytes = tokio::select! { frame = read_frame(&mut stream, MAX_REQUEST, &mut pending) => frame?, changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return Ok(()); } continue; } };
        let request = match decode_frame::<RequestFrame>(&bytes).and_then(decode_request) {
            Ok(request) => request,
            Err(error) => {
                let response = StateResponse {
                    ok: false,
                    error: Some(format!("Invalid command: {error}")),
                    state: ClientSnapshot::from_core(&handle.core_state()),
                    view: None,
                };
                write_frame(&mut stream, &response_frame(response, instance_id)).await?;
                continue;
            }
        };
        let response = match request {
            DecodedRequest::Command(Command::Overview) => overview_response(&handle.core_state()),
            DecodedRequest::Command(cmd) => command(&handle, cmd).await,
            DecodedRequest::Ack(cmd) => {
                let ack = tokio::task::spawn_blocking({
                    let handle = handle.clone();
                    move || handle.request_ack(cmd)
                })
                .await
                .map_err(|error| anyhow::anyhow!("Core command task failed: {error}"))?;
                write_frame(&mut stream, &ack_frame(ack, instance_id)).await?;
                continue;
            }
            DecodedRequest::Watch { revision } => {
                let Some(response) = wait_for_revision(
                    &mut stream,
                    &mut pending,
                    &handle,
                    revision,
                    &mut updates,
                    &mut shutdown,
                )
                .await
                else {
                    return Ok(());
                };
                response
            }
        };
        write_frame(&mut stream, &response_frame(response, instance_id)).await?;
    }
}

async fn run_server(
    listener: UnixListener,
    handle: AppHandle,
    updates: watch::Receiver<u64>,
    mut shutdown: watch::Receiver<bool>,
) {
    let instance_id = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u16)
        .wrapping_add(std::process::id() as u16);
    let mut clients: Vec<tokio::task::JoinHandle<Result<()>>> = Vec::new();
    let mut revisions = updates.clone();
    loop {
        tokio::select! {
            accepted = listener.accept() => { let Ok((stream, _)) = accepted else { continue; }; clients.retain(|task| !task.is_finished()); if clients.len() < 16 { clients.push(tokio::spawn(serve_connection(stream, handle.clone(), instance_id, updates.clone(), shutdown.clone()))); } }
            changed = revisions.changed() => { if changed.is_err() { break; } }
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { break; } }
        }
    }
    for task in clients {
        task.abort();
        let _ = task.await;
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

pub struct WatcherSession {
    runtime: tokio::runtime::Runtime,
    stream: UnixStream,
    cancelled: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
}
fn client_runtime() -> Result<tokio::runtime::Runtime> {
    Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("creating IPC runtime")
}

pub fn watch_session(path: &Path) -> Result<WatcherSession> {
    watch_session_with_cancel(
        path,
        Arc::new(AtomicBool::new(false)),
        Arc::new(tokio::sync::Notify::new()),
    )
}

pub fn watch_session_with_cancel(
    path: &Path,
    cancelled: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
) -> Result<WatcherSession> {
    let runtime = client_runtime()?;
    let stream = runtime
        .block_on(UnixStream::connect(path))
        .with_context(|| {
            format!(
                "Cannot connect to Rivu at {}. Start `rivu` or `rivu serve` first.",
                path.display()
            )
        })?;
    Ok(WatcherSession {
        runtime,
        stream,
        cancelled,
        cancel_notify,
    })
}
impl WatcherSession {
    pub fn cancellation(&self) -> (Arc<AtomicBool>, Arc<tokio::sync::Notify>) {
        (self.cancelled.clone(), self.cancel_notify.clone())
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.cancel_notify.notify_waiters();
    }
    pub fn watch(&mut self, revision: u16) -> Result<StateResponse> {
        self.watch_until(revision)
            .and_then(|response| response.ok_or_else(|| anyhow::anyhow!("Watcher cancelled")))
    }
    pub(crate) fn watch_until(&mut self, revision: u16) -> Result<Option<StateResponse>> {
        let request = RequestFrame {
            instance_id: CLIENT_INSTANCE_UNKNOWN,
            request: RequestKind::Watch { revision },
        };
        self.runtime.block_on(async {
            let notified = self.cancel_notify.notified();
            tokio::pin!(notified);
            // Register before checking the flag so notify_waiters cannot be lost.
            notified.as_mut().enable();
            if self.cancelled.load(Ordering::Acquire) {
                return Ok(None);
            }
            tokio::select! {
                _ = &mut notified => Ok(None),
                response = async {
                    let mut pending = BytesMut::new();
                    write_frame(&mut self.stream, &request).await?;
                    let bytes = read_frame(&mut self.stream, MAX_RESPONSE, &mut pending).await?;
                    Ok(Some(unpack_response(decode_frame::<ResponseFrame>(&bytes)?)?))
                } => {
                    if self.cancelled.load(Ordering::Acquire) { Ok(None) } else { response }
                }
            }
        })
    }
}
pub fn watch(path: &Path, revision: u16) -> Result<StateResponse> {
    watch_session(path)?.watch(revision)
}
async fn request_inner(
    path: &Path,
    command: &Command,
    cancellation: Option<(Arc<AtomicBool>, Arc<tokio::sync::Notify>)>,
) -> Result<StateResponse> {
    let operation = async {
        let mut stream = UnixStream::connect(path).await.with_context(|| {
            format!(
                "Cannot connect to Rivu at {}. Start `rivu` or `rivu serve` first.",
                path.display()
            )
        })?;
        let request = RequestFrame {
            instance_id: CLIENT_INSTANCE_UNKNOWN,
            request: RequestKind::Command(serde_json::to_string(command)?),
        };
        write_frame(&mut stream, &request).await?;
        let bytes = if matches!(command, Command::OptimizeDatabase) {
            read_frame(&mut stream, MAX_RESPONSE, &mut BytesMut::new()).await?
        } else {
            timeout(
                Duration::from_secs(15),
                read_frame(&mut stream, MAX_RESPONSE, &mut BytesMut::new()),
            )
            .await
            .context("Reading IPC response timed out")??
        };
        unpack_response(decode_frame::<ResponseFrame>(&bytes)?)
    };
    let Some((cancelled, cancel_notify)) = cancellation else {
        return operation.await;
    };
    let notified = cancel_notify.notified();
    tokio::pin!(notified);
    // Register before checking the flag so notify_waiters cannot be lost.
    notified.as_mut().enable();
    if cancelled.load(Ordering::Acquire) {
        bail!("IPC request cancelled");
    }
    tokio::select! {
        _ = &mut notified => bail!("IPC request cancelled"),
        response = operation => response,
    }
}

pub fn request(path: &Path, command: &Command) -> Result<StateResponse> {
    let runtime = client_runtime()?;
    runtime.block_on(request_inner(path, command, None))
}

pub(crate) fn request_with_cancel(
    path: &Path,
    command: &Command,
    cancelled: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
) -> Result<StateResponse> {
    let runtime = client_runtime()?;
    runtime.block_on(request_inner(
        path,
        command,
        Some((cancelled, cancel_notify)),
    ))
}

pub fn request_ack(path: &Path, command: &Command) -> Result<Ack> {
    let runtime = client_runtime()?;
    runtime.block_on(async {
        let mut stream = UnixStream::connect(path).await.with_context(|| {
            format!(
                "Cannot connect to Rivu at {}. Start `rivu` or `rivu serve` first.",
                path.display()
            )
        })?;
        let request = RequestFrame {
            instance_id: CLIENT_INSTANCE_UNKNOWN,
            request: RequestKind::Ack(serde_json::to_string(command)?),
        };
        write_frame(&mut stream, &request).await?;
        let bytes = timeout(
            Duration::from_secs(15),
            read_frame(&mut stream, MAX_RESPONSE, &mut BytesMut::new()),
        )
        .await
        .context("Reading IPC acknowledgement timed out")??;
        unpack_ack(decode_frame::<ResponseFrame>(&bytes)?)
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    include!("ipc_tests.rs");
}
