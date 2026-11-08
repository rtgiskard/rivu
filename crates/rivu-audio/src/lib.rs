//! Public audio commands, events, and engine ownership.
//!
//! The worker schedules playback; playback owns the source-to-output pipeline.
//! Decoder internals and real-time output state remain in their respective modules.
//!
//! Pause deactivates the device without consuming queued music. End-of-track
//! waits for PipeWire's stream clock to pass the final valid frame's predicted
//! playback time, not merely an empty ring.
//! Listened time is a conservative, callback-confirmed count; unconfirmed
//! device buffers on stop are not counted as heard. Accuracy at the physical
//! output remains bounded by PipeWire's playback timestamps.
//!
//! Lifecycle events use a bounded runtime-neutral async queue with worker-thread
//! backpressure; progress is best effort. Core owns its only receiver. Ordinary
//! command admission never waits for capacity. Stop/snapshot temporarily owns
//! that same receiver and drains it while awaiting capacity and acknowledgement.
//! Shutdown cancels blocked delivery before joining the synchronous worker.
//!
//! Scheduling rounds admit at most 64 commands, including coalesced seeks and
//! read-ahead barriers. Opening retains decoder progress and yields after at
//! most 64 decode work steps; queued controls update or cancel that opening
//! before the next round. Cancelled openings publish no lifecycle completion.
//! A refill submits at most 16,384 frames from the initially vacant ring prefix
//! and decodes at most 64 PCM chunks; reclaimed space belongs to the next round.
//! A partial packet tail stays ahead of later PCM and EOF. Ready commands,
//! opening or refill work run another round without a timer wait. These are
//! work bounds, not wall-clock bounds on decoder/device/file calls.

#[cfg(not(target_os = "linux"))]
compile_error!("Rivu requires Linux and native PipeWire audio output");

#[cfg(feature = "ffmpeg")]
mod ffmpeg;
#[cfg(not(feature = "ffmpeg"))]
mod ffmpeg {
    use super::{Channel, MediaInfo};
    use anyhow::{Result, bail};
    use std::{fs::File, path::Path, sync::atomic::AtomicBool};

    pub(super) fn availability() -> Result<String> {
        bail!("FFmpeg extension is not compiled; rebuild with --features ffmpeg")
    }

    pub(super) fn ensure_loaded() -> Result<()> {
        availability().map(|_| ())
    }

    pub(super) async fn load_completed() {
        std::future::pending::<()>().await;
    }

    #[cfg(test)]
    pub(super) fn load_attempted() -> bool {
        false
    }

    pub(super) struct Input;

    impl Input {
        pub(super) fn is_pcm(&self) -> bool {
            unreachable!("FFmpeg input cannot be opened without the ffmpeg feature")
        }
    }

    pub(super) fn inspect_wave(_: &Path, _: &File) -> Result<Option<Input>> {
        Ok(None)
    }

    pub(super) struct Decoder;

    impl Decoder {
        pub(super) fn from_file(
            _: &Path,
            _: File,
            _: rivu_metadata::Tags,
            _: Option<&AtomicBool>,
        ) -> Result<Self> {
            bail!("FFmpeg extension is not compiled; rebuild with --features ffmpeg")
        }

        pub(super) fn from_input(
            _: Input,
            _: rivu_metadata::Tags,
            _: Option<&AtomicBool>,
        ) -> Result<Self> {
            availability()?;
            unreachable!("FFmpeg input cannot be opened without the ffmpeg feature")
        }

        pub(super) fn info(&self) -> &MediaInfo {
            unreachable!("FFmpeg decoder cannot be opened without the ffmpeg feature")
        }

        pub(super) fn time_base(&self) -> Result<symphonia::core::units::TimeBase> {
            unreachable!("FFmpeg decoder cannot be opened without the ffmpeg feature")
        }

        pub(super) fn vorbis_timing(&self) -> Option<(u32, Option<f64>)> {
            unreachable!("FFmpeg decoder cannot be opened without the ffmpeg feature")
        }

