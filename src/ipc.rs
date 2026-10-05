use crate::{
    core::AppHandle,
    model::{AppState, Command, QueueEntry, Response, Track},
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
    response: Response,
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
                    let spawn =
                        thread::Builder::new()
                            .name("rivu-client".into())
                            .spawn(move || {
                                let _ = serve_connection(
                                    stream,
                                    &client_handle,
                                    &client_overview,
                                    server_instance_id,
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
    let mut header = [0u8; 4];
    stream
        .read_exact(&mut header)
        .context("Reading IPC frame length")?;
    let length = u32::from_le_bytes(header) as usize;
    if length > limit {
        bail!("IPC frame exceeds maximum size");
    }
    let mut bytes = vec![0u8; length];
    stream
        .read_exact(&mut bytes)
        .context("Reading IPC frame payload")?;
    Ok(bytes)
}

fn response_frame(response: Response, instance_id: u16) -> ResponseFrame {
    let revision = response.state.revision as u16;
    ResponseFrame {
        instance_id,
        revision,
        response,
    }
}

fn unpack_response(frame: ResponseFrame) -> Response {
    frame.response
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
    handle: &AppHandle,
    revision: u16,
    overview: &Mutex<OverviewCache>,
) -> Response {
    let updates = handle.subscribe();
    loop {
        let state = handle.snapshot();
        if (state.revision as u16) != revision || state.shutting_down {
            return overview_response(state, overview);
        }
        if updates.recv().is_err() {
            return overview_response(handle.snapshot(), overview);
        }
    }
}

fn serve_connection(
    mut stream: UnixStream,
    handle: &AppHandle,
    overview: &Mutex<OverviewCache>,
    instance_id: u16,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let bytes = read_frame(&mut stream, MAX_REQUEST)?;
    let response = match decode_frame::<RequestFrame>(&bytes) {
        Ok(RequestFrame {
            request: RequestKind::Command(Command::Overview),
            ..
        }) => overview_response(handle.snapshot(), overview),
        Ok(RequestFrame {
            request: RequestKind::Command(Command::Status),
            ..
        }) => handle.request(Command::Status),
        Ok(RequestFrame {
            request: RequestKind::Command(command),
            ..
        }) => {
            let response = handle.request(command);
            if response.ok {
                overview_response(response.state, overview)
            } else {
                response
            }
        }
        Ok(RequestFrame {
            request: RequestKind::Watch { revision },
            ..
        }) => wait_for_revision(handle, revision, overview),
        Err(error) => Response {
            ok: false,
            error: Some(format!("Invalid command: {error}")),
            state: handle.snapshot(),
        },
    };
    stream.write_all(&encode_frame(&response_frame(response, instance_id))?)?;
    Ok(())
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
pub fn watch(path: &Path, revision: u16) -> Result<Response> {
    let mut stream = UnixStream::connect(path).with_context(|| {
        format!(
            "Cannot connect to Rivu at {}. Start `rivu` or `rivu serve` first.",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let request = RequestFrame {
        instance_id: CLIENT_INSTANCE_UNKNOWN,
        request: RequestKind::Watch { revision },
    };
    stream.write_all(&encode_frame(&request)?)?;
    Ok(unpack_response(decode_frame(&read_frame(
        &mut stream,
        MAX_RESPONSE,
    )?)?))
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
