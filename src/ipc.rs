use crate::{
    core::AppHandle,
    model::{Command, DatabaseOptimization, Query, ScanProgress, SystemState},
    response::{Ack, QueryResponse, StateResponse, StateRevisions, StateSections},
};
use anyhow::{Context, Error, Result, bail};
use bincode::{
    config,
    serde::{decode_from_slice, encode_into_std_write},
};
use bytes::{Buf, Bytes, BytesMut};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    runtime::Builder,
    sync::watch,
    time::timeout,
};

const PROTOCOL_VERSION: u16 = 1;
const MAX_REQUEST: usize = 64 * 1024;
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_CLIENTS: usize = 16;
const UNKNOWN_INSTANCE: [u8; 16] = [0; 16];

#[derive(Serialize, Deserialize)]
struct RequestFrame {
    version: u16,
    instance_id: [u8; 16],
    request: RequestKind,
}

#[derive(Serialize, Deserialize)]
enum RequestKind {
    Query { query: String },
    State { sections: StateSections },
    Command { command: String },
    Watch { revisions: StateRevisions },
}

enum DecodedRequest {
    Query(Query),
    State(StateSections),
    Command(Command),
    Watch(StateRevisions),
}

fn decode_request(frame: RequestFrame) -> Result<DecodedRequest> {
    Ok(match frame.request {
        RequestKind::Query { query } => {
            DecodedRequest::Query(serde_json::from_str(&query).context("Decoding query")?)
        }
        RequestKind::State { sections } => DecodedRequest::State(sections),
        RequestKind::Command { command } => {
            DecodedRequest::Command(serde_json::from_str(&command).context("Decoding command")?)
        }
        RequestKind::Watch { revisions } => DecodedRequest::Watch(revisions),
    })
}
fn decode_request_header(bytes: &[u8]) -> Result<(u16, [u8; 16])> {
    let ((version, instance_id), _) =
        decode_from_slice(bytes, config::standard()).context("Decoding IPC request header")?;
    Ok((version, instance_id))
}

fn decode_request_frame(bytes: &[u8]) -> Result<RequestFrame> {
    let (version, _) = decode_request_header(bytes)?;
    if version != PROTOCOL_VERSION {
        bail!("Unsupported IPC protocol version {version}");
    }
    decode_frame(bytes)
}

fn decode_response_frame(bytes: &[u8]) -> Result<ResponseFrame> {
    let (version, _) = decode_from_slice::<u16, _>(bytes, config::standard())
        .context("Decoding IPC response header")?;
    if version != PROTOCOL_VERSION {
        bail!("Unsupported IPC protocol version {version}");
    }
    decode_frame(bytes)
}