        pub(super) fn identity(&self) -> rivu_protocol::audio::SourceIdentity {
            unreachable!("FFmpeg decoder cannot be opened without the ffmpeg feature")
        }

        pub(super) fn layout(&self) -> &[Channel] {
            unreachable!("FFmpeg decoder cannot be opened without the ffmpeg feature")
        }

        pub(super) fn completed_bitrate(&self) -> Option<u64> {
            None
        }

        pub(super) fn next_frames(
            &mut self,
            _: &mut Vec<f32>,
            _: Option<&AtomicBool>,
            _: &mut usize,
        ) -> Result<super::source::DecodeStep<super::source::FrameTiming>> {
            bail!("FFmpeg extension is not compiled; rebuild with --features ffmpeg")
        }

        pub(super) fn seek(&mut self, _: f64, _: Option<&AtomicBool>) -> Result<()> {
            bail!("FFmpeg extension is not compiled; rebuild with --features ffmpeg")
        }
    }
}
pub mod capture;
mod media;
pub use media::MediaInfo;
use rivu_format::decoder_hint::DecoderHint;
mod volume;

mod bitrate;
mod channels;
mod graph;
mod output;
mod pcm;
mod playback;
mod processor;
mod timeline;
pub use pcm::{PcmAudio, PcmBudget, PcmBuilder};
mod render;
mod source;
mod worker;

pub use channels::Channel;
pub use graph::{GraphPlan, GraphRunner};
pub use output::devices;
pub use processor::{NativeNodePlan, NativeProcessor};
pub use render::{RenderFormat, SourceRenderer};
use rivu_protocol::audio::{PlaybackRange, SourceIdentity};
pub use source::{
    AudioBackend, ProbeResult, probe, probe_details, probe_file, probe_with_cancel,
    source_identity, source_identity_from_metadata,
};

/// Read the published runtime status without starting or waiting for a load.
pub fn ffmpeg_status() -> anyhow::Result<String> {
    ffmpeg::availability()
}

/// Wait for the process-wide explicit load to publish success or failure.
/// Does not start loading; without FFmpeg support this remains pending.
pub async fn ffmpeg_load_completed() {
    ffmpeg::load_completed().await;
}

/// Immutable click-time description of the actual source and active processing.
/// A persisted processing graph is intentionally not consulted by a scan.
#[derive(Clone, Debug)]
pub struct AudioRenderSnapshot {
    pub path: PathBuf,
    pub pcm: Option<PcmAudio>,
    pub range: Option<PlaybackRange>,
    /// Optional read window relative to the selected source, without resetting DSP context.
    pub read_range: Option<PlaybackRange>,
    pub decoder_hint: DecoderHint,
    pub expected_identity: Option<SourceIdentity>,
    pub signal: rivu_format::config::VisualizationSignal,
    pub equalizer: Arc<rivu_dsp::equalizer::config::EqualizerConfig>,
    pub(crate) graph: Option<graph::GraphSnapshot>,
}

impl AudioRenderSnapshot {
    pub fn original(
        path: PathBuf,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
        signal: rivu_format::config::VisualizationSignal,
        equalizer: Arc<rivu_dsp::equalizer::config::EqualizerConfig>,
    ) -> Self {
        Self {
            path,
            pcm: None,
            range,
            read_range: None,
            decoder_hint,
            expected_identity: None,
            signal,
            equalizer,
            graph: None,
        }
    }
}

/// Bounded, lossless rendering of an immutable source and processing snapshot.
pub enum RenderEvent<'a> {
    Start(RenderFormat),
    Pcm {
        media_start: f64,
        media_step: f64,
        samples: &'a [f32],
    },
    Complete {
        frames: u64,
        duration: f64,
    },
}

