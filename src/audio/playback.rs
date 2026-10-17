//! One track's decode-to-output pipeline and audible playback accounting.

use super::{
    MediaInfo, PlaybackRange, WaveformFrame,
    output::{Output, OutputError},
    source::{DECODE_WORK_BUDGET, DecodeStep, Source},
    waveform::{WaveformAccumulator, WaveformCoverage},
};
use crate::analysis::AnalysisWorker;
use crate::model::DecoderHint;
use anyhow::{Result, bail};
use crossbeam_channel::Receiver;
use parking_lot::RwLock;
use std::{path::PathBuf, sync::Arc};

const PENDING_BUFFER_CAPACITY: usize = 16_384;

pub(super) struct PlaybackOptions {
    pub(super) path: PathBuf,
    pub(super) range: Option<PlaybackRange>,
    pub(super) decoder_hint: DecoderHint,
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
    waveform_accumulator: Option<WaveformAccumulator>,
    waveform_weights: Vec<[f32; 2]>,
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
            decoder_hint,
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
        let mut source = Source::open(&path, media_read_buffer_len, decoder_hint, ffmpeg_enabled)?;
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
        let waveform_weights = super::output::analysis_weights(source.layout());
        {
            let mut waveform = waveform.write();
            waveform.set_still_mode(still_mode);
            waveform.select_source(&path, range, decoder_hint, &source);
        }
        let waveform_coverage = WaveformCoverage::new(start, source.waveform_timestamp_tolerance());
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
            waveform_accumulator: None,
            waveform_weights,
            waveform_coverage,
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
        let heard = self.prior_heard + self.output.close();
        self.source.seek(position)?;
        let output = create(&self.source, self.paused)?;
        self.output = output;
        self.pending.clear();
        self.offset_frames = 0;
        self.base_position = position;
        self.prior_heard = heard;
        self.eof = false;
        self.waveform_coverage =
            WaveformCoverage::new(position, self.source.waveform_timestamp_tolerance());
        Ok(())
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn info(&self) -> &MediaInfo {
        self.source.info()
    }

    pub(super) fn completed_bitrate(&self) -> Option<u64> {
        self.finished()
            .then(|| self.source.completed_bitrate())
            .flatten()
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
        if self.still_mode == enabled {
            return;
        }
        if enabled {
            self.waveform_accumulator = None;
            // Discarding the prefix invalidates coverage even when no PCM is
            // decoded during a quick on/off transition.
            self.waveform_coverage.skip();
        }
        self.waveform.write().set_still_mode(enabled);
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
        self.base_position + self.output.position()
    }
    pub(super) fn refill_delay(&self) -> std::time::Duration {
        self.output.refill_delay(self.eof)
    }

    /// Returns whether another finite refill can run without waiting.
    pub(super) fn fill(&mut self) -> Result<bool> {
        self.fill_with(|| {})
    }

