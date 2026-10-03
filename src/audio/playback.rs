//! One track's decode-to-output pipeline and audible playback accounting.

use super::{
    MediaInfo, Stereo,
    output::{Converter, Output},
    source::Source,
};
use crate::analysis::AnalysisWorker;
use anyhow::Result;
use crossbeam_channel::Receiver;
use std::path::{Path, PathBuf};

const PENDING_BUFFER_CAPACITY: usize = 16_384;

pub(super) struct PlaybackOptions {
    pub(super) path: PathBuf,
    pub(super) generation: u64,
    pub(super) start: f64,
    pub(super) paused: bool,
    pub(super) device: Option<String>,
    pub(super) volume: f32,
    pub(super) prior_heard: f64,
    pub(super) media_read_buffer_len: usize,
}

pub(super) struct Playback {
    source: Source,
    output: Output,
    converter: Converter,
    pending: Vec<Stereo>,
    offset: usize,
    generation: u64,
    path: PathBuf,
    base_position: f64,
    prior_heard: f64,
    eof: bool,
    paused: bool,
}
impl Playback {
    pub(super) fn new(options: PlaybackOptions, analyzer: &AnalysisWorker) -> Result<Self> {
        let PlaybackOptions {
            path,
            generation,
            start,
            paused,
            device,
            volume,
            prior_heard,
            media_read_buffer_len,
        } = options;
        let mut source = Source::open(&path, media_read_buffer_len)?;
        if start > 0.0 {
            source.seek(start)?;
        }
        let output = Output::new(device.as_deref(), volume, paused, analyzer)?;
        let converter = Converter::new(source.info().sample_rate, output.rate())?;
        Ok(Self {
            source,
            output,
            converter,
            pending: Vec::with_capacity(PENDING_BUFFER_CAPACITY),
            offset: 0,
            generation,
            path,
            base_position: start,
            prior_heard,
            eof: false,
            paused,
        })
    }
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn info(&self) -> &MediaInfo {
        self.source.info()
    }

    pub(super) fn paused(&self) -> bool {
        self.paused
    }

    pub(super) fn pause(&mut self, paused: bool) -> Result<()> {
        self.output.pause(paused)?;
        self.paused = paused;
        Ok(())
    }

    pub(super) fn set_volume(&self, volume: f32) {
        self.output.set_volume(volume);
    }

    pub(super) fn errors(&self) -> &Receiver<cpal::Error> {
        self.output.errors()
    }

    pub(super) fn heard(&self) -> f64 {
        self.prior_heard + self.output.heard()
    }
    pub(super) fn position(&self) -> f64 {
        let value = self.base_position + self.output.heard();
        self.source
            .info()
            .duration
            .map_or(value, |duration| value.min(duration))
    }
    pub(super) fn fill(&mut self) -> Result<()> {
        while self.output.has_room() {
            if self.offset < self.pending.len() {
                let count = self.output.push(&self.pending[self.offset..]);
                self.offset += count;
                if self.offset < self.pending.len() {
                    return Ok(());
                }
            }
            if self.eof {
                return Ok(());
            }
            self.pending.clear();
            self.offset = 0;
            if let Some(frames) = self.source.next_frames()? {
                self.converter.push(frames, &mut self.pending)?;
            } else {
                self.converter.finish(&mut self.pending)?;
                self.eof = true;
            }
        }
        Ok(())
    }
    pub(super) fn finished(&self) -> bool {
        self.eof && self.offset == self.pending.len() && self.output.drained()
    }
    pub(super) fn close(&mut self) -> f64 {
        self.prior_heard + self.output.close()
    }
}
