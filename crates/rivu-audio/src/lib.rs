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
//! Lifecycle events use Tokio's bounded async queue with worker-thread
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
        ) -> Result<super::source::DecodeStep<f64>> {
            bail!("FFmpeg extension is not compiled; rebuild with --features ffmpeg")
        }

        pub(super) fn seek(&mut self, _: f64, _: Option<&AtomicBool>) -> Result<()> {
            bail!("FFmpeg extension is not compiled; rebuild with --features ffmpeg")
        }
    }
}
pub mod analysis;
mod media;
pub use media::{DecoderHint, MediaInfo};
mod volume;

mod bitrate;
mod channels;
mod graph;
mod output;
mod playback;
mod render;
mod source;
mod waveform;
mod worker;

use channels::Channel;
pub use graph::GraphRunner;
pub use output::devices;
pub use render::{RenderInfo, render_source};
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
use parking_lot::{Mutex, RwLock};
use std::{
    path::PathBuf,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
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
    /// Processed WAV uses no range and Some(original_info); restore uses the
    /// original path/range/decoder and None. Core retains the original Track.
    Audition {
        path: PathBuf,
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
    Seek(f64),
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
    WaveformSnapshot(Sender<std::result::Result<Option<WaveformScanSnapshot>, String>>),
    StopAndSnapshot(oneshot::Sender<AudioSnapshot>),
    Shutdown,
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
    commands: mpsc::Sender<AudioCommand>,
    command_wake: Sender<()>,
    pub events: Option<mpsc::Receiver<AudioEvent>>,
    pub analysis: Arc<RwLock<AnalysisFrame>>,
    pub analysis_control: Arc<AnalysisControl>,
    pub waveform: Arc<RwLock<WaveformFrame>>,
    worker: Option<JoinHandle<()>>,
    shutdown: Mutex<Option<AudioShutdown>>,
}
impl AudioEngine {
    pub fn new(media_read_buffer_mb: u32) -> Result<Self> {
        let (commands, receive) = mpsc::channel(64);
        let (command_wake, commands_ready) = bounded(1);
        let (send_events, events) = mpsc::channel(128);
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
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    anyhow::anyhow!("Audio command queue is full; command not queued")
                }
                mpsc::error::TrySendError::Closed(_) => anyhow::anyhow!("Audio worker stopped"),
            })?;
        // This is only a coalesced queue-ready notification. The bounded Tokio
        // queue owns every command and wakes async capacity waiters itself.
        let _ = self.command_wake.try_send(());
        Ok(())
    }

    /// Core is the sole ordinary producer. Until its next send, the worker can
    /// only free slots, so a complete batch can be admitted after persistence.
    pub fn ensure_command_capacity(&self, count: usize) -> Result<()> {
        anyhow::ensure!(
            count <= self.commands.capacity(),
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
    pub fn restore_events(&mut self, events: mpsc::Receiver<AudioEvent>) {
        assert!(
            self.events.is_none(),
            "audio event receiver already restored"
        );
        self.events = Some(events);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_commands(
        &mut self,
        commands: mpsc::Sender<AudioCommand>,
    ) -> mpsc::Sender<AudioCommand> {
        let previous = std::mem::replace(&mut self.commands, commands);
        let _ = self.command_wake.try_send(());
        previous
    }
}

/// A stop-only handle cannot submit ordinary commands behind Core's preflight.
pub struct AudioStop {
    commands: mpsc::Sender<AudioCommand>,
    command_wake: Sender<()>,
    events: mpsc::Receiver<AudioEvent>,
    reply: Option<oneshot::Sender<AudioSnapshot>>,
    stopped: oneshot::Receiver<AudioSnapshot>,
    deadline: tokio::time::Instant,
}

impl AudioStop {
    fn new(
        commands: mpsc::Sender<AudioCommand>,
        command_wake: Sender<()>,
        events: mpsc::Receiver<AudioEvent>,
    ) -> Self {
        let (reply, stopped) = oneshot::channel();
        Self {
            commands,
            command_wake,
            events,
            reply: Some(reply),
            stopped,
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
        }
    }

    /// Cancellation retains both the sole event receiver and request state.
    /// Resuming this handle never replays an already admitted stop command.
    /// Acknowledgement includes the final position and listened-time counters,
    /// sampled only after the synchronous worker closes the physical output.
    pub async fn wait(&mut self, mut observe: impl FnMut(AudioEvent)) -> Result<AudioSnapshot> {
        let deadline = tokio::time::sleep_until(self.deadline);
        tokio::pin!(deadline);
        if self.reply.is_some() {
            let capacity = self.commands.reserve();
            tokio::pin!(capacity);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut deadline => anyhow::bail!("Audio output did not stop before the deadline"),
                    event = self.events.recv() => {
                        observe(event.context("Audio worker stopped before stop was submitted")?);
                    }
                    permit = &mut capacity => {
                        let permit = permit.context("Audio worker stopped")?;
                        let reply = self.reply.take().expect("stop request was not admitted");
                        permit.send(AudioCommand::StopAndSnapshot(reply));
                        let _ = self.command_wake.try_send(());
                        break;
                    }
                }
            }
        }
        let mut events_open = true;
        loop {
            tokio::select! {
                biased;
                _ = &mut deadline => anyhow::bail!("Audio output did not stop before the deadline"),
                // The worker emits these FIFO events before physically closing
                // playback. Drain them before accepting its acknowledgement.
                event = self.events.recv(), if events_open => {
                    match event {
                        Some(event) => observe(event),
                        None => events_open = false,
                    }
                }
                snapshot = &mut self.stopped => return snapshot.context("Audio worker stopped before acknowledging stop"),
            }
        }
    }

    /// Return ownership of the sole event receiver to the engine.
    pub fn into_events(self) -> mpsc::Receiver<AudioEvent> {
        self.events
    }
}

// Cancelling the async send and waking synchronous selects are one lifetime.
// Drop runs before the wake sender disconnects, including AudioEngine::drop.
struct AudioShutdown {
    _wake: Sender<()>,
    events: CancellationToken,
}

struct AudioShutdownReceiver {
    signal: Receiver<()>,
    events: CancellationToken,
}

impl Drop for AudioShutdown {
    fn drop(&mut self) {
        self.events.cancel();
    }
}

fn shutdown_channel() -> (AudioShutdown, AudioShutdownReceiver) {
    let (wake, receiver) = bounded(0);
    let events = CancellationToken::new();
    (
        AudioShutdown {
            _wake: wake,
            events: events.clone(),
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