pub fn render_stream(
    snapshot: &AudioRenderSnapshot,
    media_read_buffer_mib: u32,
    ffmpeg_enabled: bool,
    cancelled: &std::sync::atomic::AtomicBool,
    emit: impl FnMut(RenderEvent<'_>) -> Result<()>,
) -> Result<()> {
    render::render_stream(
        snapshot,
        media_read_buffer_mib,
        ffmpeg_enabled,
        cancelled,
        emit,
    )
}

use crate::capture::PcmControl;
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use futures::{
    FutureExt,
    channel::oneshot,
    future::{BoxFuture, Either},
};
use parking_lot::Mutex;
use rivu_format::config::VisualizationSignal;
use std::{
    path::PathBuf,
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use worker::Worker;

const BYTES_PER_MEBIBYTE: usize = 1024 * 1024;

/// Authoritative library/display metadata, independent of decoder source tags.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaybackMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
}

/// Acknowledged final clock snapshot, sampled after the output stream retires.
/// Position is selected-source-relative; listened time remains cumulative across
/// seeks, device restarts and original/processed audition switches.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioSnapshot {
    pub generation: u64,
    pub position_seconds: f64,
    pub listened_seconds: f64,
}

/// A seek rejected before decoder, output or analysis state can change.
#[derive(Clone, Copy, Debug)]
pub struct SeekRejected;

impl std::fmt::Display for SeekRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Seek must be a finite, nonnegative, representable time")
    }
}

impl std::error::Error for SeekRejected {}

/// Validate the shared time representation before enqueueing a seek.
/// The opened source also validates its absolute timeline and backend timebase.
pub fn validate_seek_position(seconds: f64) -> Result<(), SeekRejected> {
    if !seconds.is_finite() || seconds < 0.0 || seconds >= i64::MAX as f64 {
        return Err(SeekRejected);
    }
    Ok(())
}

pub enum AudioCommand {
    Load {
        path: PathBuf,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
        metadata: Arc<PlaybackMetadata>,
        generation: u64,
        start_seconds: f64,
        paused: bool,
    },
    /// Switch between original and aligned processed audio without resetting heard time.
    /// Processed PCM retains the original path/range metadata. Core retains the
    /// original Track; PCM already contains the selected timeline.
    Audition {
        path: PathBuf,
        pcm: Option<PcmAudio>,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
        source_info: Option<MediaInfo>,
        metadata: Arc<PlaybackMetadata>,
        generation: u64,
        start_seconds: f64,
        paused: bool,
        listened_seconds: f64,
    },
    /// Consume one admitted, immutable graph runner (possibly a cached-prefix
    /// suffix). Its opened descriptor must match the admitted identity and format.
    /// Graph state never leaks into subsequent tracks.
    GraphAudition {
        path: PathBuf,
        pcm: Option<PcmAudio>,
        expected_identity: SourceIdentity,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
        source_info: Option<MediaInfo>,
        metadata: Arc<PlaybackMetadata>,
        generation: u64,
        start_seconds: f64,
        paused: bool,
        listened_seconds: f64,
        runner: Box<GraphRunner>,
    },
    /// Update only the matching opening/playback generation, without reopening.
    Metadata {
        generation: u64,
        metadata: Arc<PlaybackMetadata>,
    },
    Pause(bool),
    Stop,
    Seek {
        seconds: f64,
        revision: u64,
    },
    Volume(f32),
    /// Prepared and applied on the audio worker, never in the device callback.
    Equalizer(Arc<rivu_dsp::equalizer::config::EqualizerConfig>),
    OutputSettings {
        request: u64,
        device: Option<String>,
        auto_mix: bool,
    },
    /// Enable visualization; disabling drops results/storage and suspends work, preserving playback.
    VisualizationEnabled(bool),
    MediaReadBuffer(u32),
    FfmpegEnabled(bool),
    /// Select the captured tap without reopening decoder or output.
    VisualizationSignal(VisualizationSignal),
    /// Capture the actual active source/graph without blocking device callbacks.
    RenderSnapshot(oneshot::Sender<std::result::Result<Option<AudioRenderSnapshot>, String>>),
    StopAndSnapshot(oneshot::Sender<AudioSnapshot>),
    Shutdown,
}

