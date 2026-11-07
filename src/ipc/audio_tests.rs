use super::*;
use rivu_format::{config::VisualizationSignal, decoder_hint::DecoderHint};
use rivu_protocol::{
    model::{CueSegment, Track},
    response::QueryResponse,
};

async fn request(stream: &mut Async<UnixStream>, request: &Request, bytes: &mut Vec<u8>) {
    encode_frame_into(request, bytes).unwrap();
    write_frame(stream, bytes).await.unwrap();
}
async fn connect(directory: &Path) -> (Async<UnixStream>, Vec<u8>) {
    let mut stream = rivu_protocol::audio::connect(directory).await.unwrap();
    let mut bytes = Vec::new();
    request(
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
    (stream, bytes)
}
fn track(path: std::path::PathBuf) -> Track {
    Track {
        id: 9,
        path,
        fingerprint: None,
        cue: Some(CueSegment {
            sheet: "source.cue".into(),
            number: 1,
            start_frame: 75,
            end_frame: Some(150),
        }),
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        duration: Some(1.0),
        codec: "pcm".into(),
        channels: 2,
        sample_rate: 8_000,
        bitrate_bps: None,
        bitrate_exact: false,
        track_number: None,
        disc_number: None,
        bits_per_sample: Some(32),
        release_date: None,
        favorite: false,
        decoder_hint: DecoderHint::Native,
        missing: false,
        play_count: 0,
        last_played: None,
    }
}

#[test]
fn finite_pcm_preserves_cue_relative_positions_and_lossless_requested_ranges() {
    let directory = crate::test_support::ipc::socket_test_directory();
    let path = directory.path().join("source.wav");
    let mut writer = hound::WavWriter::create(
        &path,
        hound::WavSpec {
            channels: 2,
            sample_rate: 8_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .unwrap();
    for frame in 0..24_000 {
        writer.write_sample(frame as f32 / 32_768.0).unwrap();
        writer.write_sample(-(frame as f32) / 65_536.0).unwrap();
    }
    writer.finalize().unwrap();
    let track = Arc::new(track(path));
    let (handle, queries) = CoreHandle::test_query_port();
    let tasks = crate::execution::TaskRuntime::start().unwrap();
    let service =
        AudioServer::start(directory.path(), handle, 7, &tasks.handle, &tasks.workers).unwrap();
    async_io::block_on(timeout(Duration::from_secs(10), async {
        for range in [None, Some(PlaybackRange { start_seconds: 0.25, end_seconds: Some(0.75) })] {
            let (mut stream, mut bytes) = connect(directory.path()).await;
            request(&mut stream, &Request::Read { expected: ExpectedSource::from(track.as_ref()), source: None, signal: VisualizationSignal::Before, range }, &mut bytes).await;
            let authority = async {
                queries.receive().await.respond(QueryResponse { revisions: Default::default(), result: Ok(ViewResponse::Track(Some(track.as_ref().clone()))) });
            };
            let receive = async {
                let start = read_frame::<Response>(&mut stream, &mut bytes).await.unwrap();
                assert!(matches!(start, Response::Start { source: None, sample_rate: 8_000, duration: Some(1.0), ref layout, .. } if layout.len() == 2));
                let first = if range.is_some() { 2_000 } else { 0 };
                let end = if range.is_some() { 6_000 } else { 8_000 };
                let mut position = first;
                let mut sequence = 0;
                loop {
                    match read_frame::<Response>(&mut stream, &mut bytes).await.unwrap() {
                        Response::Pcm { sequence: next, media_start, media_step, samples } => {
                            sequence += 1;
                            assert_eq!(next, sequence);
                            assert_eq!(media_step, 1.0 / 8_000.0);
                            assert!((media_start - position as f64 / 8_000.0).abs() < 1e-9);
                            assert!(samples.len().is_multiple_of(2));
                            assert!(samples.len() <= CHUNK_FRAMES * 2);
                            for &sample in samples.as_chunks::<2>().0 {
                                let original_frame = 8_000 + position;
                                assert_eq!(sample, [original_frame as f32 / 32_768.0, -(original_frame as f32) / 65_536.0]);
                                position += 1;
                            }
                        },
                        Response::Complete { frames, duration } => {
                            assert_eq!(position, end);
                            assert_eq!(frames, end - first);
                            assert_eq!(duration, 1.0);
                            break;
                        },
                        other => panic!("Unexpected PCM stream response: {other:?}"),
                    }
                }
            };
            futures::join!(authority, receive);
        }
    })).unwrap();
    drop(service);
}

#[test]
fn finite_stream_rejects_wrong_authority_and_unbound_connections() {
    let directory = crate::test_support::ipc::socket_test_directory();
    let (handle, queries) = CoreHandle::test_query_port();
    let tasks = crate::execution::TaskRuntime::start().unwrap();
    let service =
        AudioServer::start(directory.path(), handle, 7, &tasks.handle, &tasks.workers).unwrap();
    async_io::block_on(timeout(Duration::from_secs(5), async {
        for (version, instance) in [(VERSION + 1, 7), (VERSION, 8)] {
            let mut stream = rivu_protocol::audio::connect(directory.path())
                .await
                .unwrap();
            let mut bytes = Vec::new();
            request(
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
        let (mut stream, mut bytes) = connect(directory.path()).await;
        request(
            &mut stream,
            &Request::Read {
                expected: ExpectedSource::from(&track("missing.wav".into())),
                source: None,
                signal: VisualizationSignal::After,
                range: None,
            },
            &mut bytes,
        )
        .await;
        let authority = async {
            queries.receive().await.respond(QueryResponse {
                revisions: Default::default(),
                result: Ok(ViewResponse::Track(None)),
            });
        };
        let (result, ()) =
            futures::join!(read_frame::<Response>(&mut stream, &mut bytes), authority);
        assert!(matches!(result.unwrap(), Response::Error(_)));
    }))
    .unwrap();
    drop(service);
}

#[test]
fn cancellation_retires_a_backpressured_renderer_before_service_shutdown_returns() {
    if crate::test_support::run_in_subprocess(
        "ipc::audio::tests::cancellation_retires_a_backpressured_renderer_before_service_shutdown_returns",
        Duration::from_secs(15),
    ) {
        return;
    }
    let directory = crate::test_support::ipc::socket_test_directory();
    let path = directory.path().join("long.wav");
    let mut writer = hound::WavWriter::create(
        &path,
        hound::WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .unwrap();
    for _ in 0..800_000 {
        writer.write_sample(0.25_f32).unwrap();
    }
    writer.finalize().unwrap();
    let mut track = track(path);
    track.cue = None;
    track.duration = Some(100.0);
    track.channels = 1;
    let track = Arc::new(track);
    let (handle, queries) = CoreHandle::test_query_port();
    let tasks = crate::execution::TaskRuntime::with_blocking_limit(1).unwrap();
    let service =
        AudioServer::start(directory.path(), handle, 7, &tasks.handle, &tasks.workers).unwrap();
    let stream = async_io::block_on(timeout(Duration::from_secs(5), async {
        let (mut stream, mut bytes) = connect(directory.path()).await;
        request(
            &mut stream,
            &Request::Read {
                expected: ExpectedSource::from(track.as_ref()),
                source: None,
                signal: VisualizationSignal::Before,
                range: None,
            },
            &mut bytes,
        )
        .await;
        let authority = async {
            queries.receive().await.respond(QueryResponse {
                revisions: Default::default(),
                result: Ok(ViewResponse::Track(Some(track.as_ref().clone()))),
            });
        };
        let (result, ()) =
            futures::join!(read_frame::<Response>(&mut stream, &mut bytes), authority);
        assert!(matches!(result.unwrap(), Response::Start { .. }));
        // Leave the dedicated connection unread. Shutdown must release both the
        // socket writer and the synchronous producer's bounded-channel wait.
        stream
    }))
    .unwrap();
    drop(service);
    let available = async_io::block_on(timeout(
        Duration::from_secs(2),
        tasks.workers.execute(|| 41),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(available, 41);
    drop(stream);
}

fn batcher(channels: usize) -> (PcmBatcher, Receiver<Response>) {
    let (chunks, received) = async_channel::bounded(BUFFERED_CHUNKS);
    let (buffers, available) = async_channel::bounded(PCM_BUFFERS);
    let batch = PcmBatcher::new(
        channels,
        chunks,
        available,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    for _ in 0..PCM_BUFFERS {
        buffers
            .send_blocking(Vec::with_capacity(batch.sample_limit))
            .unwrap();
    }
    (batch, received)
}

#[test]
fn batching_preserves_samples_and_partial_tail_across_small_render_events() {
    for channels in [1, 2, 18] {
        let (mut batch, received) = batcher(channels);
        let frames = batch.sample_limit / channels + 17;
        let samples: Vec<_> = (0..frames * channels).map(|i| i as f32 / 32768.0).collect();
        let step = 1.0 / 48_000.0;
        for (index, block) in samples.chunks(37 * channels).enumerate() {
            batch
                .push(2.0 + (index * 37) as f64 * step, step, block)
                .unwrap();
        }
        let Response::Pcm {
            sequence,
            media_start,
            media_step,
            samples: full,
        } = received.try_recv().unwrap()
        else {
            panic!("expected PCM")
        };
        assert_eq!(sequence, 1);
        assert_eq!(media_start, 2.0);
        assert_eq!(media_step, step);
        assert_eq!(full, samples[..batch.sample_limit]);
        assert!(full.len() * size_of::<f32>() <= CHUNK_BYTES);
        assert!(received.try_recv().is_err());
        batch.flush().unwrap();
        let Response::Pcm {
            sequence,
            media_start,
            samples: tail,
            ..
        } = received.try_recv().unwrap()
        else {
            panic!("expected tail")
        };
        assert_eq!(sequence, 2);
        assert!(
            (media_start - (2.0 + (batch.sample_limit / channels) as f64 * step)).abs() < 1e-12
        );
        assert_eq!(tail, samples[batch.sample_limit..]);
        batch.flush().unwrap();
        assert!(received.try_recv().is_err());
    }
}

#[test]
fn batching_splits_single_sample_jumps_and_changed_media_steps() {
    let (mut batch, received) = batcher(2);
    let step = 1.0 / 48_000.0;
    batch.push(0.0, step, &[0.25; 4]).unwrap();
    batch.push(3.0 * step, step, &[0.5; 2]).unwrap();
    let Response::Pcm {
        sequence,
        media_start,
        samples,
        ..
    } = received.try_recv().unwrap()
    else {
        panic!("expected segment")
    };
    assert_eq!((sequence, media_start, samples), (1, 0.0, vec![0.25; 4]));
    batch.push(4.0 * step, 2.0 * step, &[0.75; 4]).unwrap();
    let Response::Pcm {
        sequence,
        media_start,
        media_step,
        samples,
    } = received.try_recv().unwrap()
    else {
        panic!("expected segment")
    };
    assert_eq!(
        (sequence, media_start, media_step, samples),
        (2, 3.0 * step, step, vec![0.5; 2])
    );
    batch.flush().unwrap();
    let Response::Pcm {
        sequence,
        media_start,
        media_step,
        samples,
    } = received.try_recv().unwrap()
    else {
        panic!("expected tail")
    };
    assert_eq!(
        (sequence, media_start, media_step, samples),
        (3, 4.0 * step, 2.0 * step, vec![0.75; 4])
    );
}

#[test]
fn cancellation_discards_an_unsent_partial_packet() {
    let (mut batch, received) = batcher(2);
    batch.push(0.0, 1.0 / 48_000.0, &[0.5; 20]).unwrap();
    batch.cancelled.store(true, Ordering::Release);
    assert!(batch.flush().is_err());
    assert!(received.try_recv().is_err());
}
