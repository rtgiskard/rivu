use super::{
    SessionState, WireCommand, WireQuery, WireRequest, WireResponse, read_response_frame,
    state_response, unpack_error, unpack_hello, validate_hello, write_request_frame_buffered,
};
use crate::{
    model::{Command, Query},
    response::{Ack, QueryResponse, StateResponse, StateRevisions, StateSections},
};
use anyhow::{Context, Result, bail};
use bytes::BytesMut;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{net::UnixStream, runtime::Builder, time::timeout};
pub struct WatcherSession {
    runtime: tokio::runtime::Runtime,
    stream: UnixStream,
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
        read_buffer: BytesMut::new(),
        write_buffer: Vec::new(),
        session: SessionState::Unbound,
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

    fn ensure_handshake(&self) -> Result<()> {
        if self.session.is_bound() {
            Ok(())
        } else {
            bail!("Watcher session requires a Hello handshake")
        }
    }

    fn exchange(&mut self, request: &WireRequest) -> Result<WireResponse> {
        self.runtime.block_on(async {
            let notified = self.cancel_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.cancelled.load(Ordering::Acquire) {
                bail!("IPC request cancelled");
            }
            tokio::select! {
                _ = &mut notified => bail!("IPC request cancelled"),
                response = async {
                    write_request_frame_buffered(&mut self.stream, request, &mut self.write_buffer).await?;
                    read_response_frame(&mut self.stream, &mut self.read_buffer).await
                } => {
                    let response = response?;
                    if self.cancelled.load(Ordering::Acquire) {
                        bail!("IPC request cancelled");
                    }
                    Ok(response)
                }
            }
        })
    }

    pub fn get_state(&mut self, sections: StateSections) -> Result<StateResponse> {
        let request = if self.session.is_bound() {
            WireRequest::State(sections)
        } else {
            WireRequest::Hello(sections)
        };
        match unpack_error(self.exchange(&request)?)? {
            WireResponse::Hello(response) => {
                let response = validate_hello(response)?;
                self.session = SessionState::Bound;
                Ok(state_response(response.state))
            }
            WireResponse::State(state) if self.session.is_bound() => Ok(state_response(state)),
            _ => bail!("IPC response was not a state or Hello response"),
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

async fn one_shot(
    path: &Path,
    request: WireRequest,
    cancellation: Option<(Arc<AtomicBool>, Arc<tokio::sync::Notify>)>,
) -> Result<WireResponse> {
    let operation = async {
        let mut stream = UnixStream::connect(path).await.with_context(|| {
            format!(
                "Cannot connect to Rivu at {}. Start `rivu` or `rivu serve` first.",
                path.display()
            )
        })?;
        let mut pending = BytesMut::new();
        let mut write_buffer = Vec::new();
        let state_sections = match &request {
            WireRequest::State(sections) => Some(*sections),
            _ => None,
        };
        write_request_frame_buffered(
            &mut stream,
            &WireRequest::Hello(state_sections.unwrap_or_default()),
            &mut write_buffer,
        )
        .await?;
        let response = timeout(
            Duration::from_secs(120),
            read_response_frame(&mut stream, &mut pending),
        )
        .await
        .context("Reading IPC Hello response timed out")??;
        let hello = unpack_hello(response)?;
        if state_sections.is_some() {
            return Ok(WireResponse::State(hello.state));
        }
        write_request_frame_buffered(&mut stream, &request, &mut write_buffer).await?;
        let response = timeout(
            Duration::from_secs(120),
            read_response_frame(&mut stream, &mut pending),
        )
        .await
        .context("Reading IPC response timed out")??;
        unpack_error(response)
    };
    let Some((cancelled, notify)) = cancellation else {
        return operation.await;
    };
    let notified = notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    if cancelled.load(Ordering::Acquire) {
        bail!("IPC request cancelled");
    }
    tokio::select! { _ = &mut notified => bail!("IPC request cancelled"), response = operation => response }
}

pub fn query(path: &Path, query: &Query) -> Result<QueryResponse> {
    let runtime = client_runtime()?;
    let frame = runtime.block_on(one_shot(
        path,
        WireRequest::Query(WireQuery::from(query.clone())),
        None,
    ))?;
    match frame {
        WireResponse::Query(response) => Ok(response),
        _ => bail!("IPC response was not a query response"),
    }
}

pub(crate) fn query_with_cancel(
    path: &Path,
    query: &Query,
    cancelled: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
) -> Result<QueryResponse> {
    let runtime = client_runtime()?;
    let frame = runtime.block_on(one_shot(
        path,
        WireRequest::Query(WireQuery::from(query.clone())),
        Some((cancelled, notify)),
    ))?;
    match frame {
        WireResponse::Query(response) => Ok(response),
        _ => bail!("IPC response was not a query response"),
    }
}

pub fn get_state(path: &Path, sections: StateSections) -> Result<StateResponse> {
    let runtime = client_runtime()?;
    let frame = runtime.block_on(one_shot(path, WireRequest::State(sections), None))?;
    match frame {
        WireResponse::State(response) => Ok(state_response(response)),
        _ => bail!("IPC response was not a state response"),
    }
}

pub fn request_ack(path: &Path, command: &Command) -> Result<Ack> {
    let runtime = client_runtime()?;
    let frame = runtime.block_on(one_shot(
        path,
        WireRequest::Command(WireCommand::from(command.clone())),
        None,
    ))?;
    match frame {
        WireResponse::Ack(response) => Ok(response),
        _ => bail!("IPC response was not an acknowledgement"),
    }
}
