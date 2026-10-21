//! One track's decode-to-output pipeline and audible playback accounting.

use super::{
    MediaInfo, PlaybackMetadata, PlaybackRange, WaveformFrame,
    graph::GraphRunner,
    output::{Output, OutputError, OutputOptions},
    source::{DECODE_WORK_BUDGET, DecodeStep, Source},
    waveform::{WaveformAccumulator, WaveformCoverage},
};
use crate::analysis::{AnalysisProducer, AnalysisWorker};
use crate::config::VisualizationSignal;
use crate::model::DecoderHint;
use anyhow::{Result, bail};
use crossbeam_channel::Receiver;
use parking_lot::RwLock;
use rivu_dsp::equalizer::{Equalizer, EqualizerConfig};
use std::{path::PathBuf, sync::Arc};

const PENDING_BUFFER_CAPACITY: usize = 16_384;
const MAX_GRAPH_PACKET_FRAMES: usize = 65_536;

pub(super) struct PlaybackOptions {
    pub(super) path: PathBuf,
    pub(super) range: Option<PlaybackRange>,
    pub(super) decoder_hint: DecoderHint,
    pub(super) metadata: Arc<PlaybackMetadata>,
    pub(super) generation: u64,
    pub(super) start: f64,
    pub(super) paused: bool,
    pub(super) device: Option<String>,
    pub(super) volume: f32,
    pub(super) pipewire_auto_mix: bool,
    pub(super) still_mode: bool,
    pub(super) equalizer: Arc<EqualizerConfig>,
    pub(super) source_info: Option<MediaInfo>,
    pub(super) listened_seconds: f64,
    pub(super) graph: Option<GraphRunner>,
}

pub(super) struct Playback {
    source: Source,
    output: Output,
    equalizer: Equalizer,
    graph: Option<GraphRunner>,
    graph_skip_frames: usize,
    graph_input_frames: u64,
    graph_output_frames: u64,
    graph_output_remaining: Option<u64>,
    source_eof: bool,
    // Audition keeps the selected source's metadata/timeline, not the WAV's tags.
    source_info: Option<MediaInfo>,
    pending: Vec<f32>,
    pending_start: f64,
    before_skip_frames: usize,
    before_output_frames: u64,
    offset_frames: usize,
    generation: u64,
    metadata: Arc<PlaybackMetadata>,
    base_position: f64,
    prior_heard: f64,
    eof: bool,
    paused: bool,
    visualization: VisualizationCapture,
    scan_source: super::WaveformScanSnapshot,
}

struct VisualizationCapture {
    waveform: Arc<RwLock<WaveformFrame>>,
    accumulator: Option<WaveformAccumulator>,
    weights: Vec<[f32; 2]>,
    coverage: WaveformCoverage,
    still_mode: bool,
    producer: Option<AnalysisProducer>,
    signal: VisualizationSignal,
    last_end: Option<f64>,
    timestamp_tolerance: f64,
}

impl VisualizationCapture {
    fn begin_traversal(&mut self) {
        self.last_end = None;
        if let Some(accumulator) = &mut self.accumulator {
            accumulator.begin_traversal();
        }
    }

    fn push(&mut self, start: f64, pcm: &[f32], rate: u32, channels: usize) {
        if pcm.is_empty() {
            return;
        }
        if self.last_end.is_some_and(|end| {
            (start - end).abs() > self.timestamp_tolerance.max(0.5 / rate as f64)
        }) {
            self.begin_traversal();
            if let Some(producer) = &mut self.producer {
                producer.discontinuity();
            }
        }
        self.last_end = Some(start + (pcm.len() / channels) as f64 / rate as f64);
        if let Some(producer) = &mut self.producer {
            producer.push(pcm, &self.weights);
        }
        if self.still_mode {
            self.coverage.skip();
            return;
        }
        self.coverage.record(start, pcm.len() / channels, rate);
        let accumulator = self.accumulator.get_or_insert_with(|| {
            let mut waveform = self.waveform.write();
            waveform.prepare_bins(rate);
            let mut accumulator = WaveformAccumulator::new(waveform.clone());
            accumulator.set_channel_weights(&self.weights);
            accumulator
        });
        accumulator.push(start, pcm, rate, channels);
        accumulator.publish(&mut self.waveform.write());
    }

