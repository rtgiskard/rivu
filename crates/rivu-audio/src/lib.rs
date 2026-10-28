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
        pub(super) fn from_file(_: &Path, _: File, _: Option<&AtomicBool>) -> Result<Self> {
            bail!("FFmpeg extension is not compiled; rebuild with --features ffmpeg")
        }

        pub(super) fn from_input(_: Input, _: Option<&AtomicBool>) -> Result<Self> {
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

        pub(super) fn identity(&self) -> super::source::SourceIdentity {
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
pub mod analysis;
mod media;
pub use media::MediaInfo;
use rivu_format::media::DecoderHint;
mod volume;

mod bitrate;
mod channels;
mod graph;
mod output;
mod pcm;
mod playback;
pub use pcm::{PcmAudio, PcmBudget, PcmBuilder};
mod render;
mod source;
mod waveform;
mod worker;

use channels::Channel;
pub use graph::GraphRunner;
pub use output::devices;
pub use render::{RenderFormat, SourceRenderer};
pub use source::{
    ProbeBackend, ProbeResult, SourceIdentity, metadata_budget, probe, probe_details, probe_file,
    probe_with_cancel,
};
pub use waveform::{WaveformFrame, WaveformIdentity};

/// Read the published runtime status without starting or waiting for a load.
pub fn ffmpeg_status() -> anyhow::Result<String> {
    ffmpeg::availability()
}

/// Whether an explicit load has published either a runtime or its failure.
pub fn ffmpeg_load_attempted() -> bool {
    ffmpeg::load_attempted()
}

/// Immutable click-time description of the actual source and active processing.
/// A persisted processing graph is intentionally not consulted by a scan.
#[derive(Clone, Debug)]
pub struct WaveformScanSnapshot {
    pub path: PathBuf,
    pub pcm: Option<PcmAudio>,
    pub range: Option<PlaybackRange>,
    pub decoder_hint: DecoderHint,
    pub expected_identity: Option<SourceIdentity>,
    pub signal: rivu_format::config::VisualizationSignal,
    pub equalizer: Arc<rivu_dsp::equalizer::config::EqualizerConfig>,
    pub(crate) graph: Option<graph::GraphSnapshot>,
}

impl WaveformScanSnapshot {
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
            decoder_hint,
            expected_identity: None,
            signal,
            equalizer,
            graph: None,
        }
    }
}

/// Scan only on an explicit request, preserving the captured signal and graph.
pub fn waveform(
    snapshot: &WaveformScanSnapshot,
    media_read_buffer_mb: u32,
    ffmpeg_enabled: bool,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<WaveformFrame> {
    source::Source::waveform(snapshot, media_read_buffer_mb, ffmpeg_enabled, cancelled)
}

use crate::analysis::{AnalysisControl, AnalysisFrame, AnalysisSettings, AnalysisWorker};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use futures::{
    FutureExt,
    channel::oneshot,
    future::{BoxFuture, Either},
};
use parking_lot::{Mutex, RwLock};
use std::{
    path::PathBuf,
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use worker::Worker;

const BYTES_PER_MEBIBYTE: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackRange {
    pub start_seconds: f64,
    pub end_seconds: Option<f64>,
}

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
    /// Drop visualization results/storage and suspend their work, preserving playback.
    StillMode(bool),
    MediaReadBuffer(u32),
    FfmpegEnabled(bool),
    /// Update analysis only; never reopen the decoder or output stream.
    AnalysisSettings(AnalysisSettings),
    /// Capture the actual active source/graph without blocking device callbacks.
    WaveformSnapshot(oneshot::Sender<std::result::Result<Option<WaveformScanSnapshot>, String>>),
    StopAndSnapshot(oneshot::Sender<AudioSnapshot>),
    Shutdown,
}

impl AudioCommand {
    fn changes_analysis_source(&self) -> bool {
        matches!(
            self,
            Self::Load { .. }
                | Self::Audition { .. }
                | Self::GraphAudition { .. }
                | Self::Seek { .. }
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
        /// Precise selected-stream average, only after complete playback drains.
        bitrate_bps: Option<u64>,
    },
    DecoderStopped {
        generation: u64,
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

pub struct AudioEngine {
    commands: async_channel::Sender<AudioCommand>,
    command_wake: Sender<()>,
    pub events: Option<async_channel::Receiver<AudioEvent>>,
    pub analysis: Arc<RwLock<AnalysisFrame>>,
    pub analysis_control: Arc<AnalysisControl>,
    pub waveform: Arc<RwLock<WaveformFrame>>,
    worker: Option<JoinHandle<()>>,
    shutdown: Mutex<Option<AudioShutdown>>,
}
impl AudioEngine {
    pub fn new(media_read_buffer_mb: u32) -> Result<Self> {
        let (commands, receive) = async_channel::bounded(64);
        let (command_wake, commands_ready) = bounded(1);
        let (send_events, events) = async_channel::bounded(128);
        let (shutdown, cancelled) = shutdown_channel();
        let analysis = Arc::new(RwLock::new(AnalysisFrame::default()));
        let waveform = Arc::new(RwLock::new(WaveformFrame::default()));
        let worker_waveform = waveform.clone();
        let analyzer = AnalysisWorker::new(analysis.clone())?;
        let analysis_control = Arc::clone(&analyzer.control);
        let media_read_buffer_len = usize::try_from(media_read_buffer_mb)
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
                    worker_waveform,
                    media_read_buffer_len,
                )
                .run()
            })?;
        Ok(Self {
            commands,
            command_wake,
            events: Some(events),
            analysis,
            analysis_control,
            waveform,
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
        let changes_analysis = command.changes_analysis_source();
        if changes_analysis {
            self.ensure_command_capacity(1)?;
            self.analysis_control
                .desired_intent
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        self.commands
            .try_send(command)
            .inspect_err(|_| {
                if changes_analysis {
                    self.analysis_control
                        .desired_intent
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                }
            })
            .map_err(|error| match error {
                async_channel::TrySendError::Full(_) => {
                    anyhow::anyhow!("Audio command queue is full; command not queued")
                }
                async_channel::TrySendError::Closed(_) => anyhow::anyhow!("Audio worker stopped"),
            })?;
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
        commands: async_channel::Sender<AudioCommand>,
    ) -> async_channel::Sender<AudioCommand> {
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

impl AudioStop {
    fn new(
        commands: async_channel::Sender<AudioCommand>,
        command_wake: Sender<()>,
        events: async_channel::Receiver<AudioEvent>,
    ) -> Self {
        let (reply, stopped) = oneshot::channel();
        Self {
            events,
            admission: Some(
                async move {
                    commands
                        .send(AudioCommand::StopAndSnapshot(reply))
                        .await
                        .map_err(|_| anyhow::anyhow!("Audio worker stopped"))?;
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