impl AudioCommand {
    fn changes_pcm_source(&self) -> bool {
        matches!(
            self,
            Self::Load { .. }
                | Self::Audition { .. }
                | Self::GraphAudition { .. }
                | Self::Seek { .. }
                | Self::Pause(_)
                | Self::Stop
                | Self::StopAndSnapshot(_)
        )
    }
}
#[derive(Clone, Debug)]
pub enum AudioEvent {
    Started {
        generation: u64,
        info: MediaInfo,
    },
    Progress {
        generation: u64,
        position_seconds: f64,
        listened_seconds: f64,
    },
    Ended {
        generation: u64,
        /// Decoded duration after a complete, seek-free origin-to-EOF traversal drains.
        completed_duration: Option<f64>,
        /// Precise selected-stream average, only after complete playback drains.
        bitrate_bps: Option<u64>,
    },
    DecoderStopped {
        snapshot: AudioSnapshot,
    },
    /// A rejected input leaves playback active; only its matching intent may roll back.
    SeekRejected {
        revision: u64,
        snapshot: AudioSnapshot,
        message: String,
    },
    Failed {
        generation: u64,
        message: String,
    },
    OutputSettingsFailed {
        request: u64,
        device: Option<String>,
        auto_mix: bool,
        message: String,
    },
    EqualizerSettingsFailed {
        generation: Option<u64>,
        config: Arc<rivu_dsp::equalizer::config::EqualizerConfig>,
        message: String,
    },
}

/// A queued command carries the PCM fence chosen at admission, not execution.
pub struct QueuedAudioCommand {
    pub command: AudioCommand,
    pcm_intent: bool,
}

#[cfg(any(test, feature = "test-support"))]
impl QueuedAudioCommand {
    /// Fill a test queue without reserving a production PCM fence.
    pub fn test_unfenced(command: AudioCommand) -> Self {
        Self {
            command,
            pcm_intent: false,
        }
    }
}

struct AudioAdmission {
    visualization_enabled: bool,
    signal: VisualizationSignal,
    device: Option<String>,
    auto_mix: bool,
}

impl Default for AudioAdmission {
    fn default() -> Self {
        Self {
            visualization_enabled: true,
            signal: VisualizationSignal::After,
            device: None,
            auto_mix: true,
        }
    }
}

impl AudioAdmission {
    fn changes_pcm(&self, command: &AudioCommand) -> bool {
        match command {
            AudioCommand::VisualizationEnabled(enabled) => self.visualization_enabled != *enabled,
            AudioCommand::VisualizationSignal(signal) => self.signal != *signal,
            AudioCommand::OutputSettings {
                device, auto_mix, ..
            } => self.device != *device || self.auto_mix != *auto_mix,
            _ => command.changes_pcm_source(),
        }
    }
}

