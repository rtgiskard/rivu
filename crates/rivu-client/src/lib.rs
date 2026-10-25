use anyhow::{Context, Result, bail};
use async_io::Async;
use bytes::BytesMut;
use futures_lite::future;
use rivu_protocol::protocol::{
    SessionState, WireCommand, WireQuery, WireRequest, WireResponse, state_response,
};
use rivu_protocol::transport::{
    read_response_frame, timeout, unpack_error, unpack_hello, write_request_frame_buffered,
};
use rivu_protocol::{
    AppCommand,
    model::Query,
    query_wait_timeout,
    response::{Ack, AckStatus, QueryResponse, StateResponse, StateRevisions, StateSections},
};
use std::{
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Debug)]
pub struct InstanceChanged;

impl std::fmt::Display for InstanceChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Rivu instance changed; reopen the TUI to use the new instance")
    }
}

impl std::error::Error for InstanceChanged {}

/// Transport evidence for a failed command exchange; no mutation is replayed.
#[derive(Debug)]
pub struct CommandFailure {
    pub status: AckStatus,
}

impl std::fmt::Display for CommandFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self.status {
            AckStatus::NotAdmitted => "Command not admitted (not sent)",
            _ => "Command outcome unknown; do not automatically retry",
        })
    }
}

impl std::error::Error for CommandFailure {}

#[derive(Clone, Default)]
pub struct SessionStop {
    state: Arc<SessionStopState>,
}

#[derive(Default)]
struct SessionStopState {
    cancelled: AtomicBool,
    notify: event_listener::Event,
}

impl SessionStop {
    pub fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Release);
        self.state.notify.notify(usize::MAX);
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        // Register before checking the sticky flag so cancellation cannot fall
        // between the check and installing this waiter's wakeup.
        let listener = self.state.notify.listen();
        if !self.is_cancelled() {
            listener.await;
        }
    }
}

/// Serial IPC requests share a socket, handshake, and framing buffers.
/// A failed exchange discards the socket; only a later request reconnects, and
/// the failed request is never replayed because its mutation may have executed.
pub struct ClientSession {
    data_dir: PathBuf,
    stream: Option<Async<UnixStream>>,
    stop: SessionStop,
    read_buffer: BytesMut,
    write_buffer: Vec<u8>,
    session: SessionState,
    expected_instance: Option<u32>,
    #[cfg(test)]
    response_clock: Option<Arc<tests::ResponseClock>>,
}

/// Create a session for a data directory; connect and handshake on its first request.
pub fn client_session(data_dir: &Path) -> Result<ClientSession> {
    client_session_with_cancel(data_dir, SessionStop::default())
}

pub fn client_session_with_cancel(data_dir: &Path, stop: SessionStop) -> Result<ClientSession> {
    Ok(ClientSession {
        data_dir: data_dir.to_owned(),
        stream: None,
        stop,
        read_buffer: BytesMut::new(),
        write_buffer: Vec::new(),
        session: SessionState::Unbound,
        expected_instance: None,
        #[cfg(test)]
        response_clock: None,
    })
}

impl ClientSession {
    pub fn instance_id(&self) -> Option<u32> {
        match self.session {
            SessionState::Unbound => None,
            SessionState::Bound(instance_id) => Some(instance_id),
        }
    }

    /// Pin a related session to the watcher's initial Hello identity. Unlike
    /// state revisions, this identity survives disconnects and rejects a new
    /// server before any operation or its initial snapshot can be accepted.
    pub fn bind_instance(&mut self, instance_id: u32) -> Result<()> {
        anyhow::ensure!(instance_id != 0, "Cannot bind IPC instance 0");
        if self
            .expected_instance
            .is_some_and(|expected| expected != instance_id)
            || self
                .instance_id()
                .is_some_and(|current| current != instance_id)
        {
            return Err(InstanceChanged.into());
        }
        self.expected_instance = Some(instance_id);
        Ok(())
    }

    pub fn cancellation(&self) -> SessionStop {
        self.stop.clone()
    }

    pub fn cancel(&self) {
        self.stop.cancel();
    }

    fn disconnect(&mut self) {
        self.stream = None;
        self.session = SessionState::Unbound;
        self.read_buffer.clear();
        if self.read_buffer.capacity() > rivu_protocol::transport::RETAINED_BUFFER_LIMIT {
            self.read_buffer = BytesMut::new();
        }
        rivu_protocol::transport::release_large_buffer(&mut self.write_buffer);
    }

    fn ensure_handshake(&mut self) -> Result<()> {
        if !self.session.is_bound() {
            self.get_state(StateSections::default())?;
        }
        Ok(())
    }

