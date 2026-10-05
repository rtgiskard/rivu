//! Public audio commands, events, and engine ownership.
//!
//! The worker schedules playback; playback owns the source-to-output pipeline.
//! Decoder internals and real-time output state remain in their respective modules.
//!
//! Pause is software-gated: the device continues receiving silence without
//! consuming queued music. End-of-track waits for CPAL's stream clock to pass
//! the final valid frame's predicted playback time, not merely an empty ring.
//! Listened time is a conservative, callback-confirmed count; unconfirmed
//! device buffers on stop or xrun recovery are not counted as heard. Accuracy
//! at the physical output remains bounded by the backend's playback timestamps.
//!
//! Lifecycle events use bounded worker-thread backpressure; progress is best
//! effort. Consumers must keep draining events while submitting commands.
//! Shutdown cancels blocked event delivery without involving the output callback.

mod detector;
#[cfg(feature = "ffmpeg")]
mod ffmpeg;
#[cfg(not(feature = "ffmpeg"))]
mod ffmpeg {
    use super::{Channel, MediaInfo};
    use anyhow::{Result, bail};
    use std::path::Path;

    pub(super) fn availability() -> Result<String> {
        bail!("FFmpeg extension is not compiled; rebuild with --features ffmpeg")
    }

    pub(super) struct Decoder;

    impl Decoder {
        pub(super) fn open(_: &Path) -> Result<Self> {
            bail!("FFmpeg extension is not compiled; rebuild with --features ffmpeg")
        }

        pub(super) fn info(&self) -> &MediaInfo {
            unreachable!("FFmpeg decoder cannot be opened without the ffmpeg feature")
        }

        pub(super) fn layout(&self) -> &[Channel] {
            unreachable!("FFmpeg decoder cannot be opened without the ffmpeg feature")
        }

        pub(super) fn next_frames(&mut self) -> Result<Option<(&[f32], f64)>> {
            bail!("FFmpeg extension is not compiled; rebuild with --features ffmpeg")
        }

        pub(super) fn seek(&mut self, _: f64) -> Result<()> {
            bail!("FFmpeg extension is not compiled; rebuild with --features ffmpeg")
        }
    }
}
mod format;
mod output;
mod playback;
mod source;
mod waveform;
mod worker;

use format::Channel;
pub use output::devices;
pub use source::probe;
pub use waveform::WaveformFrame;

pub fn ffmpeg_status() -> anyhow::Result<String> {
    ffmpeg::availability()
}

/// Scan a full envelope only for an explicit user request. Normal playback
/// publishes peaks directly from its existing decoded PCM instead.
pub fn waveform(
    path: &Path,
    range: Option<PlaybackRange>,
    media_read_buffer_mb: u32,
    ffmpeg_enabled: bool,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<WaveformFrame> {
    source::Source::waveform(path, range, media_read_buffer_mb, ffmpeg_enabled, cancelled)
}

use crate::analysis::{AnalysisFrame, AnalysisSettings, AnalysisWorker};
use crate::library::MediaInfo;
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
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

pub enum AudioCommand {
    Load {
        path: PathBuf,
        range: Option<PlaybackRange>,
        generation: u64,
        start_seconds: f64,
        paused: bool,
    },
    Pause(bool),
    Stop,
    Seek(f64),
    Volume(f32),
    OutputSettings {
        device: Option<String>,
        auto_mix: bool,
    },
    Analysis(bool),
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
    },
    DecoderStopped {
        generation: u64,
    },
    Failed {
        generation: u64,
        message: String,
    },
}

pub struct AudioEngine {
    pub commands: Sender<AudioCommand>,
    pub events: Receiver<AudioEvent>,
    pub analysis: Arc<RwLock<AnalysisFrame>>,
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
            waveform,
            worker: Some(worker),
            shutdown: Mutex::new(Some(shutdown)),
        })
    }
    pub fn send(&self, command: AudioCommand) -> Result<()> {
        if matches!(command, AudioCommand::Shutdown) {
            self.shutdown.lock().take();
            return Ok(());
        }
        self.commands.send(command).context("Audio worker stopped")
    }
    pub fn stop_and_snapshot(&self) -> Result<(u64, f64)> {
        let (send, receive) = bounded(1);
        self.send(AudioCommand::StopAndSnapshot(send))?;
        receive
            .recv_timeout(Duration::from_secs(5))
            .context("Audio output did not stop")
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