#[derive(Serialize, Deserialize)]
struct ResponseFrame {
    version: u16,
    instance_id: [u8; 16],
    response: WireResponse,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum WireResponse {
    Query(QueryResponse),
    State(WireStateResponse),
    Ack(Ack),
    Watch(StateRevisions),
    Error(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WireStateResponse {
    revisions: StateRevisions,
    playback: Option<crate::response::PlaybackSnapshot>,
    queue: Option<crate::model::QueueState>,
    library: Option<crate::model::LibrarySnapshot>,
    #[serde(with = "wire_system_option")]
    system: Option<WireSystemState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

impl From<&SystemState> for WireSystemState {
    fn from(value: &SystemState) -> Self {
        Self {
            scanning: value.scanning,
            scan_message: value.scan_message.clone(),
            scan_progress: value.scan_progress.clone(),
            last_error: value.last_error.clone(),
            devices: value.devices.clone(),
            selected_device: value.selected_device.clone(),
            revision: value.revision,
            config: value.config.clone(),
            config_path: value.config_path.clone(),
            mpris_status: value.mpris_status.clone(),
            ffmpeg_status: value.ffmpeg_status.clone(),
            database_optimization: value.database_optimization.clone(),
            shutting_down: value.shutting_down,
        }
    }
}

impl From<WireSystemState> for SystemState {
    fn from(value: WireSystemState) -> Self {
        Self {
            scanning: value.scanning,
            scan_message: value.scan_message,
            scan_progress: value.scan_progress,
            last_error: value.last_error,
            devices: value.devices,
            selected_device: value.selected_device,
            revision: value.revision,
            config: value.config,
            config_path: value.config_path,
            mpris_status: value.mpris_status,
            ffmpeg_status: value.ffmpeg_status,
            database_optimization: value.database_optimization,
            shutting_down: value.shutting_down,
        }
    }
}

mod wire_system_option {
    use super::WireSystemState;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(value: &Option<WireSystemState>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<WireSystemState>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<WireSystemState>::deserialize(deserializer)
    }
}

// Config's omitted fields are unsafe with bincode's positional encoding. Keep
// the complete config as JSON while the enclosing state remains bincode.
mod wire_config {
    use crate::config::Config;
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;

    pub fn serialize<S: serde::Serializer>(
        value: &Arc<Config>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serde_json::to_string(value.as_ref())
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

fn wire_state(response: StateResponse) -> WireStateResponse {
    WireStateResponse {
        revisions: response.revisions,
        playback: response.playback,
        queue: response.queue,
        library: response.library,
        system: response.system.as_ref().map(WireSystemState::from),
    }
}

fn state_response(response: WireStateResponse) -> StateResponse {
    StateResponse {
        revisions: response.revisions,
        playback: response.playback,
        queue: response.queue,
        library: response.library,
        system: response.system.map(SystemState::from),
    }
}

struct FrameWriter<'a> {
    bytes: &'a mut Vec<u8>,
}

impl Write for FrameWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > 4 + MAX_RESPONSE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC frame exceeds maximum size",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_frame_into<T: Serialize>(value: &T, bytes: &mut Vec<u8>) -> Result<()> {
    bytes.clear();
    bytes.resize(4, 0);
    let mut writer = FrameWriter { bytes };
    encode_into_std_write(value, &mut writer, config::standard()).context("Encoding IPC frame")?;
    let payload_len = writer.bytes.len() - 4;
    let length = u32::try_from(payload_len).context("IPC frame is too large")?;
    writer.bytes[..4].copy_from_slice(&length.to_le_bytes());
    Ok(())
}

fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    encode_frame_into(value, &mut bytes)?;
    Ok(bytes)
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
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (bridge_stop, bridge_stop_rx) = crossbeam_channel::bounded(1);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let worker_path = path.clone();
        let worker = thread::Builder::new().name("rivu-ipc".into()).spawn(move || {
            let runtime = match Builder::new_current_thread().enable_io().enable_time().build() {
                Ok(runtime) => runtime,
                Err(error) => { let _ = ready_tx.send(Err(anyhow::Error::from(error))); return; }
            };
            let listener = { let _guard = runtime.enter(); UnixListener::bind(&worker_path) };
            let listener = match listener {
                Ok(listener) => listener,
                Err(error) => { let _ = ready_tx.send(Err(anyhow::Error::from(error))); return; }
            };
            if let Err(error) = fs::set_permissions(&worker_path, fs::Permissions::from_mode(0o600)) {
                let _ = ready_tx.send(Err(anyhow::Error::from(error))); return;
            }
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
            let Ok(bridge) = bridge else {
                let _ = ready_tx.send(Err(anyhow::anyhow!("starting IPC revision bridge"))); return;
            };
            let _ = ready_tx.send(Ok(()));
            runtime.block_on(run_server(listener, handle, revision_rx, shutdown_rx));
            let _ = bridge.join();
        })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                path,
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
    let mut scratch = [0u8; 8192];
    while pending.len() < 4 {
        let need = 4 - pending.len();
        let read = stream
            .read(&mut scratch[..need])
            .await
            .context("Reading IPC frame")?;
        if read == 0 {
            bail!("IPC connection closed while reading frame");
        }
        pending.extend_from_slice(&scratch[..read]);
    }
    let length = u32::from_le_bytes(pending[..4].try_into().unwrap()) as usize;
    if length > limit {
        bail!("IPC frame exceeds maximum size");
    }
    pending.reserve((4 + length).saturating_sub(pending.len()));
    while pending.len() < 4 + length {
        let need = (4 + length - pending.len()).min(scratch.len());
        let read = stream
            .read(&mut scratch[..need])
            .await
            .context("Reading IPC frame")?;
        if read == 0 {
            bail!("IPC connection closed while reading frame");
        }
        pending.extend_from_slice(&scratch[..read]);
    }
    pending.advance(4);
    Ok(pending.split_to(length).freeze())
}

async fn write_frame<S: AsyncWrite + Unpin>(stream: &mut S, value: &impl Serialize) -> Result<()> {
    stream
        .write_all(&encode_frame(value)?)
        .await
        .context("Writing IPC frame")?;
    stream.flush().await.context("Flushing IPC frame")?;
    Ok(())
}
async fn write_frame_buffered<S: AsyncWrite + Unpin>(
    stream: &mut S,
    value: &impl Serialize,
    buffer: &mut Vec<u8>,
) -> Result<()> {
    encode_frame_into(value, buffer)?;
    stream
        .write_all(buffer)
        .await
        .context("Writing IPC frame")?;
    stream.flush().await.context("Flushing IPC frame")?;
    Ok(())
}

fn error_frame(error: impl Into<String>, instance_id: [u8; 16]) -> ResponseFrame {
    ResponseFrame {
        version: PROTOCOL_VERSION,
        instance_id,
        response: WireResponse::Error(error.into()),
    }
}

fn response_frame(response: WireResponse, instance_id: [u8; 16]) -> ResponseFrame {
    ResponseFrame {
        version: PROTOCOL_VERSION,
        instance_id,
        response,
    }
}
fn validate_instance(request: [u8; 16], server: [u8; 16]) -> Result<()> {
    if request != UNKNOWN_INSTANCE && request != server {
        bail!("IPC server instance changed");
    }
    Ok(())
}

fn unpack_error(response: WireResponse) -> Result<WireResponse> {
    if let WireResponse::Error(error) = response {
        bail!("IPC server error: {error}");
    }
    Ok(response)
}

async fn run_command(handle: &AppHandle, command: Command) -> Result<Ack> {
    let duration = if matches!(command, Command::OptimizeDatabase) {
        Duration::from_secs(120)
    } else {
        Duration::from_secs(15)
    };
    timeout(
        duration,
        tokio::task::spawn_blocking({
            let handle = handle.clone();
            move || handle.request_ack(command)
        }),
    )
    .await
    .context("Reading IPC acknowledgement timed out")?
    .map_err(|error| anyhow::anyhow!("Core command task failed: {error}"))
}

async fn run_query(handle: &AppHandle, query: Query) -> Result<QueryResponse> {
    timeout(
        Duration::from_secs(15),
        tokio::task::spawn_blocking({
            let handle = handle.clone();
            move || handle.query(query)
        }),
    )
    .await
    .context("Reading IPC query timed out")?
    .map_err(|error| anyhow::anyhow!("Core query task failed: {error}"))
}

async fn wait_for_revisions(
    stream: &mut UnixStream,
    pending: &mut BytesMut,
    handle: &AppHandle,
    expected: StateRevisions,
    updates: &mut watch::Receiver<StateRevisions>,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<Option<StateRevisions>> {
    let mut scratch = [0u8; 8192];
    loop {
        let current = handle.state_revisions();
        if current != expected {
            return Some(Some(current));
        }
        tokio::select! {
            changed = updates.changed() => { if changed.is_err() { return None; } }
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return None; } }
            ready = stream.readable() => {
                if ready.is_err() { return None; }
                match stream.try_read(&mut scratch) {
                    Ok(0) => return None,
                    Ok(read) => {
                        pending.extend_from_slice(&scratch[..read]);
                        if pending.len() >= 4 {
                            let length = u32::from_le_bytes(pending[..4].try_into().unwrap()) as usize;
                            if length > MAX_REQUEST { return None; }
                            if pending.len() >= 4 + length { return Some(None); }
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
    instance_id: [u8; 16],
    mut updates: watch::Receiver<StateRevisions>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut pending = BytesMut::new();
    let mut handshaken = false;
    loop {
        let bytes = tokio::select! {
            frame = read_frame(&mut stream, MAX_REQUEST, &mut pending) => frame?,
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return Ok(()); } continue; }
        };
        let (version, request_instance) = match decode_request_header(&bytes) {
            Ok(header) => header,
            Err(error) => {
                write_frame(
                    &mut stream,
                    &error_frame(format!("Invalid IPC request header: {error}"), instance_id),
                )
                .await?;
                continue;
            }
        };
        if version != PROTOCOL_VERSION {
            write_frame(
                &mut stream,
                &error_frame(
                    format!("Unsupported IPC protocol version {version}"),
                    instance_id,
                ),
            )
            .await?;
            continue;
        }
        if let Err(error) = validate_instance(request_instance, instance_id) {
            write_frame(&mut stream, &error_frame(error.to_string(), instance_id)).await?;
            continue;
        }
        let frame = match decode_request_frame(&bytes) {
            Ok(frame) => frame,
            Err(error) => {
                write_frame(
                    &mut stream,
                    &error_frame(format!("Invalid IPC request: {error}"), instance_id),
                )
                .await?;
                continue;
            }
        };
        let request = match decode_request(frame) {
            Ok(request) => request,
            Err(error) => {
                write_frame(
                    &mut stream,
                    &error_frame(format!("Invalid IPC request: {error}"), instance_id),
                )
                .await?;
                continue;
            }
        };
        match request {
            DecodedRequest::State(sections) => {
                handshaken = true;
                let state = handle.state(sections);
                write_frame(
                    &mut stream,
                    &response_frame(WireResponse::State(wire_state(state)), instance_id),
                )
                .await?;
            }
            DecodedRequest::Query(query) => {
                let response = run_query(&handle, query).await?;
                write_frame(
                    &mut stream,
                    &response_frame(WireResponse::Query(response), instance_id),
                )
                .await?;
            }
            DecodedRequest::Command(command) => {
                let response = run_command(&handle, command).await?;
                write_frame(
                    &mut stream,
                    &response_frame(WireResponse::Ack(response), instance_id),
                )
                .await?;
            }
            DecodedRequest::Watch(expected) => {
                if !handshaken {
                    write_frame(
                        &mut stream,
                        &error_frame("Watcher session requires a state handshake", instance_id),
                    )
                    .await?;
                    continue;
                }
                let Some(result) = wait_for_revisions(
                    &mut stream,
                    &mut pending,
                    &handle,
                    expected,
                    &mut updates,
                    &mut shutdown,
                )
                .await
                else {
                    return Ok(());
                };
                let Some(revisions) = result else {
                    continue;
                };
                write_frame(
                    &mut stream,
                    &response_frame(WireResponse::Watch(revisions), instance_id),
                )
                .await?;
            }
        }
    }
}

async fn run_server(
    listener: UnixListener,
    handle: AppHandle,
    updates: watch::Receiver<StateRevisions>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut rng = rand::rng();
    let mut instance_id = [0u8; 16];
    while instance_id == UNKNOWN_INSTANCE {
        rng.fill(&mut instance_id);
    }
    let mut clients = Vec::new();
    let mut revisions = updates.clone();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue; };
                clients.retain(|task: &tokio::task::JoinHandle<Result<()>>| !task.is_finished());
                if clients.len() < MAX_CLIENTS { clients.push(tokio::spawn(serve_connection(stream, handle.clone(), instance_id, updates.clone(), shutdown.clone()))); }
            }
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
    read_buffer: BytesMut,
    write_buffer: Vec<u8>,
    instance_id: Option<[u8; 16]>,
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
        read_buffer: BytesMut::new(),
        write_buffer: Vec::new(),
        instance_id: None,
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

    fn ensure_handshake(&self) -> Result<[u8; 16]> {
        self.instance_id
            .ok_or_else(|| anyhow::anyhow!("Watcher session requires a state handshake"))
    }

    fn request_frame(&self, request: RequestKind) -> RequestFrame {
        RequestFrame {
            version: PROTOCOL_VERSION,
            instance_id: self.instance_id.unwrap_or(UNKNOWN_INSTANCE),
            request,
        }
    }

    fn exchange(&mut self, request: RequestFrame) -> Result<ResponseFrame> {
        self.runtime.block_on(async {
            let notified = self.cancel_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.cancelled.load(Ordering::Acquire) {
                bail!("IPC request cancelled");
            }
            tokio::select! {
                _ = &mut notified => bail!("IPC request cancelled"),
                response = async {
                    write_frame_buffered(&mut self.stream, &request, &mut self.write_buffer).await?;
                    let bytes = read_frame(&mut self.stream, MAX_RESPONSE, &mut self.read_buffer).await?;
                    decode_response_frame(&bytes)
                } => {
                    let frame = response?;
                    if let Some(expected) = self.instance_id {
                        if frame.instance_id != expected { bail!("IPC server instance changed"); }
                    } else if frame.instance_id == UNKNOWN_INSTANCE { bail!("IPC response did not identify server instance"); }
                    if self.cancelled.load(Ordering::Acquire) { bail!("IPC request cancelled"); }
                    Ok(frame)
                }
            }
        })
    }

    pub fn get_state(&mut self, sections: StateSections) -> Result<StateResponse> {
        let frame = self.exchange(self.request_frame(RequestKind::State { sections }))?;
        match unpack_error(frame.response)? {
            WireResponse::State(state) => {
                self.instance_id = Some(frame.instance_id);
                Ok(state_response(state))
            }
            _ => bail!("IPC response was not a state response"),
        }
    }

    pub fn query(&mut self, query: &Query) -> Result<QueryResponse> {
        let _ = self.ensure_handshake()?;
        let payload = serde_json::to_string(query)?;
        let frame = self.exchange(self.request_frame(RequestKind::Query { query: payload }))?;
        match unpack_error(frame.response)? {
            WireResponse::Query(response) => Ok(response),
            _ => bail!("IPC response was not a query response"),
        }
    }

    pub fn watch(&mut self, revisions: StateRevisions) -> Result<StateRevisions> {
        self.watch_until(revisions)?
            .ok_or_else(|| anyhow::anyhow!("Watcher cancelled"))
    }

    pub(crate) fn watch_until(
        &mut self,
        revisions: StateRevisions,
    ) -> Result<Option<StateRevisions>> {
        let _ = self.ensure_handshake()?;
        let frame = match self.exchange(self.request_frame(RequestKind::Watch { revisions })) {
            Ok(frame) => frame,
            Err(_) if self.cancelled.load(Ordering::Acquire) => return Ok(None),
            Err(error) => return Err(error),
        };
        match unpack_error(frame.response)? {
            WireResponse::Watch(revisions) => Ok(Some(revisions)),
            _ => bail!("IPC response was not a watch response"),
        }
    }
}

async fn one_shot(
    path: &Path,
    request: RequestKind,
    cancellation: Option<(Arc<AtomicBool>, Arc<tokio::sync::Notify>)>,
) -> Result<ResponseFrame> {
    let operation = async {
        let mut stream = UnixStream::connect(path).await.with_context(|| {
            format!(
                "Cannot connect to Rivu at {}. Start `rivu` or `rivu serve` first.",
                path.display()
            )
        })?;
        let frame = RequestFrame {
            version: PROTOCOL_VERSION,
            instance_id: UNKNOWN_INSTANCE,
            request,
        };
        write_frame(&mut stream, &frame).await?;
        let bytes = timeout(
            Duration::from_secs(120),
            read_frame(&mut stream, MAX_RESPONSE, &mut BytesMut::new()),
        )
        .await
        .context("Reading IPC response timed out")??;
        let frame = decode_response_frame(&bytes)?;
        if frame.version != PROTOCOL_VERSION {
            bail!("Unsupported IPC protocol version {}", frame.version);
        }
        unpack_error(frame.response.clone())?;
        Ok(frame)
    };
    let Some((cancelled, notify)) = cancellation else {
        return operation.await;
    };
    let notified = notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    if cancelled.load(Ordering::Acquire) {
        bail!("IPC request cancelled");
    }
    tokio::select! { _ = &mut notified => bail!("IPC request cancelled"), response = operation => response }
}

pub fn query(path: &Path, query: &Query) -> Result<QueryResponse> {
    let runtime = client_runtime()?;
    let frame = runtime.block_on(one_shot(
        path,
        RequestKind::Query {
            query: serde_json::to_string(query)?,
        },
        None,
    ))?;
    match frame.response {
        WireResponse::Query(response) => Ok(response),
        _ => bail!("IPC response was not a query response"),
    }
}

pub(crate) fn query_with_cancel(
    path: &Path,
    query: &Query,
    cancelled: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
) -> Result<QueryResponse> {
    let runtime = client_runtime()?;
    let frame = runtime.block_on(one_shot(
        path,
        RequestKind::Query {
            query: serde_json::to_string(query)?,
        },
        Some((cancelled, notify)),
    ))?;
    match frame.response {
        WireResponse::Query(response) => Ok(response),
        _ => bail!("IPC response was not a query response"),
    }
}

pub fn get_state(path: &Path, sections: StateSections) -> Result<StateResponse> {
    let runtime = client_runtime()?;
    let frame = runtime.block_on(one_shot(path, RequestKind::State { sections }, None))?;
    match frame.response {
        WireResponse::State(response) => Ok(state_response(response)),
        _ => bail!("IPC response was not a state response"),
    }
}

pub fn request_ack(path: &Path, command: &Command) -> Result<Ack> {
    let runtime = client_runtime()?;
    let frame = runtime.block_on(one_shot(
        path,
        RequestKind::Command {
            command: serde_json::to_string(command)?,
        },
        None,
    ))?;
    match frame.response {
        WireResponse::Ack(response) => Ok(response),
        _ => bail!("IPC response was not an acknowledgement"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("ipc_tests.rs");
}
