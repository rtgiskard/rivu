//! A bounded, progressively filled envelope of the PCM already decoded for playback.

use super::{
    PlaybackRange,
    source::{Source, SourceIdentity},
};
use rivu_format::media::DecoderHint;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

const MAX_BINS: usize = 1024;
const INITIAL_UNKNOWN_SPAN: f64 = 60.0;

/// Coverage of one decoder traversal. Cached bins do not prove that seek-skipped
/// intervals or PCM omitted in Still Mode were accumulated during this traversal.
pub(super) struct WaveformCoverage {
    continuous: bool,
    end_seconds: f64,
    timestamp_tolerance: f64,
}

impl WaveformCoverage {
    pub(super) fn new(start_seconds: f64, timestamp_tolerance: f64) -> Self {
        Self {
            continuous: start_seconds == 0.0,
            end_seconds: 0.0,
            timestamp_tolerance,
        }
    }

    pub(super) fn record(&mut self, start_seconds: f64, frames: usize, sample_rate: u32) {
        if !start_seconds.is_finite() || sample_rate == 0 {
            self.continuous = false;
            return;
        }
        let rate = f64::from(sample_rate);
        if start_seconds > self.end_seconds + self.timestamp_tolerance.max(0.5 / rate) {
            self.continuous = false;
        }
        self.end_seconds = self
            .end_seconds
            .max(((start_seconds * rate).round() + frames as f64) / rate);
    }

    /// PCM omitted from the envelope cannot be recovered by later packets.
    pub(super) fn skip(&mut self) {
        self.continuous = false;
    }

    pub(super) fn finish(&self, frame: &mut WaveformFrame) {
        frame.finish_at_eof(self.end_seconds, self.continuous);
    }
}

/// One earlier contiguous candidate, never a history of seeks. Choosing the
/// earliest prefix ensures a packetized full traversal can reach cached RMS.
#[derive(Clone, Debug)]
pub(crate) struct RmsPrefix {
    span: std::ops::Range<u64>,
    energy: [f64; 3],
    counts: [u64; 3],
}

/// Captured when a decoder is loaded, never refreshed from a path or library
/// snapshot. Keep the initial duration separate from the EOF-corrected timeline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WaveformIdentity {
    Progressive {
        content: SourceIdentity,
        ffmpeg: bool,
        duration: Option<f64>,
        sample_rate: u32,
        channels: u16,
    },
    FullScan {
        duration: Option<f64>,
        sample_rate: u32,
    },
}

#[derive(Clone, Debug)]
pub struct WaveformFrame {
    pub revision: u64,
    pub complete: bool,
    pub path: Option<PathBuf>,
    pub range: Option<PlaybackRange>,
    pub decoder_hint: DecoderHint,
    pub duration: Option<f64>,
    pub span_seconds: f64,
    // Sample width is bin_frames / bin_count, independent of EOF array truncation.
    // Widen the numerator so coarsening cannot overflow the u64 sample domain.
    pub bin_frames: u128,
    pub bin_count: u64,
    pub max_peak: f32,
    /// `None` is not yet decoded; `Some(0.0)` is decoded silence.
    pub peaks: Vec<Option<f32>>,
    /// RMS of the current contiguous decoded interval in each rolling bin.
    pub rms: Vec<Option<f32>>,
    pub rms_counts: Vec<u64>,
    pub left_peaks: Vec<Option<f32>>,
    pub right_peaks: Vec<Option<f32>>,
    pub left_rms: Vec<Option<f32>>,
    pub right_rms: Vec<Option<f32>>,
    pub left_rms_counts: Vec<u64>,
    pub right_rms_counts: Vec<u64>,
    pub(crate) channel_weights: Vec<[f32; 2]>,
    pub rms_spans: Vec<Option<std::ops::Range<u64>>>,
    pub(crate) rms_prefixes: Vec<Option<RmsPrefix>>,
    /// Last PCM revision per bin, used to fence asynchronous full scans.
    pub bin_revisions: Vec<u64>,
    pub bin_epochs: Vec<u64>,
    pub(crate) traversal: u64,
    pub(crate) suspended: bool,
    pub(crate) suspension_generation: u64,
    pub(crate) full_scan_cancelled: Weak<AtomicBool>,
    pub identity: Option<WaveformIdentity>,
    pub scan_source_identity: Option<SourceIdentity>,
}

impl Default for WaveformFrame {
    fn default() -> Self {
        Self {
            revision: 0,
            path: None,
            complete: false,
            range: None,
            decoder_hint: DecoderHint::Auto,
            duration: None,
            span_seconds: 0.0,
            bin_frames: 0,
            bin_count: 0,
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
            rms_spans: Vec::new(),
            rms_prefixes: Vec::new(),
            bin_revisions: Vec::new(),
            bin_epochs: Vec::new(),
            traversal: 1,
            suspended: false,
            suspension_generation: 0,
            full_scan_cancelled: Weak::new(),
            identity: None,
            scan_source_identity: None,
        }
    }
}

