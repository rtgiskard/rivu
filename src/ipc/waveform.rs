//! Bounded waveform projections and explicit scans from the authoritative audio source.
//! The socket carries at most 1024-bin envelopes, never live PCM or graph pointers.
use crate::{core::CoreHandle, execution::BlockingExecutor};
use anyhow::{Context, Result, bail, ensure};
use async_channel::{Receiver, Sender};
use async_io::Async;
use futures::{FutureExt, StreamExt, io::AsyncReadExt, stream::FuturesUnordered};
use rivu_audio::WaveformScanSnapshot;
use rivu_protocol::waveform::{ExpectedSource, PlaybackRange, WaveformFrame};
use rivu_protocol::{
    model::Query,
    response::ViewResponse,
    transport::timeout,
    waveform::{Request, VERSION, WaveKey, address, encode_frame_into, read_frame, write_frame},
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
type Response = rivu_protocol::waveform::Response<Box<WaveformFrame>>;

const MAX_CLIENTS: usize = 8;
// 1024 bins need at most 101 encoded bytes each, plus path and scalar metadata.
const PROGRESSIVE_SCRATCH_CAPACITY: usize = 128 * 1024;

fn progressive_response(
    source: u64,
    frame: &WaveformFrame,
    known: Option<WaveKey>,
) -> rivu_protocol::waveform::Response<&WaveformFrame> {
    let key = WaveKey {
        source,
        revision: frame.revision,
        suspension: frame.suspension_generation(),
        suspended: frame.is_suspended(),
    };
    rivu_protocol::waveform::Response::Progressive {
        key,
        frame: (known != Some(key)).then_some(frame),
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
                futures_lite::future::yield_now().await;
                if stopped.is_closed() {
                    break;
                }
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
    let mut bytes = Vec::new();
    let request = timeout(
        Duration::from_secs(1),
        read_frame::<Request>(&mut stream, &mut bytes),
    )
    .await??;
    ensure!(
        matches!(request, Request::Hello { version: VERSION, instance: id } if id == instance),
        "Waveform Hello instance/version mismatch"
    );
    encode_frame_into(
        &Response::Hello {
            version: VERSION,
            instance,
        },
        &mut bytes,
    )?;
    write_frame(&mut stream, &mut bytes).await?;
    loop {
        // A pipelined client must not monopolize the shared service executor.
        futures_lite::future::yield_now().await;
        if stopped.is_closed() {
            return Ok(());
        }
        let request = futures::select! {
            _ = stopped.recv().fuse() => return Ok(()),
            request = read_frame::<Request>(&mut stream, &mut bytes).fuse() => request?,
        };
        match request {
            Request::Hello { .. } => {
                encode_frame_into(
                    &Response::Error("Waveform session is already bound".into()),
                    &mut bytes,
                )?;
            }
            Request::Progressive { known } => {
                let source = handle
                    .state(rivu_protocol::response::StateSections {
                        playback: true,
                        ..Default::default()
                    })
                    .playback
                    .expect("playback section requested")
                    .source_generation;
                // Reserve outside the audio lock. The producer publishes at most 1024
                // bins; this reusable seat covers their worst-case encoded envelope.
                // Arc/COW publication would instead copy all bins on the audio writer
                // whenever a slow client retained a snapshot. Borrow only for this
                // bounded serialization pass, never for the socket write below.
                bytes.reserve_exact(PROGRESSIVE_SCRATCH_CAPACITY.saturating_sub(bytes.len()));
                let frame = handle.waveform.read();
                encode_frame_into(&progressive_response(source, &frame, known), &mut bytes)?;
            }
            Request::Full { expected, source } => {
                let cancelled = Arc::new(AtomicBool::new(false));
                let cancellation = CancelOnDrop(Arc::clone(&cancelled));
                let full = full_scan(&handle, &workers, &expected, source, cancelled).fuse();
                futures::pin_mut!(full);
                let mut extra = [0; 1];
                let result = futures::select! {
                    _ = stopped.recv().fuse() => return Ok(()),
                    _ = stream.read(&mut extra).fuse() => return Ok(()),
                    result = full => result,
                };
                drop(cancellation);
                let response = match result {
                    Ok(frame) => Response::Full(Box::new(frame)),
                    Err(error) => Response::Error(format!("{error:#}")),
                };
                encode_frame_into(&response, &mut bytes)?;
            }
        }
        write_frame(&mut stream, &mut bytes).await?;
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
    requested: &ExpectedSource,
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
        requested.matches(&track),
        "Full waveform metadata changed before capture"
    );
    let range = track.cue.as_ref().map(|cue| PlaybackRange {
        start_seconds: cue.start_seconds(),
        end_seconds: cue.end_seconds(),
    });
    let captured = match captured {
        Some(reply) => reply
            .await
            .context("Capturing waveform source")?
            .map_err(anyhow::Error::msg)?,
        None => None,
    };
    let mut snapshot = captured.unwrap_or_else(|| {
        WaveformScanSnapshot::original(
            track.path.clone(),
            range,
            track.decoder_hint,
            config.visualization_signal,
            Arc::new(config.equalizer.clone()),
        )
    });
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

    async fn send(stream: &mut Async<UnixStream>, request: &Request, bytes: &mut Vec<u8>) {
        encode_frame_into(request, bytes).unwrap();
        write_frame(stream, bytes).await.unwrap();
    }

    #[test]
    fn pipelined_progressive_requests_yield_to_suspension_and_shutdown() {
        use std::{
            future::Future,
            task::{Context, Waker},
        };

        let (handle, _queries) = CoreHandle::test_query_port();
        let tasks = crate::execution::TaskRuntime::start().unwrap();
        let (stream, peer) = UnixStream::pair().unwrap();
        let stream = Async::new(stream).unwrap();
        let mut peer = Async::new(peer).unwrap();
        let (stop, stopped) = async_channel::bounded(1);
        async_io::block_on(timeout(Duration::from_secs(5), async {
            let mut bytes = Vec::new();
            send(
                &mut peer,
                &Request::Hello {
                    version: VERSION,
                    instance: 7,
                },
                &mut bytes,
            )
            .await;
            for _ in 0..3 {
                send(&mut peer, &Request::Progressive { known: None }, &mut bytes).await;
            }
            peer.get_ref().shutdown(std::net::Shutdown::Write).unwrap();
            stream.readable().await.unwrap();
            stream.writable().await.unwrap();
            let mut serving = std::pin::pin!(session(
                stream,
                handle.clone(),
                7,
                Arc::clone(&tasks.workers),
                stopped,
            ));
            let mut context = Context::from_waker(Waker::noop());
            assert!(serving.as_mut().poll(&mut context).is_pending());
            assert!(matches!(
                read_frame::<Response>(&mut peer, &mut bytes).await.unwrap(),
                Response::Hello { instance: 7, .. }
            ));

            for suspended in [false, true] {
                handle
                    .waveform_control
                    .set_visualization_enabled(&mut handle.waveform.write(), !suspended);
                assert!(serving.as_mut().poll(&mut context).is_pending());
                let Response::Progressive { key, frame } =
                    read_frame::<Response>(&mut peer, &mut bytes).await.unwrap()
                else {
                    panic!("expected progressive response");
                };
                assert_eq!(key.suspended, suspended);
                assert_eq!(frame.unwrap().is_suspended(), suspended);
            }
            // Shutdown wins before the third buffered request, not just after EOF.
            stop.close();
            assert!(matches!(
                serving.as_mut().poll(&mut context),
                std::task::Poll::Ready(Ok(()))
            ));
        }))
        .unwrap();
    }

    #[test]
    fn waveform_socket_rejects_an_unbound_instance_or_version() {
        let directory = crate::ipc::tests::socket_test_directory();
        let (handle, _queries) = CoreHandle::test_query_port();
        let tasks = crate::execution::TaskRuntime::start().unwrap();
        let service =
            WaveServer::start(directory.path(), handle, 7, &tasks.handle, &tasks.workers).unwrap();
        async_io::block_on(timeout(Duration::from_secs(5), async {
            for (version, instance) in [(VERSION - 1, 7), (VERSION, 8)] {
                let mut stream = rivu_protocol::waveform::connect(directory.path())
                    .await
                    .unwrap();
                let mut bytes = Vec::new();
                send(
                    &mut stream,
                    &Request::Hello { version, instance },
                    &mut bytes,
                )
                .await;
                assert!(
                    read_frame::<Response>(&mut stream, &mut bytes)
                        .await
                        .is_err()
                );
            }
        }))
        .unwrap();
        drop(service);
    }

    #[test]
    fn full_socket_reloads_authority_before_accepting_a_source_expectation() {
        let directory = crate::ipc::tests::socket_test_directory();
        let (handle, queries) = CoreHandle::test_query_port();
        let tasks = crate::execution::TaskRuntime::start().unwrap();
        let service =
            WaveServer::start(directory.path(), handle, 7, &tasks.handle, &tasks.workers).unwrap();
        async_io::block_on(timeout(Duration::from_secs(5), async {
            let mut stream = rivu_protocol::waveform::connect(directory.path())
                .await
                .unwrap();
            let mut bytes = Vec::new();
            send(
                &mut stream,
                &Request::Hello {
                    version: VERSION,
                    instance: 7,
                },
                &mut bytes,
            )
            .await;
            assert!(matches!(
                read_frame::<Response>(&mut stream, &mut bytes)
                    .await
                    .unwrap(),
                Response::Hello { .. }
            ));
            send(
                &mut stream,
                &Request::Full {
                    expected: ExpectedSource {
                        id: 9,
                        path: "not-authoritative.wav".into(),
                        cue: None,
                        fingerprint: None,
                        decoder_hint: rivu_format::decoder_hint::DecoderHint::Auto,
                    },
                    source: None,
                },
                &mut bytes,
            )
            .await;
            let reply = async {
                queries
                    .receive()
                    .await
                    .respond(rivu_protocol::response::QueryResponse {
                        revisions: Default::default(),
                        result: Ok(ViewResponse::Track(None)),
                    });
            };
            let (response, ()) =
                futures::join!(read_frame::<Response>(&mut stream, &mut bytes), reply);
            let Response::Error(error) = response.unwrap() else {
                panic!("a missing authoritative track must reject the scan");
            };
            assert!(error.contains("track no longer exists"));
        }))
        .unwrap();
        drop(service);
    }

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
            let mut bytes = Vec::new();
            send(
                &mut stream,
                &Request::Hello {
                    version: VERSION,
                    instance: 7,
                },
                &mut bytes,
            )
            .await;
            assert!(matches!(
                read_frame::<Response>(&mut stream, &mut bytes)
                    .await
                    .unwrap(),
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
                send(&mut stream, &Request::Progressive { known }, &mut bytes).await;
                let Response::Progressive { key, frame } =
                    read_frame::<Response>(&mut stream, &mut bytes)
                        .await
                        .unwrap()
                else {
                    panic!("expected progressive response");
                };
                assert_eq!(key.suspended, !enabled);
                assert_eq!(frame.unwrap().is_suspended(), !enabled);
                known = Some(key);
                send(&mut stream, &Request::Progressive { known }, &mut bytes).await;
                assert!(matches!(
                    read_frame::<Response>(&mut stream, &mut bytes)
                        .await
                        .unwrap(),
                    Response::Progressive { frame: None, .. }
                ));
            }
        }))
        .unwrap();
        drop(service);
    }

    // Exercise service cache decisions without PCM.
    fn refresh(server: &WaveformFrame, known: &mut Option<WaveKey>) -> bool {
        let rivu_protocol::waveform::Response::Progressive { key, frame } =
            progressive_response(7, server, *known)
        else {
            panic!("expected progressive response");
        };
        *known = Some(key);
        frame.is_some()
    }

    #[test]
    fn idle_off_on_refresh_resumes_snapshot_and_allows_new_scan() {
        // Neither paused nor stopped playback produces PCM. Keep the frame untouched
        // except for the actual visualization toggle API throughout this transition.
        let mut server = WaveformFrame::default();
        let control = rivu_audio::WaveformControl::default();
        let mut known = None;
        assert!(refresh(&server, &mut known));
        assert!(!refresh(&server, &mut known));

        let old_scan = Arc::new(AtomicBool::new(false));
        control.cancel_scan_on_suspend(&server, &old_scan);
        assert!(!old_scan.load(Ordering::Relaxed));

        control.set_visualization_enabled(&mut server, false);
        assert!(refresh(&server, &mut known));
        assert!(old_scan.load(Ordering::Relaxed));
        let disabled_scan = Arc::new(AtomicBool::new(false));
        control.cancel_scan_on_suspend(&server, &disabled_scan);
        assert!(disabled_scan.load(Ordering::Relaxed));
        assert!(!refresh(&server, &mut known));

        let revision = server.revision;
        let suspension = server.suspension_generation();
        control.set_visualization_enabled(&mut server, true);
        assert_eq!(server.revision, revision);
        assert_eq!(server.suspension_generation(), suspension);
        assert!(refresh(&server, &mut known));
        let resumed_scan = Arc::new(AtomicBool::new(false));
        control.cancel_scan_on_suspend(&server, &resumed_scan);
        assert!(!resumed_scan.load(Ordering::Relaxed));
        assert!(!refresh(&server, &mut known));

        // The resumed guard remains live: the next disable still cancels it.
        control.set_visualization_enabled(&mut server, false);
        assert!(refresh(&server, &mut known));
        assert!(resumed_scan.load(Ordering::Relaxed));
    }
}
