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
    /// Per-bin RMS energy, accumulated from the same PCM pass as `peaks`.
    pub(crate) rms: Vec<Option<f32>>,
    pub(crate) rms_counts: Vec<u64>,
    pub(crate) left_peaks: Vec<Option<f32>>,
    pub(crate) right_peaks: Vec<Option<f32>>,
    pub(crate) left_rms: Vec<Option<f32>>,
    pub(crate) right_rms: Vec<Option<f32>>,
    pub(crate) left_rms_counts: Vec<u64>,
    pub(crate) right_rms_counts: Vec<u64>,
    pub(crate) channel_weights: Vec<[f32; 2]>,
    pub(crate) last_push_start: Option<f64>,
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
            rms: Vec::new(),
            rms_counts: Vec::new(),
            left_peaks: Vec::new(),
            right_peaks: Vec::new(),
            left_rms: Vec::new(),
            right_rms: Vec::new(),
            left_rms_counts: Vec::new(),
            right_rms_counts: Vec::new(),
            channel_weights: Vec::new(),
            last_push_start: None,
        }
    }
}

fn merge_bin(peaks: &mut [Option<f32>], rms: &mut [Option<f32>], counts: &mut [u64], index: usize) {
    peaks[index] = match (peaks[index * 2], peaks[index * 2 + 1]) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    };
    let left = index * 2;
    let right = left + 1;
    let count = counts[left].saturating_add(counts[right]);
    rms[index] = if count == 0 {
        None
    } else {
        let left_energy = rms[left].map_or(0.0, |value| {
            f64::from(value) * f64::from(value) * counts[left] as f64
        });
        let right_energy = rms[right].map_or(0.0, |value| {
            f64::from(value) * f64::from(value) * counts[right] as f64
        });
        Some(((left_energy + right_energy) / count as f64).sqrt() as f32)
    };
    counts[index] = count;
}

fn update_bin(
    peaks: &mut [Option<f32>],
    rms: &mut [Option<f32>],
    counts: &mut [u64],
    index: usize,
    peak: f32,
    energy: f64,
    sample_count: u64,
) -> bool {
    if sample_count == 0 {
        return false;
    }
    let mut changed = false;
    let previous = peaks[index];
    let next = Some(previous.unwrap_or(0.0).max(peak));
    if previous != next {
        peaks[index] = next;
        changed = true;
    }
    let old_count = counts[index];
    let old_energy = rms[index].map_or(0.0, |value| {
        f64::from(value) * f64::from(value) * old_count as f64
    });
    let count = old_count.saturating_add(sample_count);
    let next_rms = ((old_energy + energy) / count as f64).sqrt() as f32;
    if rms[index] != Some(next_rms) {
        rms[index] = Some(next_rms);
        changed = true;
    }
    counts[index] = count;
    changed
}

/// Playback-owned statistics. PCM traversal and timeline compaction happen here,
/// never under the GUI's shared-frame write lock.
pub(super) struct WaveformAccumulator {
    frame: WaveformFrame,
    published_revision: u64,
    dirty: Option<std::ops::Range<usize>>,
}

impl WaveformAccumulator {
    pub(super) fn new(frame: WaveformFrame) -> Self {
        Self {
            published_revision: frame.revision,
            frame,
            dirty: None,
        }
    }

    pub(super) fn set_channel_weights(&mut self, weights: &[[f32; 2]]) {
        self.frame.set_channel_weights(weights);
    }

    pub(super) fn push(&mut self, start: f64, samples: &[f32], rate: u32, channels: usize) {
        self.frame
            .push_bins(start, samples, rate, channels, &mut self.dirty);
    }

    pub(super) fn finish(&mut self, coverage: &WaveformCoverage) {
        coverage.finish(&mut self.frame);
    }

