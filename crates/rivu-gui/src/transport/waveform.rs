//! Independent waveform client; authoritative scanning remains in Core.
use anyhow::{Result, bail, ensure};
use async_channel::Sender;
use futures::{FutureExt, channel::oneshot};
use parking_lot::{Mutex, RwLock};
use rivu_protocol::waveform::WaveformFrame;
use rivu_protocol::{
    transport::{release_large_buffer, timeout},
    waveform::{
        ExpectedSource, IO_TIMEOUT, MAX_FRAME, Request, VERSION, address, connect, decode,
        encode_frame_into, read_frame, write_frame,
    },
};
use std::{
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};
type Response = rivu_protocol::waveform::Response<Box<WaveformFrame>>;
type Wakeup = Arc<dyn Fn() + Send + Sync>;
type ErrorCallback = Arc<dyn Fn(String) + Send + Sync>;
fn exchange(stream: &mut UnixStream, request: &Request, bytes: &mut Vec<u8>) -> Result<Response> {
    encode_frame_into(request, bytes)?;
    stream.write_all(bytes)?;
    bytes.clear();
    release_large_buffer(bytes);
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix)?;
    let length = u32::from_le_bytes(prefix) as usize;
    ensure!(
        length <= MAX_FRAME,
        "Waveform response exceeds its byte limit"
    );
    if length > bytes.capacity() {
        bytes.reserve_exact(length);
    }
    bytes.resize(length, 0);
    stream.read_exact(bytes)?;
    let response = decode(bytes);
    bytes.clear();
    release_large_buffer(bytes);
    let response = response?;
    if let Response::Error(error) = response {
        bail!(error);
    }
    Ok(response)
}
fn validate_hello(response: Response, expected: u32) -> Result<()> {
    match response {
        Response::Hello {
            version: VERSION,
            instance,
        } if instance == expected => Ok(()),
        _ => bail!("Waveform transport belongs to a different instance or version"),
    }
}
/// A progressive-envelope reader. Refresh requests coalesce; idle readers never poll.
pub(crate) struct WaveClient {
    pub(crate) waveform: Arc<RwLock<WaveformFrame>>,
    demand: Sender<()>,
    enabled: AtomicBool,
    stream: Arc<Mutex<Option<UnixStream>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    wakeup: Arc<RwLock<Option<Wakeup>>>,
    error: Arc<Mutex<Option<String>>>,
    error_callback: Arc<RwLock<Option<ErrorCallback>>>,
}
impl WaveClient {
    pub(crate) fn connect(data_dir: &Path, instance: u32) -> Result<Self> {
        let mut stream = UnixStream::connect_addr(&address(data_dir)?)?;
        rivu_protocol::wire::verify_peer(&stream)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        let mut bytes = Vec::new();
        validate_hello(
            exchange(
                &mut stream,
                &Request::Hello {
                    version: VERSION,
                    instance,
                },
                &mut bytes,
            )?,
            instance,
        )?;
        let control = Arc::new(Mutex::new(Some(stream.try_clone()?)));
        let waveform = Arc::new(RwLock::new(WaveformFrame::default()));
        let wakeup = Arc::new(RwLock::new(None::<Wakeup>));
        let error = Arc::new(Mutex::new(None));
        let error_callback = Arc::new(RwLock::new(None::<ErrorCallback>));
        let (demand, requests) = async_channel::bounded(1);
        let output = Arc::clone(&waveform);
        let notify = Arc::clone(&wakeup);
        let failure = Arc::clone(&error);
        let on_error = Arc::clone(&error_callback);
        let worker = thread::Builder::new()
            .name("rivu-waveform-client".into())
            .spawn(move || {
                let mut known = None;
                while requests.recv_blocking().is_ok() {
                    let result = exchange(&mut stream, &Request::Progressive { known }, &mut bytes);
                    match result {
                        Ok(Response::Progressive { key, frame }) => {
                            known = Some(key);
                            if let Some(frame) = frame {
                                let retired = std::mem::replace(&mut *output.write(), *frame);
                                drop(retired);
                                let notify = notify.read().clone();
                                if let Some(notify) = notify {
                                    notify();
                                }
                            }
                        }
                        result => {
                            if requests.is_closed() {
                                break;
                            }
                            let message = match result {
                                Err(error) => format!("Waveform transport failed: {error:#}"),
                                _ => "Unexpected waveform response".into(),
                            };
                            *failure.lock() = Some(message.clone());
                            let callback = on_error.read().clone();
                            if let Some(callback) = callback {
                                callback(message);
                            }
                            break;
                        }
                    }
                }
            })?;
        let client = Self {
            waveform,
            demand,
            enabled: AtomicBool::new(false),
            stream: control,
            worker: Mutex::new(Some(worker)),
            wakeup,
            error,
            error_callback,
        };
        Ok(client)
    }
    pub(crate) fn set_enabled(&self, enabled: bool) {
        if self.enabled.swap(enabled, Ordering::AcqRel) != enabled && enabled {
            // Reacquire even while paused: no PCM or business publication may follow.
            self.refresh();
        }
    }
    pub(crate) fn refresh(&self) {
        if self.enabled.load(Ordering::Acquire) {
            let _ = self.demand.try_send(());
        }
    }
    pub(crate) fn set_wakeup(&self, callback: Wakeup) {
        *self.wakeup.write() = Some(callback);
    }
    pub(crate) fn set_error_callback(&self, callback: ErrorCallback) {
        *self.error_callback.write() = Some(Arc::clone(&callback));
        if let Some(error) = self.error.lock().clone() {
            callback(error);
        }
    }
    pub(crate) fn stop(&self) {
        self.demand.close();
        if let Some(stream) = self.stream.lock().take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Some(worker) = self.worker.lock().take() {
            let _ = worker.join();
        }
    }
}
impl Drop for WaveClient {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) async fn scan_full(
    data_dir: PathBuf,
    instance: u32,
    expected: ExpectedSource,
    source: Option<u64>,
    cancelled: oneshot::Receiver<()>,
) -> Result<WaveformFrame> {
    let operation = async {
        let mut stream = timeout(IO_TIMEOUT, connect(&data_dir)).await??;
        let mut bytes = Vec::new();
        encode_frame_into(
            &Request::Hello {
                version: VERSION,
                instance,
            },
            &mut bytes,
        )?;
        write_frame(&mut stream, &mut bytes).await?;
        validate_hello(
            timeout(IO_TIMEOUT, read_frame(&mut stream, &mut bytes)).await??,
            instance,
        )?;
        encode_frame_into(&Request::Full { expected, source }, &mut bytes)?;
        write_frame(&mut stream, &mut bytes).await?;
        match read_frame(&mut stream, &mut bytes).await? {
            Response::Full(frame) => Ok(*frame),
            Response::Error(error) => bail!(error),
            _ => bail!("Unexpected full waveform response"),
        }
    }
    .fuse();
    futures::pin_mut!(operation);
    futures::select_biased! {
        _ = cancelled.fuse() => bail!("Full waveform request cancelled"),
        result = operation => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivu_protocol::waveform::WaveKey;
    use serde::de::DeserializeOwned;
    use std::os::unix::net::UnixListener;

    fn receive<T: DeserializeOwned>(stream: &mut UnixStream) -> T {
        let mut prefix = [0; 4];
        stream.read_exact(&mut prefix).unwrap();
        let mut bytes = vec![0; u32::from_le_bytes(prefix) as usize];
        stream.read_exact(&mut bytes).unwrap();
        decode(&bytes).unwrap()
    }

    fn respond(stream: &mut UnixStream, response: Response) {
        let mut bytes = Vec::new();
        encode_frame_into(&response, &mut bytes).unwrap();
        stream.write_all(&bytes).unwrap();
    }

    #[test]
    fn cancelling_a_full_scan_closes_its_dedicated_connection() {
        let directory = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind_addr(&address(directory.path()).unwrap()).unwrap();
        let expected =
            ExpectedSource::from(&crate::test_support::artwork_track("source.wav".into()));
        let (started, captured) = async_channel::bounded(1);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
            assert!(matches!(receive(&mut stream), Request::Hello { .. }));
            respond(
                &mut stream,
                Response::Hello {
                    version: VERSION,
                    instance: 7,
                },
            );
            assert!(matches!(receive(&mut stream), Request::Full { .. }));
            started.send_blocking(()).unwrap();
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        });
        let (cancel, cancelled) = oneshot::channel();
        let result = async_io::block_on(async {
            let cancel = async {
                captured.recv().await.unwrap();
                cancel.send(()).unwrap();
            };
            let (result, ()) = futures::join!(
                scan_full(directory.path().to_owned(), 7, expected, None, cancelled),
                cancel,
            );
            result
        });
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        server.join().unwrap();
    }

    #[test]
    fn full_scan_sends_only_source_expectations_and_receives_the_bound_frame() {
        let directory = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind_addr(&address(directory.path()).unwrap()).unwrap();
        let mut track = crate::test_support::artwork_track("source.wav".into());
        track.title = "metadata is not scan input".repeat(MAX_FRAME / 16);
        let expected = ExpectedSource::from(&track);
        let requested = expected.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
            assert!(matches!(
                receive(&mut stream),
                Request::Hello {
                    version: VERSION,
                    instance: 7
                }
            ));
            respond(
                &mut stream,
                Response::Hello {
                    version: VERSION,
                    instance: 7,
                },
            );
            let Request::Full { expected, source } = receive(&mut stream) else {
                panic!("expected full scan request");
            };
            assert_eq!(expected, requested);
            assert_eq!(source, Some(17));
            let mut frame = WaveformFrame::default();
            frame.revision = 29;
            frame.complete = true;
            frame.path = Some(expected.path);
            respond(&mut stream, Response::Full(Box::new(frame)));
        });
        let (_cancel, cancelled) = oneshot::channel();
        let frame = async_io::block_on(scan_full(
            directory.path().to_owned(),
            7,
            expected,
            Some(17),
            cancelled,
        ))
        .unwrap();
        assert_eq!(frame.revision, 29);
        assert!(frame.complete);
        assert_eq!(frame.path, Some(track.path));
        server.join().unwrap();
    }

    #[test]
    fn source_expectations_ignore_display_edits_but_reject_source_changes() {
        let original = crate::test_support::artwork_track("source.wav".into());
        let expected = ExpectedSource::from(&original);
        let mut changed = original.clone();
        changed.title = "renamed".into();
        changed.duration = Some(37.0);
        assert!(expected.matches(&changed));
        changed.id += 1;
        assert!(!expected.matches(&changed));
        changed = original.clone();
        changed.path = "replacement.wav".into();
        assert!(!expected.matches(&changed));
        changed = original.clone();
        changed.fingerprint = None;
        assert!(!expected.matches(&changed));
        changed = original.clone();
        changed.decoder_hint = rivu_format::decoder_hint::DecoderHint::Ffmpeg;
        assert!(!expected.matches(&changed));
        changed = original;
        changed.cue = Some(rivu_protocol::model::CueSegment {
            sheet: "source.cue".into(),
            number: 1,
            start_frame: 75,
            end_frame: Some(150),
        });
        assert!(!expected.matches(&changed));
    }

    #[test]
    fn blocking_exchange_rejects_oversized_and_trailing_responses() {
        for oversized in [true, false] {
            let (mut stream, mut peer) = UnixStream::pair().unwrap();
            stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
            let server = thread::spawn(move || {
                assert!(matches!(receive(&mut peer), Request::Progressive { .. }));
                if oversized {
                    peer.write_all(&((MAX_FRAME + 1) as u32).to_le_bytes())
                        .unwrap();
                } else {
                    let mut bytes = Vec::new();
                    encode_frame_into(
                        &Response::Hello {
                            version: VERSION,
                            instance: 7,
                        },
                        &mut bytes,
                    )
                    .unwrap();
                    bytes.push(0);
                    let length = (bytes.len() - 4) as u32;
                    bytes[..4].copy_from_slice(&length.to_le_bytes());
                    peer.write_all(&bytes).unwrap();
                }
            });
            assert!(
                exchange(
                    &mut stream,
                    &Request::Progressive { known: None },
                    &mut Vec::new()
                )
                .is_err()
            );
            server.join().unwrap();
        }
    }

    #[test]
    fn showing_waveform_reacquires_resumed_snapshot_without_new_pcm() {
        let directory = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind_addr(&address(directory.path()).unwrap()).unwrap();
        let frame = Arc::new(RwLock::new(WaveformFrame::default()));
        let control = rivu_audio::WaveformControl::default();
        control.set_visualization_enabled(&mut frame.write(), false);
        let server_frame = Arc::clone(&frame);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
            assert!(matches!(
                receive(&mut stream),
                Request::Hello {
                    version: VERSION,
                    instance: 7
                }
            ));
            respond(
                &mut stream,
                Response::Hello {
                    version: VERSION,
                    instance: 7,
                },
            );
            for _ in 0..3 {
                let Request::Progressive { known } = receive(&mut stream) else {
                    panic!("expected progressive request");
                };
                let frame = server_frame.read();
                let key = WaveKey {
                    source: 1,
                    revision: frame.revision,
                    suspension: frame.suspension_generation(),
                    suspended: frame.is_suspended(),
                };
                respond(
                    &mut stream,
                    Response::Progressive {
                        key,
                        frame: (known != Some(key)).then(|| Box::new(frame.clone())),
                    },
                );
            }
        });
        let client = WaveClient::connect(directory.path(), 7).unwrap();
        let (published, received) = std::sync::mpsc::sync_channel(3);
        client.set_wakeup(Arc::new(move || published.send(()).unwrap()));
        client.set_enabled(true);
        received.recv_timeout(IO_TIMEOUT).unwrap();
        assert!(client.waveform.read().is_suspended());

        client.set_enabled(false);
        let revision = frame.read().revision;
        control.set_visualization_enabled(&mut frame.write(), true);
        assert_eq!(frame.read().revision, revision);
        client.set_enabled(true);
        received.recv_timeout(IO_TIMEOUT).unwrap();
        let scan = Arc::new(AtomicBool::new(false));
        control.cancel_scan_on_suspend(&frame.read(), &scan);
        assert!(!scan.load(Ordering::Relaxed));
        assert!(!client.waveform.read().is_suspended());

        client.set_enabled(false);
        control.set_visualization_enabled(&mut frame.write(), false);
        client.set_enabled(true);
        received.recv_timeout(IO_TIMEOUT).unwrap();
        assert!(client.waveform.read().is_suspended());
        assert!(scan.load(Ordering::Relaxed));
        client.stop();
        server.join().unwrap();
    }
}
