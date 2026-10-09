//! One track's decode-to-output pipeline and audible playback accounting.

use super::{
    MediaInfo, PlaybackRange, WaveformFrame,
    output::{Output, OutputError},
    source::Source,
    waveform::{WaveformAccumulator, WaveformCoverage},
};
use crate::analysis::AnalysisWorker;
use anyhow::{Result, bail};
use crossbeam_channel::Receiver;
use parking_lot::RwLock;
use std::{path::PathBuf, sync::Arc};

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
    pub(super) pipewire_auto_mix: bool,
    pub(super) still_mode: bool,
}

pub(super) struct Playback {
    source: Source,
    output: Output,
    pending: Vec<f32>,
    offset_frames: usize,
    generation: u64,
    base_position: f64,
    prior_heard: f64,
    eof: bool,
    paused: bool,
    waveform: Arc<RwLock<WaveformFrame>>,
    waveform_accumulator: WaveformAccumulator,
    waveform_coverage: WaveformCoverage,
    still_mode: bool,
}
impl Playback {
    pub(super) fn new(
        options: PlaybackOptions,
        analyzer: &AnalysisWorker,
        waveform: Arc<RwLock<WaveformFrame>>,
    ) -> Result<Self> {
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
            pipewire_auto_mix,
            still_mode,
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
            pipewire_auto_mix,
            analyzer,
        )?;
        let weights = super::output::analysis_weights(source.layout());
        let mut waveform_accumulator = {
            let mut waveform = waveform.write();
            waveform.select(
                &path,
                range,
                source.info().duration,
                source.info().sample_rate,
            );
            WaveformAccumulator::new(waveform.clone())
        };
        waveform_accumulator.set_channel_weights(&weights);
        Ok(Self {
            source,
            output,
            pending: Vec::with_capacity(PENDING_BUFFER_CAPACITY * channels),
            offset_frames: 0,
            generation,
            base_position: start,
            prior_heard,
            eof: false,
            paused,
            waveform,
            waveform_accumulator,
            waveform_coverage: WaveformCoverage::new(start),
            still_mode,
        })
    }
    // Rebuild only device state: closing first discards queued PCM,
    // device-clock spans and the old analysis epoch before the source seeks.
    pub(super) fn restart_output(
        &mut self,
        target: Option<f64>,
        device: Option<&str>,
        volume: f32,
        auto_mix: bool,
        analyzer: &AnalysisWorker,
    ) -> Result<()> {
        self.restart_output_with(target, |source, paused| {
            Output::new(
                device,
                source.info().sample_rate,
                source.layout(),
                volume,
                paused,
                auto_mix,
                analyzer,
            )
        })
    }

    fn restart_output_with(
        &mut self,
        target: Option<f64>,
        create: impl FnOnce(&Source, bool) -> Result<Output>,
    ) -> Result<()> {
        let position = target.unwrap_or_else(|| self.position()).max(0.0);
        let position = self
            .info()
            .duration
            .map_or(position, |end| position.min(end));
        let heard = self.prior_heard + self.output.close();
        self.source.seek(position)?;
        let output = create(&self.source, self.paused)?;
        self.output = output;
        self.pending.clear();
        self.offset_frames = 0;
        self.base_position = position;
        self.prior_heard = heard;
        self.eof = false;
        self.waveform_coverage = WaveformCoverage::new(position);
        Ok(())
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
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

    pub(super) fn set_still_mode(&mut self, enabled: bool) {
        self.still_mode = enabled;
    }

    pub(super) fn errors(&self) -> &Receiver<OutputError> {
        self.output.errors()
    }

    pub(super) fn notifications(&self) -> &Receiver<()> {
        self.output.notifications()
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
    pub(super) fn refill_delay(&self) -> std::time::Duration {
        self.output.refill_delay(self.eof)
    }

    pub(super) fn fill(&mut self) -> Result<()> {
        self.output.check_timing()?;
        if self.paused {
            return Ok(());
        }
        let channels = self.source.layout().len();
        let sample_rate = self.source.info().sample_rate;
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
            if let Some((frames, start_seconds)) = self.source.next_frames()? {
                if !frames.len().is_multiple_of(channels) {
                    bail!("Decoded audio ends in an incomplete interleaved frame");
                }
                if self.still_mode {
                    if !frames.is_empty() {
                        self.waveform_coverage.skip();
                    }
                } else {
                    self.waveform_coverage.record(
                        start_seconds,
                        frames.len() / channels,
                        sample_rate,
                    );
                    self.waveform_accumulator
                        .push(start_seconds, frames, sample_rate, channels);
                    self.waveform_accumulator
                        .publish(&mut self.waveform.write());
                }
                // Submit decoded PCM directly; retain only the packet tail that
                // did not fit in the bounded output ring. At most one decoded
                // packet is pending, and its allocation is reused on refill.
                let count = self.output.push(frames);
                self.pending.extend_from_slice(&frames[count * channels..]);
            } else {
                if !self.still_mode {
                    self.waveform_accumulator.finish(&self.waveform_coverage);
                    self.waveform_accumulator
                        .publish(&mut self.waveform.write());
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn pcm() -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let rate = 8_000_u32;
        let bytes = rate * 2 * 2;
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + bytes).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt \x10\0\0\0\x01\0\x01\0").unwrap();
        file.write_all(&rate.to_le_bytes()).unwrap();
        file.write_all(&(rate * 2).to_le_bytes()).unwrap();
        file.write_all(b"\x02\0\x10\0data").unwrap();
        file.write_all(&bytes.to_le_bytes()).unwrap();
        for index in 0..rate * 2 {
            file.write_all(&((index % 16000) as i16).to_le_bytes())
                .unwrap();
        }
        file
    }

    fn test_playback(
        path: &std::path::Path,
        range: PlaybackRange,
    ) -> (Playback, impl FnMut(&mut [f32], u64) + use<>) {
        let mut source = Source::open(path, 64 * 1024, false).unwrap();
        source.restrict(range).unwrap();
        let waveform = Arc::new(RwLock::new(WaveformFrame::default()));
        waveform
            .write()
            .select(path, Some(range), source.info().duration, 8_000);
        let (output, render) = super::super::output::test_backend(8_000, false, source.layout());
        let waveform_accumulator = WaveformAccumulator::new(waveform.read().clone());
        let playback = Playback {
            source,
            output,
            pending: Vec::new(),
            offset_frames: 0,
            generation: 1,
            base_position: 0.0,
            prior_heard: 0.0,
            eof: false,
            paused: false,
            waveform,
            waveform_accumulator,
            waveform_coverage: WaveformCoverage::new(0.0),
            still_mode: false,
        };
        (playback, render)
    }
    #[test]
    fn still_mode_preserves_pcm_and_never_completes_an_interrupted_envelope() {
        let file = pcm();
        for resume_before_eof in [false, true] {
            let (mut playback, mut render) = test_playback(
                file.path(),
                PlaybackRange {
                    start_seconds: 0.0,
                    end_seconds: Some(2.0),
                },
            );
            playback.fill().unwrap();
            let initial = playback.waveform.read().clone();
            assert!(!initial.complete);
            playback.set_still_mode(true);
            let mut data = [0.0; 160];
            for block in 0..100 {
                if resume_before_eof && block == 30 {
                    let suspended = playback.waveform.read();
                    assert_eq!(suspended.revision, initial.revision);
                    assert_eq!(suspended.rms_counts, initial.rms_counts);
                    drop(suspended);
                    playback.set_still_mode(false);
                }
                playback.fill().unwrap();
                render(&mut data, block * 20);
                for (offset, sample) in data.iter().enumerate() {
                    assert_eq!(*sample, (block as usize * 160 + offset) as f32 / 32768.0);
                }
            }
            playback.fill().unwrap();
            assert!(playback.eof);
            let interrupted = playback.waveform.read().clone();
            assert!(!interrupted.complete);
            if resume_before_eof {
                assert!(interrupted.revision > initial.revision);
                assert_ne!(interrupted.rms_counts, initial.rms_counts);
            } else {
                assert_eq!(interrupted.revision, initial.revision);
                assert_eq!(interrupted.rms_counts, initial.rms_counts);
                playback.set_still_mode(false);
                playback.fill().unwrap();
                assert!(!playback.waveform.read().complete);
            }
        }
    }

    #[test]
    fn still_mode_output_restarts_keep_gaps_but_a_fresh_scan_recovers() {
        let file = pcm();
        let (mut playback, _) = test_playback(
            file.path(),
            PlaybackRange {
                start_seconds: 0.0,
                end_seconds: Some(2.0),
            },
        );
        playback.fill().unwrap();
        playback.set_still_mode(true);
        for target in [None, Some(0.0), Some(0.5)] {
            let initial_revision = playback.waveform.read().revision;
            let (output, mut render) =
                super::super::output::test_backend(8_000, false, playback.source.layout());
            playback
                .restart_output_with(target, |_, _| Ok(output))
                .unwrap();
            playback.fill().unwrap();
            assert_eq!(playback.waveform.read().revision, initial_revision);
            playback.set_still_mode(false);
            let mut data = [0.0; 160];
            for block in 0..110 {
                playback.fill().unwrap();
                render(&mut data, block * 20);
            }
            assert!(playback.eof);
            assert!(!playback.waveform.read().complete);
            playback.set_still_mode(true);
        }
        playback.set_still_mode(false);
        let (output, mut render) =
            super::super::output::test_backend(8_000, false, playback.source.layout());
        playback
            .restart_output_with(Some(0.0), |_, _| Ok(output))
            .unwrap();
        let mut data = [0.0; 160];
        for block in 0..110 {
            playback.fill().unwrap();
            render(&mut data, block * 20);
        }
        assert!(playback.eof);
        assert!(playback.waveform.read().complete);
    }

    #[test]
    fn decoded_pcm_refill_preserves_cue_samples_and_waits_for_audible_tail() {
        let file = pcm();
        let (mut playback, mut render) = test_playback(
            file.path(),
            PlaybackRange {
                start_seconds: 0.25,
                end_seconds: Some(1.75),
            },
        );
        let mut data = [0.0; 160];
        for block in 0..75 {
            playback.fill().unwrap();
            render(&mut data, block * 20);
            for (offset, sample) in data.iter().enumerate() {
                assert_eq!(
                    *sample,
                    (2000 + block as usize * 160 + offset) as f32 / 32768.0
                );
            }
        }
        playback.fill().unwrap();
        assert!(playback.eof);
        assert!(!playback.finished());
        render(&mut data, 1500);
        assert!(data.iter().all(|sample| *sample == 0.0));
        assert!(!playback.finished());
        render(&mut data, 1520);
        assert!(playback.finished());
        assert_eq!(playback.heard(), 1.5);
        assert_eq!(playback.position(), 1.5);
    }

    #[test]
    fn repeated_paused_cue_seeks_discard_old_pcm_and_preserve_exact_position() {
        let file = pcm();
        let range = PlaybackRange {
            start_seconds: 0.25,
            end_seconds: Some(1.75),
        };
        let (mut playback, mut old_render) = test_playback(file.path(), range);
        playback.fill().unwrap();
        // A complete manually installed waveform must survive later live PCM.
        playback.waveform.write().finish_scan(1.5);
        let installed = playback.waveform.read().clone();
        playback.pause(true);
        for target in [0.75, 0.125, 1.0, 0.0] {
            let (output, mut render) =
                super::super::output::test_backend(8_000, true, playback.source.layout());
            playback
                .restart_output_with(Some(target), |_, paused| {
                    assert!(paused);
                    Ok(output)
                })
                .unwrap();
            assert_eq!(playback.position(), target);
            assert_eq!(playback.heard(), 0.0);
            let mut data = [1.0; 64];
            old_render(&mut data, 0);
            assert_eq!(data, [0.0; 64]);
            playback.fill().unwrap();
            render(&mut data, 0);
            assert_eq!(data, [0.0; 64]);
            playback.pause(false);
            playback.fill().unwrap();
            render(&mut data, 1);
            let first = ((target + 0.25) * 8_000.0).round() as usize;
            for (offset, sample) in data.iter().enumerate() {
                assert_eq!(*sample, (first + offset) as f32 / 32768.0);
            }
            playback.pause(true);
        }
        assert!(playback.waveform.read().complete);
        assert_eq!(playback.waveform.read().peaks, installed.peaks);
        assert_eq!(playback.waveform.read().rms_counts, installed.rms_counts);
    }
}
