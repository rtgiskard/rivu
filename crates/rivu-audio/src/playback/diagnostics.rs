use super::Playback;
use rivu_protocol::model::{AudioOutputState, AudioSourceFormat, AudioStreamFormat, OutputMetrics};
use std::sync::Arc;

/// Immutable format facts are reused between low-frequency snapshots.
#[derive(Default)]
pub(super) struct OutputHistory {
    source: Option<Arc<AudioSourceFormat>>,
    stream: Option<(u64, Arc<AudioStreamFormat>)>,
    archived_segment: Option<u64>,
    underruns: u64,
    missing_seconds: f64,
}

impl Playback {
    pub(super) fn archive_output_diagnostics(&mut self) {
        let output = self.output.diagnostics();
        if self.diagnostics.archived_segment != Some(output.segment_id) {
            self.diagnostics.underruns = self
                .diagnostics
                .underruns
                .saturating_add(output.underrun_events);
            self.diagnostics.missing_seconds +=
                output.missing_frames as f64 / f64::from(output.stream_rate.max(1));
            self.diagnostics.archived_segment = Some(output.segment_id);
        }
    }

    pub(crate) fn output_diagnostics(&mut self, volume: f32) -> AudioOutputState {
        let output = self.output.diagnostics();
        let replaygain = self.replaygain_status();
        let source = self.diagnostics.source.get_or_insert_with(|| {
            let info = self.source.info();
            Arc::new(AudioSourceFormat {
                codec: info.codec.clone(),
                sample_rate: info.sample_rate,
                bits_per_sample: info.bits_per_sample,
                channels: self
                    .source
                    .layout()
                    .iter()
                    .map(|value| value.short_name().to_owned())
                    .collect(),
                processed: self.source.pcm().is_some() || self.graph.is_some(),
            })
        });
        if self
            .diagnostics
            .stream
            .as_ref()
            .is_none_or(|(id, _)| *id != output.segment_id)
        {
            self.diagnostics.stream = Some((
                output.segment_id,
                Arc::new(AudioStreamFormat {
                    sample_rate: output.stream_rate,
                    sample_format: output.stream_format.to_owned(),
                    channels: output
                        .stream_layout
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect(),
                }),
            ));
        }
        let (_, stream) = self
            .diagnostics
            .stream
            .as_ref()
            .expect("output format initialized");
        let archived = self.diagnostics.archived_segment == Some(output.segment_id);
        let master = crate::volume::gain(volume);
        AudioOutputState {
            source_generation: self.generation,
            segment: output.segment_id,
            active: !output.closed,
            source: Arc::clone(source),
            graph_processing: self.graph.is_some(),
            monitor_eq: self.equalizer.config().enabled,
            master_gain_db: (master > 0.0).then(|| 20.0 * f64::from(master).log10()),
            replaygain,
            stream: Arc::clone(stream),
            sink: output.sink_name,
            remix_allowed: output.remix_allowed,
            graph_rate: output.graph_rate,
            quantum_frames: output
                .graph_quantum
                .and_then(|value| u32::try_from(value).ok()),
            device_rate: output.hardware_rate,
            device_format: output.hardware_format.map(str::to_owned),
            metrics: OutputMetrics {
                underrun_events: output.underrun_events,
                underrun_callbacks: output.underrun_callbacks,
                missing_frames: output.missing_frames,
                last_underrun_ago_seconds: output
                    .last_underrun_at_ns
                    .zip(output.observed_at_ns)
                    .map(|(last, now)| now.saturating_sub(last) as f64 / 1_000_000_000.0),
                pending_missing_frames: output.pending_missing_frames,
                startup_wait_frames: output.startup_wait_frames,
                buffered_frames: output.ring_frames as u64,
                low_water_frames: output.ring_low_watermark.map(|value| value as u64),
                sample_peak: output.sample_peak,
                sample_peak_hold: output.peak_hold,
                over_range_frames: output.overrange_frames,
            },
            session_underruns: self.diagnostics.underruns.saturating_add(if archived {
                0
            } else {
                output.underrun_events
            }),
            session_missing_seconds: self.diagnostics.missing_seconds
                + if archived {
                    0.0
                } else {
                    output.missing_frames as f64 / f64::from(output.stream_rate.max(1))
                },
        }
    }
}
