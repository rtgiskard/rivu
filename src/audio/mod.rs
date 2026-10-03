//! Public audio commands, events, and engine ownership.
//!
//! The worker schedules playback; playback owns the source-to-output pipeline.
//! Decoder internals and real-time output state remain in their respective modules.

mod output;
mod playback;
mod source;
mod worker;

pub use output::devices;
pub use source::probe;

use crate::analysis::{AnalysisFrame, AnalysisWorker};
use crate::library::MediaInfo;
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use parking_lot::RwLock;
use std::{
    path::PathBuf,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};
use worker::Worker;

const BYTES_PER_MEBIBYTE: usize = 1024 * 1024;

type Stereo = [f32; 2];

pub enum AudioCommand {
    Load {
        path: PathBuf,
        generation: u64,
        start_seconds: f64,
        paused: bool,
    },
    Pause(bool),
    Stop,
    Seek(f64),
    Volume(f32),
    Device(Option<String>),
    Analysis(bool),
    MediaReadBuffer(u32),
    /// Set analyzer publication rate. Values outside 5..=60 fps are ignored.
    AnalysisRate(u32),
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
    Failed {
        generation: u64,
        message: String,
    },
}

pub struct AudioEngine {
    pub commands: Sender<AudioCommand>,
    pub events: Receiver<AudioEvent>,
    pub analysis: Arc<RwLock<AnalysisFrame>>,
    worker: Option<JoinHandle<()>>,
}
impl AudioEngine {
    pub fn new(media_read_buffer_mb: u32) -> Result<Self> {
        let (commands, receive) = bounded(64);
        let (send_events, events) = bounded(128);
        let analysis = Arc::new(RwLock::new(AnalysisFrame::default()));
        let analyzer = AnalysisWorker::new(analysis.clone())?;
        let media_read_buffer_len = usize::try_from(media_read_buffer_mb)
            .expect("media read buffer size does not fit usize")
            .saturating_mul(BYTES_PER_MEBIBYTE);
        let worker = thread::Builder::new()
            .name("rivu-audio".into())
            .spawn(move || {
                Worker::new(receive, send_events, analyzer, media_read_buffer_len).run()
            })?;
        Ok(Self {
            commands,
            events,
            analysis,
            worker: Some(worker),
        })
    }
    pub fn send(&self, command: AudioCommand) -> Result<()> {
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
        let _ = self.commands.send(AudioCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