    /// The only operation requiring the shared write lock. At most MAX_BINS
    /// bins are copied (all bins only after timeline doubling); no PCM is read.
    /// A manually installed complete scan always wins over in-flight playback.
    pub(super) fn publish(&mut self, shared: &mut WaveformFrame) {
        if shared.path != self.frame.path
            || shared.range != self.frame.range
            || shared.revision != self.published_revision
            || shared.complete
        {
            self.frame.complete = true;
            self.dirty = None;
            return;
        }
        let dirty = self.dirty.take().unwrap_or(0..0);
        let end = dirty.end.min(self.frame.peaks.len());
        let start = dirty.start.min(end);
        macro_rules! publish_bins {
            ($($field:ident),+ $(,)?) => { $(
                shared.$field[start..end].copy_from_slice(&self.frame.$field[start..end]);
                shared.$field.truncate(self.frame.$field.len());
            )+ };
        }
        publish_bins!(
            peaks,
            rms,
            rms_counts,
            left_peaks,
            right_peaks,
            left_rms,
            right_rms,
            left_rms_counts,
            right_rms_counts
        );
        shared.complete = self.frame.complete;
        shared.duration = self.frame.duration;
        shared.span_seconds = self.frame.span_seconds;
        shared.max_peak = self.frame.max_peak;
        shared.last_push_start = self.frame.last_push_start;
        if shared.channel_weights != self.frame.channel_weights {
            shared
                .channel_weights
                .clone_from(&self.frame.channel_weights);
        }
        shared.revision = self.frame.revision;
        self.published_revision = shared.revision;
    }
}

impl WaveformFrame {
    pub fn matches(&self, path: &Path, range: Option<PlaybackRange>) -> bool {
        self.path.as_deref() == Some(path) && self.range == range
    }
    pub(super) fn set_channel_weights(&mut self, weights: &[[f32; 2]]) {
        self.channel_weights.clear();
        self.channel_weights.extend_from_slice(weights);
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
        self.rms.clear();
        self.rms.resize(bins, None);
        self.rms_counts.clear();
        self.rms_counts.resize(bins, 0);
        self.left_peaks.clear();
        self.left_peaks.resize(bins, None);
        self.right_peaks.clear();
        self.right_peaks.resize(bins, None);
        self.left_rms.clear();
        self.left_rms.resize(bins, None);
        self.right_rms.clear();
        self.right_rms.resize(bins, None);
        self.left_rms_counts.clear();
        self.left_rms_counts.resize(bins, 0);
        self.right_rms_counts.clear();
        self.right_rms_counts.resize(bins, 0);
        self.channel_weights.clear();
        self.max_peak = 1.0;
        self.last_push_start = None;
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
                self.rms.truncate(bins);
                self.rms_counts.truncate(bins);
                self.left_peaks.truncate(bins);
                self.right_peaks.truncate(bins);
                self.left_rms.truncate(bins);
                self.right_rms.truncate(bins);
                self.left_rms_counts.truncate(bins);
                self.right_rms_counts.truncate(bins);
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
        self.push_bins(start_seconds, samples, sample_rate, channels, &mut None);
    }

