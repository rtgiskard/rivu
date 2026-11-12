//! Source identity and worker retirement for window-requested full waveforms.
use rivu_protocol::model::Track;
use std::path::PathBuf;

/// Reduce a lossless finite stream immediately; only one chunk's folded stereo
/// and a bounded summary are retained, never a song-sized sample buffer.
pub(in crate::gui) async fn load(
    data_dir: PathBuf,
    instance: u32,
    expected: rivu_protocol::audio::ExpectedSource,
    source: Option<u64>,
    signal: rivu_format::config::VisualizationSignal,
    cancelled: futures::channel::oneshot::Receiver<()>,
    mut progress: impl FnMut(&rivu_visuals::WaveformFrame) -> anyhow::Result<()>,
) -> anyhow::Result<rivu_visuals::WaveformFrame> {
    use rivu_protocol::audio::{PlaybackRange, Response};
    let path = expected.path.clone();
    let range = expected.cue.as_ref().map(|cue| PlaybackRange {
        start_seconds: cue.start_seconds(),
        end_seconds: cue.end_seconds(),
    });
    let hint = expected.decoder_hint;
    let mut summary = rivu_visuals::WaveformSummary::default();
    let mut weights = Vec::new();
    let mut folded = Vec::new();
    crate::transport::audio::read_stream(
        data_dir,
        instance,
        expected,
        source,
        signal,
        None,
        cancelled,
        |event| {
            match event {
                Response::Start {
                    source,
                    identity,
                    sample_rate,
                    layout,
                    duration,
                } => {
                    summary.select(
                        &path,
                        range,
                        hint,
                        duration,
                        sample_rate,
                        source,
                        Some(identity),
                    );
                    weights = rivu_dsp::channels::analysis_weights(layout);
                }
                Response::Pcm {
                    media_start,
                    media_step,
                    samples,
                    ..
                } => {
                    folded.clear();
                    folded.extend(samples.chunks_exact(weights.len()).map(|frame| {
                        let mut stereo = [0.0; 2];
                        for (&sample, weight) in frame.iter().zip(&weights) {
                            stereo[0] += sample * weight[0];
                            stereo[1] += sample * weight[1];
                        }
                        stereo
                    }));
                    summary.push_mapped(media_start, media_step, &folded);
                    progress(&summary.frame)?;
                }
                Response::Complete { duration, .. } => summary.finish(duration),
                _ => unreachable!("Generic stream admission rejects control/error events"),
            }
            Ok(())
        },
    )
    .await?;
    Ok(summary.frame)
}

#[derive(Default)]
pub(in crate::gui) struct FullWaveformRequest {
    generation: u64,
    target: Option<FullWaveformTarget>,
    worker: Option<u64>,
}

struct FullWaveformTarget {
    track_id: i64,
    path: PathBuf,
    fingerprint: Option<String>,
    cue_range: Option<(u64, Option<u64>)>,
    decoder_hint: rivu_format::decoder_hint::DecoderHint,
}

impl FullWaveformTarget {
    fn cue_range(track: &Track) -> Option<(u64, Option<u64>)> {
        track
            .cue
            .as_ref()
            .map(|cue| (cue.start_frame, cue.end_frame))
    }

    fn matches_source(&self, track: &Track) -> bool {
        self.path == track.path
            && self.cue_range == Self::cue_range(track)
            && self.fingerprint == track.fingerprint
            && self.decoder_hint == track.decoder_hint
    }

    fn matches(&self, track: &Track) -> bool {
        self.track_id == track.id && self.matches_source(track)
    }
}

impl FullWaveformRequest {
    pub(in crate::gui) fn generation(&self) -> u64 {
        self.generation
    }

    pub(in crate::gui) fn begin(&mut self, track: &Track) -> u64 {
        self.invalidate();
        self.target = Some(FullWaveformTarget {
            track_id: track.id,
            path: track.path.clone(),
            fingerprint: track.fingerprint.clone(),
            cue_range: FullWaveformTarget::cue_range(track),
            decoder_hint: track.decoder_hint,
        });
        self.generation
    }

    pub(in crate::gui) fn reserve(&mut self) -> Option<(u64, i64)> {
        if self.worker.is_some() {
            return None;
        }
        let target = self.target.as_ref()?;
        self.worker = Some(self.generation);
        Some((self.generation, target.track_id))
    }

    pub(in crate::gui) fn retire(&mut self, generation: u64) {
        if self.worker == Some(generation) {
            self.worker = None;
        }
    }

    pub(in crate::gui) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.target = None;
    }

    pub(in crate::gui) fn retain_target(
        &mut self,
        track: Option<&Track>,
        visualization_enabled: bool,
    ) {
        if self.target.as_ref().is_some_and(|target| {
            !visualization_enabled || track.is_none_or(|track| !target.matches(track))
        }) {
            self.invalidate();
        }
    }

    pub(in crate::gui) fn accepts(&self, generation: u64, track: Option<&Track>) -> bool {
        self.generation == generation
            && self
                .target
                .as_ref()
                .is_some_and(|target| track.is_some_and(|track| target.matches(track)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaced_request_waits_for_physical_worker_retirement() {
        let track = crate::test_support::artwork_track("track.wav".into());
        let mut request = FullWaveformRequest::default();
        let first = request.begin(&track);
        assert_eq!(request.reserve(), Some((first, track.id)));
        let replacement = request.begin(&track);
        assert!(!request.accepts(first, Some(&track)));
        assert!(request.accepts(replacement, Some(&track)));
        assert_eq!(request.reserve(), None);
        request.retire(replacement);
        assert_eq!(request.reserve(), None);
        request.retire(first);
        assert_eq!(request.reserve(), Some((replacement, track.id)));
        request.retire(first);
        assert_eq!(request.reserve(), None);
        request.invalidate();
        assert!(!request.accepts(replacement, Some(&track)));
        request.retire(replacement);
        assert_eq!(request.reserve(), None);
    }
}
