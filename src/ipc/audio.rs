//! Lossless, bounded finite PCM rendering on an instance-bound connection.
use crate::{core::CoreHandle, execution::BlockingExecutor};
use anyhow::{Context, Result, bail, ensure};
use async_channel::{Receiver, Sender};
use async_io::Async;
use futures::{
    FutureExt, StreamExt,
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    stream::FuturesUnordered,
};
use rivu_audio::{AudioRenderSnapshot, RenderEvent};
use rivu_protocol::{
    audio::{
        ExpectedSource, PlaybackRange, Request, Response, VERSION, address, encode_frame_into,
        read_frame, write_frame,
    },
    model::Query,
    response::{StateSections, ViewResponse},
    transport::timeout,
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

const MAX_CLIENTS: usize = 8;
const BUFFERED_CHUNKS: usize = 2;
const CHUNK_FRAMES: usize = 4096;
const CHUNK_BYTES: usize = 32 * 1024;
// One filling, two queued, one writing; all buffers circulate in the same pool.
const PCM_BUFFERS: usize = BUFFERED_CHUNKS + 2;

pub(crate) struct AudioServer {
    shutdown: Sender<()>,
    worker: Option<async_executor::Task<()>>,
}
impl AudioServer {
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
        let tasks = Arc::clone(executor);
        let workers = Arc::clone(workers);
        let worker = executor.spawn(async move {
            let mut clients = FuturesUnordered::new();
            loop {
                futures_lite::future::yield_now().await;
                enum Event {
                    Stop,
                    Accepted(std::io::Result<(Async<UnixStream>, SocketAddr)>),
                    Completed,
                }
                let completed = async {
                    if clients.is_empty() {
                        futures::future::pending::<()>().await;
                    }
                    clients.next().await
                };
                let event = futures::select! {
                    _ = stopped.recv().fuse() => Event::Stop,
                    accepted = listener.accept().fuse() => Event::Accepted(accepted),
                    _ = completed.fuse() => Event::Completed,
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
                                tracing::debug!(%error, "audio_stream_session_ended");
                            }
                        }));
                    }
                    Event::Accepted(Ok(_)) => {}
                    Event::Accepted(Err(error)) => {
                        tracing::warn!(%error, "audio_stream_accept_failed");
                        break;
                    }
                }
            }
            stopped.close();
            while clients.next().await.is_some() {}
        });
        Ok(Self {
            shutdown,
            worker: Some(worker),
        })
    }
}
impl Drop for AudioServer {
    fn drop(&mut self) {
        self.shutdown.close();
        if let Some(worker) = self.worker.take() {
            async_io::block_on(worker);
        }
    }
}

async fn send(
    stream: &mut (impl AsyncWrite + Unpin),
    response: &Response,
    bytes: &mut Vec<u8>,
) -> Result<()> {
    encode_frame_into(response, bytes)?;
    write_frame(stream, bytes).await
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
        "Audio stream instance/version mismatch"
    );
    send(
        &mut stream,
        &Response::Hello {
            version: VERSION,
            instance,
        },
        &mut bytes,
    )
    .await?;
    let request = futures::select! {
        _ = stopped.recv().fuse() => return Ok(()),
        request = read_frame::<Request>(&mut stream, &mut bytes).fuse() => request?,
    };
    let Request::Read {
        expected,
        source,
        signal,
        range,
    } = request
    else {
        bail!("Audio stream session requires one Read request");
    };
    let (mut reader, mut writer) = stream.split();
    let mut extra = [0; 1];
    // The captured config and audio command are fixed before waiting for a query
    // or a blocking-worker lease. A current source must never fall back to disk.
    let capture = capture(&handle, &expected, source, signal, range, &workers).fuse();
    futures::pin_mut!(capture);
    let captured = futures::select! {
        _ = stopped.recv().fuse() => return Ok(()),
        _ = reader.read(&mut extra).fuse() => return Ok(()),
        captured = capture => captured,
    };
    let (snapshot, config) = match captured {
        Ok(captured) => captured,
        Err(error) => {
            send(
                &mut writer,
                &Response::Error(format!("{error:#}")),
                &mut bytes,
            )
            .await?;
            return Ok(());
        }
    };
    stream_render(
        &mut reader,
        &mut writer,
        snapshot,
        config,
        source,
        &workers,
        &stopped,
        &mut bytes,
    )
    .await
}