    fn finish(&mut self) {
        if let Some(accumulator) = &mut self.accumulator {
            accumulator.finish(&self.coverage);
            accumulator.publish(&mut self.waveform.write());
        }
    }
}
impl Playback {
    pub(super) fn from_source(
        mut source: Source,
        options: PlaybackOptions,
        analyzer: &AnalysisWorker,
        waveform: Arc<RwLock<WaveformFrame>>,
    ) -> Result<Self> {
        let PlaybackOptions {
            path,
            range,
            decoder_hint,
            metadata,
            generation,
            start,
            paused,
            device,
            volume,
            pipewire_auto_mix,
            still_mode,
            equalizer,
            source_info,
            listened_seconds,
            graph,
        } = options;
        if let Some(range) = range {
            source.restrict(range)?;
        }
        if start > 0.0 {
            source.seek(start)?;
        }
        let channels = source.layout().len();
        if let Some(info) = &source_info
            && (info.sample_rate != source.info().sample_rate
                || usize::from(info.channels) != channels)
        {
            bail!("Processed audition format differs from the selected source");
        }
        let scan_equalizer = Arc::clone(&equalizer);
        let equalizer = Equalizer::new(&equalizer, source.info().sample_rate, channels)?;
        if let Some(graph) = &graph
            && (graph.sample_rate() != source.info().sample_rate || graph.channels() != channels)
        {
            bail!(
                "Opened source format differs from admitted realtime graph; no resampling or channel fallback is allowed"
            );
        }
        let graph_skip_frames = graph.as_ref().map_or(0, GraphRunner::latency_frames);
        let graph_output_remaining = graph.as_ref().and_then(|graph| {
            graph.published_frames().map(|frames| {
                frames.saturating_sub((start * source.info().sample_rate as f64).round() as u64)
            })
        });
        let output = Output::new(
            source.info().sample_rate,
            source.layout(),
            OutputOptions {
                device: device.as_deref(),
                volume,
                paused,
                auto_mix: pipewire_auto_mix,
                metadata: Arc::clone(&metadata),
            },
            analyzer,
        )?;
        let waveform_weights = super::channels::analysis_weights(source.layout());
        {
            let mut waveform = waveform.write();
            waveform.set_still_mode(still_mode);
            if let Some(info) = &source_info {
                let display_path = waveform.path.clone().unwrap_or_else(|| path.clone());
                let display_range = waveform.range;
                let display_hint = waveform.decoder_hint;
                waveform.select_audition_source(
                    &display_path,
                    display_range,
                    display_hint,
                    &source,
                    info.duration,
                );
            } else {
                waveform.select_source(&path, range, decoder_hint, &source);
            }
        }
        let timestamp_tolerance = source.waveform_timestamp_tolerance();
        let waveform_coverage = WaveformCoverage::new(start, timestamp_tolerance);
        let mut scan_source = super::WaveformScanSnapshot::original(
            path,
            range,
            decoder_hint,
            analyzer.settings().signal,
            scan_equalizer,
        );
        scan_source.expected_identity = Some(source.identity());
        scan_source.graph = graph.as_ref().map(GraphRunner::snapshot);
        let before_skip_frames = graph.as_ref().map_or(0, GraphRunner::source_lead_frames);
        Ok(Self {
            source,
            output,
            equalizer,
            graph,
            graph_skip_frames,
            graph_input_frames: 0,
            graph_output_frames: 0,
            graph_output_remaining,
            source_eof: false,
            source_info,
            pending: Vec::with_capacity(PENDING_BUFFER_CAPACITY * channels),
            pending_start: start,
            before_skip_frames,
            before_output_frames: 0,
            offset_frames: 0,
            generation,
            metadata,
            base_position: start,
            prior_heard: listened_seconds,
            eof: false,
            paused,
            visualization: VisualizationCapture {
                waveform,
                accumulator: None,
                weights: waveform_weights,
                coverage: waveform_coverage,
                still_mode,
                producer: Some(analyzer.audio_producer()),
                signal: analyzer.settings().signal,
                last_end: None,
                timestamp_tolerance,
            },
            scan_source,
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
        let metadata = Arc::clone(&self.metadata);
        self.restart_output_with(target, |source, paused| {
            Output::new(
                source.info().sample_rate,
                source.layout(),
                OutputOptions {
                    device,
                    volume,
                    paused,
                    auto_mix,
                    metadata,
                },
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
        self.equalizer.reset();
        if let Some(graph) = &mut self.graph {
            graph.reset();
            self.graph_skip_frames = graph.latency_frames();
            self.before_skip_frames = graph.source_lead_frames();
            self.graph_output_remaining = graph.published_frames().map(|frames| {
                frames.saturating_sub(
                    (position * self.source.info().sample_rate as f64).round() as u64
                )
            });
        }
        self.before_output_frames = 0;
        self.graph_input_frames = 0;
        self.graph_output_frames = 0;
        self.source_eof = false;
        let output = create(&self.source, self.paused)?;
        self.output = output;
        self.pending.clear();
        self.offset_frames = 0;
        self.base_position = position;
        self.prior_heard = heard;
        self.eof = false;
        self.visualization.coverage =
            WaveformCoverage::new(position, self.source.waveform_timestamp_tolerance());
        self.visualization.begin_traversal();
        Ok(())
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn info(&self) -> &MediaInfo {
        self.source_info
            .as_ref()
            .unwrap_or_else(|| self.source.info())
    }

    pub(super) fn completed_bitrate(&self) -> Option<u64> {
        if self.source_info.is_some() {
            return None;
        }
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

    pub(super) fn set_equalizer(&mut self, config: &EqualizerConfig) -> Result<()> {
        self.equalizer.reconfigure(config)?;
        self.scan_source.equalizer = Arc::new(config.clone());
        self.visualization.begin_traversal();
        Ok(())
    }

    pub(super) fn set_signal(&mut self, signal: VisualizationSignal) {
        if self.visualization.signal != signal {
            self.visualization.signal = signal;
            self.scan_source.signal = signal;
            self.visualization.begin_traversal();
        }
    }

    pub(super) fn waveform_snapshot(&self) -> super::WaveformScanSnapshot {
        self.scan_source.clone()
    }

    pub(super) fn set_metadata(&mut self, metadata: Arc<PlaybackMetadata>) -> Result<()> {
        if self.metadata != metadata {
            self.output.set_metadata(Arc::clone(&metadata))?;
            self.metadata = metadata;
        }
        Ok(())
    }

    pub(super) fn set_still_mode(&mut self, enabled: bool) {
        if self.visualization.still_mode == enabled {
            return;
        }
        if enabled {
            self.visualization.accumulator = None;
            // Discarding the prefix invalidates coverage even when no PCM is
            // decoded during a quick on/off transition.
            self.visualization.coverage.skip();
        }
        self.visualization.waveform.write().set_still_mode(enabled);
        self.visualization.still_mode = enabled;
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
            if self.source_eof {
                // Drain true upstream EOF through each node. No synthetic
                // source frames may influence terminal impulse repair/STFT.
                self.pending
                    .resize(super::graph::GRAPH_BLOCK_FRAMES * channels, 0.0);
                let frames = self
                    .graph
                    .as_mut()
                    .expect("graph drain")
                    .finish_interleaved(&mut self.pending)?;
                self.pending.truncate(frames * channels);
                self.publish_graph_pending(channels)?;
                remaining_packets -= 1;
                self.eof = frames == 0 || self.graph_output_remaining == Some(0);
                if self.eof {
                    self.visualization.finish();
                }
                continue;
            }
            match self
                .source
                .next_frames_with_budget(&mut remaining_packets)?
            {
                DecodeStep::Frames((frames, start_seconds)) => {
                    if !frames.len().is_multiple_of(channels) {
                        bail!("Decoded audio ends in an incomplete interleaved frame");
                    }
                    if self.graph.is_some() && frames.len() / channels > MAX_GRAPH_PACKET_FRAMES {
                        bail!(
                            "Decoded packet exceeds realtime graph's 65,536-frame work bound; use Offline execution"
                        );
                    }
                    if let Some(graph) = &self.graph {
                        let skip = self.before_skip_frames.min(frames.len() / channels);
                        self.before_skip_frames -= skip;
                        let mut samples = &frames[skip * channels..];
                        if let Some(total) = graph.published_frames() {
                            let remaining = total.saturating_sub(
                                (self.base_position * sample_rate as f64).round() as u64
                                    + self.before_output_frames,
                            );
                            samples = &samples[..samples.len().min(remaining as usize * channels)];
                        }
                        let start = self.base_position
                            + self.before_output_frames as f64 / sample_rate as f64;
                        if self.visualization.signal == VisualizationSignal::Before {
                            self.visualization
                                .push(start, samples, sample_rate, channels);
                        }
                        self.before_output_frames += (samples.len() / channels) as u64;
                    } else if self.visualization.signal == VisualizationSignal::Before {
                        self.visualization
                            .push(start_seconds, frames, sample_rate, channels);
                    }
                    if self.graph.is_none() && self.equalizer.is_bypassed() {
                        if self.visualization.signal == VisualizationSignal::After {
                            self.visualization
                                .push(start_seconds, frames, sample_rate, channels);
                        }
                        // Settled disabled EQ keeps the original borrowed fast path.
                        let prefix = frames.len().min(remaining_frames * channels);
                        let count = self.output.push(&frames[..prefix]);
                        remaining_frames -= count;
                        self.pending.extend_from_slice(&frames[count * channels..]);
                    } else {
                        // Process each packet once. Pending output is already
                        // processed even if settings change before its submission.
                        if self.graph.is_some() {
                            remaining_packets = remaining_packets.saturating_sub(
                                (frames.len() / channels)
                                    .div_ceil(super::graph::GRAPH_BLOCK_FRAMES)
                                    .saturating_sub(1),
                            );
                        }
                        self.pending.extend_from_slice(frames);
                        self.pending_start = start_seconds;
                        self.process_graph_pending(channels)?;
                        let prefix = self.pending.len().min(remaining_frames * channels);
                        let count = self.output.push(&self.pending[..prefix]);
                        remaining_frames -= count;
                        self.offset_frames = count;
                    }
                    after_packet();
                }
                DecodeStep::Yield => break,
                DecodeStep::Eof => {
                    if self.graph.is_none()
                        || self.visualization.signal == VisualizationSignal::Before
                    {
                        self.visualization.finish();
                    }
                    self.source_eof = true;
                    if self.graph.is_some() {
                        if self.graph_output_remaining.is_none() {
                            self.graph_output_remaining = Some(
                                self.graph_input_frames
                                    .saturating_sub(self.graph_output_frames),
                            );
                        }
                        self.eof = self.graph_output_remaining == Some(0);
                    } else {
                        self.eof = true;
                    }
                }
            }
        }
        Ok(
            (!self.eof || self.offset_frames < self.pending.len() / channels)
                && self.output.vacant_frames() > 0,
        )
    }

    fn process_graph_pending(&mut self, channels: usize) -> Result<()> {
        if let Some(graph) = &mut self.graph {
            self.graph_input_frames += (self.pending.len() / channels) as u64;
            for block in self
                .pending
                .chunks_mut(super::graph::GRAPH_BLOCK_FRAMES * channels)
            {
                graph.process_interleaved(block)?;
            }
        }
        self.publish_graph_pending(channels)
    }

    fn publish_graph_pending(&mut self, channels: usize) -> Result<()> {
        let start = if self.graph.is_some() {
            self.base_position
                + self.graph_output_frames as f64 / self.source.info().sample_rate as f64
        } else {
            self.pending_start
        };
        if self.graph.is_some() {
            let skip = self.graph_skip_frames.min(self.pending.len() / channels);
            self.graph_skip_frames -= skip;
            self.pending.copy_within(skip * channels.., 0);
            self.pending.truncate(self.pending.len() - skip * channels);
            if let Some(remaining) = &mut self.graph_output_remaining {
                let frames = (self.pending.len() / channels).min(*remaining as usize);
                self.pending.truncate(frames * channels);
                *remaining -= frames as u64;
            }
            self.graph_output_frames += (self.pending.len() / channels) as u64;
        }
        // Monitor EQ remains separate from the persisted processing graph.
        self.equalizer.process(&mut self.pending)?;
        if self.visualization.signal == VisualizationSignal::After {
            self.visualization.push(
                start,
                &self.pending,
                self.source.info().sample_rate,
                channels,
            );
        }
        Ok(())
    }
    pub(super) fn finished(&self) -> bool {
        !self.paused
            && self.eof
            && self.offset_frames == self.pending.len() / self.source.layout().len()
            && self.output.drained()
    }
    pub(super) fn close(&mut self) -> super::AudioSnapshot {
        // Closing retires the callback/device owner first. Derive both counters
        // from that single confirmed frame count, never a preceding progress tick.
        let final_output_heard = self.output.close();
        super::AudioSnapshot {
            generation: self.generation,
            position_seconds: self.base_position + final_output_heard,
            listened_seconds: self.prior_heard + final_output_heard,
        }
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

    fn assert_mono_waveform_bin(frame: &WaveformFrame, bin: usize, samples: &[f32]) {
        let span = frame.rms_spans[bin].as_ref().unwrap();
        let samples = &samples[span.start as usize..span.end as usize];
        assert_eq!(frame.rms_counts[bin], samples.len() as u64);
        let peak = samples
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        assert_eq!(frame.peaks[bin], Some(peak));
        let energy: f64 = samples
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum();
        let rms = (energy / samples.len() as f64).sqrt() as f32;
        assert!((frame.rms[bin].unwrap() - rms).abs() < 1e-6);
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
        let waveform_weights = super::super::channels::analysis_weights(source.layout());
        let timestamp_tolerance = source.waveform_timestamp_tolerance();
        let waveform_coverage = WaveformCoverage::new(0.0, timestamp_tolerance);
        let equalizer =
            Equalizer::new(&EqualizerConfig::default(), 8_000, source.layout().len()).unwrap();
        let playback = Playback {
            source,
            output,
            equalizer,
            graph: None,
            graph_skip_frames: 0,
            graph_input_frames: 0,
            graph_output_frames: 0,
            graph_output_remaining: None,
            source_eof: false,
            source_info: None,
            pending: Vec::new(),
            pending_start: 0.0,
            before_skip_frames: 0,
            before_output_frames: 0,
            offset_frames: 0,
            generation: 1,
            metadata: Arc::default(),
            base_position: 0.0,
            prior_heard: 0.0,
            eof: false,
            paused: false,
            visualization: VisualizationCapture {
                waveform,
                accumulator: None,
                weights: waveform_weights,
                coverage: waveform_coverage,
                still_mode: false,
                producer: None,
                signal: VisualizationSignal::After,
                last_end: None,
                timestamp_tolerance,
            },
            scan_source: super::super::WaveformScanSnapshot::original(
                path.to_owned(),
                range,
                DecoderHint::Auto,
                VisualizationSignal::After,
                Arc::new(EqualizerConfig::default()),
            ),
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
        assert!(reopened.visualization.accumulator.is_none());
        assert!(!reopened.eof);

        let replacement = pcm_level(320, 26214);
        std::fs::rename(replacement.path(), file.path()).unwrap();
        // The decoder already holds A, even though the path now names B.
        reopened.fill().unwrap();
        assert!(waveform.read().revision > completed.revision);
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
        let before = playback.visualization.waveform.read().clone();
        assert!(!before.complete);
        assert!(before.peaks.iter().any(Option::is_some));
        let replacement = pcm_level(16_000, 26214);
        std::fs::rename(replacement.path(), file.path()).unwrap();
        let (output, mut render) =
            super::super::output::test_backend(8_000, false, playback.source.layout());
        playback
            .restart_output_with(Some(1.0), |_, _| Ok(output))
            .unwrap();
        assert_eq!(
            playback.visualization.waveform.read().identity,
            before.identity
        );
        assert_eq!(
            playback.visualization.waveform.read().revision,
            before.revision
        );
        assert_eq!(playback.visualization.waveform.read().peaks, before.peaks);
        playback.fill().unwrap();
        let mut samples = [0.0; 160];
        render(&mut samples, 0);
        assert!(samples.iter().all(|sample| *sample == 3277.0 / 32768.0));
        let after = playback.visualization.waveform.read();
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
        assert!(playback.visualization.accumulator.is_none());
        assert_eq!(
            playback
                .visualization
                .waveform
                .read()
                .retained_capacity_bytes(),
            0
        );
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
            let initial = playback.visualization.waveform.read().clone();
            assert!(!initial.complete);
            let pending = playback.pending.clone();
            let offset = playback.offset_frames;
            let position = playback.position();
            let heard = playback.heard();
            playback.set_still_mode(true);
            let suspended_revision = playback.visualization.waveform.read().revision;
            assert!(suspended_revision > initial.revision);
            assert_waveform_absent(&playback);
            assert_eq!(playback.pending, pending);
            assert_eq!(playback.offset_frames, offset);
            assert_eq!(playback.position(), position);
            assert_eq!(playback.heard(), heard);
            let mut data = [0.0; 160];
            for block in 0..100 {
                if resume_before_eof && block == 30 {
                    assert_eq!(
                        playback.visualization.waveform.read().revision,
                        suspended_revision
                    );
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
            let interrupted = playback.visualization.waveform.read().clone();
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
                assert!(!playback.visualization.waveform.read().complete);
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
        let retained_revision = playback.visualization.waveform.read().revision;
        let pending = playback.pending.clone();
        let pending_capacity = playback.pending.capacity();
        for pass in 0..32 {
            playback.set_still_mode(true);
            let revision = playback.visualization.waveform.read().revision;
            assert_eq!(revision, retained_revision + pass + 1);
            playback.set_still_mode(true);
            playback.fill().unwrap();
            assert_eq!(playback.visualization.waveform.read().revision, revision);
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
        assert!(!playback.visualization.waveform.read().complete);
        assert!(
            playback
                .visualization
                .waveform
                .read()
                .rms_counts
                .iter()
                .sum::<u64>()
                < 16_000
        );
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
        let capacity_bound = playback
            .visualization
            .waveform
            .read()
            .retained_capacity_bytes();
        playback.set_still_mode(true);
        for target in [None, Some(0.0), Some(0.5)] {
            let initial_revision = playback.visualization.waveform.read().revision;
            let (output, mut render) =
                super::super::output::test_backend(8_000, false, playback.source.layout());
            playback
                .restart_output_with(target, |_, _| Ok(output))
                .unwrap();
            playback.fill().unwrap();
            assert_waveform_absent(&playback);
            playback.set_still_mode(true);
            assert_waveform_absent(&playback);
            assert_eq!(
                playback.visualization.waveform.read().revision,
                initial_revision
            );
            playback.set_still_mode(false);
            assert_waveform_absent(&playback);
            let mut data = [0.0; 160];
            for block in 0..110 {
                playback.fill().unwrap();
                render(&mut data, block * 20);
            }
            assert!(playback.eof);
            assert!(!playback.visualization.waveform.read().complete);
            assert!(
                playback
                    .visualization
                    .waveform
                    .read()
                    .retained_capacity_bytes()
                    <= capacity_bound
            );
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
        assert!(playback.visualization.waveform.read().complete);
        let waveform = playback.visualization.waveform.read();
        for (bin, rms) in waveform.rms.iter().enumerate() {
            let first = (bin * 16_000).div_ceil(waveform.peaks.len());
            let end = ((bin + 1) * 16_000).div_ceil(waveform.peaks.len());
            let count = (end - first) as u64;
            assert_eq!(waveform.rms_counts[bin], count);
            assert_eq!(waveform.left_rms_counts[bin], count);
            assert_eq!(waveform.right_rms_counts[bin], count);
            let energy: f64 = (first..end)
                .map(|index| (index as f64 / 32768.0).powi(2))
                .sum();
            let expected = (energy / count as f64).sqrt() as f32;
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
            playback
                .visualization
                .waveform
                .read()
                .rms_counts
                .iter()
                .sum::<u64>(),
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
            !playback.visualization.waveform.read().complete,
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
            playback
                .visualization
                .waveform
                .read()
                .rms_counts
                .iter()
                .sum::<u64>(),
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
        let initial = playback.visualization.waveform.read().clone();
        playback.pause(true);
        for target in [0.75, 0.125, 1.0, 0.0] {
            let prior_revision = playback.visualization.waveform.read().revision;
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
            assert_eq!(
                playback.visualization.waveform.read().revision,
                prior_revision
            );
            render(&mut data, 0);
            assert_eq!(data, [0.0; 64]);
            playback.pause(false);
            playback.fill().unwrap();
            assert!(playback.visualization.waveform.read().revision > prior_revision);
            render(&mut data, 1);
            let first = ((target + 0.25) * 8_000.0).round() as usize;
            for (offset, sample) in data.iter().enumerate() {
                assert_eq!(*sample, (first + offset) as f32 / 32768.0);
            }
            playback.pause(true);
        }
        let rolling = playback.visualization.waveform.read();
        assert!(rolling.revision > initial.revision);
        assert_eq!(rolling.path, initial.path);
        let count = rolling.rms_counts.iter().sum::<u64>();
        assert!(count > 0 && count <= 12_000);
        let expected: Vec<f32> = (2_000..14_000)
            .map(|frame| frame as f32 / 32768.0)
            .collect();
        for bin in 0..rolling.peaks.len() {
            if rolling.rms_counts[bin] > 0 {
                assert_mono_waveform_bin(&rolling, bin, &expected);
            }
        }
    }

    #[test]
    fn selected_worker_pcm_matches_nonflat_eq_before_master_volume() {
        use ringbuf::traits::Consumer;
        let file = pcm_samples(320);
        let config = EqualizerConfig {
            enabled: true,
            bands: vec![rivu_dsp::equalizer::EqBand {
                gain_db: 12.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let original: Vec<f32> = (0..320).map(|frame| frame as f32 / 32768.0).collect();
        let mut processed = original.clone();
        let mut reference = Equalizer::new(&EqualizerConfig::default(), 8_000, 1).unwrap();
        reference.reconfigure(&config).unwrap();
        reference.process(&mut processed).unwrap();
        assert!(
            processed
                .iter()
                .zip(&original)
                .any(|(after, before)| (after - before).abs() > 1e-5)
        );
        for signal in [VisualizationSignal::Before, VisualizationSignal::After] {
            for volume in [0.0, 0.25, 1.0] {
                let (mut playback, mut render) =
                    test_playback_with_waveform(file.path(), None, Arc::default());
                let worker = AnalysisWorker::new(Arc::default()).unwrap();
                worker.set_playback_active(true);
                worker.control.set_display_enabled(true);
                let (producer, mut captured) = AnalysisProducer::capture_for_test(&worker, 1024);
                playback.visualization.producer = Some(producer);
                playback.set_signal(signal);
                playback.set_equalizer(&config).unwrap();
                playback.set_volume(volume);
                playback.fill().unwrap();
                let expected = if signal == VisualizationSignal::Before {
                    &original
                } else {
                    &processed
                };
                let weights = super::super::output::analysis_weights(playback.source.layout());
                for sample in expected {
                    assert_eq!(
                        captured.try_pop().unwrap().samples,
                        [sample * weights[0][0], sample * weights[0][1]]
                    );
                }
                assert!(captured.try_pop().is_none());
                let mut audible = [0.0; 320];
                render(&mut audible, 0);
                for (actual, expected) in audible.iter().zip(&processed) {
                    assert_eq!(*actual, expected * crate::volume::gain(volume));
                }
                let frame = playback.visualization.waveform.read();
                assert_eq!(frame.rms_counts.iter().sum::<u64>(), 320);
                for bin in 0..frame.peaks.len() {
                    if frame.rms_counts[bin] > 0 {
                        assert_mono_waveform_bin(&frame, bin, expected);
                    }
                }
            }
        }
    }

    #[test]
    fn full_scan_captures_active_graph_guard_mapping_eq_and_source() {
        use rivu_format::config::processing_graph::GraphPort;
        let file = pcm_level(149, 8192);
        let mut graph =
            GraphRunner::compile(&unity_fir_graph(), GraphPort { node: 2, port: 0 }, 8_000, 1)
                .unwrap();
        graph.set_source_context(-32, 117).unwrap();
        let source = Source::open(file.path(), 64 * 1024, DecoderHint::Native, false).unwrap();
        let config = Arc::new(EqualizerConfig {
            enabled: true,
            preamp_db: 6.0,
            ..Default::default()
        });
        let mut snapshot = super::super::WaveformScanSnapshot::original(
            file.path().to_owned(),
            None,
            DecoderHint::Native,
            VisualizationSignal::Before,
            config,
        );
        snapshot.expected_identity = Some(source.identity());
        snapshot.graph = Some(graph.snapshot());
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let before = super::super::waveform(&snapshot, 2, false, &cancelled).unwrap();
        assert_eq!(before.duration, Some(117.0 / 8_000.0));
        assert_eq!(before.rms_counts.iter().sum::<u64>(), 117);
        assert!(
            before
                .peaks
                .iter()
                .flatten()
                .all(|peak| (*peak - 0.25).abs() < 1e-6)
        );
        assert!(
            before
                .rms
                .iter()
                .flatten()
                .all(|rms| (*rms - 0.25).abs() < 1e-6)
        );
        snapshot.signal = VisualizationSignal::After;
        let after = super::super::waveform(&snapshot, 2, false, &cancelled).unwrap();
        assert_eq!(after.duration, before.duration);
        assert_eq!(after.rms_counts.iter().sum::<u64>(), 117);
        let expected = 0.25 * 10.0_f32.powf(6.0 / 20.0);
        assert!(
            after
                .peaks
                .iter()
                .flatten()
                .all(|peak| (*peak - expected).abs() < 1e-5)
        );
        assert!(
            after
                .rms
                .iter()
                .flatten()
                .all(|rms| (*rms - expected).abs() < 1e-5)
        );
        let replacement = pcm_level(149, 16384);
        snapshot.path = replacement.path().to_owned();
        assert!(super::super::waveform(&snapshot, 2, false, &cancelled).is_err());
    }

    #[test]
    fn equalizer_processes_packet_once_across_updates_pause_and_ring_wrap() {
        let file = pcm();
        let (mut playback, _) = test_playback_with_waveform(file.path(), None, Arc::default());
        let config = EqualizerConfig {
            enabled: true,
            preamp_db: 6.0,
            ..EqualizerConfig::default()
        };
        playback.equalizer = Equalizer::new(&config, 8_000, 1).unwrap();
        let (output, mut render) = super::super::output::test_backend_with_capacity(
            8_000,
            false,
            playback.source.layout(),
            17,
        );
        playback.output = output;
        playback.fill().unwrap();
        assert!(playback.pending.len() > 17);
        let first_packet = playback.pending.len();
        let pending = playback.pending.clone();
        playback.pause(true);
        assert!(!playback.fill().unwrap());
        assert_eq!(playback.pending, pending);
        playback.pause(false);
        let updated = EqualizerConfig {
            enabled: true,
            output_gain_db: -6.0,
            ..EqualizerConfig::default()
        };
        playback.set_equalizer(&updated).unwrap();
        let mut expected: Vec<f32> = (0..16_000).map(|frame| frame as f32 / 32768.0).collect();
        let mut reference = Equalizer::new(&config, 8_000, 1).unwrap();
        reference.process(&mut expected[..first_packet]).unwrap();
        reference.reconfigure(&updated).unwrap();
        reference.process(&mut expected[first_packet..]).unwrap();
        let mut consumed = 0;
        while consumed < expected.len() {
            playback.fill().unwrap();
            let count = 17.min(expected.len() - consumed);
            let mut samples = vec![0.0; count];
            render(&mut samples, consumed as u64 / 8);
            assert_eq!(samples, expected[consumed..consumed + count]);
            consumed += count;
        }
        playback.fill().unwrap();
        assert!(playback.eof);
        let waveform = playback.visualization.waveform.read();
        let count = waveform.rms_counts.iter().sum::<u64>();
        assert!(count > 0 && count <= 16_000);
        for bin in 0..waveform.peaks.len() {
            if waveform.rms_counts[bin] > 0 {
                assert_mono_waveform_bin(&waveform, bin, &expected);
            }
        }
    }

    #[test]
    fn equalizer_seek_and_device_restart_discard_filter_history() {
        let file = pcm();
        let (mut playback, _) = test_playback_with_waveform(file.path(), None, Arc::default());
        let config = EqualizerConfig {
            enabled: true,
            bands: vec![rivu_dsp::equalizer::EqBand {
                gain_db: 12.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        playback.equalizer = Equalizer::new(&config, 8_000, 1).unwrap();
        playback.fill().unwrap();
        for target in [Some(0.25), None] {
            let position = target.unwrap_or_else(|| playback.position());
            let (output, mut render) =
                super::super::output::test_backend(8_000, false, playback.source.layout());
            playback
                .restart_output_with(target, |_, _| Ok(output))
                .unwrap();
            playback.fill().unwrap();
            let first = (position * 8_000.0).round() as usize;
            let mut expected: Vec<f32> = (first..first + 64)
                .map(|frame| frame as f32 / 32768.0)
                .collect();
            Equalizer::new(&config, 8_000, 1)
                .unwrap()
                .process(&mut expected)
                .unwrap();
            let mut samples = [0.0; 64];
            render(&mut samples, 0);
            assert_eq!(samples.as_slice(), expected);
        }
    }

    fn unity_fir_graph() -> rivu_format::config::processing_graph::ProcessingGraph {
        use rivu_format::config::processing_graph::{
            EqImplementation, GraphEdge, GraphNode, GraphNodeKind, GraphPort, ProcessingGraph,
        };
        let mut graph = ProcessingGraph::default();
        graph.nodes.push(GraphNode {
            id: 3,
            position: [0.0; 2],
            kind: GraphNodeKind::Equalizer {
                config: EqualizerConfig {
                    enabled: true,
                    ..Default::default()
                },
                implementation: EqImplementation::LinearPhase { taps: 513 },
            },
        });
        graph.next_id = 4;
        graph.edges = vec![
            GraphEdge {
                from: GraphPort { node: 1, port: 0 },
                to: GraphPort { node: 3, port: 0 },
            },
            GraphEdge {
                from: GraphPort { node: 3, port: 0 },
                to: GraphPort { node: 2, port: 0 },
            },
        ];
        graph
    }

    #[test]
    fn graph_terminal_impulse_uses_true_eof_before_downstream_fir() {
        use rivu_dsp::denoise::{DenoiseConfig, DenoiseMode, Denoiser, ImpulseConfig};
        use rivu_format::config::processing_graph::{
            GraphEdge, GraphNode, GraphNodeKind, GraphPort,
        };
        for frames in [1, 7, 117, 511] {
            let mut file = pcm_level(frames, 0);
            file.seek(SeekFrom::Start(44 + u64::from(frames - 1) * 2))
                .unwrap();
            file.write_all(&24576_i16.to_le_bytes()).unwrap();
            let config = DenoiseConfig {
                mode: DenoiseMode::Off,
                impulse: ImpulseConfig {
                    enabled: true,
                    ..Default::default()
                },
                ..Default::default()
            };
            let input: Vec<f32> = (0..frames)
                .map(|frame| if frame + 1 == frames { 0.75 } else { 0.0 })
                .collect();
            let mut reference = Denoiser::new(8_000, 1, config.clone(), None).unwrap();
            let mut expected = Vec::new();
            reference
                .process_block(&input, &mut expected, &mut || false)
                .unwrap();
            reference.finish(&mut expected, &mut || false).unwrap();
            assert_eq!(expected.last(), Some(&0.75));
            let mut graph = unity_fir_graph();
            graph.nodes.push(GraphNode {
                id: 4,
                position: [0.0; 2],
                kind: GraphNodeKind::Denoise {
                    config,
                    profile: None,
                },
            });
            graph.next_id = 5;
            graph.edges[0] = GraphEdge {
                from: GraphPort { node: 1, port: 0 },
                to: GraphPort { node: 4, port: 0 },
            };
            graph.edges.push(GraphEdge {
                from: GraphPort { node: 4, port: 0 },
                to: GraphPort { node: 3, port: 0 },
            });
            if frames >= 7 {
                // Reproduce the old consumer's fake-source-zero EOF policy:
                // its last source impulse becomes interior and is erased.
                let mut ordinary =
                    GraphRunner::compile(&graph, GraphPort { node: 2, port: 0 }, 8_000, 1).unwrap();
                let delay = ordinary.latency_frames();
                let mut padded = input.clone();
                padded.resize(padded.len() + delay, 0.0);
                ordinary.process_interleaved(&mut padded).unwrap();
                assert!(padded[delay + frames as usize - 1].abs() < 2e-6);
            }
            let runner =
                GraphRunner::compile(&graph, GraphPort { node: 2, port: 0 }, 8_000, 1).unwrap();
            let (mut playback, mut render) =
                test_playback_with_waveform(file.path(), None, Arc::default());
            playback.graph_skip_frames = runner.latency_frames();
            playback.graph = Some(runner);
            for _ in 0..16 {
                playback.fill().unwrap();
                if playback.eof {
                    break;
                }
            }
            assert!(playback.eof);
            let mut actual = vec![0.0; frames as usize];
            render(&mut actual, 0);
            for (actual, expected) in actual.iter().zip(&expected) {
                assert!(
                    (actual - expected).abs() < 2e-6,
                    "frames={frames}: {actual} != {expected}"
                );
            }
            render(&mut [0.0; 160], 100);
            assert!(playback.finished());
            assert_eq!(playback.position(), f64::from(frames) / 8_000.0);
            assert_eq!(playback.heard(), f64::from(frames) / 8_000.0);
        }
    }

    #[test]
    fn graph_short_source_discards_latency_flushes_eof_and_counts_only_source_frames() {
        use rivu_format::config::processing_graph::GraphPort;
        let file = pcm_samples(117);
        let (mut playback, mut render) =
            test_playback_with_waveform(file.path(), None, Arc::default());
        let graph = unity_fir_graph();
        let runner =
            GraphRunner::compile(&graph, GraphPort { node: 2, port: 0 }, 8_000, 1).unwrap();
        playback.graph_skip_frames = runner.latency_frames();
        playback.graph = Some(runner);
        for _ in 0..8 {
            playback.fill().unwrap();
            if playback.eof {
                break;
            }
        }
        assert!(playback.eof);
        assert_eq!(playback.graph_skip_frames, 0);
        let mut samples = [0.0; 117];
        render(&mut samples, 0);
        for (index, sample) in samples.iter().enumerate() {
            assert!((*sample - index as f32 / 32768.0).abs() < 2e-6);
        }
        render(&mut [0.0; 160], 50);
        assert!(playback.finished());
        assert_eq!(playback.heard(), 117.0 / 8_000.0);
        assert_eq!(playback.position(), 117.0 / 8_000.0);
    }

    #[test]
    fn graph_seek_discards_all_node_and_alignment_histories() {
        use rivu_format::config::processing_graph::GraphPort;
        let file = pcm();
        let (mut playback, _) = test_playback_with_waveform(file.path(), None, Arc::default());
        let graph = unity_fir_graph();
        let runner =
            GraphRunner::compile(&graph, GraphPort { node: 2, port: 0 }, 8_000, 1).unwrap();
        playback.graph_skip_frames = runner.latency_frames();
        playback.graph = Some(runner);
        playback.fill().unwrap();
        let (output, mut render) =
            super::super::output::test_backend(8_000, false, playback.source.layout());
        playback
            .restart_output_with(Some(0.25), |_, _| Ok(output))
            .unwrap();
        playback.fill().unwrap();
        let mut samples = [0.0; 64];
        render(&mut samples, 0);
        for (index, sample) in samples.iter().enumerate() {
            assert!((*sample - (2000 + index) as f32 / 32768.0).abs() < 2e-6);
        }
        assert!(!playback.source_eof);
    }

    #[test]
    fn disabled_equalizer_keeps_exact_pcm_despite_saved_gain() {
        let file = pcm_samples(320);
        let (mut playback, mut render) =
            test_playback_with_waveform(file.path(), None, Arc::default());
        playback.equalizer = Equalizer::new(
            &EqualizerConfig {
                preamp_db: 24.0,
                output_gain_db: -12.0,
                ..Default::default()
            },
            8_000,
            1,
        )
        .unwrap();
        playback.fill().unwrap();
        let mut samples = [0.0; 320];
        render(&mut samples, 0);
        assert_eq!(samples, std::array::from_fn(|frame| frame as f32 / 32768.0));
        assert!(playback.equalizer.is_bypassed());
    }

    #[test]
    fn audition_visualizes_actual_pcm_and_retains_source_metadata() {
        let original = pcm_level(320, 3277);
        let waveform = Arc::new(RwLock::new(WaveformFrame::default()));
        let (mut source, _) = test_playback_with_waveform(original.path(), None, waveform.clone());
        source.fill().unwrap();
        let envelope = waveform.read().clone();
        let info = source.info().clone();
        let processed = pcm_level(320, 26214);
        let (mut audition, mut render) =
            test_playback_with_waveform(processed.path(), None, Arc::default());
        audition.visualization.waveform = waveform.clone();
        audition.source_info = Some(info.clone());
        waveform.write().select_audition_source(
            original.path(),
            None,
            DecoderHint::Auto,
            &audition.source,
            info.duration,
        );
        audition.prior_heard = 1.25;
        audition.fill().unwrap();
        let mut samples = [0.0; 320];
        render(&mut samples, 0);
        assert!(samples.iter().all(|sample| *sample == 26214.0 / 32768.0));
        assert_eq!(audition.info().codec, info.codec);
        assert_eq!(audition.info().duration, info.duration);
        assert_ne!(waveform.read().identity, envelope.identity);
        assert!(
            waveform
                .read()
                .peaks
                .iter()
                .flatten()
                .all(|peak| (*peak - 26214.0 / 32768.0).abs() < 1e-6)
        );
        assert!(waveform.read().revision > envelope.revision);
        assert!(audition.heard() >= 1.25);
        assert_eq!(audition.completed_bitrate(), None);
    }

    #[test]
    fn retiring_snapshot_uses_final_confirmed_position_not_last_published_progress() {
        for paused in [false, true] {
            let file = pcm();
            let (mut playback, _) = test_playback(
                file.path(),
                PlaybackRange {
                    start_seconds: 0.25,
                    end_seconds: Some(1.75),
                },
            );
            let (output, mut render) =
                super::super::output::test_backend(8_000, false, playback.source.layout());
            playback
                .restart_output_with(Some(0.25), |_, _| Ok(output))
                .unwrap();
            playback.prior_heard = 12.0;
            playback.fill().unwrap();
            render(&mut [0.0; 64], 0);
            let published_position = playback.position();
            assert_eq!(published_position, 0.25);
            playback.pause(paused);
            // The callback confirms 32 frames from the earlier scheduled span
            // after the last progress publication, including while pausing.
            render(&mut [0.0; 64], 24);
            let snapshot = playback.close();
            assert_eq!(
                snapshot,
                super::super::AudioSnapshot {
                    generation: 1,
                    position_seconds: 0.25 + 32.0 / 8_000.0,
                    listened_seconds: 12.0 + 32.0 / 8_000.0,
                }
            );
            assert!(snapshot.position_seconds > published_position);
            assert!(
                !playback.output.drained(),
                "queued future PCM is not yet heard"
            );
        }
    }
}