    fn fill_with(&mut self, mut after_packet: impl FnMut()) -> Result<bool> {
        self.output.check_timing()?;
        if self.paused {
            return Ok(false);
        }
        let channels = self.source.layout().len();
        let sample_rate = self.source.info().sample_rate;
        // Concurrent callback consumption belongs to the next refill, not this
        // prefix. Both submitted frames and decoded packets bound this round.
        let mut remaining_frames = self.output.vacant_frames();
        let mut remaining_packets = DECODE_WORK_BUDGET;
        while remaining_frames > 0 {
            if self.offset_frames < self.pending.len() / channels {
                let end =
                    (self.offset_frames + remaining_frames).min(self.pending.len() / channels);
                let count = self
                    .output
                    .push(&self.pending[self.offset_frames * channels..end * channels]);
                remaining_frames -= count;
                self.offset_frames += count;
                if self.offset_frames < self.pending.len() / channels {
                    break;
                }
            }
            if self.eof || remaining_frames == 0 || remaining_packets == 0 {
                break;
            }
            self.pending.clear();
            self.offset_frames = 0;
            match self
                .source
                .next_frames_with_budget(&mut remaining_packets)?
            {
                DecodeStep::Frames((frames, start_seconds)) => {
                    if !frames.len().is_multiple_of(channels) {
                        bail!("Decoded audio ends in an incomplete interleaved frame");
                    }
                    if !frames.is_empty() {
                        self.waveform_coverage.record(
                            start_seconds,
                            frames.len() / channels,
                            sample_rate,
                        );
                    }
                    if self.still_mode {
                        if !frames.is_empty() {
                            self.waveform_coverage.skip();
                        }
                    } else if !frames.is_empty() {
                        let accumulator = self.waveform_accumulator.get_or_insert_with(|| {
                            let mut waveform = self.waveform.write();
                            waveform.prepare_bins(sample_rate);
                            let mut accumulator = WaveformAccumulator::new(waveform.clone());
                            accumulator.set_channel_weights(&self.waveform_weights);
                            accumulator
                        });
                        accumulator.push(start_seconds, frames, sample_rate, channels);
                        accumulator.publish(&mut self.waveform.write());
                    }
                    // Submit PCM directly; retain only the packet tail that
                    // did not fit, reusing its allocation on the next refill.
                    let prefix = frames.len().min(remaining_frames * channels);
                    let count = self.output.push(&frames[..prefix]);
                    remaining_frames -= count;
                    self.pending.extend_from_slice(&frames[count * channels..]);
                    after_packet();
                }
                DecodeStep::Yield => break,
                DecodeStep::Eof => {
                    if let Some(accumulator) = &mut self.waveform_accumulator {
                        accumulator.finish(&self.waveform_coverage);
                        accumulator.publish(&mut self.waveform.write());
                    }
                    self.eof = true;
                }
            }
        }
        Ok(
            (!self.eof || self.offset_frames < self.pending.len() / channels)
                && self.output.vacant_frames() > 0,
        )
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
    use std::io::{Seek, SeekFrom, Write};

    fn pcm() -> tempfile::NamedTempFile {
        pcm_samples(16_000)
    }

    fn pcm_samples(samples: u32) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let rate = 8_000_u32;
        let bytes = samples * 2;
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + bytes).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt \x10\0\0\0\x01\0\x01\0").unwrap();
        file.write_all(&rate.to_le_bytes()).unwrap();
        file.write_all(&(rate * 2).to_le_bytes()).unwrap();
        file.write_all(b"\x02\0\x10\0data").unwrap();
        file.write_all(&bytes.to_le_bytes()).unwrap();
        for index in 0..samples {
            file.write_all(&((index % 16000) as i16).to_le_bytes())
                .unwrap();
        }
        file
    }

    fn pcm_level(samples: u32, level: i16) -> tempfile::NamedTempFile {
        let mut file = pcm_samples(samples);
        file.seek(SeekFrom::Start(44)).unwrap();
        for _ in 0..samples {
            file.write_all(&level.to_le_bytes()).unwrap();
        }
        file
    }

    fn test_playback(
        path: &std::path::Path,
        range: PlaybackRange,
    ) -> (Playback, impl FnMut(&mut [f32], u64) + use<>) {
        test_playback_with_waveform(
            path,
            Some(range),
            Arc::new(RwLock::new(WaveformFrame::default())),
        )
    }

    fn test_playback_with_waveform(
        path: &std::path::Path,
        range: Option<PlaybackRange>,
        waveform: Arc<RwLock<WaveformFrame>>,
    ) -> (Playback, impl FnMut(&mut [f32], u64) + use<>) {
        let mut source = Source::open(path, 64 * 1024, DecoderHint::Auto, false).unwrap();
        if let Some(range) = range {
            source.restrict(range).unwrap();
        }
        waveform
            .write()
            .select_source(path, range, DecoderHint::Auto, &source);
        let (output, render) = super::super::output::test_backend(8_000, false, source.layout());
        let waveform_weights = super::super::output::analysis_weights(source.layout());
        let waveform_coverage = WaveformCoverage::new(0.0, source.waveform_timestamp_tolerance());
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
            waveform_accumulator: None,
            waveform_weights,
            waveform_coverage,
            still_mode: false,
        };
        (playback, render)
    }

    #[test]
    fn same_path_replacement_retires_complete_envelopes_but_not_an_open_decoder() {
        let file = pcm_level(320, 3277);
        let waveform = Arc::new(RwLock::new(WaveformFrame::default()));
        let (mut first, mut render) =
            test_playback_with_waveform(file.path(), None, waveform.clone());
        first.fill().unwrap();
        render(&mut [0.0; 320], 0);
        render(&mut [0.0; 160], 100);
        assert!(first.finished());
        let completed = waveform.read().clone();
        assert!(completed.complete);
        assert!(
            completed
                .peaks
                .iter()
                .all(|peak| *peak == Some(3277.0 / 32768.0))
        );

        // An unchanged Load reuses the completed result without a PCM scan or
        // allocating a playback accumulator before ordinary playback needs it.
        let (mut reopened, _) = test_playback_with_waveform(file.path(), None, waveform.clone());
        assert_eq!(waveform.read().revision, completed.revision);
        assert_eq!(waveform.read().identity, completed.identity);
        assert!(reopened.waveform_accumulator.is_none());
        assert!(!reopened.eof);

        let replacement = pcm_level(320, 26214);
        std::fs::rename(replacement.path(), file.path()).unwrap();
        // The decoder already holds A, even though the path now names B.
        reopened.fill().unwrap();
        assert_eq!(waveform.read().revision, completed.revision);
        assert_eq!(waveform.read().peaks, completed.peaks);
        let (output, mut render) =
            super::super::output::test_backend(8_000, false, reopened.source.layout());
        reopened
            .restart_output_with(Some(0.0), |_, _| Ok(output))
            .unwrap();
        reopened.fill().unwrap();
        let mut samples = [0.0; 320];
        render(&mut samples, 0);
        assert!(samples.iter().all(|sample| *sample == 3277.0 / 32768.0));
        assert_eq!(waveform.read().identity, completed.identity);
        assert_eq!(waveform.read().rms_counts, completed.rms_counts);

        let (mut fresh, mut render) =
            test_playback_with_waveform(file.path(), None, waveform.clone());
        assert!(waveform.read().revision > completed.revision);
        assert_ne!(waveform.read().identity, completed.identity);
        assert!(!waveform.read().complete);
        assert!(waveform.read().peaks.is_empty());
        fresh.fill().unwrap();
        render(&mut samples, 0);
        render(&mut [0.0; 160], 100);
        assert!(fresh.finished());
        assert!(samples.iter().all(|sample| *sample == 26214.0 / 32768.0));
        assert!(waveform.read().complete);
        assert!(
            waveform
                .read()
                .peaks
                .iter()
                .all(|peak| *peak == Some(26214.0 / 32768.0))
        );
        assert!(waveform.read().revision > completed.revision);
    }

    #[test]
    fn seeking_retains_decoded_coverage_without_reidentifying_the_path() {
        let file = pcm_level(16_000, 3277);
        let (mut playback, _) = test_playback(
            file.path(),
            PlaybackRange {
                start_seconds: 0.0,
                end_seconds: Some(2.0),
            },
        );
        playback.fill().unwrap();
        let before = playback.waveform.read().clone();
        assert!(!before.complete);
        assert!(before.peaks.iter().any(Option::is_some));
        let replacement = pcm_level(16_000, 26214);
        std::fs::rename(replacement.path(), file.path()).unwrap();
        let (output, mut render) =
            super::super::output::test_backend(8_000, false, playback.source.layout());
        playback
            .restart_output_with(Some(1.0), |_, _| Ok(output))
            .unwrap();
        assert_eq!(playback.waveform.read().identity, before.identity);
        assert_eq!(playback.waveform.read().revision, before.revision);
        assert_eq!(playback.waveform.read().peaks, before.peaks);
        playback.fill().unwrap();
        let mut samples = [0.0; 160];
        render(&mut samples, 0);
        assert!(samples.iter().all(|sample| *sample == 3277.0 / 32768.0));
        let after = playback.waveform.read();
        assert_eq!(after.identity, before.identity);
        assert!(!after.complete);
        for (old, new) in before.peaks.iter().zip(&after.peaks) {
            if old.is_some() {
                assert_eq!(new, old);
            }
        }
        assert!(
            after.peaks.iter().filter(|peak| peak.is_some()).count()
                > before.peaks.iter().filter(|peak| peak.is_some()).count()
        );
    }

    fn assert_waveform_absent(playback: &Playback) {
        assert!(playback.waveform_accumulator.is_none());
        assert_eq!(playback.waveform.read().retained_capacity_bytes(), 0);
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
            assert_waveform_absent(&playback);
            playback.fill().unwrap();
            let initial = playback.waveform.read().clone();
            assert!(!initial.complete);
            let pending = playback.pending.clone();
            let offset = playback.offset_frames;
            let position = playback.position();
            let heard = playback.heard();
            playback.set_still_mode(true);
            let suspended_revision = playback.waveform.read().revision;
            assert!(suspended_revision > initial.revision);
            assert_waveform_absent(&playback);
            assert_eq!(playback.pending, pending);
            assert_eq!(playback.offset_frames, offset);
            assert_eq!(playback.position(), position);
            assert_eq!(playback.heard(), heard);
            let mut data = [0.0; 160];
            for block in 0..100 {
                if resume_before_eof && block == 30 {
                    assert_eq!(playback.waveform.read().revision, suspended_revision);
                    assert_waveform_absent(&playback);
                    playback.set_still_mode(false);
                    assert_waveform_absent(&playback);
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
                assert!(interrupted.revision > suspended_revision);
                assert!(interrupted.rms_counts.iter().sum::<u64>() < 16_000);
                for (old, fresh) in initial.peaks.iter().zip(&interrupted.peaks) {
                    if old.is_some() {
                        assert!(fresh.is_none());
                    }
                }
                assert!(interrupted.retained_capacity_bytes() <= initial.retained_capacity_bytes());
            } else {
                assert_eq!(interrupted.revision, suspended_revision);
                assert_waveform_absent(&playback);
                playback.set_still_mode(false);
                playback.fill().unwrap();
                assert!(!playback.waveform.read().complete);
                assert_waveform_absent(&playback);
            }
        }
    }

    #[test]
    fn repeated_paused_still_roundtrips_do_not_restore_or_complete_old_coverage() {
        let file = pcm();
        let (mut playback, mut render) = test_playback(
            file.path(),
            PlaybackRange {
                start_seconds: 0.0,
                end_seconds: Some(2.0),
            },
        );
        playback.fill().unwrap();
        playback.pause(true);
        let retained_revision = playback.waveform.read().revision;
        let pending = playback.pending.clone();
        let pending_capacity = playback.pending.capacity();
        for pass in 0..32 {
            playback.set_still_mode(true);
            let revision = playback.waveform.read().revision;
            assert_eq!(revision, retained_revision + pass + 1);
            playback.set_still_mode(true);
            playback.fill().unwrap();
            assert_eq!(playback.waveform.read().revision, revision);
            assert_waveform_absent(&playback);
            playback.set_still_mode(false);
            playback.fill().unwrap();
            assert!(playback.paused());
            assert_eq!(playback.pending, pending);
            assert_eq!(playback.pending.capacity(), pending_capacity);
            assert_waveform_absent(&playback);
        }
        playback.pause(false);
        let mut data = [0.0; 160];
        for block in 0..100 {
            playback.fill().unwrap();
            render(&mut data, block * 20);
            for (offset, sample) in data.iter().enumerate() {
                assert_eq!(*sample, (block as usize * 160 + offset) as f32 / 32768.0);
            }
        }
        playback.fill().unwrap();
        assert!(playback.eof);
        assert!(!playback.waveform.read().complete);
        assert!(playback.waveform.read().rms_counts.iter().sum::<u64>() < 16_000);
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
        let capacity_bound = playback.waveform.read().retained_capacity_bytes();
        playback.set_still_mode(true);
        for target in [None, Some(0.0), Some(0.5)] {
            let initial_revision = playback.waveform.read().revision;
            let (output, mut render) =
                super::super::output::test_backend(8_000, false, playback.source.layout());
            playback
                .restart_output_with(target, |_, _| Ok(output))
                .unwrap();
            playback.fill().unwrap();
            assert_waveform_absent(&playback);
            playback.set_still_mode(true);
            assert_waveform_absent(&playback);
            assert_eq!(playback.waveform.read().revision, initial_revision);
            playback.set_still_mode(false);
            assert_waveform_absent(&playback);
            let mut data = [0.0; 160];
            for block in 0..110 {
                playback.fill().unwrap();
                render(&mut data, block * 20);
            }
            assert!(playback.eof);
            assert!(!playback.waveform.read().complete);
            assert!(playback.waveform.read().retained_capacity_bytes() <= capacity_bound);
            playback.set_still_mode(true);
            assert_waveform_absent(&playback);
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
        let waveform = playback.waveform.read();
        assert!(waveform.rms_counts.iter().all(|count| *count == 10));
        assert!(waveform.left_rms_counts.iter().all(|count| *count == 10));
        assert!(waveform.right_rms_counts.iter().all(|count| *count == 10));
        for (bin, rms) in waveform.rms.iter().enumerate() {
            let energy: f64 = (bin * 10..bin * 10 + 10)
                .map(|index| (index as f64 / 32768.0).powi(2))
                .sum();
            let expected = (energy / 10.0).sqrt() as f32;
            assert!((rms.unwrap() - expected).abs() < 1e-6);
        }
    }

    #[test]
    fn refill_snapshot_yields_to_consumption_and_preserves_the_packet_tail() {
        let file = pcm();
        let (mut playback, mut render) = test_playback(
            file.path(),
            PlaybackRange {
                start_seconds: 0.0,
                end_seconds: Some(2.0),
            },
        );
        let prefix = playback.output.vacant_frames();
        let mut consumed = 0;
        let ready = playback
            .fill_with(|| {
                let mut data = [0.0; 256];
                render(&mut data, consumed as u64 / 8);
                for (offset, sample) in data.iter().enumerate() {
                    assert_eq!(*sample, (consumed + offset) as f32 / 32768.0);
                }
                consumed += data.len();
            })
            .unwrap();
        // A callback consumes after every decoded packet. The old refill could
        // follow reclaimed space; this round submits only its initial prefix.
        assert!(consumed > 0);
        assert_eq!(prefix - playback.output.vacant_frames() + consumed, prefix);
        assert!(ready, "available refill work must not wait for a timer");
        assert!(!playback.pending.is_empty());
        assert!(!playback.eof);
        assert!(!playback.finished());
        let mut data = [0.0; 160];
        while consumed < 16_000 {
            playback.fill().unwrap();
            let count = data.len().min(16_000 - consumed);
            render(&mut data[..count], consumed as u64 / 8);
            for (offset, sample) in data[..count].iter().enumerate() {
                assert_eq!(*sample, (consumed + offset) as f32 / 32768.0);
            }
            consumed += count;
        }
        assert!(!playback.fill().unwrap());
        assert!(playback.eof);
        assert!(!playback.finished(), "EOF still waits for the audible tail");
        render(&mut data, 2020);
        assert!(data.iter().all(|sample| *sample == 0.0));
        assert!(playback.finished());
        assert_eq!(playback.heard(), 2.0);
        assert_eq!(
            playback.waveform.read().rms_counts.iter().sum::<u64>(),
            16_000
        );
    }

    #[test]
    fn packet_budget_yields_with_room_then_reaches_ordered_eof() {
        let file = pcm_samples(96_000);
        let (mut playback, _) = test_playback(
            file.path(),
            PlaybackRange {
                start_seconds: 0.0,
                end_seconds: Some(12.0),
            },
        );
        let (output, mut render) = super::super::output::test_backend_with_capacity(
            8_000,
            false,
            playback.source.layout(),
            128_000,
        );
        playback.output = output;
        let mut packets = 0;
        assert!(playback.fill_with(|| packets += 1).unwrap());
        assert_eq!(packets, DECODE_WORK_BUDGET);
        assert!(playback.output.vacant_frames() > 0);
        assert!(!playback.eof);
        assert!(
            !playback.waveform.read().complete,
            "yield is not decoder EOF"
        );
        assert!(!playback.fill().unwrap());
        assert!(playback.eof);
        let mut data = vec![0.0; 96_000];
        render(&mut data, 0);
        for (index, sample) in data.iter().enumerate() {
            assert_eq!(*sample, (index % 16_000) as f32 / 32768.0);
        }
        assert!(!playback.finished());
        render(&mut [0.0; 8], 12_020);
        assert!(playback.finished());
        assert_eq!(playback.heard(), 12.0);
        assert_eq!(
            playback.waveform.read().rms_counts.iter().sum::<u64>(),
            96_000
        );
    }

    #[test]
    fn complete_bitrate_waits_for_output_drain_and_cue_end_never_upgrades() {
        let file = pcm_samples(320);
        for restricted in [false, true] {
            let (mut playback, mut render) = test_playback(
                file.path(),
                PlaybackRange {
                    start_seconds: 0.0,
                    end_seconds: Some(0.04),
                },
            );
            if !restricted {
                playback.source =
                    Source::open(file.path(), 64 * 1024, DecoderHint::Native, false).unwrap();
            }
            assert_eq!(playback.completed_bitrate(), None);
            playback.fill().unwrap();
            assert!(playback.eof);
            assert_eq!(
                playback.completed_bitrate(),
                None,
                "decoded EOF is not audible EOF"
            );
            let mut data = [0.0; 160];
            render(&mut data, 0);
            render(&mut data, 20);
            render(&mut data, 40);
            assert_eq!(
                playback.completed_bitrate(),
                None,
                "queued device tail has not elapsed"
            );
            render(&mut data, 60);
            assert!(playback.finished());
            assert_eq!(
                playback.completed_bitrate(),
                (!restricted).then_some(128_000)
            );
        }
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