pub struct AudioEngine {
    commands: async_channel::Sender<QueuedAudioCommand>,
    admission: Mutex<AudioAdmission>,
    command_wake: Sender<()>,
    pub events: Option<async_channel::Receiver<AudioEvent>>,
    pub pcm: Arc<PcmControl>,
    worker: Option<JoinHandle<()>>,
    shutdown: Mutex<Option<AudioShutdown>>,
}
impl AudioEngine {
    pub fn new(media_read_buffer_mib: u32) -> Result<Self> {
        let (commands, receive) = async_channel::bounded(64);
        let (command_wake, commands_ready) = bounded(1);
        let (send_events, events) = async_channel::bounded(128);
        let (shutdown, cancelled) = shutdown_channel();
        let pcm = Arc::new(PcmControl::default());
        let analyzer = Arc::clone(&pcm);
        let media_read_buffer_len = usize::try_from(media_read_buffer_mib)
            .expect("media read buffer size does not fit usize")
            .saturating_mul(BYTES_PER_MEBIBYTE);
        let worker = thread::Builder::new()
            .name("rivu-audio".into())
            .spawn(move || {
                Worker::new(
                    receive,
                    commands_ready,
                    send_events,
                    cancelled,
                    analyzer,
                    media_read_buffer_len,
                )
                .run()
            })?;
        Ok(Self {
            commands,
            admission: Mutex::new(AudioAdmission::default()),
            command_wake,
            events: Some(events),
            pcm,
            worker: Some(worker),
            shutdown: Mutex::new(Some(shutdown)),
        })
    }
    /// Admit an ordinary command without waiting on a worker blocked by events.
    /// Success means queued, not applied; a full queue explicitly rejects it.
    pub fn send(&self, command: AudioCommand) -> Result<()> {
        if matches!(command, AudioCommand::Shutdown) {
            self.shutdown.lock().take();
            return Ok(());
        }
        let mut admission = self.admission.lock();
        let changes_pcm = admission.changes_pcm(&command);
        self.ensure_command_capacity(1)?;
        let enabled = match &command {
            AudioCommand::VisualizationEnabled(enabled) => *enabled,
            _ => admission.visualization_enabled,
        };
        let signal = match &command {
            AudioCommand::VisualizationSignal(signal) => *signal,
            _ => admission.signal,
        };
        let output = match &command {
            AudioCommand::OutputSettings {
                device, auto_mix, ..
            } if changes_pcm => Some((device.clone(), *auto_mix)),
            _ => None,
        };
        if changes_pcm {
            self.pcm.admit_intent();
        }
        // Semantic admission and the actual FIFO send share the same order.
        match self.commands.try_send(QueuedAudioCommand {
            command,
            pcm_intent: changes_pcm,
        }) {
            Ok(()) => {
                admission.visualization_enabled = enabled;
                admission.signal = signal;
                if let Some((device, auto_mix)) = output {
                    admission.device = device;
                    admission.auto_mix = auto_mix;
                }
            }
            Err(error) => {
                if changes_pcm {
                    self.pcm.cancel_intent();
                }
                return Err(match error {
                    async_channel::TrySendError::Full(_) => {
                        anyhow::anyhow!("Audio command queue is full; command not queued")
                    }
                    async_channel::TrySendError::Closed(_) => {
                        anyhow::anyhow!("Audio worker stopped")
                    }
                });
            }
        }
        // This is only a coalesced queue-ready notification. The bounded
        // queue owns every command and wakes async capacity waiters itself.
        let _ = self.command_wake.try_send(());
        Ok(())
    }

    /// Core is the sole ordinary producer. Until its next send, the worker can
    /// only free slots, so a complete batch can be admitted after persistence.
    pub fn ensure_command_capacity(&self, count: usize) -> Result<()> {
        anyhow::ensure!(
            count
                <= self
                    .commands
                    .capacity()
                    .expect("bounded audio command queue")
                    - self.commands.len(),
            "Audio command queue has insufficient capacity; commands not queued"
        );
        Ok(())
    }

    /// Temporarily transfer the sole event receiver to a cancellation-safe stop.
    /// Return it through `restore_events` after the request has settled.
    pub fn stop_request(&mut self) -> AudioStop {
        AudioStop::new(
            self.commands.clone(),
            self.command_wake.clone(),
            self.events.take().expect("audio events already borrowed"),
            Arc::clone(&self.pcm),
        )
    }

