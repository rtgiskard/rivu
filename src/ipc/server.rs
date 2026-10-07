use super::{
    HELLO_TIMEOUT, HelloResponse, MAX_CLIENTS, MAX_REQUEST, PROTOCOL_VERSION, SessionState,
    UNKNOWN_INSTANCE, WireRequest, WireResponse, decode_request_frame, read_frame, wire_state,
    write_error_frame, write_frame_buffered,
};
use crate::core::AppHandle;
use crate::{
    model::{Command, Query},
    response::{Ack, QueryResponse, StateRevisions},
};
use anyhow::{Context, Error, Result, bail};
use bytes::BytesMut;
use rand::RngExt;
use std::{
    fs,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::{
    net::{UnixListener, UnixStream},
    runtime::Builder,
    sync::watch,
    time::timeout,
};
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
    instance_id: u32,
    mut updates: watch::Receiver<StateRevisions>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut pending = BytesMut::new();
    let mut write_buffer = Vec::new();
    let mut session = SessionState::Unbound;
    loop {
        let bytes = if session.is_bound() {
            tokio::select! {
                frame = read_frame(&mut stream, MAX_REQUEST, &mut pending) => frame?,
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return Ok(()); }
                    continue;
                }
            }
        } else {
            tokio::select! {
                frame = timeout(HELLO_TIMEOUT, read_frame(&mut stream, MAX_REQUEST, &mut pending)) => {
                    match frame {
                        Ok(frame) => frame?,
                        Err(_) => return Ok(()),
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return Ok(()); }
                    continue;
                }
            }
        };
        let (version, request) = match decode_request_frame(&bytes) {
            Ok(frame) => frame,
            Err(error) => {
                write_error_frame(
                    &mut stream,
                    &mut write_buffer,
                    format!("Invalid IPC request: {error}"),
                )
                .await?;
                return Ok(());
            }
        };
        if version != PROTOCOL_VERSION {
            write_error_frame(
                &mut stream,
                &mut write_buffer,
                format!("Unsupported IPC protocol version {version}"),
            )
            .await?;
            return Ok(());
        }
        if !session.is_bound() && !matches!(request, WireRequest::Hello(_)) {
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
                session = SessionState::Bound;
            }
            WireRequest::State(sections) => {
                let response = WireResponse::State(wire_state(handle.state(sections)));
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
            }
            WireRequest::Query(query) => {
                let query = match Query::try_from(query) {
                    Ok(query) => query,
                    Err(error) => {
                        write_error_frame(
                            &mut stream,
                            &mut write_buffer,
                            format!("Invalid IPC query: {error}"),
                        )
                        .await?;
                        continue;
                    }
                };
                let response = WireResponse::Query(run_query(&handle, query).await?);
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
            }
            WireRequest::Command(command) => {
                let command = match Command::try_from(command) {
                    Ok(command) => command,
                    Err(error) => {
                        write_error_frame(
                            &mut stream,
                            &mut write_buffer,
                            format!("Invalid IPC command: {error}"),
                        )
                        .await?;
                        continue;
                    }
                };
                let response = WireResponse::Ack(run_command(&handle, command).await?);
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
            }
            WireRequest::Watch(expected) => {
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
                let response = WireResponse::Watch(revisions);
                write_frame_buffered(&mut stream, &response, &mut write_buffer).await?;
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
    let mut instance_id = UNKNOWN_INSTANCE;
    while instance_id == UNKNOWN_INSTANCE {
        instance_id = rng.random();
    }
    let mut clients = Vec::new();
    let revisions = updates;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue; };
                clients.retain(|task: &tokio::task::JoinHandle<Result<()>>| !task.is_finished());
                if clients.len() < MAX_CLIENTS { clients.push(tokio::spawn(serve_connection(stream, handle.clone(), instance_id, revisions.clone(), shutdown.clone()))); }
            }
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
