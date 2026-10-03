use crate::{
    core::AppHandle,
    model::{Command, QueueEntry, Response, Track},
};
use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const MAX_REQUEST: u64 = 64 * 1024;
const MAX_RESPONSE: u64 = 64 * 1024 * 1024;

#[derive(Default)]
struct OverviewCache {
    library: Option<Arc<Vec<Track>>>,
    queue: Option<Arc<Vec<QueueEntry>>>,
    current: Option<u64>,
    tracks: Arc<Vec<Track>>,
}

impl OverviewCache {
    fn compact(&mut self, response: &mut Response) {
        let state = &mut response.state;
        let library_changed = self
            .library
            .as_ref()
            .is_none_or(|cached| !Arc::ptr_eq(cached, &state.library));
        let queue_changed = self
            .queue
            .as_ref()
            .is_none_or(|cached| !Arc::ptr_eq(cached, &state.queue));
        if library_changed || queue_changed || self.current != state.current_queue_id {
            let mut ids: HashSet<i64> = state
                .queue
                .iter()
                .take(12)
                .map(|entry| entry.track_id)
                .collect();
            if let Some(track) = state.current_track() {
                ids.insert(track.id);
            }
            self.tracks = Arc::new(
                state
                    .library
                    .iter()
                    .filter(|track| ids.contains(&track.id))
                    .cloned()
                    .collect(),
            );
            self.library = Some(state.library.clone());
            self.queue = Some(state.queue.clone());
            self.current = state.current_queue_id;
        }
        state.library = self.tracks.clone();
        state.playlists = Arc::new(Vec::new());
        state.history = Arc::new(Vec::new());
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
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let active = Arc::new(AtomicUsize::new(0));
        let overview = Arc::new(Mutex::new(OverviewCache::default()));
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
                                let _ = serve_connection(stream, &client_handle, &client_overview);
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

fn read_message(stream: &mut UnixStream, limit: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    let read = BufReader::new(stream.take(limit + 1)).read_until(b'\n', &mut bytes)?;
    if read == 0 {
        bail!("Connection closed without a message");
    }
    if read as u64 > limit || bytes.last() != Some(&b'\n') {
        bail!("Message too large or incomplete");
    }
    Ok(bytes)
}

fn serve_connection(
    mut stream: UnixStream,
    handle: &AppHandle,
    overview: &Mutex<OverviewCache>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let bytes = read_message(&mut stream, MAX_REQUEST)?;
    let response = match serde_json::from_slice::<Command>(&bytes) {
        Ok(command) => {
            let compact = matches!(command, Command::Overview);
            let mut response = handle.request(command);
            if compact {
                overview.lock().compact(&mut response);
            }
            response
        }
        Err(error) => Response {
            ok: false,
            error: Some(format!("Invalid command: {error}")),
            state: handle.snapshot(),
        },
    };
    serde_json::to_writer(&mut stream, &response)?;
    stream.write_all(b"\n")?;
    Ok(())
}

pub fn request(path: &Path, command: &Command) -> Result<Response> {
    let mut stream = UnixStream::connect(path).with_context(|| {
        format!(
            "Cannot connect to Rivu at {}. Start `rivu` or `rivu serve` first.",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    serde_json::to_writer(&mut stream, command)?;
    stream.write_all(b"\n")?;
    let response: Response = serde_json::from_slice(&read_message(&mut stream, MAX_RESPONSE)?)?;
    Ok(response)
}