    fn exchange(&mut self, request: &WireRequest, wait_timeout: Duration) -> Result<WireResponse> {
        self.exchange_with_write_evidence(request, wait_timeout, &mut false)
    }

    fn exchange_with_write_evidence(
        &mut self,
        request: &WireRequest,
        wait_timeout: Duration,
        write_started: &mut bool,
    ) -> Result<WireResponse> {
        #[cfg(test)]
        let response_clock = self
            .response_clock
            .as_deref()
            .filter(|_| matches!(request, WireRequest::Command(_)))
            .map(|clock| (clock, clock.now()));
        let result = async_io::block_on(async {
            future::or(
                async {
                    self.stop.cancelled().await;
                    bail!("IPC request cancelled")
                },
                async {
                    if self.stream.is_none() {
                        self.stream = Some(rivu_protocol::protocol::connect(&self.data_dir).await.with_context(|| {
                            format!(
                                "Cannot connect to Rivu at {}. Start `rivu` or `rivu serve` first.",
                                self.data_dir.display()
                            )
                        })?);
                    }
                    let stream = self.stream.as_mut().unwrap();
                    write_request_frame_buffered(stream, request, &mut self.write_buffer, write_started).await?;
                    let response = read_response_frame(stream, &mut self.read_buffer);
                    let response = if matches!(request, WireRequest::Watch(_)) {
                        response.await
                    } else {
                        let deadline = rivu_protocol::transport::response_timeout(wait_timeout);
                        #[cfg(test)]
                        let response = match response_clock {
                            Some((clock, started)) => clock.timeout_at(started + deadline, response).await,
                            None => timeout(deadline, response).await,
                        };
                        #[cfg(not(test))]
                        let response = timeout(deadline, response).await;
                        response.context("Reading IPC response timed out")?
                    }?;
                    if self.stop.is_cancelled() {
                        bail!("IPC request cancelled");
                    }
                    Ok(response)
                },
            ).await
        });
        if result.is_err() {
            self.disconnect();
        }
        result
    }

    pub fn get_state(&mut self, sections: StateSections) -> Result<StateResponse> {
        if !self.session.is_bound() {
            let response = self.exchange(&WireRequest::Hello(sections), query_wait_timeout())?;
            return match unpack_hello(response) {
                Ok(response) => {
                    let instance_id = u32::from_le_bytes(response.instance_id);
                    if self
                        .expected_instance
                        .is_some_and(|expected| expected != instance_id)
                    {
                        self.disconnect();
                        return Err(InstanceChanged.into());
                    }
                    self.session = SessionState::Bound(instance_id);
                    Ok(state_response(response.state))
                }
                Err(error) => {
                    self.disconnect();
                    Err(error)
                }
            };
        }
        match unpack_error(self.exchange(&WireRequest::State(sections), query_wait_timeout())?)? {
            WireResponse::State(state) => Ok(state_response(state)),
            _ => bail!("IPC response was not a state response"),
        }
    }

    pub fn query(&mut self, query: Query) -> Result<QueryResponse> {
        self.ensure_handshake()?;
        let request = WireRequest::Query(WireQuery::from(query));
        match unpack_error(self.exchange(&request, query_wait_timeout())?)? {
            WireResponse::Query(response) => Ok(response),
            _ => bail!("IPC response was not a query response"),
        }
    }

    pub fn request_ack(&mut self, command: impl Into<AppCommand>) -> Result<Ack> {
        self.ensure_handshake().context(CommandFailure {
            status: AckStatus::NotAdmitted,
        })?;
        let command: AppCommand = command.into();
        let wait_timeout = command.wait_timeout();
        let request = WireRequest::Command(WireCommand::from(command));
        let mut write_started = false;
        let result = self
            .exchange_with_write_evidence(&request, wait_timeout, &mut write_started)
            .and_then(unpack_error)
            .and_then(|response| match response {
                WireResponse::Ack(response) => Ok(response),
                _ => bail!("IPC response was not an acknowledgement"),
            });
        if result.is_err() {
            self.disconnect();
        }
        result.context(CommandFailure {
            status: if write_started {
                AckStatus::Unknown
            } else {
                AckStatus::NotAdmitted
            },
        })
    }

    pub fn watch(&mut self, revisions: StateRevisions) -> Result<StateRevisions> {
        self.watch_until(revisions)?
            .ok_or_else(|| anyhow::anyhow!("Watcher cancelled"))
    }

