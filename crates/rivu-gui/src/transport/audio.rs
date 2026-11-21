//! Generic bounded finite PCM consumer; display analysis belongs to callers.
use anyhow::{Result, bail, ensure};
use futures::{FutureExt, channel::oneshot};
use rivu_format::config::VisualizationSignal;
use rivu_protocol::{
    audio::{
        self, ExpectedSource, IO_TIMEOUT, PlaybackRange, Request, Response, ResponseDecoder,
        VERSION,
    },
    transport::timeout,
};
use std::path::PathBuf;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn read_stream(
    data_dir: PathBuf,
    instance: u32,
    expected: ExpectedSource,
    source: Option<u64>,
    signal: VisualizationSignal,
    range: Option<PlaybackRange>,
    cancelled: oneshot::Receiver<()>,
    mut consume: impl FnMut(&Response) -> Result<()>,
) -> Result<()> {
    let operation = async {
        let mut stream = timeout(IO_TIMEOUT, audio::connect(&data_dir)).await??;
        let mut bytes = Vec::new();
        let mut decoder = ResponseDecoder::default();
        audio::encode_frame_into(
            &Request::Hello {
                version: VERSION,
                instance,
            },
            &mut bytes,
        )?;
        audio::write_frame(&mut stream, &mut bytes).await?;
        match timeout(IO_TIMEOUT, decoder.read_frame(&mut stream, &mut bytes)).await?? {
            Response::Hello {
                version: VERSION,
                instance: actual,
            } if *actual == instance => (),
            _ => bail!("Audio transport belongs to a different instance or version"),
        }
        audio::encode_frame_into(
            &Request::Read {
                expected,
                source,
                signal,
                range,
            },
            &mut bytes,
        )?;
        audio::write_frame(&mut stream, &mut bytes).await?;
        let mut started = false;
        let mut sequence = 1_u64;
        let mut frames = 0_u64;
        let mut channels = 0;
        loop {
            let response = decoder.read_frame(&mut stream, &mut bytes).await?;
            match response {
                Response::Start {
                    source: actual,
                    sample_rate,
                    layout,
                    duration,
                    ..
                } => {
                    ensure!(!started, "Duplicate PCM stream start");
                    ensure!(
                        source.is_none() || *actual == source,
                        "PCM source changed before capture"
                    );
                    ensure!(
                        *sample_rate != 0 && rivu_protocol::pcm::valid_layout(layout),
                        "Invalid PCM stream format"
                    );
                    ensure!(
                        duration.is_none_or(|duration| duration.is_finite() && duration >= 0.0),
                        "Invalid PCM duration"
                    );
                    started = true;
                    channels = layout.len();
                }
                Response::Pcm {
                    sequence: next,
                    media_start,
                    media_step,
                    samples,
                } => {
                    ensure!(started && *next == sequence, "PCM stream sequence mismatch");
                    ensure!(
                        media_start.is_finite() && media_step.is_finite() && *media_step > 0.0,
                        "Invalid PCM media mapping"
                    );
                    ensure!(
                        samples.len().is_multiple_of(channels) && !samples.is_empty(),
                        "Invalid PCM sample count"
                    );
                    ensure!(
                        samples.iter().all(|sample| sample.is_finite()),
                        "Nonfinite PCM sample"
                    );
                    sequence = sequence
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("PCM sequence overflow"))?;
                    frames = frames
                        .checked_add((samples.len() / channels) as u64)
                        .ok_or_else(|| anyhow::anyhow!("PCM frame count overflow"))?;
                }
                Response::Complete {
                    frames: complete,
                    duration,
                } => {
                    ensure!(
                        started && *complete == frames && duration.is_finite() && *duration >= 0.0,
                        "Invalid PCM completion"
                    );
                    consume(response)?;
                    return Ok(());
                }
                Response::Error(error) => bail!("{error}"),
                Response::Hello { .. } => bail!("Unexpected PCM handshake"),
            }
            consume(response)?;
            futures_lite::future::yield_now().await;
        }
    }
    .fuse();
    futures::pin_mut!(operation);
    futures::select_biased! {
        _ = cancelled.fuse() => bail!("PCM request cancelled"),
        result = operation => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::net::{UnixListener, UnixStream},
        thread,
        time::Duration,
    };

    fn receive(stream: &mut UnixStream) -> Request {
        let mut prefix = [0; 4];
        stream.read_exact(&mut prefix).unwrap();
        let mut bytes = vec![0; u32::from_le_bytes(prefix) as usize];
        stream.read_exact(&mut bytes).unwrap();
        audio::decode(&bytes).unwrap()
    }

    fn send(stream: &mut UnixStream, response: Response) {
        let mut bytes = Vec::new();
        audio::encode_frame_into(&response, &mut bytes).unwrap();
        stream.write_all(&bytes).unwrap();
    }

    fn hello(stream: &mut UnixStream) {
        stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        assert!(matches!(
            receive(stream),
            Request::Hello {
                version: VERSION,
                instance: 7
            }
        ));
        send(
            stream,
            Response::Hello {
                version: VERSION,
                instance: 7,
            },
        );
        assert!(matches!(receive(stream), Request::Read { .. }));
    }

    fn endpoint() -> (rivu::test_support::ipc::SocketTestDirectory, UnixListener) {
        let directory = rivu::test_support::ipc::socket_test_directory();
        let listener = UnixListener::bind_addr(&audio::address(directory.path()).unwrap()).unwrap();
        (directory, listener)
    }

    fn start(source: Option<u64>, layout: Vec<u32>) -> Response {
        Response::Start {
            source,
            identity: audio::SourceIdentity::Pcm {
                storage: 1,
                start: 0,
                frames: 7,
            },
            sample_rate: 8,
            layout,
            duration: Some(0.875),
        }
    }

    #[test]
    fn borrowed_chunks_preserve_mapping_and_reject_interleaved_handshakes() {
        if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
            return;
        }
        for interleaved_handshake in [false, true] {
            let (directory, listener) = endpoint();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                hello(&mut stream);
                send(&mut stream, start(None, vec![0, 1]));
                let mut frames = 0;
                for (index, count) in [4, 8, 2].into_iter().enumerate() {
                    send(
                        &mut stream,
                        Response::Pcm {
                            sequence: index as u64 + 1,
                            media_start: frames as f64 / 8.0,
                            media_step: 0.125,
                            samples: vec![index as f32 + 1.0; count],
                        },
                    );
                    frames += count / 2;
                }
                send(
                    &mut stream,
                    if interleaved_handshake {
                        Response::Hello {
                            version: VERSION,
                            instance: 7,
                        }
                    } else {
                        Response::Complete {
                            frames: 7,
                            duration: 0.875,
                        }
                    },
                );
            });
            let expected = ExpectedSource::from(&rivu_protocol::test_support::artwork_track(
                "source.wav".into(),
            ));
            let (_cancel, cancelled) = oneshot::channel();
            let mut callbacks = 0;
            let mut received_frames = 0;
            let result = async_io::block_on(read_stream(
                directory.path().to_owned(),
                7,
                expected,
                None,
                VisualizationSignal::Before,
                None,
                cancelled,
                |response| {
                    match response {
                        Response::Start { layout, .. } => assert_eq!(layout, &[0, 1]),
                        Response::Pcm {
                            sequence,
                            media_start,
                            media_step,
                            samples,
                        } => {
                            assert_eq!(*sequence, callbacks);
                            assert_eq!(*media_start, received_frames as f64 / 8.0);
                            assert_eq!(*media_step, 0.125);
                            assert!(samples.iter().all(|sample| *sample == *sequence as f32));
                            assert_eq!(samples.len(), [4, 8, 2][callbacks as usize - 1]);
                            received_frames += samples.len() / 2;
                        }
                        Response::Complete { frames, duration } => {
                            assert_eq!(*frames, received_frames as u64);
                            assert_eq!(*duration, 0.875);
                        }
                        _ => panic!("control/error response reached the consumer"),
                    }
                    callbacks += 1;
                    Ok(())
                },
            ));
            if interleaved_handshake {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("Unexpected PCM handshake")
                );
                assert_eq!(callbacks, 4);
            } else {
                result.unwrap();
                assert_eq!(callbacks, 5);
            }
            assert_eq!(received_frames, 7);
            server.join().unwrap();
        }
    }

    #[test]
    fn cancellation_closes_dedicated_finite_connection() {
        if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
            return;
        }
        let (directory, listener) = endpoint();
        let (started, ready) = async_channel::bounded(1);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            hello(&mut stream);
            started.send_blocking(()).unwrap();
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        });
        let expected = ExpectedSource::from(&rivu_protocol::test_support::artwork_track(
            "source.wav".into(),
        ));
        let (cancel, cancelled) = oneshot::channel();
        let result = async_io::block_on(async {
            let cancel = async {
                ready.recv().await.unwrap();
                cancel.send(()).unwrap();
            };
            let (result, ()) = futures::join!(
                read_stream(
                    directory.path().to_owned(),
                    7,
                    expected,
                    None,
                    VisualizationSignal::Before,
                    None,
                    cancelled,
                    |_| Ok(())
                ),
                cancel
            );
            result
        });
        assert!(result.is_err());
        server.join().unwrap();
    }

    #[test]
    fn rejected_start_checks_source_echo_and_native_layout_before_callback() {
        if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
            return;
        }
        let invalid_format = "Invalid PCM stream format";
        for (requested, response, error) in [
            (
                Some(1),
                start(Some(2), vec![0, 1]),
                "PCM source changed before capture",
            ),
            (None, start(None, vec![]), invalid_format),
            (None, start(None, vec![0, 0]), invalid_format),
            (None, start(None, vec![18]), invalid_format),
            (None, start(None, vec![0; 19]), invalid_format),
        ] {
            let (directory, listener) = endpoint();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                hello(&mut stream);
                send(&mut stream, response);
            });
            let expected = ExpectedSource::from(&rivu_protocol::test_support::artwork_track(
                "source.wav".into(),
            ));
            let (_cancel, cancelled) = oneshot::channel();
            let mut callbacks = 0;
            let result = async_io::block_on(read_stream(
                directory.path().to_owned(),
                7,
                expected,
                requested,
                VisualizationSignal::Before,
                None,
                cancelled,
                |_| {
                    callbacks += 1;
                    Ok(())
                },
            ));
            let reason = result.unwrap_err().to_string();
            assert!(reason.contains(error), "expected {error}, got {reason}");
            assert_eq!(callbacks, 0);
            server.join().unwrap();
        }
    }
}
