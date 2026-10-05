use crate::{
    config::Config,
    core::AppHandle,
    model::{
        AppState, Command, DatabaseOptimization, HistoryEntry, PlaybackStatus, Playlist,
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
    io::{Read, Write},
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
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
    Watch { revision: u16 },
}

#[derive(Serialize, Deserialize)]
struct ResponseFrame {
    instance_id: u16,
    revision: u16,
    response: WireResponse,
}

const FLAG_SHUFFLE: u16 = 1;
const FLAG_SCANNING: u16 = 1 << 1;
const FLAG_SHUTTING_DOWN: u16 = 1 << 2;

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
}

impl From<WireResponse> for Response {
    fn from(response: WireResponse) -> Self {
        Self {
            ok: response.ok,
            error: response.error,
            state: match response.state {
                WireState::Overview(state) => state.into(),
                WireState::Full(state) => (*state).into(),
            },
        }
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
    worker: Option<JoinHandle<()>>,
}

pub fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    match UnixListener::bind(path) {
        Ok(listener) => {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            Ok(listener)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            match UnixStream::connect(path) {
                Ok(_) => bail!(
                    "Rivu is already running at {}. Use its CLI or TUI.",
                    path.display()
                ),
                Err(connect)
                    if matches!(
                        connect.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    if fs::symlink_metadata(path)?.file_type().is_socket() {
                        fs::remove_file(path)?;
                        let listener = UnixListener::bind(path)?;
                        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
                        Ok(listener)
                    } else {
                        bail!("Refusing to replace non-socket {}", path.display());
                    }
                }
                Err(connect) => Err(connect).context("Checking existing Rivu socket"),
            }
        }
        Err(error) => Err(error).context("Binding local Rivu socket"),
    }
}

impl Server {
    pub fn start(path: PathBuf, listener: UnixListener, handle: AppHandle) -> Result<Self> {
        let instance_id = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u16)
            .wrapping_add(std::process::id() as u16);
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let active = Arc::new(AtomicUsize::new(0));
        let overview = Arc::new(Mutex::new(OverviewCache::default()));
        let server_instance_id = instance_id;
        let worker = thread::Builder::new()
            .name("rivu-ipc".into())
            .spawn(move || {
                let mut clients: Vec<JoinHandle<()>> = Vec::new();
                for connection in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(stream) = connection else {
                        continue;
                    };
                    if active.fetch_add(1, Ordering::AcqRel) >= 16 {
                        active.fetch_sub(1, Ordering::AcqRel);
                        continue;
                    }
                    let count = active.clone();
                    let client_handle = handle.clone();
                    let client_overview = overview.clone();
                    let client_stop = stop.clone();
                    let spawn =
                        thread::Builder::new()
                            .name("rivu-client".into())
                            .spawn(move || {
                                let _ = serve_connection(
                                    stream,
                                    &client_handle,
                                    &client_overview,
                                    server_instance_id,
                                    &client_stop,
                                );
                                count.fetch_sub(1, Ordering::AcqRel);
                            });
                    match spawn {
                        Ok(worker) => {
                            clients.retain(|worker| !worker.is_finished());
                            clients.push(worker);
                        }
                        Err(_) => {
                            active.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                }
                for client in clients {
                    let _ = client.join();
                }
            })?;
        Ok(Self {
            path,
            stopping,
            worker: Some(worker),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.path);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

fn read_frame(stream: &mut UnixStream, limit: usize) -> Result<Vec<u8>> {
    let mut pending = Vec::new();
    read_frame_with_prefix(stream, limit, &mut pending)
}

fn read_frame_with_prefix(
    stream: &mut UnixStream,
    limit: usize,
    pending: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    let mut header = [0u8; 4];
    let mut offset = 0;
    while offset < header.len() {
        if !pending.is_empty() {
            header[offset] = pending.remove(0);
            offset += 1;
        } else {
            stream
                .read_exact(&mut header[offset..])
                .context("Reading IPC frame length")?;
            offset = header.len();
        }
    }
    let length = u32::from_le_bytes(header) as usize;
    if length > limit {
        bail!("IPC frame exceeds maximum size");
    }
    let mut bytes = vec![0u8; length];
    let mut offset = 0;
    while offset < bytes.len() {
        if !pending.is_empty() {
            bytes[offset] = pending.remove(0);
            offset += 1;
        } else {
            stream
                .read_exact(&mut bytes[offset..])
                .context("Reading IPC frame payload")?;
            offset = bytes.len();
        }
    }
    Ok(bytes)
}

fn read_frame_interruptible<F: Fn() -> bool>(
    stream: &mut UnixStream,
    limit: usize,
    cancelled: F,
) -> Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    let mut offset = 0;
    while offset < header.len() {
        if cancelled() {
            return Ok(None);
        }
        match stream.read(&mut header[offset..]) {
            Ok(0) => bail!("IPC peer closed while reading frame length"),
            Ok(count) => offset += count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error).context("Reading IPC frame length"),
        }
    }
    let length = u32::from_le_bytes(header) as usize;
    if length > limit {
        bail!("IPC frame exceeds maximum size");
    }
    let mut bytes = vec![0u8; length];
    let mut offset = 0;
    while offset < bytes.len() {
        if cancelled() {
            return Ok(None);
        }
        match stream.read(&mut bytes[offset..]) {
            Ok(0) => bail!("IPC peer closed while reading frame payload"),
            Ok(count) => offset += count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error).context("Reading IPC frame payload"),
        }
    }
    Ok(Some(bytes))
}

