use super::{
    SessionState, WireCommand, WireQuery, WireRequest, WireResponse, read_response_frame,
    state_response, unpack_error, unpack_hello, write_request_frame_buffered,
};
use crate::{
    app::AppCommand,
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
    })
}

impl ClientSession {
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
    }

    fn ensure_handshake(&mut self) -> Result<()> {
        if !self.session.is_bound() {
            self.get_state(StateSections::default())?;
        }
        Ok(())
    }

    fn exchange(&mut self, request: &WireRequest) -> Result<WireResponse> {
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
                        timeout(Duration::from_secs(120), response)
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
            let response = self.exchange(&WireRequest::Hello(sections))?;
            return match unpack_hello(response) {
                Ok(response) => {
                    self.session = SessionState::Bound;
                    Ok(state_response(response.state))
                }
                Err(error) => {
                    self.disconnect();
                    Err(error)
                }
            };
        }
        match unpack_error(self.exchange(&WireRequest::State(sections))?)? {
            WireResponse::State(state) => Ok(state_response(state)),
            _ => bail!("IPC response was not a state response"),
        }
    }

    pub fn query(&mut self, query: &Query) -> Result<QueryResponse> {
        self.ensure_handshake()?;
        let request = WireRequest::Query(WireQuery::from(query.clone()));
        match unpack_error(self.exchange(&request)?)? {
            WireResponse::Query(response) => Ok(response),
            _ => bail!("IPC response was not a query response"),
        }
    }

    pub fn request_ack(&mut self, command: &(impl Clone + Into<AppCommand>)) -> Result<Ack> {
        self.ensure_handshake()?;
        let command: AppCommand = command.clone().into();
        let request = WireRequest::Command(WireCommand::from(command));
        match unpack_error(self.exchange(&request)?)? {
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
        let response = match self.exchange(&request) {
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
