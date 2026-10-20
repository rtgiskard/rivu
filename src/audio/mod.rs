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
//! Lifecycle events use bounded worker-thread backpressure; progress is best
//! effort. Ordinary command admission never waits for capacity, allowing the
//! consumer to return to draining events after an explicit queue-full error.
//! Stop/snapshot drains events while waiting; shutdown cancels blocked delivery.
//!
//! Scheduling rounds admit at most 64 commands, including coalesced seeks and
//! read-ahead barriers. Opening retains decoder progress and yields after at
//! most 64 decode work steps; queued controls update or cancel that opening
//! before the next round. Cancelled openings publish no lifecycle completion.
//! A refill submits only the initially vacant ring prefix and decodes at most
//! 64 PCM chunks; concurrently reclaimed space belongs to the next round.
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
mod bitrate;
mod channels;
mod output;
mod playback;
mod source;
mod waveform;
mod worker;

use channels::Channel;
pub use output::devices;
pub(crate) use source::{SourceIdentity, metadata_budget, probe_file};
pub use source::{probe, probe_details};
pub use waveform::WaveformFrame;

pub fn ffmpeg_status() -> anyhow::Result<String> {
    ffmpeg::availability()
}

pub fn ffmpeg_load_attempted() -> bool {
    ffmpeg::load_attempted()
}

/// Scan a full envelope only for an explicit user request. Normal playback
/// publishes peaks directly from its existing decoded PCM instead.
pub fn waveform(
    path: &Path,
    range: Option<PlaybackRange>,
    decoder_hint: DecoderHint,
    media_read_buffer_mb: u32,
    ffmpeg_enabled: bool,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<WaveformFrame> {
    source::Source::waveform(
        path,
        range,
        decoder_hint,
        media_read_buffer_mb,
        ffmpeg_enabled,
        cancelled,
    )
}

use crate::analysis::{AnalysisControl, AnalysisFrame, AnalysisSettings, AnalysisWorker};
use crate::model::{DecoderHint, MediaInfo};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError, after, bounded, select};
use parking_lot::{Mutex, RwLock};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
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
    /// Update only the matching opening/playback generation, without reopening.
    Metadata {
        generation: u64,
        metadata: Arc<PlaybackMetadata>,
    },
    Pause(bool),
    Stop,
    Seek(f64),
    Volume(f32),
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
    StopAndSnapshot(Sender<(u64, f64)>),
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
}

pub struct AudioEngine {
    commands: Sender<AudioCommand>,
    pub events: Receiver<AudioEvent>,
    pub analysis: Arc<RwLock<AnalysisFrame>>,
    pub(crate) analysis_control: Arc<AnalysisControl>,
    pub waveform: Arc<RwLock<WaveformFrame>>,
    worker: Option<JoinHandle<()>>,
    shutdown: Mutex<Option<Sender<()>>>,
}
impl AudioEngine {
    pub fn new(media_read_buffer_mb: u32) -> Result<Self> {
        let (commands, receive) = bounded(64);
        let (send_events, events) = bounded(128);
        let (shutdown, cancelled) = bounded(0);
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
            events,
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
                TrySendError::Full(_) => {
                    anyhow::anyhow!("Audio command queue is full; command not queued")
                }
                TrySendError::Disconnected(_) => anyhow::anyhow!("Audio worker stopped"),
            })
    }

    /// Core is the sole ordinary producer. Until its next send, the worker can
    /// only free slots, so a complete batch can be admitted after persistence.
    pub(crate) fn ensure_command_capacity(&self, count: usize) -> Result<()> {
        let capacity = self
            .commands
            .capacity()
            .expect("audio commands are bounded");
        anyhow::ensure!(
            count <= capacity.saturating_sub(self.commands.len()),
            "Audio command queue has insufficient capacity; commands not queued"
        );
        Ok(())
    }

    pub(crate) fn stop_request(&self) -> AudioStop {
        AudioStop {
            commands: self.commands.clone(),
            events: self.events.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn replace_commands(
        &mut self,
        commands: Sender<AudioCommand>,
    ) -> Sender<AudioCommand> {
        std::mem::replace(&mut self.commands, commands)
    }
}

/// A stop-only handle cannot submit ordinary commands behind Core's preflight.
pub(crate) struct AudioStop {
    commands: Sender<AudioCommand>,
    events: Receiver<AudioEvent>,
}

impl AudioStop {
    pub(crate) fn wait(self, observe: impl FnMut(AudioEvent)) -> Result<(u64, f64)> {
        stop_and_snapshot(&self.commands, &self.events, observe)
    }
}

/// Preserve command order while freeing reverse-channel capacity until stop is acknowledged.
fn stop_and_snapshot(
    commands: &Sender<AudioCommand>,
    events: &Receiver<AudioEvent>,
    mut observe: impl FnMut(AudioEvent),
) -> Result<(u64, f64)> {
    let (reply, stopped) = bounded(1);
    let deadline = after(Duration::from_secs(5));
    loop {
        select! {
            send(commands, AudioCommand::StopAndSnapshot(reply)) -> sent => {
                sent.context("Audio worker stopped")?;
                break;
            }
            recv(events) -> event => {
                observe(event.context("Audio worker stopped before stop was submitted")?);
            }
            recv(deadline) -> _ => anyhow::bail!("Audio output did not stop before the deadline"),
        }
    }
    loop {
        select! {
            recv(stopped) -> snapshot => return snapshot.context("Audio worker stopped before acknowledging stop"),
            recv(events) -> event => {
                observe(event.context("Audio worker stopped before acknowledging stop")?);
            }
            recv(deadline) -> _ => anyhow::bail!("Audio output did not stop before the deadline"),
        }
    }
}
impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shutdown.get_mut().take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