fn response_frame(response: Response, instance_id: u16, compact: bool) -> ResponseFrame {
    let revision = response.state.revision as u16;
    ResponseFrame {
        instance_id,
        revision,
        response: WireResponse::from_response(response, compact),
    }
}

fn unpack_response(frame: ResponseFrame) -> Response {
    frame.response.into()
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

fn wait_for_revision(
    stream: &mut UnixStream,
    pending: &mut Vec<u8>,
    handle: &AppHandle,
    revision: u16,
    overview: &Mutex<OverviewCache>,
    stopping: &AtomicBool,
) -> Option<Response> {
    if stream.set_nonblocking(true).is_err() {
        return None;
    }
    let updates = handle.subscribe();
    let mut probe = [0u8; 1];
    loop {
        if stopping.load(Ordering::Acquire) {
            let _ = stream.set_nonblocking(false);
            return None;
        }
        let state = handle.snapshot();
        if (state.revision as u16) != revision || state.shutting_down {
            let _ = stream.set_nonblocking(false);
            return Some(overview_response(state, overview));
        }
        match stream.read(&mut probe) {
            Ok(0) => {
                let _ = stream.set_nonblocking(false);
                return None;
            }
            Ok(count) => pending.extend_from_slice(&probe[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
                let _ = stream.set_nonblocking(false);
                return None;
            }
        }
        let _ = updates.recv_timeout(Duration::from_millis(100));
    }
}

fn serve_connection(
    mut stream: UnixStream,
    handle: &AppHandle,
    overview: &Mutex<OverviewCache>,
    instance_id: u16,
    stopping: &AtomicBool,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut pending = Vec::new();
    loop {
        let bytes = read_frame_with_prefix(&mut stream, MAX_REQUEST, &mut pending)?;
        let request = match decode_frame::<RequestFrame>(&bytes) {
            Ok(request) => request,
            Err(error) => {
                let response = Response {
                    ok: false,
                    error: Some(format!("Invalid command: {error}")),
                    state: handle.snapshot(),
                };
                stream.write_all(&encode_frame(&response_frame(
                    response,
                    instance_id,
                    false,
                ))?)?;
                continue;
            }
        };
        let (response, compact) = match request.request {
            RequestKind::Command(Command::Overview) => {
                (overview_response(handle.snapshot(), overview), false)
            }
            RequestKind::Command(Command::Status) => (handle.request(Command::Status), false),
            RequestKind::Command(command) => {
                let response = handle.request(command);
                if response.ok {
                    (overview_response(response.state, overview), false)
                } else {
                    (response, false)
                }
            }
            RequestKind::Watch { revision } => {
                let Some(response) = wait_for_revision(
                    &mut stream,
                    &mut pending,
                    handle,
                    revision,
                    overview,
                    stopping,
                ) else {
                    return Ok(());
                };
                (response, true)
            }
        };
        stream.write_all(&encode_frame(&response_frame(
            response,
            instance_id,
            compact,
        ))?)?;
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
    stream: UnixStream,
}

pub fn watch_session(path: &Path) -> Result<WatcherSession> {
    let stream = UnixStream::connect(path).with_context(|| {
        format!(
            "Cannot connect to Rivu at {}. Start `rivu` or `rivu serve` first.",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    Ok(WatcherSession { stream })
}

impl WatcherSession {
    pub fn watch(&mut self, revision: u16) -> Result<Response> {
        self.watch_until(revision, || false)
            .and_then(|response| response.ok_or_else(|| anyhow::anyhow!("Watcher cancelled")))
    }

    pub(crate) fn watch_until(
        &mut self,
        revision: u16,
        cancelled: impl Fn() -> bool,
    ) -> Result<Option<Response>> {
        let request = RequestFrame {
            instance_id: CLIENT_INSTANCE_UNKNOWN,
            request: RequestKind::Watch { revision },
        };
        self.stream.write_all(&encode_frame(&request)?)?;
        let bytes = read_frame_interruptible(&mut self.stream, MAX_RESPONSE, cancelled)?;
        bytes
            .map(|bytes| decode_frame::<ResponseFrame>(&bytes).map(unpack_response))
            .transpose()
    }
}

pub fn watch(path: &Path, revision: u16) -> Result<Response> {
    watch_session(path)?.watch(revision)
}

pub fn request(path: &Path, command: &Command) -> Result<Response> {
    let mut stream = UnixStream::connect(path).with_context(|| {
        format!(
            "Cannot connect to Rivu at {}. Start `rivu` or `rivu serve` first.",
            path.display()
        )
    })?;
    stream.set_read_timeout(if matches!(command, Command::OptimizeDatabase) {
        None
    } else {
        Some(Duration::from_secs(15))
    })?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let request = RequestFrame {
        instance_id: CLIENT_INSTANCE_UNKNOWN,
        request: RequestKind::Command(command.clone()),
    };
    stream.write_all(&encode_frame(&request)?)?;
    Ok(unpack_response(decode_frame(&read_frame(
        &mut stream,
        MAX_RESPONSE,
    )?)?))
}