/// Compact at most four disjoint intervals on the stack. Adjacent intervals
/// merge; across gaps, display the longest and retain the earliest prefix.
fn compact_statistics(intervals: [Option<RmsPrefix>; 4]) -> (Option<RmsPrefix>, Option<RmsPrefix>) {
    let mut merged: [Option<RmsPrefix>; 4] = [None, None, None, None];
    let mut len = 0;
    for interval in intervals.into_iter().flatten() {
        if len > 0 && merged[len - 1].as_ref().unwrap().span.end == interval.span.start {
            let previous = merged[len - 1].as_mut().unwrap();
            previous.span.end = interval.span.end;
            for side in 0..3 {
                previous.energy[side] += interval.energy[side];
                previous.counts[side] = previous.counts[side].saturating_add(interval.counts[side]);
            }
        } else {
            merged[len] = Some(interval);
            len += 1;
        }
    }
    if len == 0 {
        return (None, None);
    }
    let mut longest = 0;
    for index in 1..len {
        let span = &merged[index].as_ref().unwrap().span;
        let best = &merged[longest].as_ref().unwrap().span;
        if span.end - span.start > best.end - best.start {
            longest = index;
        }
    }
    let primary = merged[longest].take().unwrap();
    let prefix = merged
        .into_iter()
        .flatten()
        .find(|interval| interval.span.end <= primary.span.start);
    (Some(primary), prefix)
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
    let mut changed = false;
    let previous = peaks[index];
    let next = Some(previous.unwrap_or(0.0).max(peak));
    if previous != next {
        peaks[index] = next;
        changed = true;
    }
    if sample_count == 0 {
        return changed;
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
    changed |= count != old_count;
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
    pub(super) fn new(mut frame: WaveformFrame) -> Self {
        frame.begin_traversal();
        Self {
            published_revision: frame.revision,
            frame,
            dirty: None,
        }
    }

    /// Keep the rolling display, replacing a bin on its next visit.
    pub(super) fn begin_traversal(&mut self) {
        self.frame.begin_traversal();
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
    /// EOF is coverage metadata, never a reason to freeze a rolling envelope.
    pub(super) fn publish(&mut self, shared: &mut WaveformFrame) {
        if shared.path != self.frame.path
            || shared.range != self.frame.range
            || shared.decoder_hint != self.frame.decoder_hint
            || shared.identity != self.frame.identity
            || shared.revision != self.published_revision
            || shared.suspended
        {
            self.frame.complete = true;
            self.dirty = None;
            return;
        }
        // Publish statistics only: controls (including scan cancellation) may
        // have changed since the private seed was captured without a revision.
        let initial = shared.peaks.is_empty();
        let dirty = self.dirty.take().unwrap_or(0..0);
        let end = dirty.end.min(self.frame.peaks.len());
        let start = dirty.start.min(end);
        macro_rules! publish_bins {
            ($($field:ident),+ $(,)?) => { $(
                if initial {
                    shared.$field.clone_from(&self.frame.$field);
                } else {
                    shared.$field[start..end].clone_from_slice(&self.frame.$field[start..end]);
                    shared.$field.truncate(self.frame.$field.len());
                }
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
            right_rms_counts,
            bin_revisions,
            bin_epochs,
            rms_spans,
            rms_prefixes
        );
        shared.complete = self.frame.complete;
        shared.duration = self.frame.duration;
        shared.span_seconds = self.frame.span_seconds;
        shared.bin_frames = self.frame.bin_frames;
        shared.bin_count = self.frame.bin_count;
        shared.max_peak = self.frame.max_peak;
        shared.traversal = self.frame.traversal;
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
    fn begin_traversal(&mut self) {
        self.traversal = self.traversal.wrapping_add(1);
        if self.traversal == 0 {
            self.bin_epochs.fill(0);
            self.traversal = 1;
        }
        self.complete = false;
    }

    fn clear_bin(&mut self, bin: usize) {
        for values in [
            &mut self.peaks,
            &mut self.rms,
            &mut self.left_peaks,
            &mut self.right_peaks,
            &mut self.left_rms,
            &mut self.right_rms,
        ] {
            values[bin] = None;
        }
        for counts in [
            &mut self.rms_counts,
            &mut self.left_rms_counts,
            &mut self.right_rms_counts,
        ] {
            counts[bin] = 0;
        }
        self.rms_spans[bin] = None;
        self.rms_prefixes[bin] = None;
    }

    pub fn matches(
        &self,
        path: &Path,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
    ) -> bool {
        self.path.as_deref() == Some(path)
            && self.range == range
            && self.decoder_hint == decoder_hint
    }

    pub fn suspension_generation(&self) -> u64 {
        self.suspension_generation
    }

    pub fn cancel_scan_on_suspend(&mut self, cancelled: &Arc<AtomicBool>) {
        if self.suspended {
            cancelled.store(true, Ordering::Relaxed);
        } else {
            self.full_scan_cancelled = Arc::downgrade(cancelled);
        }
    }
    #[cfg(test)]
    pub(super) fn retained_capacity_bytes(&self) -> usize {
        fn bytes<T>(values: &Vec<T>) -> usize {
            values.capacity() * std::mem::size_of::<T>()
        }
        bytes(&self.peaks)
            + bytes(&self.rms)
            + bytes(&self.rms_counts)
            + bytes(&self.left_peaks)
            + bytes(&self.right_peaks)
            + bytes(&self.left_rms)
            + bytes(&self.right_rms)
            + bytes(&self.left_rms_counts)
            + bytes(&self.right_rms_counts)
            + bytes(&self.channel_weights)
            + bytes(&self.rms_spans)
            + bytes(&self.rms_prefixes)
            + bytes(&self.bin_revisions)
            + bytes(&self.bin_epochs)
    }

    /// Still Mode owns this guard so late manual scans cannot restore results.
    pub fn set_still_mode(&mut self, suspended: bool) {
        if self.suspended == suspended {
            return;
        }
        self.suspended = suspended;
        if suspended {
            if let Some(cancelled) = self.full_scan_cancelled.upgrade() {
                cancelled.store(true, Ordering::Relaxed);
            }
            *self = Self {
                revision: self.revision.wrapping_add(1),
                path: self.path.take(),
                range: self.range,
                decoder_hint: self.decoder_hint,
                identity: self.identity,
                duration: self.duration,
                span_seconds: self.span_seconds,
                suspended,
                suspension_generation: self.suspension_generation.wrapping_add(1),
                ..Self::default()
            };
        }
    }

    pub(super) fn set_channel_weights(&mut self, weights: &[[f32; 2]]) {
        if self.suspended {
            return;
        }
        self.channel_weights.clear();
        self.channel_weights.extend_from_slice(weights);
    }

    fn bin_statistics(&self, index: usize) -> Option<RmsPrefix> {
        self.rms_spans[index].clone().map(|span| {
            let counts = [
                self.rms_counts[index],
                self.left_rms_counts[index],
                self.right_rms_counts[index],
            ];
            let values = [self.rms[index], self.left_rms[index], self.right_rms[index]];
            let energy = std::array::from_fn(|side| {
                values[side].map_or(0.0, |value| f64::from(value).powi(2) * counts[side] as f64)
            });
            RmsPrefix {
                span,
                energy,
                counts,
            }
        })
    }

    fn compact_bin(&mut self, index: usize) {
        let left = index * 2;
        let right = left + 1;
        let epoch = if self.peaks[left].is_none() {
            self.bin_epochs.get(right).copied().unwrap_or(0)
        } else if self.peaks.get(right).is_none_or(Option::is_none)
            || self.bin_epochs.get(right) == Some(&self.bin_epochs[left])
        {
            self.bin_epochs[left]
        } else {
            0
        };
        let intervals = [
            self.rms_prefixes[left].take(),
            self.bin_statistics(left),
            self.rms_prefixes.get_mut(right).and_then(Option::take),
            (right < self.peaks.len())
                .then(|| self.bin_statistics(right))
                .flatten(),
        ];
        let (primary, prefix) = compact_statistics(intervals);
        for peaks in [&mut self.peaks, &mut self.left_peaks, &mut self.right_peaks] {
            peaks[index] = match (peaks[left], peaks.get(right).copied().flatten()) {
                (Some(left), Some(right)) => Some(left.max(right)),
                (left, right) => left.or(right),
            };
        }
        for (side, (rms, counts)) in [
            (&mut self.rms, &mut self.rms_counts),
            (&mut self.left_rms, &mut self.left_rms_counts),
            (&mut self.right_rms, &mut self.right_rms_counts),
        ]
        .into_iter()
        .enumerate()
        {
            let count = primary.as_ref().map_or(0, |primary| primary.counts[side]);
            counts[index] = count;
            rms[index] = primary.as_ref().and_then(|primary| {
                (count != 0).then(|| (primary.energy[side] / count as f64).sqrt() as f32)
            });
        }
        self.rms_spans[index] = primary.map(|primary| primary.span);
        self.rms_prefixes[index] = prefix;
        self.bin_revisions[index] =
            self.bin_revisions[left].max(self.bin_revisions.get(right).copied().unwrap_or(0));
        self.bin_epochs[index] = epoch;
    }

    /// Only a decoder with the same opened content, backend and initial audio
    /// format may reuse coverage. A seek keeps this decoder and its identity.
    pub(super) fn select_source(
        &mut self,
        path: &Path,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
        source: &Source,
    ) {
        let duration = source
            .info()
            .duration
            .filter(|value| value.is_finite() && *value > 0.0);
        self.select_identity(
            path,
            range,
            decoder_hint,
            duration,
            WaveformIdentity::Progressive {
                content: source.identity(),
                ffmpeg: source.uses_ffmpeg(),
                duration,
                sample_rate: source.info().sample_rate,
                channels: source.info().channels,
            },
        );
    }
    pub(super) fn select_audition_source(
        &mut self,
        path: &Path,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
        source: &Source,
        display_duration: Option<f64>,
    ) {
        let duration = display_duration.filter(|value| value.is_finite() && *value > 0.0);
        self.select_identity(
            path,
            range,
            decoder_hint,
            duration,
            WaveformIdentity::Progressive {
                content: source.identity(),
                ffmpeg: source.uses_ffmpeg(),
                duration,
                sample_rate: source.info().sample_rate,
                channels: source.info().channels,
            },
        );
    }

    fn select_identity(
        &mut self,
        path: &Path,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
        duration: Option<f64>,
        identity: WaveformIdentity,
    ) {
        if self.matches(path, range, decoder_hint) && self.identity == Some(identity) {
            return;
        }
        self.identity = Some(identity);
        self.path = Some(path.to_owned());
        self.range = range;
        self.decoder_hint = decoder_hint;
        self.duration = duration.filter(|value| value.is_finite() && *value > 0.0);
        self.span_seconds = self.duration.unwrap_or(INITIAL_UNKNOWN_SPAN);
        self.bin_frames = 0;
        self.bin_count = 0;
        self.complete = false;
        self.peaks.clear();
        self.rms.clear();
        self.rms_counts.clear();
        self.left_peaks.clear();
        self.right_peaks.clear();
        self.left_rms.clear();
        self.right_rms.clear();
        self.left_rms_counts.clear();
        self.right_rms_counts.clear();
        self.rms_spans.clear();
        self.rms_prefixes.clear();
        self.bin_revisions.clear();
        self.bin_epochs.clear();
        self.traversal = 1;
        self.channel_weights.clear();
        self.max_peak = 1.0;
        self.revision = self.revision.wrapping_add(1);
    }

    pub(super) fn select(
        &mut self,
        path: &Path,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
        duration: Option<f64>,
        sample_rate: u32,
    ) {
        let duration = duration.filter(|value| value.is_finite() && *value > 0.0);
        self.select_identity(
            path,
            range,
            decoder_hint,
            duration,
            WaveformIdentity::FullScan {
                duration,
                sample_rate,
            },
        );
        self.prepare_bins(sample_rate);
    }

    pub(super) fn prepare_bins(&mut self, sample_rate: u32) {
        if self.suspended || !self.peaks.is_empty() {
            return;
        }
        let bins = self.duration.map_or(MAX_BINS, |duration| {
            ((duration * f64::from(sample_rate)).round().max(1.0) as usize).min(MAX_BINS)
        });
        self.bin_frames = (self.span_seconds * f64::from(sample_rate))
            .round()
            .max(1.0) as u64 as u128;
        self.bin_count = bins as u64;
        self.peaks.resize(bins, None);
        self.rms.resize(bins, None);
        self.rms_counts.resize(bins, 0);
        self.left_peaks.resize(bins, None);
        self.right_peaks.resize(bins, None);
        self.left_rms.resize(bins, None);
        self.right_rms.resize(bins, None);
        self.left_rms_counts.resize(bins, 0);
        self.right_rms_counts.resize(bins, 0);
        self.rms_spans.resize(bins, None);
        self.rms_prefixes.resize(bins, None);
        self.bin_revisions.resize(bins, 0);
        self.bin_epochs.resize(bins, 0);
    }

    pub(super) fn finish_scan(&mut self, end_seconds: f64) {
        self.finish_at_eof(end_seconds, true);
    }

    fn finish_at_eof(&mut self, end_seconds: f64, complete: bool) {
        if self.suspended || self.complete || !end_seconds.is_finite() || end_seconds <= 0.0 {
            return;
        }
        self.duration = Some(end_seconds);
        if !self.peaks.is_empty() && self.span_seconds > 0.0 {
            // EOF proves the timeline end independently of PCM coverage.
            // Keep unknown bins inside that endpoint, including a skipped tail.
            let bins = ((end_seconds / self.span_seconds * self.peaks.len() as f64).ceil()
                as usize)
                .clamp(1, self.peaks.len());
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
            self.rms_spans.truncate(bins);
            self.rms_prefixes.truncate(bins);
            self.bin_revisions.truncate(bins);
            self.bin_epochs.truncate(bins);
        }
        self.complete = complete;
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
        if self.suspended
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
        let mut rescale = false;
        // Metadata duration may be an estimate. Grow the bounded timeline for
        // actual decoded PCM, retaining pairwise peak and RMS statistics.
        if end_seconds > self.span_seconds + 0.5 / rate {
            self.duration = None;
            while end_seconds > self.span_seconds + 0.5 / rate {
                let retained = self.peaks.len().div_ceil(2);
                for index in 0..retained {
                    self.compact_bin(index);
                }
                self.peaks[retained..].fill(None);
                self.rms[retained..].fill(None);
                self.rms_counts[retained..].fill(0);
                self.left_peaks[retained..].fill(None);
                self.right_peaks[retained..].fill(None);
                self.left_rms[retained..].fill(None);
                self.right_rms[retained..].fill(None);
                self.left_rms_counts[retained..].fill(0);
                self.right_rms_counts[retained..].fill(0);
                self.rms_spans[retained..].fill(None);
                self.rms_prefixes[retained..].fill(None);
                self.bin_revisions[retained..].fill(0);
                self.bin_epochs[retained..].fill(0);
                self.span_seconds *= 2.0;
                self.bin_frames = self.bin_frames.saturating_mul(2);
                changed = true;
                *dirty = Some(0..self.peaks.len());
            }
        }
        let total_frames = self.bin_frames;
        let bins = u128::from(self.bin_count);
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
            if frame == u64::MAX {
                break;
            }
            let bin = (u128::from(frame) * bins / total_frames) as usize;
            if bin >= self.peaks.len() {
                break;
            }
            let boundary = ((bin as u128 + 1) * total_frames)
                .div_ceil(bins)
                .min(u128::from(u64::MAX)) as u64;
            let mut take = (boundary - frame).min((count - offset) as u64) as usize;
            if self.bin_epochs[bin] != self.traversal {
                rescale |= self.max_peak > 1.0
                    && [&self.peaks, &self.left_peaks, &self.right_peaks]
                        .iter()
                        .any(|peaks| peaks[bin].is_some_and(|peak| peak >= self.max_peak));
                self.clear_bin(bin);
                self.bin_epochs[bin] = self.traversal;
                self.complete = false;
                changed = true;
            }
            let primary = self.rms_spans[bin].as_ref();
            let prefix = self.rms_prefixes[bin].as_ref();
            let mut represented = false;
            for span in [primary, prefix.map(|prefix| &prefix.span)]
                .into_iter()
                .flatten()
            {
                if frame < span.start {
                    take = take.min((span.start - frame) as usize);
                } else if frame < span.end {
                    take = take.min((span.end - frame) as usize);
                    represented = true;
                }
            }
            if represented {
                // Cached main/prefix overlaps are skipped before reading PCM.
                offset += take;
                continue;
            }
            let end = frame + take as u64;
            let accumulate = primary.is_none_or(|span| end == span.start || frame == span.end);
            let collect_prefix = !accumulate
                && primary.is_some_and(|span| end <= span.start)
                && prefix
                    .is_none_or(|prefix| frame < prefix.span.start || frame == prefix.span.end);
            let dirty = dirty.get_or_insert(bin..bin + 1);
            dirty.start = dirty.start.min(bin);
            dirty.end = dirty.end.max(bin + 1);
            self.bin_revisions[bin] = self.revision.wrapping_add(1);
            let mut combined_peak = 0.0_f32;
            let mut combined_energy = 0.0_f64;
            let combined_count = (take * channels) as u64;
            let mut side_peaks = [0.0_f32; 2];
            let mut side_energy = [0.0_f64; 2];
            let side_count = take as u64;
            // Source conversions already guarantee finite decoded PCM.
            for frame_offset in 0..take {
                let base = (offset + frame_offset) * channels;
                let mut mixed = [0.0_f32; 2];
                for channel in 0..channels {
                    let sample = samples[base + channel];
                    let magnitude = sample.abs();
                    combined_peak = combined_peak.max(magnitude);
                    combined_energy += f64::from(sample) * f64::from(sample);
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
                side_peaks[0] = side_peaks[0].max(mixed[0].abs());
                side_energy[0] += f64::from(mixed[0]) * f64::from(mixed[0]);
                if channels != 1 {
                    side_peaks[1] = side_peaks[1].max(mixed[1].abs());
                    side_energy[1] += f64::from(mixed[1]) * f64::from(mixed[1]);
                }
                if channels == 1 {
                    side_peaks[1] = side_peaks[0];
                    side_energy[1] = side_energy[0];
                }
            }
            changed |= update_bin(
                &mut self.peaks,
                &mut self.rms,
                &mut self.rms_counts,
                bin,
                combined_peak,
                combined_energy,
                if accumulate { combined_count } else { 0 },
            );
            changed |= update_bin(
                &mut self.left_peaks,
                &mut self.left_rms,
                &mut self.left_rms_counts,
                bin,
                side_peaks[0],
                side_energy[0],
                if accumulate { side_count } else { 0 },
            );
            changed |= update_bin(
                &mut self.right_peaks,
                &mut self.right_rms,
                &mut self.right_rms_counts,
                bin,
                side_peaks[1],
                side_energy[1],
                if accumulate { side_count } else { 0 },
            );
            if accumulate {
                let end = frame + take as u64;
                self.rms_spans[bin] = Some(match &self.rms_spans[bin] {
                    Some(span) => span.start.min(frame)..span.end.max(end),
                    None => frame..end,
                });
            }
            if collect_prefix {
                let energy = [combined_energy, side_energy[0], side_energy[1]];
                let counts = [combined_count, side_count, side_count];
                match &mut self.rms_prefixes[bin] {
                    Some(prefix) if end == prefix.span.start || frame == prefix.span.end => {
                        prefix.span = prefix.span.start.min(frame)..prefix.span.end.max(end);
                        for side in 0..3 {
                            prefix.energy[side] += energy[side];
                            prefix.counts[side] = prefix.counts[side].saturating_add(counts[side]);
                        }
                    }
                    slot => {
                        *slot = Some(RmsPrefix {
                            span: frame..end,
                            energy,
                            counts,
                        })
                    }
                }
                changed = true;
            }
            if self.rms_prefixes[bin].as_ref().is_some_and(|prefix| {
                self.rms_spans[bin]
                    .as_ref()
                    .is_some_and(|span| prefix.span.end == span.start)
            }) {
                let prefix = self.rms_prefixes[bin].take().unwrap();
                self.rms_spans[bin].as_mut().unwrap().start = prefix.span.start;
                changed |= update_bin(
                    &mut self.peaks,
                    &mut self.rms,
                    &mut self.rms_counts,
                    bin,
                    0.0,
                    prefix.energy[0],
                    prefix.counts[0],
                );
                changed |= update_bin(
                    &mut self.left_peaks,
                    &mut self.left_rms,
                    &mut self.left_rms_counts,
                    bin,
                    0.0,
                    prefix.energy[1],
                    prefix.counts[1],
                );
                changed |= update_bin(
                    &mut self.right_peaks,
                    &mut self.right_rms,
                    &mut self.right_rms_counts,
                    bin,
                    0.0,
                    prefix.energy[2],
                    prefix.counts[2],
                );
            }
            self.max_peak = self
                .max_peak
                .max(combined_peak)
                .max(side_peaks[0])
                .max(side_peaks[1]);
            offset += take;
        }
        if changed {
            self.revision = self.revision.wrapping_add(1);
            if rescale {
                self.max_peak = self
                    .peaks
                    .iter()
                    .chain(&self.left_peaks)
                    .chain(&self.right_peaks)
                    .flatten()
                    .copied()
                    .fold(1.0, f32::max);
            }
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
        assert_eq!(actual.identity, expected.identity);
        assert_eq!(actual.duration, expected.duration);
        assert_eq!(actual.span_seconds, expected.span_seconds);
        assert_eq!(actual.max_peak, expected.max_peak);
    }

    #[test]
    fn first_publication_preserves_registered_scan_cancellation() {
        let mut shared = WaveformFrame::default();
        shared.select_identity(
            Path::new("track.wav"),
            None,
            DecoderHint::Auto,
            Some(2.0),
            WaveformIdentity::FullScan {
                duration: Some(2.0),
                sample_rate: 8_000,
            },
        );
        // Decoder captures its seed before a manual scan registers its token.
        let mut seed = shared.clone();
        seed.prepare_bins(8_000);
        let mut pending = WaveformAccumulator::new(seed);
        let cancelled = Arc::new(AtomicBool::new(false));
        shared.cancel_scan_on_suspend(&cancelled);
        let revision = shared.revision;
        pending.push(0.0, &[0.75; 160], 8_000, 1);
        pending.publish(&mut shared);
        assert!(shared.revision > revision);
        assert_eq!(shared.rms_counts.iter().sum::<u64>(), 160);
        assert!(!cancelled.load(Ordering::Relaxed));
        assert!(shared.peaks.len() <= MAX_BINS);
        shared.set_still_mode(true);
        assert!(cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.retained_capacity_bytes(), 0);
        let suspended_revision = shared.revision;
        shared.set_still_mode(false);
        pending.push(0.5, &[1.0; 160], 8_000, 1);
        pending.publish(&mut shared);
        assert_eq!(shared.revision, suspended_revision);
        assert_eq!(shared.suspension_generation(), 1);
        assert_eq!(shared.retained_capacity_bytes(), 0);
    }

    #[test]
    fn still_mode_releases_all_bins_and_blocks_late_scans_across_source_changes() {
        let mut shared = frame(Some(2.0), 8_000);
        shared.set_channel_weights(&[[1.0, 0.0], [0.0, 1.0]]);
        shared.push(0.0, &[0.75; 1600], 8_000, 2);
        let mut pending = WaveformAccumulator::new(shared.clone());
        let retained = shared.retained_capacity_bytes();
        assert!(retained > 0);
        let revision = shared.revision;
        shared.set_still_mode(true);
        let suspended_revision = shared.revision;
        assert!(suspended_revision > revision);
        assert_eq!(shared.suspension_generation(), 1);
        assert_eq!(shared.retained_capacity_bytes(), 0);
        assert!(shared.matches(Path::new("track.wav"), None, DecoderHint::Auto));
        assert_eq!(shared.duration, Some(2.0));
        assert!(!shared.complete);
        for _ in 0..100 {
            shared.set_still_mode(true);
            shared.select(
                Path::new("track.wav"),
                None,
                DecoderHint::Auto,
                Some(2.0),
                8_000,
            );
            shared.set_channel_weights(&[[1.0, 0.0]]);
            shared.push(0.0, &[1.0], 8_000, 1);
            shared.finish_scan(2.0);
            pending.publish(&mut shared);
            assert_eq!(shared.revision, suspended_revision);
            assert_eq!(shared.retained_capacity_bytes(), 0);
            assert_eq!(shared.suspension_generation(), 1);
        }
        for path in ["other.wav", "track.wav"] {
            shared.select(Path::new(path), None, DecoderHint::Auto, Some(2.0), 8_000);
            assert!(shared.matches(Path::new(path), None, DecoderHint::Auto));
            assert_eq!(shared.retained_capacity_bytes(), 0);
        }
        shared.set_still_mode(false);
        let resumed_revision = shared.revision;
        assert_eq!(shared.retained_capacity_bytes(), 0);
        shared.prepare_bins(8_000);
        let mut resumed = WaveformAccumulator::new(shared.clone());
        resumed.push(1.0, &[0.0; 160], 8_000, 1);
        resumed.publish(&mut shared);
        assert!(shared.revision > resumed_revision);
        assert!(shared.peaks[..MAX_BINS / 2].iter().all(Option::is_none));
        assert!(shared.peaks.iter().flatten().all(|peak| *peak == 0.0));
        assert_eq!(shared.rms_counts.iter().sum::<u64>(), 160);
        assert!(shared.retained_capacity_bytes() <= retained);
        assert_eq!(shared.suspension_generation(), 1);
        shared.set_still_mode(true);
        assert_eq!(shared.suspension_generation(), 2);
        assert_eq!(shared.retained_capacity_bytes(), 0);
    }

    #[test]
    fn cached_seek_reopens_replace_statistics_without_reweighting() {
        let rate = (MAX_BINS * 2) as u32;
        let mut shared = frame(Some(1.0), rate);
        let mut accumulator = WaveformAccumulator::new(shared.clone());
        accumulator.push(0.0, &[1.0], rate, 1);
        accumulator.publish(&mut shared);
        let revision = shared.revision;
        // Same RMS/peak, but newly represented PCM must still be published.
        accumulator.push(1.0 / f64::from(rate), &[1.0], rate, 1);
        accumulator.publish(&mut shared);
        assert!(shared.revision > revision);
        assert_eq!(shared.rms_counts[0], 2);
        for _ in 0..100 {
            shared.select(
                Path::new("track.wav"),
                None,
                DecoderHint::Auto,
                Some(1.0),
                rate,
            );
            let mut reopened = WaveformAccumulator::new(shared.clone());
            let revision = shared.revision;
            reopened.push(0.0, &[1.0, 1.0], rate, 1);
            reopened.publish(&mut shared);
            assert!(shared.revision > revision);
            assert_eq!(shared.rms_counts[0], 2);
        }
        let mut reopened = WaveformAccumulator::new(shared.clone());
        reopened.push(2.0 / f64::from(rate), &[0.5, 0.0], rate, 1);
        reopened.publish(&mut shared);
        assert_eq!(shared.rms_counts[..2], [2, 2]);
        assert_eq!(shared.peaks[..2], [Some(1.0), Some(0.5)]);
        assert!(!shared.complete);
    }

    #[test]
    fn local_publication_rejects_source_switch() {
        let mut shared = frame(Some(4.0), 1);
        let mut accumulator = WaveformAccumulator::new(shared.clone());
        accumulator.push(0.0, &[2.0], 1, 1);
        shared.select(
            Path::new("other.wav"),
            None,
            DecoderHint::Auto,
            Some(4.0),
            1,
        );
        let expected = shared.clone();
        accumulator.publish(&mut shared);
        assert_same_bins(&shared, &expected);
    }

    fn frame(duration: Option<f64>, rate: u32) -> WaveformFrame {
        let mut frame = WaveformFrame::default();
        frame.select(
            Path::new("track.wav"),
            None,
            DecoderHint::Auto,
            duration,
            rate,
        );
        frame
    }

    fn assert_statistics(actual: &WaveformFrame, expected: &WaveformFrame) {
        assert_eq!(actual.peaks, expected.peaks);
        assert_eq!(actual.left_peaks, expected.left_peaks);
        assert_eq!(actual.right_peaks, expected.right_peaks);
        assert_eq!(actual.rms_counts, expected.rms_counts);
        assert_eq!(actual.left_rms_counts, expected.left_rms_counts);
        assert_eq!(actual.right_rms_counts, expected.right_rms_counts);
        for (actual, expected) in [
            (&actual.rms, &expected.rms),
            (&actual.left_rms, &expected.left_rms),
            (&actual.right_rms, &expected.right_rms),
        ] {
            for (actual, expected) in actual.iter().zip(expected) {
                match (actual, expected) {
                    (Some(actual), Some(expected)) => assert!((actual - expected).abs() < 1e-6),
                    (None, None) => {}
                    _ => panic!("RMS coverage differs: {actual:?} vs {expected:?}"),
                }
            }
        }
    }

    #[test]
    fn seeking_back_does_not_reweight_the_original_two_sample_bin() {
        let rate = (MAX_BINS * 2) as u32;
        let mut frame = frame(Some(1.0), rate);
        frame.push(0.0, &[1.0], rate, 1);
        frame.push(1.0 / f64::from(rate), &[0.0], rate, 1);
        let expected = (0.5_f32).sqrt();
        let revision = frame.revision;
        for _ in 0..100 {
            frame.push(0.0, &[1.0], rate, 1);
            frame.push(1.0 / f64::from(rate), &[0.0], rate, 1);
            assert_eq!(frame.rms[0], Some(expected));
            assert_eq!(frame.rms_counts[0], 2);
            assert_eq!(frame.left_rms[0], Some(expected));
            assert_eq!(frame.right_rms[0], Some(expected));
            assert_eq!(frame.left_rms_counts[0], 2);
            assert_eq!(frame.right_rms_counts[0], 2);
            assert_eq!(frame.revision, revision);
        }
    }

    #[test]
    fn overlapping_packets_prefixes_and_longer_same_timestamp_packets_are_unique() {
        let rate = (MAX_BINS * 8) as u32;
        let samples: Vec<f32> = (0..16)
            .flat_map(|index| [index as f32 / 16.0, -(index as f32) / 32.0])
            .collect();
        let mut expected = frame(Some(1.0), rate);
        expected.push(0.0, &samples, rate, 2);
        for packets in [
            vec![2..4, 0..4, 0..16, 5..12],
            vec![6..8, 4..7, 0..5, 7..16, 0..16],
            vec![0..1, 1..3, 3..8, 8..16, 0..16],
        ] {
            let mut actual = frame(Some(1.0), rate);
            for packet in packets {
                actual.push(
                    packet.start as f64 / f64::from(rate),
                    &samples[packet.start * 2..packet.end * 2],
                    rate,
                    2,
                );
            }
            assert_statistics(&actual, &expected);
            assert_eq!(actual.rms_counts[..2], [16, 16]);
            assert_eq!(actual.left_rms_counts[..2], [8, 8]);
            assert_eq!(actual.right_rms_counts[..2], [8, 8]);
        }
    }

    #[test]
    fn disjoint_previews_keep_peaks_and_a_bridge_recovers_all_rms_samples() {
        let mut actual = frame(Some(1.0), 12800);
        actual.push(0.0, &[1.0], 12800, 1);
        actual.push(3.0 / 12800.0, &[2.0], 12800, 1);
        assert_eq!(actual.peaks[0], Some(2.0));
        assert_eq!(actual.rms[0], Some(1.0));
        assert_eq!(actual.rms_counts[0], 1);
        assert!(!actual.complete);
        // The overlap is skipped; the newly connected suffix includes the
        // previously peak-only preview exactly once.
        let samples = [1.0, 0.0, 0.5, 2.0, 0.0, 0.0, 0.0, 0.0];
        actual.push(0.0, &samples, 12800, 1);
        let mut expected = frame(Some(1.0), 12800);
        expected.push(0.0, &samples, 12800, 1);
        assert_statistics(&actual, &expected);
        actual.push(3.0 / 12800.0, &[2.0], 12800, 1);
        assert_statistics(&actual, &expected);
    }

    #[test]
    fn reopened_traversals_replace_visited_bins_and_replay_after_completion() {
        let rate = (MAX_BINS * 8) as u32;
        let mut shared = frame(Some(1.0), rate);
        let mut initial = WaveformAccumulator::new(shared.clone());
        initial.push(0.0, &vec![2.0; rate as usize * 2], rate, 2);
        let mut coverage = WaveformCoverage::new(0.0, 0.0);
        coverage.record(0.0, rate as usize, rate);
        initial.finish(&coverage);
        initial.publish(&mut shared);
        assert!(shared.complete);
        assert_eq!(shared.max_peak, 2.0);
        let old_tail = shared.peaks[1];
        let mut replay = WaveformAccumulator::new(shared.clone());
        replay.push(0.0, &[0.25, 0.125, 0.25, 0.125], rate, 2);
        replay.publish(&mut shared);
        assert!(!shared.complete);
        assert_eq!(shared.peaks[0], Some(0.25));
        assert_eq!(shared.left_peaks[0], Some(0.25));
        assert_eq!(shared.right_peaks[0], Some(0.125));
        assert_eq!(shared.left_rms[0], Some(0.25));
        assert_eq!(shared.right_rms[0], Some(0.125));
        assert_eq!(shared.rms_counts[0], 4);
        assert_eq!(shared.left_rms_counts[0], 2);
        assert_eq!(shared.peaks[1], old_tail);
        replay.begin_traversal();
        replay.push(0.0, &[0.0625, 0.03125], rate, 2);
        replay.publish(&mut shared);
        assert_eq!(shared.peaks[0], Some(0.0625));
        assert_eq!(shared.left_rms[0], Some(0.0625));
        assert_eq!(shared.right_rms[0], Some(0.03125));
        assert_eq!(shared.rms_counts[0], 2);
        assert_eq!(shared.left_rms_counts[0], 1);
        assert_eq!(shared.peaks[1], old_tail);
    }

    #[test]
    fn unknown_doubling_keeps_unique_spans_and_full_traversal_recovers_gaps() {
        for adjacent in [false, true] {
            let mut actual = frame(None, 100);
            let mut samples = vec![0.0; 6001];
            samples[1] = 1.0;
            samples[4] = 2.0;
            if adjacent {
                actual.push(0.0, &samples[..8], 100, 1);
            } else {
                actual.push(0.01, &[1.0], 100, 1);
                actual.push(0.04, &[2.0], 100, 1);
            }
            actual.push(60.0, &[0.0], 100, 1);
            assert_eq!(actual.span_seconds, 120.0);
            assert_eq!(actual.peaks[0], Some(2.0));
            let count = if adjacent { 8 } else { 1 };
            assert_eq!(actual.rms_counts[0], count);
            actual.select(Path::new("track.wav"), None, DecoderHint::Auto, None, 100);
            actual.push(0.01, &[1.0], 100, 1);
            actual.push(0.04, &[2.0], 100, 1);
            assert_eq!(actual.rms_counts[0], count);
            assert!(!actual.complete);
            for start in (0..samples.len()).step_by(17) {
                let end = (start + 17).min(samples.len());
                actual.push(start as f64 / 100.0, &samples[start..end], 100, 1);
            }
            let mut expected = frame(None, 100);
            expected.push(0.0, &samples, 100, 1);
            assert_statistics(&actual, &expected);
            let mut coverage = WaveformCoverage::new(0.0, 0.0);
            coverage.record(0.0, samples.len(), 100);
            coverage.finish(&mut actual);
            coverage.finish(&mut expected);
            assert!(actual.complete);
            assert_eq!(actual.duration, Some(60.01));
            assert_eq!(actual.span_seconds, expected.span_seconds);
            assert_statistics(&actual, &expected);
            actual.select(Path::new("track.wav"), None, DecoderHint::Auto, None, 100);
            actual.push(0.0, &samples, 100, 1);
            assert_statistics(&actual, &expected);
        }
    }

    #[test]
    fn a_full_traversal_prefix_survives_a_packet_that_doubles_the_timeline() {
        let samples: Vec<f32> = (0..6001).map(|index| (index % 7) as f32 / 7.0).collect();
        let mut actual = frame(None, 100);
        actual.push(59.99, &samples[5999..6000], 100, 1);
        actual.push(0.0, &samples[..5997], 100, 1);
        actual.push(59.97, &samples[5997..5998], 100, 1);
        // The cached last bin now has a gap between its prefix and RMS span.
        // This packet doubles before decoding that gap, so compaction must
        // join the prefix with the preceding bin's continuous statistics.
        actual.push(59.98, &samples[5998..], 100, 1);
        let mut expected = frame(None, 100);
        expected.push(0.0, &samples, 100, 1);
        assert_eq!(actual.span_seconds, 120.0);
        assert_statistics(&actual, &expected);
        let mut coverage = WaveformCoverage::new(0.0, 0.0);
        coverage.record(0.0, samples.len(), 100);
        coverage.finish(&mut actual);
        coverage.finish(&mut expected);
        assert!(actual.complete);
        assert_statistics(&actual, &expected);
    }

    #[test]
    fn a_continuous_complete_scan_has_exact_bin_counts_and_rms() {
        let rate = (MAX_BINS * 2) as u32;
        let samples: Vec<f32> = (0..rate).map(|index| (index % 2) as f32).collect();
        let mut actual = frame(Some(1.0), rate);
        let mut coverage = WaveformCoverage::new(0.0, 0.0);
        for start in (0..samples.len()).step_by(7) {
            let end = (start + 7).min(samples.len());
            let timestamp = start as f64 / f64::from(rate);
            actual.push(timestamp, &samples[start..end], rate, 1);
            coverage.record(timestamp, end - start, rate);
        }
        coverage.finish(&mut actual);
        assert!(actual.complete);
        assert!(actual.rms_counts.iter().all(|count| *count == 2));
        assert!(actual.rms.iter().all(|rms| *rms == Some((0.5_f32).sqrt())));
        assert!(actual.peaks.iter().all(|peak| *peak == Some(1.0)));
    }

    #[test]
    fn packet_timestamp_gap_cannot_be_finalized_as_continuous_playback() {
        let mut frame = frame(None, 100);
        let mut coverage = WaveformCoverage::new(0.0, 0.0);
        coverage.record(0.0, 1, 100);
        frame.push(0.0, &[0.5], 100, 1);
        coverage.record(1.0, 1, 100);
        frame.push(1.0, &[0.75], 100, 1);
        coverage.finish(&mut frame);
        assert!(!frame.complete);
        assert_eq!(frame.duration, Some(1.01));
        let bin_seconds = frame.bin_frames as f64 / frame.bin_count as f64 / 100.0;
        assert!(frame.span_seconds >= 1.01 && frame.span_seconds < 1.01 + bin_seconds);
        let resumed_bin = (1.0 / bin_seconds) as usize;
        assert!(frame.peaks[1..resumed_bin].iter().all(Option::is_none));
    }

    #[test]
    fn eof_corrects_expanded_timeline_despite_a_sample_gap_across_replays() {
        let rate = 8_000;
        let mut shared = frame(Some(10.0), rate);
        for _ in 0..3 {
            let mut accumulator = WaveformAccumulator::new(shared.clone());
            let mut coverage = WaveformCoverage::new(0.0, 0.0);
            for packet in 0..100 {
                let start = f64::from(packet * 800 + 1) / f64::from(rate);
                coverage.record(start, 800, rate);
                accumulator.push(start, &[0.5; 800], rate, 1);
                accumulator.publish(&mut shared);
            }
            accumulator.finish(&coverage);
            accumulator.publish(&mut shared);
            assert!(!shared.complete);
            assert_eq!(shared.duration, Some(10.0 + 1.0 / f64::from(rate)));
            assert!(shared.span_seconds >= shared.duration.unwrap());
            assert!(shared.span_seconds < 10.03);
            assert!(shared.peaks.iter().all(|peak| *peak == Some(0.5)));
        }
    }

    #[test]
    fn quantized_packet_times_complete_but_real_gaps_remain_unknown() {
        let rate = 48_000;
        for (tolerance, gap, complete) in
            [(0.0, 0.0, false), (0.001, 0.0, true), (0.001, 0.01, false)]
        {
            let mut actual = frame(Some(2.133), rate);
            let mut coverage = WaveformCoverage::new(0.0, tolerance);
            let mut end = 0.0;
            for packet in 0..100 {
                let start = (f64::from(packet * 1024) / f64::from(rate) * 1000.0).round() / 1000.0
                    + if packet >= 50 { gap } else { 0.0 };
                coverage.record(start, 1024, rate);
                actual.push(start, &[0.5; 1024], rate, 1);
                end = start + 1024.0 / f64::from(rate);
            }
            coverage.finish(&mut actual);
            assert_eq!(actual.complete, complete);
            assert!((actual.duration.unwrap() - end).abs() < 1e-9);
            assert!(actual.span_seconds >= end && actual.span_seconds < end + 0.01);
            if gap == 0.0 {
                assert!(actual.peaks.iter().all(Option::is_some));
            } else {
                let middle = actual.peaks.len() / 2;
                assert!(
                    actual.peaks[middle - 5..middle + 5]
                        .iter()
                        .any(Option::is_none)
                );
            }
        }
    }

    #[test]
    fn eof_preserves_unknown_tail_and_allows_later_playback_to_fill_it() {
        let mut shared = frame(Some(10.0), 100);
        let mut accumulator = WaveformAccumulator::new(shared.clone());
        let mut coverage = WaveformCoverage::new(0.0, 0.0);
        coverage.record(0.0, 100, 100);
        accumulator.push(0.0, &[0.5; 100], 100, 1);
        coverage.skip();
        coverage.record(9.0, 100, 100);
        accumulator.finish(&coverage);
        accumulator.publish(&mut shared);
        assert!(!shared.complete);
        assert_eq!(shared.duration, Some(10.0));
        assert_eq!(shared.span_seconds, 10.0);
        assert!(shared.peaks[..100].iter().all(|peak| *peak == Some(0.5)));
        assert!(shared.peaks[100..].iter().all(Option::is_none));
        let mut resumed = WaveformAccumulator::new(shared.clone());
        resumed.push(9.0, &[0.75; 100], 100, 1);
        resumed.publish(&mut shared);
        assert!(shared.peaks[100..900].iter().all(Option::is_none));
        assert!(shared.peaks[900..].iter().all(|peak| *peak == Some(0.75)));
        assert!(!shared.complete);
    }

    #[test]
    fn incomplete_eof_replay_preserves_fractional_bin_boundaries() {
        let rate = 44_100;
        let mut shared = frame(Some(60.0), rate);
        let mut accumulator = WaveformAccumulator::new(shared.clone());
        let mut coverage = WaveformCoverage::new(0.15, 0.0);
        for (start, value) in [(0.15, 1.0), (1.98 - 1.0 / f64::from(rate), 0.0)] {
            coverage.record(start, 1, rate);
            accumulator.push(start, &[value], rate, 1);
        }
        accumulator.publish(&mut shared);
        let peak_bin = shared
            .peaks
            .iter()
            .position(|peak| *peak == Some(1.0))
            .unwrap();
        let bin_seconds = shared.span_seconds / shared.peaks.len() as f64;
        accumulator.finish(&coverage);
        accumulator.publish(&mut shared);
        assert!(!shared.complete);
        assert_eq!(shared.duration, Some(1.98));
        assert!(shared.span_seconds >= 1.98 && shared.span_seconds < 1.98 + bin_seconds);
        for _ in 0..3 {
            let mut replay = WaveformAccumulator::new(shared.clone());
            replay.push(0.15, &[1.0], rate, 1);
            replay.publish(&mut shared);
            let peaks: Vec<_> = shared
                .peaks
                .iter()
                .enumerate()
                .filter_map(|(index, peak)| (*peak == Some(1.0)).then_some(index))
                .collect();
            assert_eq!(peaks, [peak_bin]);
            assert_eq!(shared.rms_counts.iter().sum::<u64>(), 2);
        }
        let mut extended = WaveformAccumulator::new(shared.clone());
        extended.push(4.0, &[0.5], rate, 1);
        extended.publish(&mut shared);
        let count = shared.rms_counts.iter().sum::<u64>();
        let mut replay = WaveformAccumulator::new(shared.clone());
        replay.push(0.15, &[1.0], rate, 1);
        replay.publish(&mut shared);
        assert_eq!(shared.rms_counts.iter().sum::<u64>(), count);
        assert_eq!(
            shared
                .peaks
                .iter()
                .filter(|peak| **peak == Some(1.0))
                .count(),
            1
        );
    }

    #[test]
    fn finishing_unknown_scan_trims_only_undecoded_tail_without_moving_peaks() {
        let mut full = frame(None, 100);
        full.push(0.0, &[0.5], 100, 1);
        full.push(120.0, &[2.0], 100, 1);
        let peak_bin = full
            .peaks
            .iter()
            .position(|peak| *peak == Some(2.0))
            .unwrap();
        let bin_seconds = full.span_seconds / full.peaks.len() as f64;
        full.finish_scan(120.01);
        assert!(full.complete);
        assert_eq!(full.duration, Some(120.01));
        assert_eq!(full.peaks.len(), peak_bin + 1);
        assert_eq!(full.peaks[0], Some(0.5));
        assert_eq!(full.peaks[peak_bin], Some(2.0));
        assert!(full.span_seconds >= 120.01 && full.span_seconds < 120.01 + bin_seconds);
    }

    #[test]
    fn short_clip_preserves_channel_peaks_silence_and_overfull_samples() {
        let mut frame = frame(Some(5.0 / 48_000.0), 48_000);
        frame.push(
            0.0,
            &[0.25, -0.25, 0.0, 0.5, -0.75, 0.0, 0.0, 0.0, -2.0, 1.5],
            48_000,
            2,
        );
        assert_eq!(
            frame.peaks,
            [Some(0.25), Some(0.5), Some(0.75), Some(0.0), Some(2.0)]
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
        frame.select(
            Path::new("track.wav"),
            None,
            DecoderHint::Auto,
            Some(4.0),
            1,
        );
        frame.push(1.0, &[0.5], 1, 1);
        frame.push(1.0, &[0.25], 1, 1);
        frame.push(1.0, &[], 1, 1);
        assert_eq!(frame.revision, revision);
        assert_eq!(frame.peaks, [None, Some(0.5), None, None]);
        assert_eq!(frame.duration, Some(4.0));
    }

    #[test]
    fn progressive_reuse_requires_opened_content_backend_and_initial_format() {
        let file = tempfile::tempfile().unwrap();
        let replacement = tempfile::tempfile().unwrap();
        let content = SourceIdentity::from_file(&file).unwrap();
        let other_content = SourceIdentity::from_file(&replacement).unwrap();
        let identity =
            |content, ffmpeg, duration, sample_rate, channels| WaveformIdentity::Progressive {
                content,
                ffmpeg,
                duration,
                sample_rate,
                channels,
            };
        let original = identity(content, false, Some(4.0), 1, 1);
        let path = Path::new("track.wav");
        let mut completed = WaveformFrame::default();
        completed.select_identity(path, None, DecoderHint::Auto, Some(4.0), original);
        completed.prepare_bins(1);
        completed.push(0.0, &[0.1; 2], 1, 1);
        completed.finish_scan(2.0);
        let cached = completed.clone();
        completed.select_identity(path, None, DecoderHint::Auto, Some(4.0), original);
        assert_same_bins(&completed, &cached);
        assert_eq!(completed.duration, Some(2.0), "keep actual EOF duration");

        for next in [
            identity(other_content, false, Some(4.0), 1, 1),
            identity(content, true, Some(4.0), 1, 1),
            identity(content, false, Some(3.0), 1, 1),
            identity(content, false, Some(4.0), 2, 1),
            identity(content, false, Some(4.0), 1, 2),
        ] {
            let mut fresh = cached.clone();
            fresh.select_identity(path, None, DecoderHint::Auto, Some(4.0), next);
            assert_eq!(fresh.revision, cached.revision + 1);
            assert!(!fresh.complete);
            assert!(fresh.peaks.is_empty());
            assert!(fresh.rms_counts.is_empty());
            assert_eq!(fresh.identity, Some(next));
        }

        // Explicit scans have their own owner; no bins cross that boundary.
        completed.select(path, None, DecoderHint::Auto, Some(4.0), 1);
        assert!(!completed.complete);
        assert_eq!(completed.peaks, [None; 4]);
    }

    #[test]
    fn old_content_cannot_publish_into_same_path_even_with_equal_revisions() {
        let file = tempfile::tempfile().unwrap();
        let replacement = tempfile::tempfile().unwrap();
        let identity = |file| WaveformIdentity::Progressive {
            content: SourceIdentity::from_file(file).unwrap(),
            ffmpeg: false,
            duration: Some(4.0),
            sample_rate: 1,
            channels: 1,
        };
        let mut shared = WaveformFrame::default();
        shared.select_identity(
            Path::new("track.wav"),
            None,
            DecoderHint::Auto,
            Some(4.0),
            identity(&file),
        );
        shared.prepare_bins(1);
        shared.push(0.0, &[0.1], 1, 1);
        let mut old = WaveformAccumulator::new(shared.clone());
        old.push(1.0, &[0.8], 1, 1);
        shared.identity = Some(identity(&replacement));
        let expected = shared.clone();
        old.publish(&mut shared);
        assert_same_bins(&shared, &expected);
    }

    #[test]
    fn decoder_hint_changes_reset_peaks_and_still_mode_preserves_identity() {
        let path = Path::new("track.wav");
        let mut frame = frame(Some(4.0), 1);
        for decoder_hint in [DecoderHint::Native, DecoderHint::Ffmpeg, DecoderHint::Auto] {
            frame.push(0.0, &[2.0], 1, 1);
            let revision = frame.revision;
            frame.select(path, None, decoder_hint, Some(4.0), 1);
            assert!(frame.matches(path, None, decoder_hint));
            assert_eq!(frame.revision, revision + 1);
            assert_eq!(frame.peaks, [None; 4]);
            assert_eq!(frame.max_peak, 1.0);
            assert!(!frame.complete);
            frame.push(0.0, &[0.5], 1, 1);
            let revision = frame.revision;
            frame.select(path, None, decoder_hint, Some(4.0), 1);
            assert_eq!(frame.revision, revision);
            assert_eq!(frame.peaks[0], Some(0.5));
        }

        frame.select(path, None, DecoderHint::Ffmpeg, Some(4.0), 1);
        frame.set_still_mode(true);
        assert!(frame.matches(path, None, DecoderHint::Ffmpeg));
        assert!(!frame.matches(path, None, DecoderHint::Native));
        frame.set_still_mode(false);
        assert!(frame.matches(path, None, DecoderHint::Ffmpeg));
    }

    #[test]
    fn old_decoder_accumulator_cannot_publish_into_a_new_decoder_frame() {
        let mut shared = frame(Some(4.0), 1);
        let mut accumulator = WaveformAccumulator::new(shared.clone());
        accumulator.push(0.0, &[2.0], 1, 1);
        let mut replacement = frame(Some(4.0), 1);
        replacement.decoder_hint = DecoderHint::Ffmpeg;
        shared = replacement;
        let expected = shared.clone();
        accumulator.publish(&mut shared);
        assert_same_bins(&shared, &expected);
        assert_eq!(shared.decoder_hint, DecoderHint::Ffmpeg);
    }

    #[test]
    fn different_path_or_cue_range_resets_source_and_scale() {
        let mut frame = frame(Some(4.0), 1);
        frame.push(0.0, &[2.0], 1, 1);
        let cue = Some(PlaybackRange {
            start_seconds: 10.0,
            end_seconds: Some(12.0),
        });
        frame.select(Path::new("track.wav"), cue, DecoderHint::Auto, Some(2.0), 1);
        assert!(frame.matches(Path::new("track.wav"), cue, DecoderHint::Auto));
        assert_eq!(frame.peaks, [None, None]);
        assert!(frame.rms_counts.iter().all(|count| *count == 0));
        assert_eq!(frame.max_peak, 1.0);
        frame.push(0.0, &[0.75], 1, 1);
        assert_eq!(frame.rms_counts, [1, 0]);
        assert_eq!(frame.rms[0], Some(0.75));
        let revision = frame.revision;
        frame.select(Path::new("other.wav"), cue, DecoderHint::Auto, Some(2.0), 1);
        assert_eq!(frame.peaks, [None, None]);
        assert_eq!(frame.revision, revision + 1);
        frame.push(0.0, &[0.25], 1, 1);
        assert_eq!(frame.rms_counts, [1, 0]);
        assert_eq!(frame.rms[0], Some(0.25));
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
        assert_eq!(frame.peaks[MAX_BINS * 59 / 480], Some(0.5));
        assert_eq!(frame.peaks[MAX_BINS / 2], Some(0.75));
        assert!(
            frame.peaks[MAX_BINS / 4..MAX_BINS / 2]
                .iter()
                .all(Option::is_none)
        );
        frame.push(120.0, &[0.25], 100, 1);
        assert_eq!(frame.peaks[MAX_BINS / 4], Some(0.25));
        assert_eq!(frame.peaks[0], Some(2.0));
        assert_eq!(frame.max_peak, 2.0);
    }

    #[test]
    fn underestimated_duration_grows_small_and_maximal_bins_without_losing_statistics() {
        for (rate, estimated_frames) in [(5_u32, 5_usize), (8_000, MAX_BINS)] {
            let estimate = estimated_frames as f64 / rate as f64;
            let samples: Vec<f32> = (0..estimated_frames * 3)
                .map(|index| (index % 17) as f32 / 16.0)
                .collect();
            let mut actual = frame(Some(estimate), rate);
            for (index, sample) in samples.iter().enumerate() {
                actual.push(index as f64 / rate as f64, &[*sample], rate, 1);
                assert!(!actual.complete);
                assert!(actual.peaks.len() <= MAX_BINS);
            }
            assert_eq!(actual.span_seconds, estimate * 4.0);
            for (index, samples) in samples.chunks(4).enumerate() {
                let peak = samples.iter().copied().fold(0.0_f32, f32::max);
                let energy = samples
                    .iter()
                    .map(|sample| f64::from(*sample).powi(2))
                    .sum::<f64>();
                assert_eq!(actual.peaks[index], Some(peak));
                assert_eq!(actual.rms_counts[index], samples.len() as u64);
                let rms = (energy / samples.len() as f64).sqrt() as f32;
                assert!((actual.rms[index].unwrap() - rms).abs() < 1e-6);
            }
            let end = samples.len() as f64 / rate as f64;
            actual.finish_scan(end);
            assert!(actual.complete);
            assert_eq!(actual.duration, Some(end));
            assert!(actual.span_seconds >= end);
            assert!(actual.peaks.len() <= MAX_BINS);
        }
    }

    #[test]
    fn actual_eof_replaces_an_overestimated_duration() {
        let mut actual = frame(Some(10.0), 10);
        actual.push(0.0, &[0.25; 20], 10, 1);
        actual.finish_scan(2.0);
        assert_eq!(actual.duration, Some(2.0));
        assert_eq!(actual.span_seconds, 2.0);
        assert_eq!(actual.peaks, vec![Some(0.25); 20]);
        assert_eq!(actual.rms_counts, vec![1; 20]);
        assert!(actual.complete);
    }

    #[test]
    fn packets_crossing_bins_use_decoded_sample_timestamps() {
        let total_frames = MAX_BINS * 5 / 4;
        let mut frame = frame(Some(total_frames as f64 / 100.0), 100);
        frame.push(0.01, &[0.25, 0.5, 0.75, 1.0], 100, 1);
        assert_eq!(
            frame.peaks[..4],
            [Some(0.25), Some(0.5), Some(0.75), Some(1.0)]
        );
        frame.push(f64::NAN, &[4.0], 100, 1);
        assert_eq!(frame.max_peak, 1.0);
    }
}
