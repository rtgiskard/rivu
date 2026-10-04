//! One track's decode-to-output pipeline and audible playback accounting.

use super::{
    MediaInfo, PlaybackRange,
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
    pub(super) range: Option<PlaybackRange>,
    pub(super) generation: u64,
    pub(super) start: f64,
    pub(super) paused: bool,
    pub(super) device: Option<String>,
    pub(super) volume: f32,
    pub(super) prior_heard: f64,
    pub(super) media_read_buffer_len: usize,
    pub(super) ffmpeg_enabled: bool,
}

pub(super) struct Playback {
    source: Source,
    output: Output,
    converter: Converter,
    pending: Vec<f32>,
    offset_frames: usize,
    generation: u64,
    path: PathBuf,
    range: Option<PlaybackRange>,
    base_position: f64,
    prior_heard: f64,
    eof: bool,
    paused: bool,
}
impl Playback {
    pub(super) fn new(options: PlaybackOptions, analyzer: &AnalysisWorker) -> Result<Self> {
        let PlaybackOptions {
            path,
            range,
            generation,
            start,
            paused,
            device,
            volume,
            prior_heard,
            media_read_buffer_len,
            ffmpeg_enabled,
        } = options;
        let mut source = Source::open(&path, media_read_buffer_len, ffmpeg_enabled)?;
        if let Some(range) = range {
            source.restrict(range)?;
        }
        if start > 0.0 {
            source.seek(start)?;
        }
        let channels = source.layout().len();
        let output = Output::new(
            device.as_deref(),
            source.info().sample_rate,
            source.layout(),
            volume,
            paused,
            analyzer,
        )?;
        let converter = Converter::new(source.info().sample_rate, output.rate(), channels)?;
        Ok(Self {
            source,
            output,
            converter,
            pending: Vec::with_capacity(PENDING_BUFFER_CAPACITY * channels),
            offset_frames: 0,
            generation,
            path,
            range,
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

    pub(super) fn range(&self) -> Option<PlaybackRange> {
        self.range
    }

    pub(super) fn info(&self) -> &MediaInfo {
        self.source.info()
    }

    pub(super) fn uses_ffmpeg(&self) -> bool {
        self.source.uses_ffmpeg()
    }

    pub(super) fn paused(&self) -> bool {
        self.paused
    }

    pub(super) fn pause(&mut self, paused: bool) {
        self.output.pause(paused);
        self.paused = paused;
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
        let value = self.base_position + self.output.position();
        self.source
            .info()
            .duration
            .map_or(value, |duration| value.min(duration))
    }
    pub(super) fn fill(&mut self) -> Result<()> {
        self.output.check_timing()?;
        if self.paused {
            return Ok(());
        }
        let channels = self.source.layout().len();
        while self.output.has_room() {
            if self.offset_frames < self.pending.len() / channels {
                let count = self
                    .output
                    .push(&self.pending[self.offset_frames * channels..]);
                self.offset_frames += count;
                if self.offset_frames < self.pending.len() / channels {
                    return Ok(());
                }
            }
            if self.eof {
                return Ok(());
            }
            self.pending.clear();
            self.offset_frames = 0;
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
        !self.paused
            && self.eof
            && self.offset_frames == self.pending.len() / self.source.layout().len()
            && self.output.drained()
    }
    pub(super) fn close(&mut self) -> f64 {
        self.prior_heard + self.output.close()
    }
}
