//! A bounded, progressively filled envelope of the PCM already decoded for playback.

use super::PlaybackRange;
use std::path::{Path, PathBuf};

const MAX_BINS: usize = 1600;
const INITIAL_UNKNOWN_SPAN: f64 = 60.0;

/// Coverage of one uninterrupted decoder lifetime. Previously cached bins do
/// not prove a seek-skipped interval was read during this lifetime.
pub(super) struct WaveformCoverage {
    continuous: bool,
    end_seconds: f64,
}

impl WaveformCoverage {
    pub(super) fn new(start_seconds: f64) -> Self {
        Self {
            continuous: start_seconds == 0.0,
            end_seconds: 0.0,
        }
    }

    pub(super) fn record(&mut self, start_seconds: f64, frames: usize, sample_rate: u32) {
        if !start_seconds.is_finite() || sample_rate == 0 {
            self.continuous = false;
            return;
        }
        let rate = f64::from(sample_rate);
        if start_seconds > self.end_seconds + 0.5 / rate {
            self.continuous = false;
        }
        self.end_seconds = self
            .end_seconds
            .max(((start_seconds * rate).round() + frames as f64) / rate);
    }

    pub(super) fn finish(&self, frame: &mut WaveformFrame) {
        if self.continuous && self.end_seconds > 0.0 {
            frame.finish_scan(self.end_seconds);
        }
    }
}

#[derive(Clone, Debug)]
pub struct WaveformFrame {
    pub revision: u64,
    pub complete: bool,
    pub path: Option<PathBuf>,
    pub range: Option<PlaybackRange>,
    pub duration: Option<f64>,
    pub span_seconds: f64,
    pub max_peak: f32,
    /// `None` is not yet decoded; `Some(0.0)` is decoded silence.
    pub peaks: Vec<Option<f32>>,
}

impl Default for WaveformFrame {
    fn default() -> Self {
        Self {
            revision: 0,
            path: None,
            complete: false,
            range: None,
            duration: None,
            span_seconds: 0.0,
            max_peak: 1.0,
            peaks: Vec::new(),
        }
    }
}

impl WaveformFrame {
    pub fn matches(&self, path: &Path, range: Option<PlaybackRange>) -> bool {
        self.path.as_deref() == Some(path) && self.range == range
    }

    /// The caller validates source identity and request generation before
    /// publishing a manual full scan. Preserve the shared version sequence.
    pub fn install_full(&mut self, mut frame: Self) {
        frame.revision = self.revision.wrapping_add(1);
        frame.complete = true;
        *self = frame;
    }

    /// Reopening the same source (including seeks and device changes) retains
    /// its timeline and every peak. Track metadata is deliberately not a key.
    pub(super) fn select(
        &mut self,
        path: &Path,
        range: Option<PlaybackRange>,
        duration: Option<f64>,
        sample_rate: u32,
    ) {
        if self.matches(path, range) {
            return;
        }
        self.path = Some(path.to_owned());
        self.range = range;
        self.duration = duration.filter(|value| value.is_finite() && *value > 0.0);
        self.span_seconds = self.duration.unwrap_or(INITIAL_UNKNOWN_SPAN);
        let bins = self.duration.map_or(MAX_BINS, |duration| {
            ((duration * f64::from(sample_rate)).round().max(1.0) as usize).min(MAX_BINS)
        });
        self.complete = false;
        self.peaks.clear();
        self.peaks.resize(bins, None);
        self.max_peak = 1.0;
        self.revision = self.revision.wrapping_add(1);
    }

    pub(super) fn finish_scan(&mut self, end_seconds: f64) {
        if self.complete {
            return;
        }
        if self.duration.is_none() && end_seconds.is_finite() && end_seconds > 0.0 {
            self.duration = Some(end_seconds);
            if let Some(last) = self.peaks.iter().rposition(Option::is_some) {
                let bins = last + 1;
                self.span_seconds *= bins as f64 / self.peaks.len() as f64;
                self.peaks.truncate(bins);
            }
        }
        self.complete = true;
        self.revision = self.revision.wrapping_add(1);
    }