async fn capture(
    handle: &CoreHandle,
    expected: &ExpectedSource,
    source: Option<u64>,
    signal: rivu_format::config::VisualizationSignal,
    read_range: Option<PlaybackRange>,
    workers: &BlockingExecutor,
) -> Result<(AudioRenderSnapshot, Arc<rivu_format::config::Config>)> {
    if let Some(range) = read_range {
        ensure!(
            range.start_seconds.is_finite()
                && range.start_seconds >= 0.0
                && range
                    .end_seconds
                    .is_none_or(|end| end.is_finite() && end >= range.start_seconds),
            "Invalid PCM read range"
        );
    }
    let current = handle.state(StateSections {
        playback: true,
        configuration: true,
        ..Default::default()
    });
    let config = current
        .configuration
        .expect("configuration requested")
        .config;
    let playback = current.playback.expect("playback requested");
    let captured = if let Some(source) = source {
        ensure!(
            source == playback.source_generation
                && playback
                    .current_track
                    .as_ref()
                    .is_some_and(|track| expected.matches(track)),
            "Audio source changed before capture"
        );
        Some(handle.render_snapshot(source)?)
    } else {
        None
    };
    let response = handle
        .query_retained(Query::Track {
            track_id: expected.id,
        })
        .await;
    let track = match response.result.map_err(anyhow::Error::msg)? {
        ViewResponse::Track(Some(track)) => track,
        _ => bail!("Audio source no longer exists"),
    };
    ensure!(
        expected.matches(&track),
        "Audio source metadata changed before capture"
    );
    let mut snapshot = if let Some(captured) = captured {
        captured
            .await
            .context("Capturing active audio source")?
            .map_err(anyhow::Error::msg)?
            .context("Active audio source cannot be reconstructed")?
    } else {
        let path = track.path.clone();
        let identity = workers
            .execute(move || {
                rivu_audio::source_identity(&rivu_utils::regular_file::open_regular_file(&path)?)
            })
            .await??;
        let mut snapshot = AudioRenderSnapshot::original(
            track.path.clone(),
            track.cue.as_ref().map(|cue| PlaybackRange {
                start_seconds: cue.start_seconds(),
                end_seconds: cue.end_seconds(),
            }),
            track.decoder_hint,
            signal,
            Arc::new(config.equalizer.clone()),
        );
        snapshot.expected_identity = Some(identity);
        snapshot
    };
    snapshot.signal = signal;
    snapshot.read_range = read_range;
    Ok((snapshot, config))
}

struct PcmBatcher {
    chunks: Sender<Response>,
    available: Receiver<Vec<f32>>,
    cancelled: Arc<AtomicBool>,
    channels: usize,
    sample_limit: usize,
    pending: Option<Vec<f32>>,
    media_start: f64,
    media_step: f64,
    sequence: u64,
}

