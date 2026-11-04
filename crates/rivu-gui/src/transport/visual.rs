//! Continuous PCM transport and GUI-owned background display analysis.
use anyhow::{Context, Result, ensure};
use async_channel::{Receiver, Sender};
use async_io::Async;
use futures::{
    FutureExt,
    io::{AsyncReadExt, AsyncWriteExt},
};
use parking_lot::{Mutex, RwLock};
use rivu_analysis::{AnalysisEngine, AnalysisFrame, AnalysisSettings, DisplayDemand};
use rivu_format::config::Config;
use rivu_protocol::pcm::{
    ControlReader, DEADLINE, MAGIC, PcmState, STATUS_SIZE, address, decode_status, request,
};
use rivu_shm::{PCM_QUEUE_BLOCKS, SharedPcmWindow};
use std::{
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
type AsyncStream = Async<UnixStream>;
type Wakeup = Arc<dyn Fn() + Send + Sync>;
type ErrorCallback = Arc<dyn Fn(String) + Send + Sync>;

struct Desired {
    demand: DisplayDemand,
    revision: u64,
    display_token: u64,
    requested: bool,
    settings: AnalysisSettings,
    visualization_enabled: bool,
}
impl Default for Desired {
    fn default() -> Self {
        let config = Config::default();
        Self {
            demand: DisplayDemand::default(),
            revision: 0,
            display_token: 0,
            requested: false,
            settings: AnalysisSettings::from(&config),
            visualization_enabled: config.visualization_enabled,
        }
    }
}
#[derive(Clone, Copy)]
struct Current {
    sequence: u64,
    display_token: u64,
    epoch: u64,
    intent: u64,
    revision: u64,
}
#[derive(Default)]
struct ErrorState {
    message: Option<String>,
    callback: Option<ErrorCallback>,
}
struct ClientState {
    desired: Mutex<Desired>,
    current: RwLock<Option<Current>>,
    wakeup: RwLock<Option<Wakeup>>,
    error: Mutex<ErrorState>,
    stopped: AtomicBool,
}
impl ClientState {
    fn wake(&self) {
        let wakeup = self.wakeup.read().clone();
        if let Some(wakeup) = wakeup {
            wakeup();
        }
    }
    fn fail(&self, message: String) {
        *self.current.write() = None;
        let callback = {
            let mut error = self.error.lock();
            error.message = Some(message.clone());
            error.callback.clone()
        };
        if let Some(callback) = callback {
            callback(message);
        }
        self.wake();
    }
}
/// Independent-process client. Only this local staging object is exposed to GUI readers.
pub(crate) struct VisualClient {
    pub(crate) analysis: Arc<RwLock<AnalysisFrame>>,
    state: Arc<ClientState>,
    notifications: Sender<()>,
    socket: UnixStream,
    worker: Mutex<Option<JoinHandle<()>>>,
}
impl VisualClient {
    pub(crate) fn connect(data_dir: &Path, expected_instance: u32) -> Result<Self> {
        ensure!(
            expected_instance != 0,
            "Analysis requires a bound command instance"
        );
        let mut stream = UnixStream::connect_addr(&address(data_dir)?)
            .context("Connecting analysis control endpoint")?;
        rivu_protocol::wire::verify_peer(&stream)?;
        stream.set_read_timeout(Some(DEADLINE))?;
        stream.set_write_timeout(Some(DEADLINE))?;
        stream.write_all(&request(0, 0, u64::from(expected_instance)))?;
        let mut welcome = [0; 16];
        stream.read_exact(&mut welcome)?;
        ensure!(
            &welcome[..8] == MAGIC
                && u32::from_le_bytes(welcome[8..12].try_into().unwrap()) == expected_instance,
            "Analysis service identity mismatch"
        );
        let length = u32::from_le_bytes(welcome[12..].try_into().unwrap()) as usize;
        ensure!(length <= 4096, "Shared PCM path exceeds control bound");
        let mut root = vec![0; length];
        stream.read_exact(&mut root)?;
        use std::os::unix::ffi::OsStringExt;
        let root = std::path::PathBuf::from(std::ffi::OsString::from_vec(root));
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(None)?;
        stream.set_nonblocking(true)?;
        let socket = stream.try_clone()?;
        let state = Arc::new(ClientState {
            desired: Mutex::new(Desired::default()),
            current: RwLock::new(None),
            wakeup: RwLock::new(None),
            error: Mutex::new(ErrorState::default()),
            stopped: AtomicBool::new(false),
        });
        let analysis = Arc::new(RwLock::new(AnalysisFrame::default()));
        let (notifications, receiver) = async_channel::bounded(1);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let worker_state = state.clone();
        let staging = analysis.clone();
        let worker = thread::Builder::new()
            .name("rivu-visual-client".into())
            .spawn(move || {
                let initialized =
                    (|| -> Result<_> { Ok((SharedPcmWindow::open(&root)?, Async::new(stream)?)) })(
                    );
                match initialized {
                    Ok((subscriber, stream)) => {
                        let _ = ready_tx.send(Ok(()));
                        if let Err(error) = async_io::block_on(run_client(
                            stream,
                            subscriber,
                            &staging,
                            &worker_state,
                            receiver,
                        )) && !worker_state.stopped.load(Ordering::Acquire)
                        {
                            worker_state.fail(format!("Analysis connection lost: {error:#}"));
                        }
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            })?;
        if let Err(error) = ready_rx
            .recv()
            .context("Analysis client initialization stopped")?
        {
            let _ = worker.join();
            return Err(error);
        }
        Ok(Self {
            analysis,
            state,
            notifications,
            socket,
            worker: Mutex::new(Some(worker)),
        })
    }
    pub(crate) fn set_display_demand(&self, demand: DisplayDemand) {
        let mut desired = self.state.desired.lock();
        if desired.demand == demand {
            return;
        }
        if desired.demand.enabled != demand.enabled {
            desired.revision = desired.revision.wrapping_add(1).max(1);
        }
        desired.demand = demand;
        desired.display_token = desired.display_token.wrapping_add(1).max(1);
        desired.requested &= demand.enabled;
        drop(desired);
        *self.state.current.write() = None;
        let _ = self.notifications.try_send(());
    }
    /// Revoke cached acceptance before the command/watch state changes become visible.
    pub(crate) fn invalidate(&self) {
        let mut desired = self.state.desired.lock();
        desired.revision = desired.revision.wrapping_add(1).max(1);
        desired.display_token = desired.display_token.wrapping_add(1).max(1);
        *self.state.current.write() = None;
        drop(desired);
        let _ = self.notifications.try_send(());
    }
    pub(crate) fn request_analysis_frame(&self) {
        let mut desired = self.state.desired.lock();
        if !desired.demand.enabled || self.state.stopped.load(Ordering::Acquire) {
            return;
        }
        desired.requested = true;
        drop(desired);
        let _ = self.notifications.try_send(());
    }
    pub(crate) fn configure(&self, config: &Config) {
        let settings = AnalysisSettings::from(config);
        let mut desired = self.state.desired.lock();
        if desired.settings == settings
            && desired.visualization_enabled == config.visualization_enabled
        {
            return;
        }
        let incompatible = !desired.settings.signal_compatible_with(settings)
            || desired.visualization_enabled != config.visualization_enabled;
        desired.settings = settings;
        desired.visualization_enabled = config.visualization_enabled;
        if incompatible {
            desired.revision = desired.revision.wrapping_add(1).max(1);
            desired.display_token = desired.display_token.wrapping_add(1).max(1);
            *self.state.current.write() = None;
        }
        drop(desired);
        let _ = self.notifications.try_send(());
    }
    pub(crate) fn analysis_frame_is_current(&self, frame: &AnalysisFrame) -> bool {
        let desired = self.state.desired.lock();
        desired.demand.enabled
            && desired.visualization_enabled
            && !self.state.stopped.load(Ordering::Acquire)
            && self.state.current.read().is_some_and(|current| {
                current.revision == desired.revision
                    && current.display_token == desired.display_token
                    && current.sequence == frame.sequence
                    && current.display_token == frame.display_token
                    && current.epoch == frame.audio_epoch
                    && current.intent == frame.audio_intent
            })
    }
    pub(crate) fn set_wakeup(&self, callback: Wakeup) {
        *self.state.wakeup.write() = Some(callback);
    }
    pub(crate) fn set_error_callback(&self, callback: ErrorCallback) {
        let message = {
            let mut error = self.state.error.lock();
            error.callback = Some(callback.clone());
            error.message.clone()
        };
        if let Some(message) = message {
            callback(message);
        }
    }
    pub(crate) fn stop(&self) {
        self.state.stopped.store(true, Ordering::Release);
        *self.state.current.write() = None;
        self.notifications.close();
        let _ = self.socket.shutdown(Shutdown::Both);
        let worker = self.worker.lock().take();
        if let Some(worker) = worker
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}
impl Drop for VisualClient {
    fn drop(&mut self) {
        self.stop();
    }
}
async fn run_client(
    stream: AsyncStream,
    window: SharedPcmWindow,
    staging: &RwLock<AnalysisFrame>,
    state: &ClientState,
    notifications: Receiver<()>,
) -> Result<()> {
    let mut sent_revision = 0;
    let mut acknowledged = None;
    let mut pcm_state = PcmState::default();
    let mut minimum_sequence = 0;
    let mut last_sequence = 0;
    let mut status_reader = ControlReader::<STATUS_SIZE>::new();
    let (mut reader, mut writer) = stream.split();
    let mut engine = AnalysisEngine::new(state.desired.lock().settings);
    let mut frame = AnalysisFrame::default();
    let mut layout = Vec::new();
    let mut weights = Vec::new();
    let mut drain_again = false;
    let mut capture_revision = 0;
    loop {
        // Wait for control/cursor notifications; a completed FFT may additionally
        // wait for local display cadence. No playback-time polling is needed.
        let delay = if drain_again {
            Some(Duration::ZERO)
        } else if acknowledged == Some(sent_revision) {
            engine.publication_delay(Instant::now())
        } else {
            None
        };
        enum Event {
            Status(Result<[u8; STATUS_SIZE]>),
            Desired,
            Continue,
        }
        let deadline = async {
            if let Some(delay) = delay {
                async_io::Timer::after(delay).await;
            } else {
                futures::future::pending::<()>().await;
            }
        };
        let event = futures::select! {
            result = status_reader.read(&mut reader).fuse() => Event::Status(result),
            result = notifications.recv().fuse() => {
                if result.is_err() { return Ok(()); } Event::Desired
            },
            () = deadline.fuse() => Event::Continue,
        };
        if state.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let (demand, revision, display_token, settings, requested) = {
            let mut desired = state.desired.lock();
            let mut demand = desired.demand;
            demand.enabled &= desired.visualization_enabled;
            (
                demand,
                desired.revision,
                desired.display_token,
                desired.settings,
                std::mem::take(&mut desired.requested),
            )
        };
        if revision != sent_revision {
            // Only consumer enablement crosses IPC. Panel bits, cadence and
            // display permits stay local; revision fences subscription changes.
            rivu_protocol::transport::timeout(
                DEADLINE,
                writer.write_all(&request(1, u8::from(demand.enabled), revision)),
            )
            .await??;
            sent_revision = revision;
        }
        if let Event::Status(result) = event {
            let (next, revision, minimum) = decode_status(&result?)?;
            if revision == sent_revision {
                pcm_state = next;
                minimum_sequence = minimum;
                acknowledged = Some(revision);
            }
        }
        let admitted = acknowledged == Some(revision);
        let previous_sequence = frame.sequence;
        engine.configure(settings, demand, pcm_state, display_token, &mut frame);
        if revision != capture_revision {
            engine.restart_capture(&mut frame);
            // Local subscriptions start after the currently published prefix,
            // even when another GUI keeps global capture enabled.
            last_sequence = window.sequence();
            capture_revision = revision;
        }
        if requested {
            engine.request_frame();
        }
        drain_again = false;
        if admitted && demand.enabled && pcm_state.active {
            let latest = window.sequence();
            last_sequence = last_sequence
                .max(minimum_sequence)
                .max(latest.saturating_sub(PCM_QUEUE_BLOCKS as u64));
            for _ in 0..PCM_QUEUE_BLOCKS {
                if last_sequence >= latest {
                    break;
                }
                let sequence = last_sequence + 1;
                let Some(block) = window.block(sequence) else {
                    last_sequence = sequence;
                    continue;
                };
                let header = block.header();
                if !pcm_state.accepts(header) {
                    last_sequence = sequence;
                    continue;
                }
                // Coarse following allows a block of lead. Samples stay in the
                // Core window until the audible cursor approaches their span.
                if header.output_frame
                    > pcm_state
                        .heard_frame
                        .saturating_add(rivu_protocol::pcm::PCM_BLOCK_FRAMES as u64)
                {
                    break;
                }
                if layout != block.layout() {
                    layout.clear();
                    layout.extend_from_slice(block.layout());
                    weights = rivu_dsp::channels::analysis_weights(layout.iter().copied());
                }
                let samples = (0..block.len()).map(|index| {
                    let mut stereo = [0.0; 2];
                    for (channel, weight) in weights.iter().enumerate() {
                        let sample = block.sample(index, channel);
                        stereo[0] += sample * weight[0];
                        stereo[1] += sample * weight[1];
                    }
                    stereo
                });
                if engine.ingest(header, samples, &mut frame) {
                    engine.request_frame();
                }
                last_sequence = sequence;
            }
            drain_again = last_sequence == latest && window.sequence() > latest;
        }
        // Catch up before publishing; never expose an intermediate SHM backlog.
        let published = admitted && !drain_again && engine.publish(&mut frame, Instant::now());
        // Never hold desired while acquiring staging: readers check acceptance
        // while holding staging, and a token revoke must not deadlock them.
        if frame.sequence != previous_sequence {
            staging.write().clone_from(&frame);
        }
        let desired = state.desired.lock();
        if desired.revision == revision
            && desired.display_token == display_token
            && !state.stopped.load(Ordering::Acquire)
            && frame.sequence != previous_sequence
        {
            *state.current.write() =
                (published && admitted && pcm_state.active).then_some(Current {
                    sequence: frame.sequence,
                    display_token: frame.display_token,
                    epoch: frame.audio_epoch,
                    intent: frame.audio_intent,
                    revision,
                });
        }
        drop(desired);
        if frame.sequence != previous_sequence {
            state.wake();
        }
    }
}

#[cfg(test)]
#[path = "visual_tests.rs"]
mod tests;