    /// Restore the event receiver previously transferred to a stop request.
    pub fn restore_events(&mut self, events: async_channel::Receiver<AudioEvent>) {
        assert!(
            self.events.is_none(),
            "audio event receiver already restored"
        );
        self.events = Some(events);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_commands(
        &mut self,
        commands: async_channel::Sender<QueuedAudioCommand>,
    ) -> async_channel::Sender<QueuedAudioCommand> {
        let previous = std::mem::replace(&mut self.commands, commands);
        let _ = self.command_wake.try_send(());
        previous
    }
}

/// A stop-only handle cannot submit ordinary commands behind Core's preflight.
pub struct AudioStop {
    events: async_channel::Receiver<AudioEvent>,
    // Retain the in-flight send itself: dropping a wait must not discard its
    // owned reply sender while waiting for bounded command capacity.
    admission: Option<BoxFuture<'static, Result<()>>>,
    stopped: oneshot::Receiver<AudioSnapshot>,
    deadline: Instant,
}

// A cancelled capacity wait must not leave an unowned desired intent behind.
struct PendingPcmIntent(Option<Arc<PcmControl>>);
impl Drop for PendingPcmIntent {
    fn drop(&mut self) {
        if let Some(control) = &self.0 {
            control.cancel_intent();
        }
    }
}

impl AudioStop {
    fn new(
        commands: async_channel::Sender<QueuedAudioCommand>,
        command_wake: Sender<()>,
        events: async_channel::Receiver<AudioEvent>,
        pcm: Arc<PcmControl>,
    ) -> Self {
        let (reply, stopped) = oneshot::channel();
        Self {
            events,
            admission: Some(
                async move {
                    pcm.admit_intent();
                    let mut intent = PendingPcmIntent(Some(pcm));
                    commands
                        .send(QueuedAudioCommand {
                            command: AudioCommand::StopAndSnapshot(reply),
                            pcm_intent: true,
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("Audio worker stopped"))?;
                    intent.0 = None;
                    let _ = command_wake.try_send(());
                    Ok(())
                }
                .boxed(),
            ),
            stopped,
            deadline: Instant::now() + Duration::from_secs(5),
        }
    }

    /// Cancellation retains both the sole event receiver and request state.
    /// Resuming this handle never replays an already admitted stop command.
    /// The five-second deadline starts at request creation and covers admission,
    /// lifecycle draining and acknowledgement, including cancelled waits.
    /// Acknowledgement includes the final position and listened-time counters,
    /// sampled only after the synchronous worker closes the physical output.
    pub async fn wait(&mut self, mut observe: impl FnMut(AudioEvent)) -> Result<AudioSnapshot> {
        let deadline = async_io::Timer::at(self.deadline).fuse();
        futures::pin_mut!(deadline);
        if let Some(admission) = self.admission.as_mut() {
            let result = loop {
                let event = self.events.recv().fuse();
                let admitted = admission.as_mut().fuse();
                futures::pin_mut!(event, admitted);
                futures::select_biased! {
                    _ = deadline => anyhow::bail!("Audio output did not stop before the deadline"),
                    event = event => {
                        observe(event.context("Audio worker stopped before stop was submitted")?);
                    },
                    result = admitted => break result,
                }
            };
            self.admission = None;
            result?;
        }
        let mut events_open = true;
        loop {
            let event = if events_open {
                Either::Left(self.events.recv())
            } else {
                Either::Right(futures::future::pending())
            }
            .fuse();
            let stopped = (&mut self.stopped).fuse();
            futures::pin_mut!(event, stopped);
            futures::select_biased! {
                _ = deadline => anyhow::bail!("Audio output did not stop before the deadline"),
                // The worker emits these FIFO events before physically closing
                // playback. Drain them before accepting its acknowledgement.
                event = event => {
                    match event {
                        Ok(event) => observe(event),
                        Err(_) => events_open = false,
                    }
                },
                snapshot = stopped => return snapshot.context("Audio worker stopped before acknowledging stop"),
            }
        }
    }

    /// Return ownership of the sole event receiver to the engine.
    pub fn into_events(self) -> async_channel::Receiver<AudioEvent> {
        self.events
    }
}

// Cancelling the async send and waking synchronous selects are one lifetime.
// Drop runs before the wake sender disconnects, including AudioEngine::drop.
struct AudioShutdown {
    _wake: Sender<()>,
    events: async_channel::Sender<()>,
}

struct AudioShutdownReceiver {
    signal: Receiver<()>,
    events: async_channel::Receiver<()>,
}

impl Drop for AudioShutdown {
    fn drop(&mut self) {
        self.events.close();
    }
}

fn shutdown_channel() -> (AudioShutdown, AudioShutdownReceiver) {
    let (wake, receiver) = bounded(0);
    let (cancel, events) = async_channel::bounded(1);
    (
        AudioShutdown {
            _wake: wake,
            events: cancel,
        },
        AudioShutdownReceiver {
            signal: receiver,
            events,
        },
    )
}
impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shutdown.get_mut().take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
