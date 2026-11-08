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
use rivu_protocol::model::Track;
use rivu_protocol::pcm::{
    ControlReader, ControlRequest, DEADLINE, PcmPosition, PcmState, STATUS_SIZE, WELCOME_SIZE,
    Welcome, address, decode_status,
};
use rivu_shm::{PCM_QUEUE_BLOCKS, SharedPcmWindow};
use rivu_visuals::{WaveformFrame, WaveformSummary};
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
    settings: AnalysisSettings,
    visualization_enabled: bool,
    waveform_enabled: bool,
    waveform_presentation_visible: bool,
    source_generation: Option<u64>,
}
impl Default for Desired {
    fn default() -> Self {
        let config = Config::default();
        Self {
            demand: DisplayDemand::default(),
            revision: 0,
            display_token: 0,
            settings: AnalysisSettings::from(&config),
            visualization_enabled: config.visualization_enabled,
            waveform_enabled: false,
            waveform_presentation_visible: true,
            source_generation: None,
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
    summary: Mutex<WaveformSummary>,
    waveform: Arc<RwLock<WaveformFrame>>,
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
    pub(crate) waveform: Arc<RwLock<WaveformFrame>>,
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
        stream.write_all(
            &ControlRequest::Hello {
                instance: expected_instance,
            }
            .encode(),
        )?;
        let mut welcome = [0; WELCOME_SIZE];
        stream.read_exact(&mut welcome)?;
        let welcome = Welcome::decode(&welcome)?;
        ensure!(
            welcome.instance == expected_instance,
            "Analysis service identity mismatch"
        );
        let mut root = vec![0; welcome.path_length];
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
            summary: Mutex::new(WaveformSummary::default()),
            waveform: Arc::new(RwLock::new(WaveformFrame::default())),
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
            waveform: Arc::clone(&state.waveform),
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
        if (desired.demand.enabled || desired.waveform_enabled)
            != (demand.enabled || desired.waveform_enabled)
        {
            desired.revision = desired.revision.wrapping_add(1).max(1);
        }
        desired.demand = demand;
        desired.display_token = desired.display_token.wrapping_add(1).max(1);
        drop(desired);
        *self.state.current.write() = None;
        let _ = self.notifications.try_send(());
    }
    pub(crate) fn set_waveform_demand(&self, enabled: bool) {
        let mut desired = self.state.desired.lock();
        if desired.waveform_enabled == enabled {
            return;
        }
        if (desired.demand.enabled || desired.waveform_enabled)
            != (desired.demand.enabled || enabled)
        {
            desired.revision = desired.revision.wrapping_add(1).max(1);
        }
        desired.waveform_enabled = enabled;
        drop(desired);
        let _ = self.notifications.try_send(());
    }

    /// Hide snapshot publication without changing PCM demand or scan acceptance.
    pub(crate) fn set_waveform_presentation_visible(&self, visible: bool) {
        let mut desired = self.state.desired.lock();
        if desired.waveform_presentation_visible == visible {
            return;
        }
        desired.waveform_presentation_visible = visible;
        let publish = visible
            && desired.waveform_enabled
            && desired.visualization_enabled
            && !self.state.stopped.load(Ordering::Acquire);
        if publish {
            let summary = self.state.summary.lock();
            self.state.waveform.write().clone_from(&summary.frame);
        }
        drop(desired);
        if publish {
            self.state.wake();
        }
    }

    pub(crate) fn configure_source(
        &self,
        track: Option<&Track>,
        generation: u64,
        duration: Option<f64>,
    ) {
        let mut desired = self.state.desired.lock();
        if self.state.stopped.load(Ordering::Acquire) {
            return;
        }
        let source_generation = track.map(|_| generation);
        let unchanged = desired.source_generation == source_generation;
        if unchanged {
            return;
        }
        desired.source_generation = source_generation;
        desired.revision = desired.revision.wrapping_add(1).max(1);
        let mut summary = self.state.summary.lock();
        if let Some(track) = track {
            let range = track
                .cue
                .as_ref()
                .map(|cue| rivu_protocol::audio::PlaybackRange {
                    start_seconds: cue.start_seconds(),
                    end_seconds: cue.end_seconds(),
                });
            summary.select(
                &track.path,
                range,
                track.decoder_hint,
                duration.or(track.duration),
                track.sample_rate.max(1),
                Some(generation),
                None,
            );
        } else {
            summary.clear();
        }
        self.state.waveform.write().clone_from(&summary.frame);
        drop(summary);
        drop(desired);
        *self.state.current.write() = None;
        let _ = self.notifications.try_send(());
        self.state.wake();
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
        if desired.visualization_enabled && !config.visualization_enabled {
            let mut summary = self.state.summary.lock();
            summary.frame.request_epoch = summary.frame.request_epoch.wrapping_add(1);
            self.state.waveform.write().clone_from(&summary.frame);
        }
        desired.visualization_enabled = config.visualization_enabled;
        if incompatible {
            desired.revision = desired.revision.wrapping_add(1).max(1);
            desired.display_token = desired.display_token.wrapping_add(1).max(1);
            *self.state.current.write() = None;
        }
        drop(desired);
        let _ = self.notifications.try_send(());
    }
    /// Run only the local staging copy while all acceptance guards remain live.
    /// The closure must not call renderers or re-enter this transport.
    pub(crate) fn consume_current_analysis(
        &self,
        consume: impl FnOnce(&AnalysisFrame) -> bool,
    ) -> bool {
        let frame = self.analysis.read();
        let desired = self.state.desired.lock();
        let current = self.state.current.read();
        let accepted = desired.demand.enabled
            && desired.visualization_enabled
            && !self.state.stopped.load(Ordering::Acquire)
            && current.is_some_and(|current| {
                current.revision == desired.revision
                    && current.display_token == desired.display_token
                    && current.sequence == frame.sequence
                    && current.display_token == frame.display_token
                    && current.epoch == frame.audio_epoch
                    && current.intent == frame.audio_intent
            });
        accepted && consume(&frame)
    }

    #[cfg(test)]
    fn analysis_frame_is_current(&self, frame: &AnalysisFrame) -> bool {
        self.consume_current_analysis(|current| {
            current.sequence == frame.sequence
                && current.display_token == frame.display_token
                && current.audio_epoch == frame.audio_epoch
                && current.audio_intent == frame.audio_intent
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
        let desired = self.state.desired.lock();
        let mut summary = self.state.summary.lock();
        summary.clear();
        self.state.waveform.write().clone_from(&summary.frame);
        drop(summary);
        drop(desired);
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
    let mut minimum_position = PcmPosition::default();
    let mut position = PcmPosition::default();
    let mut status_reader = ControlReader::<STATUS_SIZE>::new();
    let (mut reader, mut writer) = stream.split();
    let mut engine = AnalysisEngine::new(state.desired.lock().settings);
    let mut frame = AnalysisFrame::default();
    let mut layout = Vec::new();
    let mut weights = Vec::new();
    let mut folded = Vec::with_capacity(rivu_protocol::pcm::PCM_BLOCK_FRAMES);
    let mut waveform_key = None;
    let mut drain_again = false;
    let mut capture_revision = 0;
    loop {
        if drain_again {
            // A growing retention window never turns catch-up into one
            // unbounded executor turn or starves ready subscription changes.
            futures_lite::future::yield_now().await;
        }
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
        let (demand, revision, display_token, settings, pcm_enabled, waveform_enabled) = {
            let desired = state.desired.lock();
            let mut demand = desired.demand;
            demand.enabled &= desired.visualization_enabled;
            (
                demand,
                desired.revision,
                desired.display_token,
                desired.settings,
                (desired.demand.enabled || desired.waveform_enabled)
                    && desired.visualization_enabled,
                desired.waveform_enabled && desired.visualization_enabled,
            )
        };
        if revision != sent_revision {
            // Only consumer enablement crosses IPC. Panel bits, cadence and
            // display identity stay local; revision fences subscription changes.
            rivu_protocol::transport::timeout(
                DEADLINE,
                writer.write_all(
                    &ControlRequest::Subscribe {
                        enabled: pcm_enabled,
                        revision,
                    }
                    .encode(),
                ),
            )
            .await??;
            sent_revision = revision;
        }
        if let Event::Status(result) = event {
            let (next, revision, minimum) = decode_status(&result?)?;
            if revision == sent_revision {
                pcm_state = next;
                minimum_position = minimum;
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
            position = window.frontier();
            capture_revision = revision;
            waveform_key = None;
        }
        drain_again = false;
        let mut waveform_generation = None;
        if admitted && pcm_enabled && pcm_state.active {
            let latest = window.sequence();
            position = position.max(minimum_position).max(PcmPosition {
                sequence: latest
                    .saturating_sub(window.retained_blocks() as u64 - 1)
                    .max(1),
                offset: 0,
            });
            let mut drained = 0;
            for _ in 0..PCM_QUEUE_BLOCKS {
                let sequence = position.sequence.max(1);
                if sequence > latest {
                    break;
                }
                drained += 1;
                let Some(block) = window.block(sequence) else {
                    // An append can fence the current tail temporarily. Keep
                    // ownership of its already-consumed prefix until it completes.
                    if sequence == latest {
                        break;
                    }
                    position = PcmPosition {
                        sequence: sequence + 1,
                        offset: 0,
                    };
                    continue;
                };
                let mut header = block.header();
                if !pcm_state.accepts(header) {
                    // Control status can lag the producer's new identity. The
                    // latest logical block may still grow under that identity;
                    // retry its original prefix once the matching status arrives.
                    if sequence == latest {
                        break;
                    }
                    position = PcmPosition {
                        sequence: sequence + 1,
                        offset: 0,
                    };
                    continue;
                }
                let offset = usize::from(position.offset);
                if offset >= block.len() {
                    if sequence < latest {
                        position = PcmPosition {
                            sequence: sequence + 1,
                            offset: 0,
                        };
                        continue;
                    }
                    break;
                }
                header.start_frame += offset as u64;
                header.output_frame += offset as u64;
                header.media_start += offset as f64 * header.media_step;
                header.gap &= offset == 0;
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
                folded.clear();
                folded.extend((offset..block.len()).map(|index| {
                    let mut stereo = [0.0; 2];
                    for (channel, weight) in weights.iter().enumerate() {
                        let sample = block.sample(index, channel);
                        stereo[0] += sample * weight[0];
                        stereo[1] += sample * weight[1];
                    }
                    stereo
                }));
                // No analysis or summary may see samples until the SHM read
                // lease has validated its complete native-channel read.
                if !block.still_current() {
                    break;
                }
                let ingested =
                    engine.ingest_checked(header, folded.iter().copied(), || true, &mut frame);
                if waveform_enabled {
                    let desired = state.desired.lock();
                    if desired.revision == revision
                        && desired.waveform_enabled
                        && desired.visualization_enabled
                        && desired.source_generation == Some(header.generation)
                        && !state.stopped.load(Ordering::Acquire)
                    {
                        let mut summary = state.summary.lock();
                        let key = (
                            header.generation,
                            header.epoch,
                            header.intent,
                            header.capture,
                        );
                        if waveform_key != Some(key) || header.gap {
                            summary.begin_traversal();
                        }
                        waveform_key = Some(key);
                        summary.push_mapped(header.media_start, header.media_step, &folded);
                        waveform_generation = Some(header.generation);
                    }
                }
                if ingested {
                    engine.request_frame();
                }
                if sequence < latest {
                    position = PcmPosition {
                        sequence: sequence + 1,
                        offset: 0,
                    };
                } else {
                    // The latest logical block remains observable while partial.
                    position = PcmPosition {
                        sequence,
                        offset: block.len() as u16,
                    };
                    break;
                }
            }
            drain_again = drained == PCM_QUEUE_BLOCKS || window.sequence() > latest;
        }
        // Catch up before publishing; never expose an intermediate SHM backlog.
        if admitted && !drain_again {
            engine.publish(&mut frame, Instant::now());
        }
        // Never hold desired while acquiring staging: readers check acceptance
        // while holding staging, and a token revoke must not deadlock them.
        if frame.sequence != previous_sequence {
            staging.write().clone_from(&frame);
        }
        let desired = state.desired.lock();
        // Publish the latest rolling state at every bounded drain yield, even
        // while analysis waits to catch up. Setters may revoke acceptance while
        // folding, so recheck all publication guards in desired -> summary order.
        let waveform_published = waveform_generation.is_some_and(|generation| {
            desired.revision == revision
                && desired.source_generation == Some(generation)
                && desired.waveform_enabled
                && desired.visualization_enabled
                && desired.waveform_presentation_visible
                && !state.stopped.load(Ordering::Acquire)
        });
        if waveform_published {
            let summary = state.summary.lock();
            state.waveform.write().clone_from(&summary.frame);
        }
        if desired.revision == revision
            && desired.display_token == display_token
            && !state.stopped.load(Ordering::Acquire)
            && frame.sequence != previous_sequence
        {
            // Current invalidations are real publications too: the runtime must
            // retire its previous picture even before a fresh FFT window exists.
            *state.current.write() = (admitted && pcm_state.active).then_some(Current {
                sequence: frame.sequence,
                display_token: frame.display_token,
                epoch: frame.audio_epoch,
                intent: frame.audio_intent,
                revision,
            });
        }
        drop(desired);
        if waveform_published || frame.sequence != previous_sequence {
            state.wake();
        }
    }
}

#[cfg(test)]
#[path = "visual_tests.rs"]
pub(crate) mod tests;