    pub fn watch_until(&mut self, revisions: StateRevisions) -> Result<Option<StateRevisions>> {
        self.ensure_handshake()?;
        let request = WireRequest::Watch(revisions);
        let response = match self.exchange(&request, query_wait_timeout()) {
            Ok(response) => response,
            Err(_) if self.stop.is_cancelled() => return Ok(None),
            Err(error) => return Err(error),
        };
        match unpack_error(response)? {
            WireResponse::Watch(revisions) => Ok(Some(revisions)),
            _ => bail!("IPC response was not a watch response"),
        }
    }
}

pub fn query(data_dir: &Path, query: Query) -> Result<QueryResponse> {
    client_session(data_dir)?.query(query)
}

pub fn get_state(data_dir: &Path, sections: StateSections) -> Result<StateResponse> {
    client_session(data_dir)?.get_state(sections)
}

pub fn request_ack(data_dir: &Path, command: impl Into<AppCommand>) -> Result<Ack> {
    client_session(data_dir)
        .context(CommandFailure {
            status: AckStatus::NotAdmitted,
        })?
        .request_ack(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivu_protocol::{
        model::Command,
        protocol::{HelloResponse, MAX_REQUEST, wire_state},
        transport::{decode_request_frame, encode_response_frame_into},
    };
    use std::sync::{Condvar, Mutex};

    #[derive(Default)]
    struct ClockState {
        now: Duration,
        observed: Duration,
    }

    /// Command-response clock only: socket handshakes retain real deadlines.
    #[derive(Default)]
    pub(super) struct ResponseClock {
        state: Mutex<ClockState>,
        changed: event_listener::Event,
        observed: Condvar,
    }

    impl ResponseClock {
        pub(super) fn now(&self) -> Duration {
            self.state.lock().unwrap().now
        }

        fn advance_and_wait_for_poll(&self, now: Duration) {
            let mut state = self.state.lock().unwrap();
            assert!(now > state.now);
            state.now = now;
            self.changed.notify(usize::MAX);
            // This is a stuck-test watchdog, not the response deadline. The
            // server cannot reply until the client has checked the new time.
            let (_state, watchdog) = self
                .observed
                .wait_timeout_while(state, Duration::from_secs(5), |state| state.observed < now)
                .unwrap();
            assert!(!watchdog.timed_out(), "response deadline was not polled");
        }

        pub(super) async fn timeout_at<T>(
            &self,
            deadline: Duration,
            operation: impl Future<Output = T>,
        ) -> Result<T, rivu_protocol::transport::Elapsed> {
            future::or(async { Ok(operation.await) }, async {
                loop {
                    let listener = self.changed.listen();
                    let expired = {
                        let mut state = self.state.lock().unwrap();
                        state.observed = state.now;
                        self.observed.notify_all();
                        state.now >= deadline
                    };
                    if expired {
                        return Err(rivu_protocol::transport::Elapsed);
                    }
                    listener.await;
                }
            })
            .await
        }
    }

    fn socket_test_directory() -> tempfile::TempDir {
        let cache =
            std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../..")).join(".cache");
        std::fs::create_dir_all(&cache).unwrap();
        tempfile::tempdir_in(cache).unwrap()
    }

    fn read_test_request(stream: &mut std::os::unix::net::UnixStream) -> WireRequest {
        use std::io::Read;
        let mut prefix = [0; 4];
        stream.read_exact(&mut prefix).unwrap();
        let length = u32::from_le_bytes(prefix) as usize;
        assert!(length <= MAX_REQUEST);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        decode_request_frame(&body).unwrap()
    }

    fn write_test_response(stream: &mut std::os::unix::net::UnixStream, response: WireResponse) {
        use std::io::Write;
        let mut frame = Vec::new();
        encode_response_frame_into(&response, &mut frame).unwrap();
        stream.write_all(&frame).unwrap();
    }

    fn complete_test_handshake(stream: &mut std::os::unix::net::UnixStream) {
        complete_test_handshake_for_instance(stream, 1);
    }

    fn complete_test_handshake_for_instance(
        stream: &mut std::os::unix::net::UnixStream,
        instance_id: u32,
    ) {
        let timeout = Some(std::time::Duration::from_secs(5));
        stream.set_read_timeout(timeout).unwrap();
        stream.set_write_timeout(timeout).unwrap();
        assert!(matches!(read_test_request(stream), WireRequest::Hello(_)));
        write_test_response(
            stream,
            WireResponse::Hello(HelloResponse {
                instance_id: instance_id.to_le_bytes(),
                state: wire_state(StateResponse {
                    revisions: StateRevisions::default(),
                    playback: None,
                    queue: None,
                    library: None,
                    system: None,
                }),
            }),
        );
    }

    fn read_test_command(stream: &mut std::os::unix::net::UnixStream) -> Command {
        let WireRequest::Command(command) = read_test_request(stream) else {
            panic!("expected command")
        };
        Command::try_from(command).unwrap()
    }

    fn reply_test_ack(stream: &mut std::os::unix::net::UnixStream, ack: Ack) {
        write_test_response(stream, WireResponse::Ack(ack));
    }

    #[test]
    fn optimize_database_accepts_reply_after_old_response_deadline() {
        use std::os::unix::net::UnixListener;

        let directory = socket_test_directory();
        let listener =
            UnixListener::bind_addr(&rivu_protocol::protocol::address(directory.path()).unwrap())
                .unwrap();
        let mut client = client_session(directory.path()).unwrap();
        let clock = Arc::new(ResponseClock::default());
        client.response_clock = Some(clock.clone());
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake(&mut stream);
            assert!(matches!(
                read_test_command(&mut stream),
                Command::OptimizeDatabase
            ));
            // Cross the old 120s deadline, then wait until the client's timeout
            // branch has observed it before making the response readable.
            clock.advance_and_wait_for_poll(Duration::from_secs(121));
            reply_test_ack(&mut stream, Ack::from_result(Ok(()), 17));
        });

        let ack = client.request_ack(Command::OptimizeDatabase).unwrap();
        assert!(ack.is_success());
        assert_eq!(ack.revision, 17);
        server.join().unwrap();
    }

    #[test]
    fn command_timeout_is_unknown_and_next_request_is_not_a_replay() {
        use rivu_protocol::model::Command;
        use std::{io::Read, os::unix::net::UnixListener};

        let directory = socket_test_directory();
        let listener =
            UnixListener::bind_addr(&rivu_protocol::protocol::address(directory.path()).unwrap())
                .unwrap();
        let clock = Arc::new(ResponseClock::default());
        let command_clock = clock.clone();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake(&mut stream);
            assert!(
                matches!(read_test_command(&mut stream), Command::CreatePlaylist { name } if name == "first")
            );
            command_clock.advance_and_wait_for_poll(
                rivu_protocol::transport::response_timeout(query_wait_timeout())
                    + Duration::from_secs(1),
            );
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake(&mut stream);
            assert!(
                matches!(read_test_command(&mut stream), Command::CreatePlaylist { name } if name == "second")
            );
            reply_test_ack(&mut stream, Ack::from_result(Ok(()), 2));
        });
        let mut client = client_session(directory.path()).unwrap();
        client.response_clock = Some(clock);
        let error = client
            .request_ack(Command::CreatePlaylist {
                name: "first".into(),
            })
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<CommandFailure>().unwrap().status,
            AckStatus::Unknown
        );
        assert!(format!("{error:#}").contains("do not automatically retry"));
        assert!(
            client
                .request_ack(Command::CreatePlaylist {
                    name: "second".into()
                })
                .unwrap()
                .is_success()
        );
        server.join().unwrap();
    }

    #[test]
    fn handshake_and_encoding_failures_do_not_send_a_command() {
        use rivu_protocol::model::Command;
        use std::{io::Read, os::unix::net::UnixListener};

        let directory = socket_test_directory();
        let listener =
            UnixListener::bind_addr(&rivu_protocol::protocol::address(directory.path()).unwrap())
                .unwrap();
        let server = std::thread::spawn(move || {
            // First connection closes before Hello completes, so nothing was sent.
            drop(listener.accept().unwrap().0);
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake(&mut stream);
            // An oversized command fails encoding and the session closes it.
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        });
        let mut client = client_session(directory.path()).unwrap();
        let error = client.request_ack(Command::Pause).unwrap_err();
        assert_eq!(
            error.downcast_ref::<CommandFailure>().unwrap().status,
            AckStatus::NotAdmitted
        );
        let error = client
            .request_ack(Command::Scan {
                paths: vec![PathBuf::from(
                    "x".repeat(rivu_protocol::protocol::MAX_REQUEST),
                )],
                force: false,
            })
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<CommandFailure>().unwrap().status,
            AckStatus::NotAdmitted
        );
        server.join().unwrap();
    }

    #[test]
    fn session_stop_wakes_all_waiters_and_remains_cancelled() {
        use std::{future::Future, task::Context};

        let stop = SessionStop::default();
        let clone = stop.clone();
        let mut first = Box::pin(stop.cancelled());
        let mut second = Box::pin(clone.cancelled());
        let mut context = Context::from_waker(std::task::Waker::noop());
        assert!(first.as_mut().poll(&mut context).is_pending());
        assert!(second.as_mut().poll(&mut context).is_pending());

        clone.cancel();
        clone.cancel();
        assert!(stop.is_cancelled());
        assert!(first.as_mut().poll(&mut context).is_ready());
        assert!(second.as_mut().poll(&mut context).is_ready());

        // A waiter first polled after cancellation must not need another notify.
        let mut late = Box::pin(stop.cancelled());
        assert!(late.as_mut().poll(&mut context).is_ready());
    }

    #[test]
    fn cancelled_exchange_releases_partial_bulk_buffers() {
        let directory = socket_test_directory();
        let mut client = client_session(directory.path()).unwrap();
        client.read_buffer.resize(1024 * 1024, 0);
        client.write_buffer.resize(1024 * 1024, 0);
        client.cancel();
        assert!(client.get_state(StateSections::ALL).is_err());
        assert!(client.read_buffer.is_empty());
        assert!(client.read_buffer.capacity() <= rivu_protocol::transport::RETAINED_BUFFER_LIMIT);
        assert!(client.write_buffer.capacity() <= rivu_protocol::transport::RETAINED_BUFFER_LIMIT);
    }

    #[test]
    fn cancelling_a_pending_handshake_never_sends_the_command() {
        use std::{io::Read, os::unix::net::UnixListener};

        let directory = socket_test_directory();
        let listener =
            UnixListener::bind_addr(&rivu_protocol::protocol::address(directory.path()).unwrap())
                .unwrap();
        let mut client = client_session(directory.path()).unwrap();
        let stop = client.cancellation();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            assert!(matches!(
                read_test_request(&mut stream),
                WireRequest::Hello(_)
            ));
            // Cancel after Hello was sent, before allowing the handshake to finish.
            stop.cancel();
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        });
        let error = client.request_ack(Command::Pause).unwrap_err();
        assert_eq!(
            error.downcast_ref::<CommandFailure>().unwrap().status,
            AckStatus::NotAdmitted
        );
        assert_eq!(client.instance_id(), None);
        server.join().unwrap();
    }

    #[test]
    fn cancelling_a_sent_command_is_unknown_and_never_replays_it() {
        use std::{io::Read, os::unix::net::UnixListener};

        let directory = socket_test_directory();
        let listener =
            UnixListener::bind_addr(&rivu_protocol::protocol::address(directory.path()).unwrap())
                .unwrap();
        let mut client = client_session(directory.path()).unwrap();
        let stop = client.cancellation();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake(&mut stream);
            assert!(matches!(
                read_test_command(&mut stream),
                Command::CreatePlaylist { name } if name == "accepted-once"
            ));
            // Receipt is synchronized before cancellation; no timing assumption
            // can misclassify this exchange as a pre-write cancellation.
            stop.cancel();
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
            listener.set_nonblocking(true).unwrap();
            listener
        });
        let error = client
            .request_ack(Command::CreatePlaylist {
                name: "accepted-once".into(),
            })
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<CommandFailure>().unwrap().status,
            AckStatus::Unknown
        );
        assert_eq!(client.instance_id(), None);
        let listener = server.join().unwrap();
        // Stopping a session is sticky, even for a later explicit request.
        let error = client.request_ack(Command::Pause).unwrap_err();
        assert_eq!(
            error.downcast_ref::<CommandFailure>().unwrap().status,
            AckStatus::NotAdmitted
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn cancelling_a_blocked_watch_wakes_the_session_and_closes_the_socket() {
        use std::{io::Read, os::unix::net::UnixListener, sync::mpsc};

        let directory = socket_test_directory();
        let listener =
            UnixListener::bind_addr(&rivu_protocol::protocol::address(directory.path()).unwrap())
                .unwrap();
        let (watch_started, wait_for_watch) = mpsc::sync_channel(0);
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake(&mut stream);
            assert!(matches!(
                read_test_request(&mut stream),
                WireRequest::Watch(_)
            ));
            watch_started.send(()).unwrap();
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        });
        let mut client = client_session(directory.path()).unwrap();
        let stop = client.cancellation();
        let cancellation = std::thread::spawn(move || {
            wait_for_watch.recv().unwrap();
            stop.cancel();
        });
        assert_eq!(client.watch_until(StateRevisions::default()).unwrap(), None);
        assert!(client.stream.is_none());
        cancellation.join().unwrap();
        server.join().unwrap();
    }
}
