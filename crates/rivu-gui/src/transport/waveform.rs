//! Independent waveform client; authoritative scanning remains in Core.
use anyhow::{Result, bail, ensure};
use async_channel::Sender;
use futures::{FutureExt, channel::oneshot};
use parking_lot::{Mutex, RwLock};
use rivu_protocol::waveform::WaveformFrame;
use rivu_protocol::{
    model::Track,
    transport::timeout,
    waveform::{
        IO_TIMEOUT, MAX_FRAME, Request, VERSION, address, connect, decode, encode, read_frame,
        write_frame,
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
type Response = rivu_protocol::waveform::Response<WaveformFrame>;
type Wakeup = Arc<dyn Fn() + Send + Sync>;
type ErrorCallback = Arc<dyn Fn(String) + Send + Sync>;
fn exchange(stream: &mut UnixStream, request: &Request) -> Result<Response> {
    let bytes = encode(request)?;
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    stream.write_all(&bytes)?;
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix)?;
    let length = u32::from_le_bytes(prefix) as usize;
    ensure!(
        length <= MAX_FRAME,
        "Waveform response exceeds its byte limit"
    );
    let mut response = vec![0; length];
    stream.read_exact(&mut response)?;
    let response = decode(&response)?;
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
        validate_hello(
            exchange(
                &mut stream,
                &Request::Hello {
                    version: VERSION,
                    instance,
                },
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
                    let result = exchange(&mut stream, &Request::Progressive { known });
                    match result {
                        Ok(Response::Progressive { key, frame }) => {
                            known = Some(key);
                            if let Some(frame) = frame {
                                *output.write() = *frame;
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
    track: Track,
    source: Option<u64>,
    cancelled: oneshot::Receiver<()>,
) -> Result<WaveformFrame> {
    let operation = async {
        let mut stream = timeout(IO_TIMEOUT, connect(&data_dir)).await??;
        write_frame(
            &mut stream,
            &Request::Hello {
                version: VERSION,
                instance,
            },
        )
        .await?;
        validate_hello(
            timeout(IO_TIMEOUT, read_frame(&mut stream)).await??,
            instance,
        )?;
        write_frame(
            &mut stream,
            &Request::Full {
                track: Box::new(track),
                source,
            },
        )
        .await?;
        match read_frame(&mut stream).await? {
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
        let bytes = encode(&response).unwrap();
        stream
            .write_all(&(bytes.len() as u32).to_le_bytes())
            .unwrap();
        stream.write_all(&bytes).unwrap();
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