impl PcmBatcher {
    fn new(
        channels: usize,
        chunks: Sender<Response>,
        available: Receiver<Vec<f32>>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self> {
        ensure!(channels != 0, "Audio renderer omitted channels");
        let frames = CHUNK_FRAMES.min(CHUNK_BYTES / size_of::<f32>() / channels);
        ensure!(frames != 0, "Audio frame exceeds chunk budget");
        Ok(Self {
            chunks,
            available,
            cancelled,
            channels,
            sample_limit: frames * channels,
            pending: None,
            media_start: 0.0,
            media_step: 0.0,
            sequence: 0,
        })
    }

    fn push(&mut self, start: f64, step: f64, samples: &[f32]) -> Result<()> {
        if let Some(pending) = &self.pending {
            let expected =
                self.media_start + (pending.len() / self.channels) as f64 * self.media_step;
            // Absorb arithmetic rounding, never a one-sample timeline jump.
            if step != self.media_step || (start - expected).abs() > step * 0.25 {
                self.flush()?;
            }
        }
        let mut offset = 0;
        while offset < samples.len() {
            ensure!(
                !self.cancelled.load(Ordering::Acquire),
                "Audio render cancelled"
            );
            if self.pending.is_none() {
                self.pending = Some(self.available.recv_blocking()?);
                self.media_start = start + (offset / self.channels) as f64 * step;
                self.media_step = step;
            }
            let pending = self.pending.as_mut().expect("acquired PCM buffer");
            let take = (self.sample_limit - pending.len()).min(samples.len() - offset);
            pending.extend_from_slice(&samples[offset..offset + take]);
            offset += take;
            if pending.len() == self.sample_limit {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        ensure!(
            !self.cancelled.load(Ordering::Acquire),
            "Audio render cancelled"
        );
        if let Some(samples) = self.pending.take() {
            self.sequence += 1;
            self.chunks.send_blocking(Response::Pcm {
                sequence: self.sequence,
                media_start: self.media_start,
                media_step: self.media_step,
                samples,
            })?;
        }
        Ok(())
    }
}

// Both the lease and the channel sender live in the physical closure. Closing
// the receiver releases backpressure; cancellation does not release the lease.
#[expect(
    clippy::too_many_arguments,
    reason = "The stream owns one admitted request and its cancellation scope"
)]
async fn stream_render(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    snapshot: AudioRenderSnapshot,
    config: Arc<rivu_format::config::Config>,
    source: Option<u64>,
    workers: &BlockingExecutor,
    stopped: &Receiver<()>,
    bytes: &mut Vec<u8>,
) -> Result<()> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let (tx, rx) = async_channel::bounded(BUFFERED_CHUNKS);
    let (buffers, available) = async_channel::bounded(PCM_BUFFERS);
    let initial_buffers = buffers.clone();
    let _cancel = CancelOnDrop {
        cancelled: Arc::clone(&cancelled),
        chunks: rx.clone(),
        buffers: buffers.clone(),
    };
    let cancellation = Arc::clone(&cancelled);
    let work = workers
        .execute(move || {
            let mut batch = None::<PcmBatcher>;
            let result = rivu_audio::render_stream(
                &snapshot,
                config.media_read_buffer_mib,
                config.ffmpeg_enabled,
                &cancellation,
                |event| {
                    match event {
                        RenderEvent::Start(format) => {
                            let batcher = PcmBatcher::new(
                                format.channels() as usize,
                                tx.clone(),
                                available.clone(),
                                Arc::clone(&cancellation),
                            )?;
                            for _ in 0..PCM_BUFFERS {
                                initial_buffers.send_blocking(Vec::<f32>::with_capacity(
                                    batcher.sample_limit,
                                ))?;
                            }
                            batch = Some(batcher);
                            tx.send_blocking(Response::Start {
                                source,
                                identity: format.source_identity,
                                sample_rate: format.sample_rate,
                                layout: format
                                    .layout
                                    .iter()
                                    .map(|channel| channel.standard_index())
                                    .collect(),
                                duration: format.duration,
                            })?;
                        }
                        RenderEvent::Pcm {
                            media_start,
                            media_step,
                            samples,
                        } => {
                            batch
                                .as_mut()
                                .context("Audio renderer omitted start metadata")?
                                .push(media_start, media_step, samples)?;
                        }
                        RenderEvent::Complete { frames, duration } => {
                            if let Some(batch) = &mut batch {
                                batch.flush()?;
                            }
                            tx.send_blocking(Response::Complete { frames, duration })?
                        }
                    }
                    Ok(())
                },
            );
            // A decoder failure can leave a valid partial packet. Preserve it
            // ahead of Error; cancellation instead drops it and unblocks waits.
            if result.is_err()
                && !cancellation.load(Ordering::Acquire)
                && let Some(batch) = &mut batch
            {
                batch.flush()?;
            }
            result
        })
        .fuse();
    futures::pin_mut!(work);
    let mut result = None;
    let mut extra = [0; 1];
    loop {
        if result.is_some() && rx.is_empty() {
            break;
        }
        enum Event {
            Stop,
            Work(Result<Result<()>>),
            Chunk(std::result::Result<Response, async_channel::RecvError>),
        }
        let event = futures::select! {
            _ = stopped.recv().fuse() => Event::Stop,
            _ = reader.read(&mut extra).fuse() => Event::Stop,
            done = work => Event::Work(done),
            chunk = rx.recv().fuse() => Event::Chunk(chunk),
        };
        match event {
            Event::Stop => {
                cancelled.store(true, Ordering::Release);
                rx.close();
                buffers.close();
                if result.is_none() {
                    let _ = work.await;
                }
                return Ok(());
            }
            Event::Work(done) => result = Some(done.and_then(|result| result)),
            Event::Chunk(Ok(chunk)) => {
                let written = {
                    let sending = send(writer, &chunk, bytes).fuse();
                    futures::pin_mut!(sending);
                    futures::select! {
                        _ = stopped.recv().fuse() => None,
                        _ = reader.read(&mut extra).fuse() => None,
                        result = sending => Some(result),
                    }
                };
                if !matches!(written, Some(Ok(()))) {
                    cancelled.store(true, Ordering::Release);
                    rx.close();
                    buffers.close();
                    if result.is_none() {
                        let _ = work.await;
                    }
                    return written.unwrap_or(Ok(()));
                }
                if let Response::Pcm { mut samples, .. } = chunk {
                    samples.clear();
                    let _ = buffers.try_send(samples);
                }
            }
            Event::Chunk(Err(_)) => {
                if result.is_none() {
                    result = Some(work.await.and_then(|result| result));
                }
                break;
            }
        }
    }
    if let Some(Err(error)) = result {
        send(writer, &Response::Error(format!("{error:#}")), bytes).await?;
    }
    Ok(())
}
struct CancelOnDrop {
    cancelled: Arc<AtomicBool>,
    chunks: Receiver<Response>,
    buffers: Sender<Vec<f32>>,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.chunks.close();
        self.buffers.close();
    }
}

#[cfg(test)]
#[path = "audio_tests.rs"]
mod tests;
