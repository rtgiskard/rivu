use crate::{
    config::Config,
    core::AppHandle,
    model::{
        Ack, AppState, Command, DatabaseOptimization, HistoryEntry, PlaybackStatus, Playlist,
        QueueEntry, RepeatMode, Response, Track,
    },
};
use anyhow::{Context, Error, Result, bail};
use bincode::{
    config,
    serde::{decode_from_slice, encode_to_vec},
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
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
    Command(Command),
    Ack(Command),
    Watch { revision: u16 },
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
const FLAG_SHUFFLE: u16 = 1;
const FLAG_SCANNING: u16 = 1 << 1;
const FLAG_SHUTTING_DOWN: u16 = 1 << 2;

fn revision_matches(current: u64, expected: u16) -> bool {
    current as u16 == expected
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PlaybackSnapshot {
    position_ms: u64,
    duration_ms: Option<u64>,
    volume: u16,
    status: u8,
    repeat: u8,
    flags: u16,
    revision: u16,
    seek_revision: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CompactOverview {
    library: Vec<Track>,
    library_revision: u64,
    library_structure_revision: u64,
    queue: Vec<QueueEntry>,
    current_queue_id: Option<u64>,
    scan_message: String,
    last_error: Option<String>,
    playback: PlaybackSnapshot,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FullStatus {
    overview: CompactOverview,
    playlists: Vec<Playlist>,
    history: Vec<HistoryEntry>,
    devices: Vec<String>,
    selected_device: Option<String>,
    config: Config,
    database_optimization: Option<DatabaseOptimization>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum WireState {
    Overview(CompactOverview),
    Full(Box<FullStatus>),
    Ack { revision: u64 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WireResponse {
    ok: bool,
    error: Option<String>,
    state: WireState,
}

fn playback_snapshot(state: &AppState) -> PlaybackSnapshot {
    PlaybackSnapshot {
        position_ms: (state.position.max(0.0) * 1000.0).round() as u64,
        duration_ms: state
            .duration
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(|value| (value * 1000.0).round() as u64),
        volume: (state.volume.clamp(0.0, 1.0) * u16::MAX as f32).round() as u16,
        status: match state.status {
            PlaybackStatus::Stopped => 0,
            PlaybackStatus::Playing => 1,
            PlaybackStatus::Paused => 2,
        },
        repeat: match state.repeat {
            RepeatMode::Off => 0,
            RepeatMode::All => 1,
            RepeatMode::One => 2,
        },
        flags: (state.shuffle as u16 * FLAG_SHUFFLE)
            | (state.scanning as u16 * FLAG_SCANNING)
            | (state.shutting_down as u16 * FLAG_SHUTTING_DOWN),
        revision: state.revision as u16,
        seek_revision: state.seek_revision as u16,
    }
}

fn state_from_overview(state: CompactOverview) -> AppState {
    let CompactOverview {
        library,
        library_revision,
        library_structure_revision,
        queue,
        current_queue_id,
        scan_message,
        last_error,
        playback,
    } = state;
    AppState {
        library: Arc::new(library),
        library_revision,
        library_structure_revision,
        playlists: Arc::new(Vec::new()),
        queue: Arc::new(queue),
        history: Arc::new(Vec::new()),
        current_queue_id,
        status: match playback.status {
            1 => PlaybackStatus::Playing,
            2 => PlaybackStatus::Paused,
            _ => PlaybackStatus::Stopped,
        },
        position: playback.position_ms as f64 / 1000.0,
        duration: playback.duration_ms.map(|value| value as f64 / 1000.0),
        volume: playback.volume as f32 / u16::MAX as f32,
        shuffle: playback.flags & FLAG_SHUFFLE != 0,
        repeat: match playback.repeat {
            1 => RepeatMode::All,
            2 => RepeatMode::One,
            _ => RepeatMode::Off,
        },
        scanning: playback.flags & FLAG_SCANNING != 0,
        scan_message,
        last_error,
        devices: Arc::new(Vec::new()),
        selected_device: None,
        revision: playback.revision as u64,
        seek_revision: playback.seek_revision as u64,
        config: Arc::new(Config::default()),
        config_path: PathBuf::new(),
        mpris_status: String::new(),
        ffmpeg_status: String::new(),
        database_optimization: None,
        shutting_down: playback.flags & FLAG_SHUTTING_DOWN != 0,
    }
}

impl From<CompactOverview> for AppState {
    fn from(state: CompactOverview) -> Self {
        state_from_overview(state)
    }
}

impl From<FullStatus> for AppState {
    fn from(state: FullStatus) -> Self {
        let mut app = state_from_overview(state.overview);
        app.playlists = Arc::new(state.playlists);
        app.history = Arc::new(state.history);
        app.devices = Arc::new(state.devices);
        app.selected_device = state.selected_device;
        app.config = Arc::new(state.config);
        app.database_optimization = state.database_optimization;
        app
    }
}

impl WireResponse {
    fn from_response(response: Response, compact: bool) -> Self {
        let state = response.state;
        let overview = CompactOverview {
            library: state.library.as_ref().clone(),
            library_revision: state.library_revision,
            library_structure_revision: state.library_structure_revision,
            queue: state.queue.as_ref().clone(),
            current_queue_id: state.current_queue_id,
            scan_message: state.scan_message.clone(),
            last_error: state.last_error.clone(),
            playback: playback_snapshot(&state),
        };
        let state = if compact {
            WireState::Overview(overview)
        } else {
            WireState::Full(Box::new(FullStatus {
                overview,
                playlists: state.playlists.as_ref().clone(),
                history: state.history.as_ref().clone(),
                devices: state.devices.as_ref().clone(),
                selected_device: state.selected_device,
                config: state.config.as_ref().clone(),
                database_optimization: state.database_optimization,
            }))
        };
        Self {
            ok: response.ok,
            error: response.error,
            state,
        }
    }
    fn from_ack(ack: Ack) -> Self {
        Self {
            ok: ack.ok,
            error: ack.error,
            state: WireState::Ack {
                revision: ack.revision,
            },
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

#[derive(Default)]
struct OverviewCache {
    library: Weak<Vec<Track>>,
    queue: Weak<Vec<QueueEntry>>,
    current: Option<u64>,
    tracks: Arc<Vec<Track>>,
}

impl OverviewCache {
    fn cached(&self, state: &AppState) -> Option<Arc<Vec<Track>>> {
        let unchanged = self.library.as_ptr() == Arc::as_ptr(&state.library)
            && self.queue.as_ptr() == Arc::as_ptr(&state.queue)
            && self.current == state.current_queue_id;
        unchanged.then(|| self.tracks.clone())
    }

    fn update(&mut self, state: &AppState, tracks: Arc<Vec<Track>>) {
        self.library = Arc::downgrade(&state.library);
        self.queue = Arc::downgrade(&state.queue);
        self.current = state.current_queue_id;
        self.tracks = tracks;
    }

    fn response(state: AppState, tracks: Arc<Vec<Track>>) -> Response {
        Response {
            ok: true,
            error: None,
            state: AppState {
                library: tracks,
                library_revision: state.library_revision,
                library_structure_revision: state.library_structure_revision,
                playlists: Arc::new(Vec::new()),
                queue: state.queue.clone(),
                history: Arc::new(Vec::new()),
                current_queue_id: state.current_queue_id,
                status: state.status,
                position: state.position,
                duration: state.duration,
                volume: state.volume,
                shuffle: state.shuffle,
                repeat: state.repeat,
                scanning: state.scanning,
                scan_message: state.scan_message.clone(),
                last_error: state.last_error.clone(),
                devices: state.devices.clone(),
                selected_device: state.selected_device.clone(),
                revision: state.revision,
                seek_revision: state.seek_revision,
                config: state.config.clone(),
                config_path: state.config_path.clone(),
                mpris_status: state.mpris_status.clone(),
                ffmpeg_status: state.ffmpeg_status.clone(),
                database_optimization: state.database_optimization.clone(),
                shutting_down: state.shutting_down,
            },
        }
    }
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
                let listener = match UnixListener::bind(&worker_path) {
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
                let (revision_tx, revision_rx) = watch::channel(handle.snapshot().revision);
                let updates = handle.subscribe();
                let bridge_handle = handle.clone();
                let bridge = thread::Builder::new()
                    .name("rivu-ipc-revisions".into())
                    .spawn(move || {
                        loop {
                            crossbeam_channel::select! {
                                recv(updates) -> message => {
                                    if message.is_ok() { let _ = revision_tx.send(bridge_handle.snapshot().revision); } else { break; }
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
    pending: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    let mut header = [0u8; 4];
    let mut offset = 0;
    while offset < 4 {
        if pending.is_empty() {
            stream
                .read_exact(&mut header[offset..])
                .await
                .context("Reading IPC frame length")?;
            offset = 4;
        } else {
            header[offset] = pending.remove(0);
            offset += 1;
        }
    }
    let length = u32::from_le_bytes(header) as usize;
    if length > limit {
        bail!("IPC frame exceeds maximum size");
    }
    let mut bytes = vec![0u8; length];
    let mut offset = 0;
    while offset < length {
        if pending.is_empty() {
            stream
                .read_exact(&mut bytes[offset..])
                .await
                .context("Reading IPC frame payload")?;
            offset = length;
        } else {
            bytes[offset] = pending.remove(0);
            offset += 1;
        }
    }
    Ok(bytes)
}

async fn write_frame<S: AsyncWrite + Unpin>(stream: &mut S, value: &impl Serialize) -> Result<()> {
    stream
        .write_all(&encode_frame(value)?)
        .await
        .context("Writing IPC frame")?;
    stream.flush().await.context("Flushing IPC frame")?;
    Ok(())
}

fn response_frame(response: Response, instance_id: u16, compact: bool) -> ResponseFrame {
    ResponseFrame {
        instance_id,
        revision: response.state.revision as u16,
        response: WireResponse::from_response(response, compact),
    }
}

fn ack_frame(ack: Ack, instance_id: u16) -> ResponseFrame {
    ResponseFrame {
        instance_id,
        revision: ack.revision as u16,
        response: WireResponse::from_ack(ack),
    }
}
fn unpack_response(frame: ResponseFrame) -> Result<Response> {
    let WireResponse { ok, error, state } = frame.response;
    let state = match state {
        WireState::Overview(state) => state.into(),
        WireState::Full(state) => (*state).into(),
        WireState::Ack { .. } => bail!("IPC acknowledgement used where state was required"),
    };
    Ok(Response { ok, error, state })
}

fn overview_response(state: AppState, overview: &Mutex<OverviewCache>) -> Response {
    let cached = overview.lock().cached(&state);
    let tracks = cached.unwrap_or_else(|| {
        let mut ids: HashSet<i64> = state.queue.iter().map(|entry| entry.track_id).collect();
        if let Some(track) = state.current_track() {
            ids.insert(track.id);
        }
        let tracks: Arc<Vec<Track>> = Arc::new(
            state
                .library
                .iter()
                .filter(|track| ids.contains(&track.id))
                .cloned()
                .collect(),
        );
        overview.lock().update(&state, tracks.clone());
        tracks
    });
    OverviewCache::response(state, tracks)
}

async fn command(handle: &AppHandle, command: Command) -> Response {
    let fallback = handle.snapshot();
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
        Ok(Err(error)) => Response {
            ok: false,
            error: Some(format!("Core command task failed: {error}")),
            state: fallback,
        },
        Err(_) => Response {
            ok: false,
            error: Some("Core command timed out".into()),
            state: fallback,
        },
    }
}

async fn wait_for_revision(
    stream: &mut UnixStream,
    pending: &mut Vec<u8>,
    handle: &AppHandle,
    revision: u16,
    overview: &Mutex<OverviewCache>,
    updates: &mut watch::Receiver<u64>,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<Response> {
    loop {
        let state = handle.snapshot();
        if !revision_matches(state.revision, revision) || state.shutting_down {
            return Some(overview_response(state, overview));
        }
        tokio::select! {
            changed = updates.changed() => { if changed.is_err() { return None; } }
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return None; } }
            ready = stream.readable() => {
                if ready.is_err() { return None; }
                let mut probe = [0u8; 1];
                match stream.try_read(&mut probe) { Ok(0) => return None, Ok(count) => pending.extend_from_slice(&probe[..count]), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}, Err(_) => return None }
            }
        }
    }
}

async fn serve_connection(
    mut stream: UnixStream,
    handle: AppHandle,
    overview: Arc<Mutex<OverviewCache>>,
    instance_id: u16,
    mut updates: watch::Receiver<u64>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut pending = Vec::new();
    loop {
        let bytes = tokio::select! { frame = read_frame(&mut stream, MAX_REQUEST, &mut pending) => frame?, changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { return Ok(()); } continue; } };
        let request = match decode_frame::<RequestFrame>(&bytes) {
            Ok(request) => request,
            Err(error) => {
                let response = Response {
                    ok: false,
                    error: Some(format!("Invalid command: {error}")),
                    state: handle.snapshot(),
                };
                write_frame(&mut stream, &response_frame(response, instance_id, false)).await?;
                continue;
            }
        };
        let (response, compact) = match request.request {
            RequestKind::Command(Command::Overview) => {
                (overview_response(handle.snapshot(), &overview), false)
            }
            RequestKind::Command(Command::Status) => {
                (command(&handle, Command::Status).await, false)
            }
            RequestKind::Command(cmd) => {
                let response = command(&handle, cmd).await;
                if response.ok {
                    (overview_response(response.state, &overview), true)
                } else {
                    (response, false)
                }
            }
            RequestKind::Ack(cmd) => {
                let response = command(&handle, cmd).await;
                let ack = Ack {
                    ok: response.ok,
                    error: response.error,
                    revision: response.state.revision,
                };
                write_frame(&mut stream, &ack_frame(ack, instance_id)).await?;
                continue;
            }

            RequestKind::Watch { revision } => {
                let Some(response) = wait_for_revision(
                    &mut stream,
                    &mut pending,
                    &handle,
                    revision,
                    &overview,
                    &mut updates,
                    &mut shutdown,
                )
                .await
                else {
                    return Ok(());
                };
                (response, true)
            }
        };
        write_frame(&mut stream, &response_frame(response, instance_id, compact)).await?;
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
    let overview = Arc::new(Mutex::new(OverviewCache::default()));
    let mut clients: Vec<tokio::task::JoinHandle<Result<()>>> = Vec::new();
    let mut revisions = updates.clone();
    loop {
        tokio::select! {
            accepted = listener.accept() => { let Ok((stream, _)) = accepted else { continue; }; clients.retain(|task| !task.is_finished()); if clients.len() < 16 { clients.push(tokio::spawn(serve_connection(stream, handle.clone(), overview.clone(), instance_id, updates.clone(), shutdown.clone()))); } }
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
    pub fn watch(&mut self, revision: u16) -> Result<Response> {
        self.watch_until(revision, || false)
            .and_then(|response| response.ok_or_else(|| anyhow::anyhow!("Watcher cancelled")))
    }
    pub(crate) fn watch_until(
        &mut self,
        revision: u16,
        cancelled: impl Fn() -> bool,
    ) -> Result<Option<Response>> {
        if cancelled() || self.cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        let request = RequestFrame {
            instance_id: CLIENT_INSTANCE_UNKNOWN,
            request: RequestKind::Watch { revision },
        };
        let cancelled_flag = self.cancelled.clone();
        let notify = self.cancel_notify.clone();
        let result = self.runtime.block_on(async { let mut pending = Vec::new(); write_frame(&mut self.stream, &request).await?; tokio::select! { bytes = read_frame(&mut self.stream, MAX_RESPONSE, &mut pending) => { let bytes = bytes?; Ok(Some(unpack_response(decode_frame::<ResponseFrame>(&bytes)?)?)) }, _ = notify.notified() => Ok(None) } });
        if cancelled_flag.load(Ordering::Acquire) {
            Ok(None)
        } else {
            result
        }
    }
}
pub fn watch(path: &Path, revision: u16) -> Result<Response> {
    watch_session(path)?.watch(revision)
}
pub fn request(path: &Path, command: &Command) -> Result<Response> {
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
            request: RequestKind::Command(command.clone()),
        };
        write_frame(&mut stream, &request).await?;
        let bytes = if matches!(command, Command::OptimizeDatabase) {
            read_frame(&mut stream, MAX_RESPONSE, &mut Vec::new()).await?
        } else {
            timeout(
                Duration::from_secs(15),
                read_frame(&mut stream, MAX_RESPONSE, &mut Vec::new()),
            )
            .await
            .context("Reading IPC response timed out")??
        };
        Ok(unpack_response(decode_frame::<ResponseFrame>(&bytes)?)?)
    })
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
            request: RequestKind::Ack(command.clone()),
        };
        write_frame(&mut stream, &request).await?;
        let bytes = timeout(
            Duration::from_secs(15),
            read_frame(&mut stream, MAX_RESPONSE, &mut Vec::new()),
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
