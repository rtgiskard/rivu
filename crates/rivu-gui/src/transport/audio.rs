//! Generic bounded finite PCM consumer; display analysis belongs to callers.
use anyhow::{Result, bail, ensure};
use futures::{FutureExt, channel::oneshot};
use rivu_format::config::VisualizationSignal;
use rivu_protocol::{
    audio::{self, ExpectedSource, IO_TIMEOUT, PlaybackRange, Request, Response, VERSION},
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
    mut consume: impl FnMut(Response) -> Result<()>,
) -> Result<()> {
    let operation = async {
        let mut stream = timeout(IO_TIMEOUT, audio::connect(&data_dir)).await??;
        let mut bytes = Vec::new();
        audio::encode_frame_into(
            &Request::Hello {
                version: VERSION,
                instance,
            },
            &mut bytes,
        )?;
        audio::write_frame(&mut stream, &mut bytes).await?;
        match timeout(IO_TIMEOUT, audio::read_frame(&mut stream, &mut bytes)).await?? {
            Response::Hello {
                version: VERSION,
                instance: actual,
            } if actual == instance => (),
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
            let response = audio::read_frame(&mut stream, &mut bytes).await?;
            match &response {
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

    #[test]
    fn cancellation_closes_dedicated_finite_connection() {
        if rivu::test_support::run_in_subprocess(
            "transport::audio::tests::cancellation_closes_dedicated_finite_connection",
            Duration::from_secs(15),
        ) {
            return;
        }
        let directory = rivu::test_support::ipc::socket_test_directory();
        let listener = UnixListener::bind_addr(&audio::address(directory.path()).unwrap()).unwrap();
        let (started, ready) = async_channel::bounded(1);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            hello(&mut stream);
            started.send_blocking(()).unwrap();
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        });
        let expected =
            ExpectedSource::from(&crate::test_support::artwork_track("source.wav".into()));
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
    fn source_echo_is_checked_before_start_callback() {
        if rivu::test_support::run_in_subprocess(
            "transport::audio::tests::source_echo_is_checked_before_start_callback",
            Duration::from_secs(15),
        ) {
            return;
        }
        let directory = rivu::test_support::ipc::socket_test_directory();
        let listener = UnixListener::bind_addr(&audio::address(directory.path()).unwrap()).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            hello(&mut stream);
            send(
                &mut stream,
                Response::Start {
                    source: Some(2),
                    identity: audio::SourceIdentity::Pcm {
                        storage: 1,
                        start: 0,
                        frames: 1,
                    },
                    sample_rate: 1,
                    layout: vec![0, 1],
                    duration: Some(1.0),
                },
            );
        });
        let expected =
            ExpectedSource::from(&crate::test_support::artwork_track("source.wav".into()));
        let (_cancel, cancelled) = oneshot::channel();
        let mut callbacks = 0;
        let result = async_io::block_on(read_stream(
            directory.path().to_owned(),
            7,
            expected,
            Some(1),
            VisualizationSignal::Before,
            None,
            cancelled,
            |_| {
                callbacks += 1;
                Ok(())
            },
        ));
        assert!(result.is_err());
        assert_eq!(callbacks, 0);
        server.join().unwrap();
    }

    #[test]
    fn invalid_native_layout_is_rejected_before_start_callback() {
        if rivu::test_support::run_in_subprocess(
            "transport::audio::tests::invalid_native_layout_is_rejected_before_start_callback",
            Duration::from_secs(15),
        ) {
            return;
        }
        for layout in [vec![], vec![0, 0], vec![18], vec![0; 19]] {
            let directory = rivu::test_support::ipc::socket_test_directory();
            let listener =
                UnixListener::bind_addr(&audio::address(directory.path()).unwrap()).unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                hello(&mut stream);
                send(
                    &mut stream,
                    Response::Start {
                        source: None,
                        identity: audio::SourceIdentity::Pcm {
                            storage: 1,
                            start: 0,
                            frames: 1,
                        },
                        sample_rate: 48_000,
                        layout,
                        duration: Some(1.0),
                    },
                );
            });
            let expected =
                ExpectedSource::from(&crate::test_support::artwork_track("source.wav".into()));
            let (_cancel, cancelled) = oneshot::channel();
            let mut callbacks = 0;
            let result = async_io::block_on(read_stream(
                directory.path().to_owned(),
                7,
                expected,
                None,
                VisualizationSignal::Before,
                None,
                cancelled,
                |_| {
                    callbacks += 1;
                    Ok(())
                },
            ));
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Invalid PCM stream format")
            );
            assert_eq!(callbacks, 0);
            server.join().unwrap();
        }
    }
}