    /// `start_seconds` is the timestamp of the first retained decoded sample,
    /// relative to the CUE segment, not the requested seek or output clock.
    pub(super) fn push(
        &mut self,
        start_seconds: f64,
        samples: &[f32],
        sample_rate: u32,
        channels: usize,
    ) {
        if self.complete
            || !start_seconds.is_finite()
            || sample_rate == 0
            || channels == 0
            || self.peaks.is_empty()
        {
            return;
        }
        let count = samples.len() / channels;
        if count == 0 {
            return;
        }
        let rate = f64::from(sample_rate);
        let end_seconds = ((start_seconds * rate).round() + count as f64) / rate;
        if !end_seconds.is_finite() || end_seconds <= 0.0 {
            return;
        }
        let mut changed = false;
        if self.duration.is_none() {
            // Doubling merges adjacent bins exactly, unlike arbitrary rescaling
            // which would misplace or lose old peaks. Empty bins stay unknown.
            while end_seconds > self.span_seconds {
                for index in 0..MAX_BINS / 2 {
                    self.peaks[index] = match (self.peaks[index * 2], self.peaks[index * 2 + 1]) {
                        (Some(left), Some(right)) => Some(left.max(right)),
                        (left, right) => left.or(right),
                    };
                }
                self.peaks[MAX_BINS / 2..].fill(None);
                self.span_seconds *= 2.0;
                changed = true;
            }
        }
        let total_frames = (self.span_seconds * rate).round().max(1.0) as u64;
        let bins = self.peaks.len() as u128;
        // Work in sample indices so fractional packet/CUE timestamps cannot
        // put a boundary sample into the previous bin through floating error.
        let first_frame = (start_seconds.max(0.0) * rate).round() as u64;
        let mut offset = if start_seconds < 0.0 {
            ((-start_seconds * rate).round() as usize).min(count)
        } else {
            0
        };
        let initial_offset = offset;
        while offset < count {
            let frame = first_frame.saturating_add((offset - initial_offset) as u64);
            if frame >= total_frames {
                break;
            }
            let bin = (u128::from(frame) * bins / u128::from(total_frames)) as usize;
            let boundary = ((bin as u128 + 1) * u128::from(total_frames)).div_ceil(bins) as u64;
            let take = (boundary - frame).min((count - offset) as u64) as usize;
            let peak = samples[offset * channels..(offset + take) * channels]
                .iter()
                .filter(|sample| sample.is_finite())
                .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
            let previous = self.peaks[bin];
            let next = Some(previous.unwrap_or(0.0).max(peak));
            if previous != next {
                self.peaks[bin] = next;
                changed = true;
            }
            self.max_peak = self.max_peak.max(peak);
            offset += take;
        }
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(duration: Option<f64>, rate: u32) -> WaveformFrame {
        let mut frame = WaveformFrame::default();
        frame.select(Path::new("track.wav"), None, duration, rate);
        frame
    }

    #[test]
    fn packet_timestamp_gap_cannot_be_finalized_as_continuous_playback() {
        let mut frame = frame(None, 100);
        let mut coverage = WaveformCoverage::new(0.0);
        coverage.record(0.0, 1, 100);
        frame.push(0.0, &[0.5], 100, 1);
        coverage.record(1.0, 1, 100);
        frame.push(1.0, &[0.75], 100, 1);
        let revision = frame.revision;
        coverage.finish(&mut frame);
        assert!(!frame.complete);
        assert_eq!(frame.duration, None);
        assert_eq!(frame.revision, revision);
        assert!(frame.peaks[1..26].iter().all(Option::is_none));
    }

    #[test]
    fn full_scan_install_preserves_versions_and_skips_playback_recomputation() {
        let mut live = frame(Some(4.0), 1);
        live.push(0.0, &[0.25], 1, 1);
        let revision = live.revision;
        let mut full = frame(Some(4.0), 1);
        full.push(0.0, &[0.5, 0.0, 0.75, 0.25], 1, 1);
        live.install_full(full);
        assert_eq!(live.revision, revision + 1);
        assert!(live.complete);
        live.push(0.0, &[2.0], 1, 1);
        live.select(Path::new("track.wav"), None, Some(4.0), 1);
        assert_eq!(live.revision, revision + 1);
        live.finish_scan(4.0);
        assert_eq!(live.revision, revision + 1);
        assert_eq!(live.peaks, [Some(0.5), Some(0.0), Some(0.75), Some(0.25)]);
        live.select(Path::new("other.wav"), None, Some(4.0), 1);
        assert!(!live.complete);
        assert!(live.peaks.iter().all(Option::is_none));
    }

    #[test]
    fn finishing_unknown_scan_trims_only_undecoded_tail_without_moving_peaks() {
        let mut full = frame(None, 100);
        full.push(0.0, &[0.5], 100, 1);
        full.push(120.0, &[2.0], 100, 1);
        full.finish_scan(120.01);
        assert!(full.complete);
        assert_eq!(full.duration, Some(120.01));
        assert_eq!(full.peaks.len(), 801);
        assert_eq!(full.peaks[0], Some(0.5));
        assert_eq!(full.peaks[800], Some(2.0));
        assert!((full.span_seconds - 120.15).abs() < 1e-9);
    }

    #[test]
    fn short_clip_preserves_channel_peaks_silence_and_overfull_samples() {
        let mut frame = frame(Some(6.0 / 48_000.0), 48_000);
        frame.push(
            0.0,
            &[
                0.25,
                -0.25,
                0.0,
                0.5,
                -0.75,
                0.0,
                0.0,
                0.0,
                f32::NAN,
                f32::INFINITY,
                -2.0,
                1.5,
            ],
            48_000,
            2,
        );
        assert_eq!(
            frame.peaks,
            [
                Some(0.25),
                Some(0.5),
                Some(0.75),
                Some(0.0),
                Some(0.0),
                Some(2.0)
            ]
        );
        assert_eq!(frame.max_peak, 2.0);
    }

    #[test]
    fn seek_gaps_stay_unknown_and_back_reads_fill_without_losing_peaks() {
        let mut frame = frame(Some(8.0), 1);
        frame.push(0.0, &[0.5, 0.0], 1, 1);
        frame.push(6.0, &[-0.75], 1, 1);
        assert_eq!(
            frame.peaks,
            [
                Some(0.5),
                Some(0.0),
                None,
                None,
                None,
                None,
                Some(0.75),
                None
            ]
        );
        frame.push(2.0, &[0.25, 0.5], 1, 1);
        frame.push(6.0, &[0.1], 1, 1);
        assert_eq!(
            frame.peaks,
            [
                Some(0.5),
                Some(0.0),
                Some(0.25),
                Some(0.5),
                None,
                None,
                Some(0.75),
                None
            ]
        );
        assert_eq!(frame.max_peak, 1.0);
    }

    #[test]
    fn same_source_and_unchanged_pcm_do_not_revise_or_clear() {
        let mut frame = frame(Some(4.0), 1);
        frame.push(1.0, &[0.5], 1, 1);
        let revision = frame.revision;
        frame.select(Path::new("track.wav"), None, Some(99.0), 1);
        frame.push(1.0, &[0.5], 1, 1);
        frame.push(1.0, &[0.25], 1, 1);
        frame.push(1.0, &[], 1, 1);
        assert_eq!(frame.revision, revision);
        assert_eq!(frame.peaks, [None, Some(0.5), None, None]);
        assert_eq!(frame.duration, Some(4.0));
    }

    #[test]
    fn different_path_or_cue_range_resets_source_and_scale() {
        let mut frame = frame(Some(4.0), 1);
        frame.push(0.0, &[2.0], 1, 1);
        let cue = Some(PlaybackRange {
            start_seconds: 10.0,
            end_seconds: Some(12.0),
        });
        frame.select(Path::new("track.wav"), cue, Some(2.0), 1);
        assert!(frame.matches(Path::new("track.wav"), cue));
        assert_eq!(frame.peaks, [None, None]);
        assert_eq!(frame.max_peak, 1.0);
        frame.push(0.0, &[0.75], 1, 1);
        let revision = frame.revision;
        frame.select(Path::new("other.wav"), cue, Some(2.0), 1);
        assert_eq!(frame.peaks, [None, None]);
        assert_eq!(frame.revision, revision + 1);
    }

    #[test]
    fn unknown_duration_grows_bounded_timeline_and_retains_distant_peaks() {
        let mut frame = frame(None, 100);
        frame.push(0.0, &[2.0], 100, 1);
        frame.push(59.0, &[0.5], 100, 1);
        frame.push(240.0, &[0.75], 100, 1);
        assert_eq!(frame.duration, None);
        assert_eq!(frame.span_seconds, 480.0);
        assert_eq!(frame.peaks.len(), MAX_BINS);
        assert_eq!(frame.peaks[0], Some(2.0));
        assert_eq!(frame.peaks[196], Some(0.5));
        assert_eq!(frame.peaks[800], Some(0.75));
        assert!(frame.peaks[400..800].iter().all(Option::is_none));
        frame.push(120.0, &[0.25], 100, 1);
        assert_eq!(frame.peaks[400], Some(0.25));
        assert_eq!(frame.peaks[0], Some(2.0));
        assert_eq!(frame.max_peak, 2.0);
    }

    #[test]
    fn packets_crossing_bins_use_decoded_sample_timestamps() {
        let mut frame = frame(Some(2000.0 / 100.0), 100);
        frame.push(0.01, &[0.25, 0.5, 0.75, 1.0], 100, 1);
        assert_eq!(
            frame.peaks[..4],
            [Some(0.25), Some(0.5), Some(0.75), Some(1.0)]
        );
        frame.push(f64::NAN, &[4.0], 100, 1);
        assert_eq!(frame.max_peak, 1.0);
    }
}