    fn push_bins(
        &mut self,
        start_seconds: f64,
        samples: &[f32],
        sample_rate: u32,
        channels: usize,
        dirty: &mut Option<std::ops::Range<usize>>,
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
        if self.last_push_start == Some(start_seconds) {
            return;
        }
        self.last_push_start = Some(start_seconds);
        let mut changed = false;
        if self.duration.is_none() {
            // Doubling merges adjacent bins exactly, unlike arbitrary rescaling
            // which would misplace or lose old peaks. Empty bins stay unknown.
            while end_seconds > self.span_seconds {
                for index in 0..MAX_BINS / 2 {
                    merge_bin(&mut self.peaks, &mut self.rms, &mut self.rms_counts, index);
                    merge_bin(
                        &mut self.left_peaks,
                        &mut self.left_rms,
                        &mut self.left_rms_counts,
                        index,
                    );
                    merge_bin(
                        &mut self.right_peaks,
                        &mut self.right_rms,
                        &mut self.right_rms_counts,
                        index,
                    );
                }
                self.peaks[MAX_BINS / 2..].fill(None);
                self.rms[MAX_BINS / 2..].fill(None);
                self.rms_counts[MAX_BINS / 2..].fill(0);
                self.left_peaks[MAX_BINS / 2..].fill(None);
                self.right_peaks[MAX_BINS / 2..].fill(None);
                self.left_rms[MAX_BINS / 2..].fill(None);
                self.right_rms[MAX_BINS / 2..].fill(None);
                self.left_rms_counts[MAX_BINS / 2..].fill(0);
                self.right_rms_counts[MAX_BINS / 2..].fill(0);
                self.span_seconds *= 2.0;
                changed = true;
                *dirty = Some(0..self.peaks.len());
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
            let dirty = dirty.get_or_insert(bin..bin + 1);
            dirty.start = dirty.start.min(bin);
            dirty.end = dirty.end.max(bin + 1);
            let boundary = ((bin as u128 + 1) * u128::from(total_frames)).div_ceil(bins) as u64;
            let take = (boundary - frame).min((count - offset) as u64) as usize;
            let mut combined_peak = 0.0_f32;
            let mut combined_energy = 0.0_f64;
            let mut combined_count = 0_u64;
            let mut side_peaks = [0.0_f32; 2];
            let mut side_energy = [0.0_f64; 2];
            let mut side_counts = [0_u64; 2];
            for frame_offset in 0..take {
                let base = (offset + frame_offset) * channels;
                let mut mixed = [0.0_f32; 2];
                let mut finite = false;
                for channel in 0..channels {
                    let sample = samples[base + channel];
                    if !sample.is_finite() {
                        continue;
                    }
                    finite = true;
                    let magnitude = sample.abs();
                    combined_peak = combined_peak.max(magnitude);
                    combined_energy += f64::from(sample) * f64::from(sample);
                    combined_count += 1;
                    let fallback = if channels == 1 {
                        [1.0, 0.0]
                    } else if channels == 2 {
                        [
                            if channel == 0 { 1.0 } else { 0.0 },
                            if channel == 1 { 1.0 } else { 0.0 },
                        ]
                    } else {
                        [0.5, 0.5]
                    };
                    let weight = self
                        .channel_weights
                        .get(channel)
                        .copied()
                        .unwrap_or(fallback);
                    mixed[0] += sample * weight[0];
                    if channels != 1 {
                        mixed[1] += sample * weight[1];
                    }
                }
                if finite {
                    side_peaks[0] = side_peaks[0].max(mixed[0].abs());
                    side_energy[0] += f64::from(mixed[0]) * f64::from(mixed[0]);
                    side_counts[0] += 1;
                    if channels != 1 {
                        side_peaks[1] = side_peaks[1].max(mixed[1].abs());
                        side_energy[1] += f64::from(mixed[1]) * f64::from(mixed[1]);
                        side_counts[1] += 1;
                    }
                } else {
                    side_counts[0] += 1;
                    if channels != 1 {
                        side_counts[1] += 1;
                    }
                }
                if channels == 1 {
                    side_peaks[1] = side_peaks[0];
                    side_energy[1] = side_energy[0];
                    side_counts[1] = side_counts[0];
                }
            }
            changed |= update_bin(
                &mut self.peaks,
                &mut self.rms,
                &mut self.rms_counts,
                bin,
                combined_peak,
                combined_energy,
                combined_count.max(1),
            );
            changed |= update_bin(
                &mut self.left_peaks,
                &mut self.left_rms,
                &mut self.left_rms_counts,
                bin,
                side_peaks[0],
                side_energy[0],
                side_counts[0],
            );
            changed |= update_bin(
                &mut self.right_peaks,
                &mut self.right_rms,
                &mut self.right_rms_counts,
                bin,
                side_peaks[1],
                side_energy[1],
                side_counts[1],
            );
            self.max_peak = self
                .max_peak
                .max(combined_peak)
                .max(side_peaks[0])
                .max(side_peaks[1]);
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
    fn assert_same_bins(actual: &WaveformFrame, expected: &WaveformFrame) {
        assert_eq!(actual.peaks, expected.peaks);
        assert_eq!(actual.rms, expected.rms);
        assert_eq!(actual.rms_counts, expected.rms_counts);
        assert_eq!(actual.left_peaks, expected.left_peaks);
        assert_eq!(actual.right_peaks, expected.right_peaks);
        assert_eq!(actual.left_rms, expected.left_rms);
        assert_eq!(actual.right_rms, expected.right_rms);
        assert_eq!(actual.left_rms_counts, expected.left_rms_counts);
        assert_eq!(actual.right_rms_counts, expected.right_rms_counts);
        assert_eq!(actual.revision, expected.revision);
        assert_eq!(actual.complete, expected.complete);
        assert_eq!(actual.duration, expected.duration);
        assert_eq!(actual.span_seconds, expected.span_seconds);
        assert_eq!(actual.max_peak, expected.max_peak);
        assert_eq!(actual.last_push_start, expected.last_push_start);
    }

    #[test]
    fn local_accumulation_publishes_counts_doubling_seek_and_finish_exactly() {
        for duration in [None, Some(240.0)] {
            let mut shared = frame(duration, 100);
            let mut expected = shared.clone();
            let mut accumulator = WaveformAccumulator::new(shared.clone());
            accumulator.set_channel_weights(&[[0.8, 0.2], [0.1, 0.9]]);
            expected.set_channel_weights(&[[0.8, 0.2], [0.1, 0.9]]);
            for start in [0.0, 0.01, 59.99, 61.0, 121.0, 0.02] {
                let samples = [0.2, -0.5, 0.8, 0.1];
                accumulator.push(start, &samples, 100, 2);
                expected.push(start, &samples, 100, 2);
                accumulator.publish(&mut shared);
                assert_same_bins(&shared, &expected);
            }
            shared.select(Path::new("track.wav"), None, duration, 100);
            let mut reopened = WaveformAccumulator::new(shared.clone());
            reopened.push(20.0, &[0.7, 0.3], 100, 2);
            expected.push(20.0, &[0.7, 0.3], 100, 2);
            reopened.publish(&mut shared);
            assert_same_bins(&shared, &expected);
            let mut coverage = WaveformCoverage::new(0.0);
            coverage.record(0.0, 12300, 100);
            reopened.finish(&coverage);
            coverage.finish(&mut expected);
            reopened.publish(&mut shared);
            assert_same_bins(&shared, &expected);
        }
    }

    #[test]
    fn local_publication_preserves_manual_completion_and_source_switch() {
        let mut shared = frame(Some(4.0), 1);
        let mut accumulator = WaveformAccumulator::new(shared.clone());
        accumulator.push(0.0, &[2.0], 1, 1);
        let mut manual = frame(Some(4.0), 1);
        manual.push(0.0, &[0.25, 0.5, 0.75, 1.0], 1, 1);
        shared.install_full(manual);
        let expected = shared.clone();
        accumulator.publish(&mut shared);
        accumulator.push(1.0, &[3.0], 1, 1);
        accumulator.publish(&mut shared);
        assert_same_bins(&shared, &expected);
        shared.select(Path::new("other.wav"), None, Some(4.0), 1);
        let expected = shared.clone();
        accumulator.publish(&mut shared);
        assert_same_bins(&shared, &expected);
    }

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
    fn multichannel_samples_fold_into_layout_aware_left_and_right() {
        let mut frame = frame(Some(1.0), 1);
        frame.set_channel_weights(&[[0.5, 0.0], [0.0, 0.5], [0.5, 0.5]]);
        frame.push(0.0, &[1.0, 2.0, 3.0], 1, 3);
        assert_eq!(frame.left_peaks, [Some(2.0)]);
        assert_eq!(frame.right_peaks, [Some(2.5)]);
        assert_eq!(frame.left_rms, [Some(2.0)]);
        assert_eq!(frame.right_rms, [Some(2.5)]);
    }

    #[test]
    fn mono_samples_are_calculated_once_and_mirrored() {
        let mut frame = frame(Some(1.0), 1);
        frame.push(0.0, &[-0.5], 1, 1);
        assert_eq!(frame.left_peaks, frame.right_peaks);
        assert_eq!(frame.left_rms, frame.right_rms);
        assert_eq!(frame.left_peaks, [Some(0.5)]);
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
