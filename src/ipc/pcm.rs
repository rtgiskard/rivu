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
    ControlReader, DEADLINE, MAGIC, PcmPosition, REQUEST_SIZE, address, word, write_status,
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
        futures_lite::future::yield_now().await;
        if shutdown.is_closed() {
            return Ok(());
        }
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
    let mut minimum_position = PcmPosition::default();
    let (mut reader, mut writer) = stream.split();
    loop {
        // Ready control I/O must still return to the publisher and other sessions.
        futures_lite::future::yield_now().await;
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
            minimum_position = state.handle.pcm.frontier();
        }
        let snapshot = state.borrow().handle.pcm.state();
        write_status(&mut writer, snapshot, revision, minimum_position).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivu_protocol::pcm::{PcmHeader, PcmState, STATUS_SIZE, decode_status, request};
    use std::io::{Read, Write};
    use std::time::Duration;

    type Status = (PcmState, u64, PcmPosition);

    struct Client(UnixStream);

    impl Client {
        fn connect(directory: &Path) -> Self {
            let mut stream = UnixStream::connect_addr(&address(directory).unwrap()).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream.write_all(&request(0, 0, 7)).unwrap();
            let mut welcome = [0; 16];
            stream.read_exact(&mut welcome).unwrap();
            assert_eq!(&welcome[..8], MAGIC);
            assert_eq!(u32::from_le_bytes(welcome[8..12].try_into().unwrap()), 7);
            let length = u32::from_le_bytes(welcome[12..].try_into().unwrap()) as usize;
            assert!(length <= 4096);
            stream.read_exact(&mut vec![0; length]).unwrap();
            Self(stream)
        }

        fn status(&mut self) -> Status {
            let mut bytes = [0; STATUS_SIZE];
            self.0.read_exact(&mut bytes).unwrap();
            decode_status(&bytes).unwrap()
        }

        fn subscribe(&mut self, enabled: bool, revision: u64) -> Status {
            self.0
                .write_all(&request(1, u8::from(enabled), revision))
                .unwrap();
            loop {
                let status = self.status();
                if status.1 == revision {
                    return status;
                }
            }
        }

        fn capture(&mut self, capture: u64) -> Status {
            loop {
                let status = self.status();
                if status.0.capture == capture {
                    return status;
                }
            }
        }
    }

    #[test]
    fn pipelined_subscription_yields_before_disconnect_retires_capture() {
        use std::{
            future::Future,
            task::{Context, Waker},
        };

        let (handle, _queries) = CoreHandle::test_query_port();
        let initial = handle.pcm.state();
        let state = std::rc::Rc::new(std::cell::RefCell::new(ServerState {
            handle: handle.clone(),
            clients: BTreeMap::new(),
        }));
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut requests = Vec::from(request(0, 0, 7));
        requests.extend_from_slice(&request(1, 1, 11));
        requests.extend_from_slice(&request(1, 0, 12));
        peer.write_all(&requests).unwrap();
        peer.shutdown(std::net::Shutdown::Write).unwrap();
        let stream = Async::new(stream).unwrap();
        async_io::block_on(stream.readable()).unwrap();
        async_io::block_on(stream.writable()).unwrap();
        {
            let mut serving = std::pin::pin!(serve_client(stream, state.clone(), 1, 7));
            let mut context = Context::from_waker(Waker::noop());
            assert!(serving.as_mut().poll(&mut context).is_pending());
            let mut welcome = [0; 16];
            peer.read_exact(&mut welcome).unwrap();
            assert_eq!(&welcome[..8], MAGIC);
            let length = u32::from_le_bytes(welcome[12..].try_into().unwrap()) as usize;
            assert!(length <= 4096);
            peer.read_exact(&mut vec![0; length]).unwrap();

            // Even with the next request and EOF already readable, the first ACK
            // returns control so the session can be retired before the next intent.
            assert!(serving.as_mut().poll(&mut context).is_pending());
            let mut bytes = [0; STATUS_SIZE];
            peer.read_exact(&mut bytes).unwrap();
            let (enabled, revision, frontier) = decode_status(&bytes).unwrap();
            assert_eq!(revision, 11);
            assert_eq!(frontier, PcmPosition::default());
            assert_eq!(enabled.capture, initial.capture + 1);
            assert_eq!(handle.pcm.state().capture, enabled.capture);
        }
        // Dropping a yielded session still removes its aggregate demand.
        assert!(state.borrow().clients.is_empty());
        assert_eq!(handle.pcm.state().capture, initial.capture + 2);
    }

    #[test]
    fn subscription_acknowledges_revision_and_frontier_while_another_client_keeps_capture() {
        let directory = crate::test_support::ipc::socket_test_directory();
        let (handle, _queries) = CoreHandle::test_query_port();
        let server = PcmServer::start(directory.path(), handle.clone(), 7).unwrap();
        let mut local = Client::connect(directory.path());
        let initial = local.subscribe(false, 1);
        assert_eq!(initial, (handle.pcm.state(), 1, PcmPosition::default()));
        assert_eq!(
            local.subscribe(false, 2),
            (initial.0, 2, PcmPosition::default())
        );

        let mut keeper = Client::connect(directory.path());
        let shared = keeper.subscribe(true, 1).0;
        assert_eq!(shared.capture, initial.0.capture + 1);
        assert_eq!(
            local.capture(shared.capture),
            (shared, 2, PcmPosition::default())
        );
        let window = handle.pcm.window();
        let publish = |start_frame| {
            assert!(window.publish(
                PcmHeader {
                    epoch: shared.epoch,
                    intent: shared.intent,
                    generation: shared.generation,
                    capture: shared.capture,
                    sample_rate: shared.sample_rate,
                    start_frame,
                    output_frame: start_frame,
                    ..Default::default()
                },
                &[0, 1],
                [0.25, -0.25].into_iter(),
            ));
            window.frontier()
        };
        let first = publish(0);
        assert_eq!(
            first,
            PcmPosition {
                sequence: 1,
                offset: 1
            }
        );
        assert_eq!(local.subscribe(false, 3), (shared, 3, first));
        assert_eq!(local.subscribe(false, 4), (shared, 4, first));
        let second = publish(1);
        assert_eq!(
            second,
            PcmPosition {
                sequence: 1,
                offset: 2
            }
        );
        assert!(second > first);
        assert_eq!(local.subscribe(true, 5), (shared, 5, second));
        let third = publish(2);
        assert_eq!(
            third,
            PcmPosition {
                sequence: 1,
                offset: 3
            }
        );
        assert!(third > second);
        assert_eq!(local.subscribe(false, 6), (shared, 6, third));
        assert_eq!(local.subscribe(true, 7), (shared, 7, third));
        drop(local);
        drop(keeper);
        drop(server);
    }

    #[test]
    fn unchanged_aggregate_does_not_wake_status_broadcasts() {
        let (handle, _queries) = CoreHandle::test_query_port();
        let updates = handle.pcm.subscribe();
        while updates.try_recv().is_ok() {}
        let (notify, _notifications) = async_channel::bounded(1);
        let mut state = ServerState {
            handle,
            clients: BTreeMap::from([(1, (true, notify.clone()))]),
        };
        state.aggregate();
        assert_eq!(updates.try_recv(), Ok(()));
        let enabled = state.handle.pcm.state();
        state.clients.insert(2, (true, notify));
        state.aggregate();
        state.clients.get_mut(&1).unwrap().0 = false;
        state.aggregate();
        state.clients.remove(&1);
        state.aggregate();
        assert_eq!(state.handle.pcm.state(), enabled);
        assert_eq!(updates.try_recv(), Err(async_channel::TryRecvError::Empty));
        state.clients.remove(&2);
        state.aggregate();
        assert_eq!(updates.try_recv(), Ok(()));
        assert_eq!(state.handle.pcm.state().capture, enabled.capture + 1);
    }

    #[test]
    fn aggregate_capture_boundaries_reach_disabled_clients_on_updates_and_disconnect() {
        let directory = crate::test_support::ipc::socket_test_directory();
        let (handle, _queries) = CoreHandle::test_query_port();
        let server = PcmServer::start(directory.path(), handle, 7).unwrap();
        let mut observer = Client::connect(directory.path());
        let initial = observer.subscribe(false, 11).0;
        let mut first = Client::connect(directory.path());
        let enabled = first.subscribe(true, 1).0;
        assert_eq!(enabled.capture, initial.capture + 1);
        assert_eq!(
            observer.capture(enabled.capture),
            (enabled, 11, PcmPosition::default())
        );

        let mut second = Client::connect(directory.path());
        assert_eq!(second.subscribe(true, 1).0, enabled);
        assert_eq!(first.subscribe(false, 2).0, enabled);
        // An unchanged aggregate must not queue a broadcast before this ACK.
        observer.0.write_all(&request(1, 0, 12)).unwrap();
        assert_eq!(observer.status(), (enabled, 12, PcmPosition::default()));
        drop(second);
        let disabled = observer.capture(enabled.capture + 1).0;
        assert!(!disabled.active);
        assert_eq!(
            observer.subscribe(false, 13),
            (disabled, 13, PcmPosition::default())
        );

        // Coalescing may skip intermediate broadcasts, but fast off/on still
        // revokes the previous capture identity even without any audio frames.
        let mut requests = Vec::new();
        for (enabled, revision) in [(true, 3), (false, 4), (true, 5)] {
            requests.extend_from_slice(&request(1, u8::from(enabled), revision));
        }
        first.0.write_all(&requests).unwrap();
        let resumed = first.capture(disabled.capture + 3).0;
        assert_eq!(
            observer.capture(resumed.capture),
            (resumed, 13, PcmPosition::default())
        );
        drop(first);
        let stopped = observer.capture(resumed.capture + 1).0;
        assert!(!stopped.active);
        assert_eq!(
            observer.subscribe(false, 14),
            (stopped, 14, PcmPosition::default())
        );
        drop(observer);
        drop(server);
    }
}
