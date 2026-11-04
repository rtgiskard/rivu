//! Bounded waveform projections and explicit scans from the authoritative audio source.
//! The socket carries at most 1024-bin envelopes, never live PCM or graph pointers.
use crate::{core::CoreHandle, execution::BlockingExecutor};
use anyhow::{Context, Result, bail, ensure};
use async_channel::{Receiver, Sender};
use async_io::Async;
use futures::{FutureExt, StreamExt, io::AsyncReadExt, stream::FuturesUnordered};
use rivu_audio::WaveformScanSnapshot;
use rivu_protocol::waveform::{PlaybackRange, WaveformFrame};
use rivu_protocol::{
    model::{Query, Track},
    response::ViewResponse,
    transport::timeout,
    waveform::{Request, VERSION, WaveKey, address, read_frame, write_frame},
};
use std::{
    os::unix::net::{SocketAddr, UnixListener, UnixStream},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
type Response = rivu_protocol::waveform::Response<WaveformFrame>;
#[cfg(test)]
use rivu_protocol::waveform::{decode, encode};

// Version 2 adds the suspension state to progressive cache keys.
const MAX_CLIENTS: usize = 8;

fn progressive_response(source: u64, frame: &WaveformFrame, known: Option<WaveKey>) -> Response {
    let key = WaveKey {
        source,
        revision: frame.revision,
        suspension: frame.suspension_generation(),
        suspended: frame.is_suspended(),
    };
    Response::Progressive {
        key,
        frame: (known != Some(key)).then(|| Box::new(frame.clone())),
    }
}

pub(crate) struct WaveServer {
    shutdown: Sender<()>,
    worker: Option<async_executor::Task<()>>,
}
impl WaveServer {
    pub(crate) fn start(
        data_dir: &Path,
        handle: CoreHandle,
        instance: u32,
        executor: &Arc<async_executor::Executor<'static>>,
        workers: &Arc<BlockingExecutor>,
    ) -> Result<Self> {
        let listener = UnixListener::bind_addr(&address(data_dir)?)?;
        listener.set_nonblocking(true)?;
        let listener = Async::new(listener)?;
        let (shutdown, stopped) = async_channel::bounded::<()>(1);
        let executor = Arc::clone(executor);
        let tasks = Arc::clone(&executor);
        let workers = Arc::clone(workers);
        let worker = executor.spawn(async move {
            let mut clients = FuturesUnordered::new();
            loop {
                enum Event {
                    Stop,
                    Accepted(std::io::Result<(Async<UnixStream>, SocketAddr)>),
                    Completed,
                }
                let event = {
                    let completed = async {
                        if clients.is_empty() {
                            futures::future::pending::<()>().await;
                        }
                        clients.next().await
                    };
                    futures::select! {
                        _ = stopped.recv().fuse() => Event::Stop,
                        accepted = listener.accept().fuse() => Event::Accepted(accepted),
                        _ = completed.fuse() => Event::Completed,
                    }
                };
                match event {
                    Event::Stop => break,
                    Event::Completed => {}
                    Event::Accepted(Ok((stream, _))) if clients.len() < MAX_CLIENTS => {
                        if rivu_protocol::wire::verify_peer(&stream).is_err() {
                            continue;
                        }
                        let handle = handle.clone();
                        let workers = Arc::clone(&workers);
                        let stopped = stopped.clone();
                        clients.push(tasks.spawn(async move {
                            if let Err(error) =
                                session(stream, handle, instance, workers, stopped).await
                            {
                                tracing::debug!(%error, "waveform_session_ended");
                            }
                        }));
                    }
                    Event::Accepted(Ok(_)) => {}
                    Event::Accepted(Err(error)) => {
                        tracing::warn!(%error, "waveform_accept_failed");
                        break;
                    }
                }
            }
            while clients.next().await.is_some() {}
        });
        Ok(Self {
            shutdown,
            worker: Some(worker),
        })
    }
}
impl Drop for WaveServer {
    fn drop(&mut self) {
        self.shutdown.close();
        if let Some(worker) = self.worker.take() {
            async_io::block_on(worker);
        }
    }
}

async fn session(
    mut stream: Async<UnixStream>,
    handle: CoreHandle,
    instance: u32,
    workers: Arc<BlockingExecutor>,
    stopped: Receiver<()>,
) -> Result<()> {
    let request = timeout(Duration::from_secs(1), read_frame::<Request>(&mut stream)).await??;
    ensure!(
        matches!(request, Request::Hello { version: VERSION, instance: id } if id == instance),
        "Waveform Hello instance/version mismatch"
    );
    write_frame(
        &mut stream,
        &Response::Hello {
            version: VERSION,
            instance,
        },
    )
    .await?;
    loop {
        let request = futures::select! {
            _ = stopped.recv().fuse() => return Ok(()),
            request = read_frame::<Request>(&mut stream).fuse() => request?,
        };
        let response = match request {
            Request::Hello { .. } => Response::Error("Waveform session is already bound".into()),
            Request::Progressive { known } => {
                let source = handle
                    .state(rivu_protocol::response::StateSections {
                        playback: true,
                        ..Default::default()
                    })
                    .playback
                    .expect("playback section requested")
                    .source_generation;
                let frame = handle.waveform.read();
                progressive_response(source, &frame, known)
            }
            Request::Full { track, source } => {
                let cancelled = Arc::new(AtomicBool::new(false));
                let cancellation = CancelOnDrop(Arc::clone(&cancelled));
                let full = full_scan(&handle, &workers, &track, source, cancelled).fuse();
                futures::pin_mut!(full);
                let mut extra = [0; 1];
                let result = futures::select! {
                    _ = stopped.recv().fuse() => return Ok(()),
                    _ = stream.read(&mut extra).fuse() => return Ok(()),
                    result = full => result,
                };
                drop(cancellation);
                match result {
                    Ok(frame) => Response::Full(Box::new(frame)),
                    Err(error) => Response::Error(format!("{error:#}")),
                }
            }
        };
        write_frame(&mut stream, &response).await?;
    }
}
struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

async fn full_scan(
    handle: &CoreHandle,
    workers: &BlockingExecutor,
    requested: &Track,
    source: Option<u64>,
    cancelled: Arc<AtomicBool>,
) -> Result<WaveformFrame> {
    let current = handle.state(rivu_protocol::response::StateSections {
        playback: true,
        configuration: true,
        ..Default::default()
    });
    let config = current
        .configuration
        .expect("configuration section requested")
        .config;
    ensure!(config.visualization_enabled, "Visualizations are disabled");
    // Register before any queued query/snapshot: a fast off/on still cancels this scan.
    handle
        .waveform_control
        .cancel_scan_on_suspend(&handle.waveform.read(), &cancelled);
    let playback = current.playback.expect("playback section requested");
    if let Some(source) = source {
        ensure!(
            source == playback.source_generation
                && playback
                    .current_track
                    .as_ref()
                    .is_some_and(|track| track.id == requested.id),
            "Full waveform source changed before capture"
        );
    }
    let captured = if let Some(source) = source {
        Some(handle.waveform_snapshot(source)?)
    } else {
        None
    };
    let response = handle
        .query_retained(Query::Track {
            track_id: requested.id,
        })
        .await;
    let track = match response.result.map_err(anyhow::Error::msg)? {
        ViewResponse::Track(Some(track)) => track,
        _ => bail!("Full waveform track no longer exists"),
    };
    ensure!(
        track.path == requested.path
            && track.cue == requested.cue
            && track.fingerprint == requested.fingerprint
            && track.decoder_hint == requested.decoder_hint,
        "Full waveform metadata changed before capture"
    );
    let range = track.cue.as_ref().map(|cue| PlaybackRange {
        start_seconds: cue.start_seconds(),
        end_seconds: cue.end_seconds(),
    });
    let fallback = WaveformScanSnapshot::original(
        track.path.clone(),
        range,
        track.decoder_hint,
        config.visualization_signal,
        Arc::new(config.equalizer.clone()),
    );
    let mut snapshot = match captured {
        Some(reply) => reply
            .await
            .context("Capturing waveform source")?
            .map_err(anyhow::Error::msg)?
            .unwrap_or(fallback),
        None => fallback,
    };
    if snapshot.path == track.path && snapshot.range == range {
        snapshot.decoder_hint = track.decoder_hint;
    }
    let mut frame = workers
        .execute(move || {
            rivu_audio::waveform(
                &snapshot,
                config.media_read_buffer_mib,
                config.ffmpeg_enabled,
                &cancelled,
            )
        })
        .await??;
    frame.path = Some(track.path.clone());
    frame.range = range;
    frame.decoder_hint = track.decoder_hint;
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progressive_socket_refreshes_suspension_without_new_pcm() {
        let directory = crate::ipc::tests::socket_test_directory();
        let (handle, _queries) = CoreHandle::test_query_port();
        let tasks = crate::execution::TaskRuntime::start().unwrap();
        let service = WaveServer::start(
            directory.path(),
            handle.clone(),
            7,
            &tasks.handle,
            &tasks.workers,
        )
        .unwrap();
        async_io::block_on(timeout(Duration::from_secs(5), async {
            let mut stream = rivu_protocol::waveform::connect(directory.path())
                .await
                .unwrap();
            write_frame(
                &mut stream,
                &Request::Hello {
                    version: VERSION,
                    instance: 7,
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_frame::<Response>(&mut stream).await.unwrap(),
                Response::Hello {
                    version: VERSION,
                    instance: 7
                }
            ));
            let mut known = None;
            for enabled in [true, false, true] {
                handle
                    .waveform_control
                    .set_visualization_enabled(&mut handle.waveform.write(), enabled);
                write_frame(&mut stream, &Request::Progressive { known })
                    .await
                    .unwrap();
                let Response::Progressive { key, frame } =
                    read_frame::<Response>(&mut stream).await.unwrap()
                else {
                    panic!("expected progressive response");
                };
                assert_eq!(key.suspended, !enabled);
                assert_eq!(frame.unwrap().is_suspended(), !enabled);
                known = Some(key);
                write_frame(&mut stream, &Request::Progressive { known })
                    .await
                    .unwrap();
                assert!(matches!(
                    read_frame::<Response>(&mut stream).await.unwrap(),
                    Response::Progressive { frame: None, .. }
                ));
            }
        }))
        .unwrap();
        drop(service);
    }

    // Exercise service cache decisions and serialized suspension metadata without PCM.
    fn refresh(
        server: &WaveformFrame,
        client: &mut WaveformFrame,
        known: &mut Option<WaveKey>,
    ) -> bool {
        let request =
            decode::<Request>(&encode(&Request::Progressive { known: *known }).unwrap()).unwrap();
        let Request::Progressive { known: requested } = request else {
            panic!("expected progressive request");
        };
        let response = progressive_response(7, server, requested);
        let response = decode::<Response>(&encode(&response).unwrap()).unwrap();
        let Response::Progressive { key, frame } = response else {
            panic!("expected progressive response");
        };
        *known = Some(key);
        if let Some(frame) = frame {
            *client = *frame;
            true
        } else {
            false
        }
    }

    #[test]
    fn idle_off_on_refresh_resumes_snapshot_and_allows_new_scan() {
        // Neither paused nor stopped playback produces PCM. Keep the frame untouched
        // except for the actual visualization toggle API throughout this transition.
        let mut server = WaveformFrame::default();
        let control = rivu_audio::WaveformControl::default();
        let mut client = WaveformFrame::default();
        let mut known = None;
        assert!(refresh(&server, &mut client, &mut known));
        assert!(!refresh(&server, &mut client, &mut known));

        let old_scan = Arc::new(AtomicBool::new(false));
        control.cancel_scan_on_suspend(&server, &old_scan);
        assert!(!old_scan.load(Ordering::Relaxed));

        control.set_visualization_enabled(&mut server, false);
        assert!(refresh(&server, &mut client, &mut known));
        assert!(old_scan.load(Ordering::Relaxed));
        let disabled_scan = Arc::new(AtomicBool::new(false));
        control.cancel_scan_on_suspend(&server, &disabled_scan);
        assert!(disabled_scan.load(Ordering::Relaxed));
        assert!(!refresh(&server, &mut client, &mut known));

        let revision = server.revision;
        let suspension = server.suspension_generation();
        control.set_visualization_enabled(&mut server, true);
        assert_eq!(server.revision, revision);
        assert_eq!(server.suspension_generation(), suspension);
        assert!(refresh(&server, &mut client, &mut known));
        let resumed_scan = Arc::new(AtomicBool::new(false));
        control.cancel_scan_on_suspend(&server, &resumed_scan);
        assert!(!resumed_scan.load(Ordering::Relaxed));
        assert!(!refresh(&server, &mut client, &mut known));

        // The resumed guard remains live: the next disable still cancels it.
        control.set_visualization_enabled(&mut server, false);
        assert!(refresh(&server, &mut client, &mut known));
        assert!(resumed_scan.load(Ordering::Relaxed));
    }
}
