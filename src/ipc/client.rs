use super::protocol::{
    SessionState, WireCommand, WireQuery, WireRequest, WireResponse, state_response,
};
use super::transport::{
    read_response_frame, unpack_error, unpack_hello, write_request_frame_buffered,
};
use crate::{
    app::AppCommand,
    core::CoreHandle,
    model::Query,
    response::{Ack, QueryResponse, StateResponse, StateRevisions, StateSections},
};
use anyhow::{Context, Result, bail};
use bytes::BytesMut;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{net::UnixStream, runtime::Builder, time::timeout};

#[derive(Debug)]
pub struct InstanceChanged;

impl std::fmt::Display for InstanceChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Rivu instance changed; reopen the TUI to use the new instance")
    }
}

impl std::error::Error for InstanceChanged {}

/// Serial IPC requests share a runtime, socket, handshake, and framing buffers.
/// A failed exchange discards the socket; only a later request reconnects, and
/// the failed request is never replayed because its mutation may have executed.
pub struct ClientSession {
    runtime: tokio::runtime::Runtime,
    data_dir: PathBuf,
    stream: Option<UnixStream>,
    cancelled: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
    read_buffer: BytesMut,
    write_buffer: Vec<u8>,
    session: SessionState,
    expected_instance: Option<u32>,
}

fn client_runtime() -> Result<tokio::runtime::Runtime> {
    Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("creating IPC runtime")
}

/// Create a session for a data directory; connect and handshake on its first request.
pub fn client_session(data_dir: &Path) -> Result<ClientSession> {
    client_session_with_cancel(
        data_dir,
        Arc::new(AtomicBool::new(false)),
        Arc::new(tokio::sync::Notify::new()),
    )
}

