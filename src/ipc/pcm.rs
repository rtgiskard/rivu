//! Core-owned bounded PCM publisher. Frontends own realtime display analysis.
use crate::core::CoreHandle;
use anyhow::{Context, Result, ensure};
use async_channel::{Receiver, Sender};
use async_io::Async;
use futures::{
    FutureExt, StreamExt,
    io::{AsyncReadExt, AsyncWriteExt},
    stream::FuturesUnordered,
};
use rivu_protocol::pcm::{
    ControlReader, DEADLINE, MAGIC, REQUEST_SIZE, address, word, write_status,
};
use rivu_shm::MAX_CLIENTS;
use std::{
    collections::BTreeMap,
    os::unix::net::{SocketAddr, UnixListener, UnixStream},
    path::Path,
    thread::{self, JoinHandle},
};
type AsyncStream = Async<UnixStream>;

struct ServerState {
    handle: CoreHandle,
    clients: BTreeMap<u64, (bool, Sender<()>)>,
}
impl ServerState {
    fn aggregate(&self) {
        self.handle
            .pcm
            .set_enabled(self.clients.values().any(|(enabled, _)| *enabled));
    }
    fn notify(&self, controls_changed: bool) {
        for (enabled, sender) in self.clients.values() {
            if *enabled || controls_changed {
                let _ = sender.try_send(());
            }
        }
    }
}

pub(crate) struct PcmServer {
    shutdown: Sender<()>,
    worker: Option<JoinHandle<()>>,
}
impl PcmServer {
    pub(crate) fn start(data_dir: &Path, handle: CoreHandle, instance_id: u32) -> Result<Self> {
        let listener =
            UnixListener::bind_addr(&address(data_dir)?).context("Binding PCM control endpoint")?;
        listener.set_nonblocking(true)?;
        let (shutdown, shutdown_rx) = async_channel::bounded(1);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("rivu-pcm-ipc".into())
            .spawn(move || {
                let result = Async::new(listener).map_err(anyhow::Error::from);
                match result {
                    Ok(listener) => {
                        let notifications = handle.pcm.subscribe();
                        let state = std::rc::Rc::new(std::cell::RefCell::new(ServerState {
                            handle,
                            clients: BTreeMap::new(),
                        }));
                        let _ = ready_tx.send(Ok(()));
                        if let Err(error) = async_io::block_on(run_server(
                            listener,
                            state.clone(),
                            notifications,
                            shutdown_rx,
                            instance_id,
                        )) {
                            tracing::warn!(%error, "pcm_server_failed");
                        }
                        state.borrow().handle.pcm.set_enabled(false);
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            })?;
        if let Err(error) = ready_rx
            .recv()
            .context("PCM server initialization stopped")?
        {
            let _ = worker.join();
            return Err(error);
        }
        Ok(Self {
            shutdown,
            worker: Some(worker),
        })
    }
}
impl Drop for PcmServer {
    fn drop(&mut self) {
        self.shutdown.close();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
type SharedServer = std::rc::Rc<std::cell::RefCell<ServerState>>;
struct ClientSubscription {
    state: SharedServer,
    id: u64,
}
impl Drop for ClientSubscription {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        state.clients.remove(&self.id);
        state.aggregate();
        state.notify(true);
    }
}
async fn run_server(
    listener: Async<UnixListener>,
    state: SharedServer,
    notifications: Receiver<()>,
    shutdown: Receiver<()>,
    instance_id: u32,
) -> Result<()> {
    let mut clients = FuturesUnordered::new();
    let mut next_client = 0u64;
    let mut last_state = state.borrow().handle.pcm.state();
    enum Event {
        Accepted(std::io::Result<(AsyncStream, SocketAddr)>),
        Completed,
        Samples,
        Stop,
    }
    loop {
        let event = {
            let completed = async {
                if clients.is_empty() {
                    std::future::pending().await
                } else {
                    clients.next().await
                }
            };
            let samples = notifications.recv();
            futures::select! {
                _ = shutdown.recv().fuse() => Event::Stop,
                connection = listener.accept().fuse() => Event::Accepted(connection),
                _ = completed.fuse() => Event::Completed,
                _ = samples.fuse() => Event::Samples,
            }
        };
        match event {
            Event::Stop => return Ok(()),
            Event::Completed => {}
            Event::Samples => {
                let state = state.borrow();
                let current = state.handle.pcm.state();
                let controls_changed = rivu_protocol::pcm::PcmState {
                    heard_frame: last_state.heard_frame,
                    ..current
                } != last_state;
                state.notify(controls_changed);
                last_state = current;
            }
            Event::Accepted(connection) => {
                let (stream, _) = connection?;
                if clients.len() >= MAX_CLIENTS
                    || rivu_protocol::wire::verify_peer(&stream).is_err()
                {
                    continue;
                }
                next_client = next_client.wrapping_add(1).max(1);
                let state = state.clone();
                let id = next_client;
                clients.push(async move {
                    if let Err(error) = serve_client(stream, state, id, instance_id).await {
                        tracing::debug!(%error, "pcm_client_retired");
                    }
                });
            }
        }
    }
}
async fn serve_client(
    mut stream: AsyncStream,
    state: SharedServer,
    id: u64,
    instance_id: u32,
) -> Result<()> {
    let mut request_reader = ControlReader::<REQUEST_SIZE>::new();
    let hello =
        rivu_protocol::transport::timeout(DEADLINE, request_reader.read(&mut stream)).await??;
    ensure!(
        hello[0] == 0 && word(&hello[8..]) == u64::from(instance_id),
        "PCM instance does not match command connection"
    );
    let mut welcome = Vec::from(*MAGIC);
    welcome.extend_from_slice(&instance_id.to_le_bytes());
    {
        use std::os::unix::ffi::OsStrExt;
        let state = state.borrow();
        let path = state.handle.pcm.window().share_path();
        let root = path.as_os_str().as_bytes();
        ensure!(root.len() <= 4096, "Shared PCM path is too long");
        welcome.extend_from_slice(&(root.len() as u32).to_le_bytes());
        welcome.extend_from_slice(root);
    }
    rivu_protocol::transport::timeout(DEADLINE, stream.write_all(&welcome)).await??;
    let (sender, notifications) = async_channel::bounded(1);
    state.borrow_mut().clients.insert(id, (false, sender));
    let _subscription = ClientSubscription {
        state: state.clone(),
        id,
    };
    let mut revision = 0;
    let mut minimum_sequence = 0;
    let (mut reader, mut writer) = stream.split();
    loop {
        let command = futures::select! {
            result = request_reader.read(&mut reader).fuse() => Some(result?),
            result = notifications.recv().fuse() => { result.context("PCM publisher stopped")?; None },
        };
        if let Some(command) = command {
            ensure!(
                command[0] == 1 && command[1] <= 1,
                "Unknown PCM subscription request"
            );
            revision = word(&command[8..]);
            let mut state = state.borrow_mut();
            state.clients.get_mut(&id).unwrap().0 = command[1] != 0;
            state.aggregate();
            minimum_sequence = state.handle.pcm.sequence();
            state.notify(true);
        }
        let snapshot = state.borrow().handle.pcm.state();
        write_status(&mut writer, snapshot, revision, minimum_sequence).await?;
    }
}