pub fn client_session_with_cancel(
    data_dir: &Path,
    cancelled: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
) -> Result<ClientSession> {
    Ok(ClientSession {
        runtime: client_runtime()?,
        data_dir: data_dir.to_owned(),
        stream: None,
        cancelled,
        cancel_notify,
        read_buffer: BytesMut::new(),
        write_buffer: Vec::new(),
        session: SessionState::Unbound,
        expected_instance: None,
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

    pub fn cancellation(&self) -> (Arc<AtomicBool>, Arc<tokio::sync::Notify>) {
        (self.cancelled.clone(), self.cancel_notify.clone())
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.cancel_notify.notify_waiters();
    }

    fn disconnect(&mut self) {
        self.stream = None;
        self.session = SessionState::Unbound;
        self.read_buffer.clear();
        if self.read_buffer.capacity() > super::transport::RETAINED_BUFFER_LIMIT {
            self.read_buffer = BytesMut::new();
        }
        super::transport::release_large_buffer(&mut self.write_buffer);
    }

    fn ensure_handshake(&mut self) -> Result<()> {
        if !self.session.is_bound() {
            self.get_state(StateSections::default())?;
        }
        Ok(())
    }

    fn exchange(&mut self, request: &WireRequest, wait_timeout: Duration) -> Result<WireResponse> {
        let result = self.runtime.block_on(async {
            let notified = self.cancel_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.cancelled.load(Ordering::Acquire) {
                bail!("IPC request cancelled");
            }
            tokio::select! {
                _ = &mut notified => bail!("IPC request cancelled"),
                response = async {
                    if self.stream.is_none() {
                        self.stream = Some(super::endpoint::connect(&self.data_dir).await.with_context(|| {
                            format!(
                                "Cannot connect to Rivu at {}. Start `rivu` or `rivu serve` first.",
                                self.data_dir.display()
                            )
                        })?);
                    }
                    let stream = self.stream.as_mut().unwrap();
                    write_request_frame_buffered(stream, request, &mut self.write_buffer).await?;
                    let response = read_response_frame(stream, &mut self.read_buffer);
                    if matches!(request, WireRequest::Watch(_)) {
                        response.await
                    } else {
                        timeout(super::transport::response_timeout(wait_timeout), response)
                            .await
                            .context("Reading IPC response timed out")?
                    }
                } => {
                    let response = response?;
                    if self.cancelled.load(Ordering::Acquire) {
                        bail!("IPC request cancelled");
                    }
                    Ok(response)
                }
            }
        });
        if result.is_err() {
            self.disconnect();
        }
        result
    }

    pub fn get_state(&mut self, sections: StateSections) -> Result<StateResponse> {
        if !self.session.is_bound() {
            let response = self.exchange(
                &WireRequest::Hello(sections),
                CoreHandle::query_wait_timeout(),
            )?;
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
        match unpack_error(self.exchange(
            &WireRequest::State(sections),
            CoreHandle::query_wait_timeout(),
        )?)? {
            WireResponse::State(state) => Ok(state_response(state)),
            _ => bail!("IPC response was not a state response"),
        }
    }

    pub fn query(&mut self, query: &Query) -> Result<QueryResponse> {
        self.ensure_handshake()?;
        let request = WireRequest::Query(WireQuery::from(query.clone()));
        match unpack_error(self.exchange(&request, CoreHandle::query_wait_timeout())?)? {
            WireResponse::Query(response) => Ok(response),
            _ => bail!("IPC response was not a query response"),
        }
    }

    pub fn request_ack(&mut self, command: &(impl Clone + Into<AppCommand>)) -> Result<Ack> {
        self.ensure_handshake()?;
        let command: AppCommand = command.clone().into();
        let wait_timeout = command.wait_timeout();
        let request = WireRequest::Command(WireCommand::from(command));
        match unpack_error(self.exchange(&request, wait_timeout)?)? {
            WireResponse::Ack(response) => Ok(response),
            _ => bail!("IPC response was not an acknowledgement"),
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
        self.ensure_handshake()?;
        let request = WireRequest::Watch(revisions);
        let response = match self.exchange(&request, CoreHandle::query_wait_timeout()) {
            Ok(response) => response,
            Err(_) if self.cancelled.load(Ordering::Acquire) => return Ok(None),
            Err(error) => return Err(error),
        };
        match unpack_error(response)? {
            WireResponse::Watch(revisions) => Ok(Some(revisions)),
            _ => bail!("IPC response was not a watch response"),
        }
    }
}

pub fn query(data_dir: &Path, query: &Query) -> Result<QueryResponse> {
    client_session(data_dir)?.query(query)
}

pub fn get_state(data_dir: &Path, sections: StateSections) -> Result<StateResponse> {
    client_session(data_dir)?.get_state(sections)
}

pub fn request_ack(data_dir: &Path, command: &(impl Clone + Into<AppCommand>)) -> Result<Ack> {
    client_session(data_dir)?.request_ack(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optimize_database_accepts_reply_after_old_response_deadline() {
        use super::super::{
            protocol::{HelloResponse, MAX_REQUEST, wire_state},
            transport::{decode_request_frame, read_frame, write_frame_buffered},
        };
        use crate::model::Command;
        use std::os::unix::net::UnixListener;

        let directory = super::super::tests::socket_test_directory();
        let listener =
            UnixListener::bind_addr(&super::super::endpoint::address(directory.path()).unwrap())
                .unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut client = client_session(directory.path()).unwrap();
        let server = {
            let _entered = client.runtime.enter();
            tokio::time::pause();
            let listener = tokio::net::UnixListener::from_std(listener).unwrap();
            // Paused time must not advance while real socket I/O is pending.
            // Only the deliberate server delay below may advance the clock.
            let keep_awake = || async {
                loop {
                    tokio::task::yield_now().await;
                }
            };
            let mut io_guards = tokio::task::JoinSet::new();
            let handshake_guard = io_guards.spawn(keep_awake());
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut pending = BytesMut::new();
                let mut buffer = Vec::new();
                let frame = read_frame(&mut stream, MAX_REQUEST, &mut pending)
                    .await
                    .unwrap();
                assert!(matches!(
                    decode_request_frame(&frame).unwrap(),
                    WireRequest::Hello(_)
                ));
                write_frame_buffered(
                    &mut stream,
                    &WireResponse::Hello(HelloResponse {
                        instance_id: 1u32.to_le_bytes(),
                        state: wire_state(StateResponse {
                            revisions: StateRevisions::default(),
                            playback: None,
                            queue: None,
                            library: None,
                            system: None,
                        }),
                    }),
                    &mut buffer,
                )
                .await
                .unwrap();
                let frame = read_frame(&mut stream, MAX_REQUEST, &mut pending)
                    .await
                    .unwrap();
                assert!(matches!(
                    decode_request_frame(&frame).unwrap(),
                    WireRequest::Command(WireCommand::OptimizeDatabase)
                ));
                handshake_guard.abort();
                // The old 120s client deadline would expire before this timer.
                tokio::time::sleep(Duration::from_secs(121)).await;
                io_guards.spawn(keep_awake());
                write_frame_buffered(
                    &mut stream,
                    &WireResponse::Ack(Ack {
                        ok: true,
                        error: None,
                        revision: 17,
                        scan_id: None,
                        relocation_id: None,
                    }),
                    &mut buffer,
                )
                .await
                .unwrap();
                // Dropping this set aborts the I/O guards on every exit path.
                io_guards
            })
        };

        let ack = client.request_ack(&Command::OptimizeDatabase).unwrap();
        assert!(ack.ok);
        assert_eq!(ack.revision, 17);
        drop(client.runtime.block_on(server).unwrap());
    }

    #[test]
    fn cancelled_exchange_releases_partial_bulk_buffers() {
        let directory = super::super::tests::socket_test_directory();
        let mut client = client_session(directory.path()).unwrap();
        client.read_buffer.resize(1024 * 1024, 0);
        client.write_buffer.resize(1024 * 1024, 0);
        client.cancel();
        assert!(client.get_state(StateSections::ALL).is_err());
        assert!(client.read_buffer.is_empty());
        assert!(client.read_buffer.capacity() <= super::super::transport::RETAINED_BUFFER_LIMIT);
        assert!(client.write_buffer.capacity() <= super::super::transport::RETAINED_BUFFER_LIMIT);
    }
}
