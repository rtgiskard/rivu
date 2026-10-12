//! A bounded, progressively filled envelope of the PCM already decoded for playback.

use super::PlaybackRange;
use crate::model::DecoderHint;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

const MAX_BINS: usize = 1600;
const INITIAL_UNKNOWN_SPAN: f64 = 60.0;

/// Coverage of one decoder traversal. Cached bins do not prove that seek-skipped
/// intervals or PCM omitted in Still Mode were accumulated during this traversal.
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

    /// PCM omitted from the envelope cannot be recovered by later packets.
    pub(super) fn skip(&mut self) {
        self.continuous = false;
    }

    pub(super) fn finish(&self, frame: &mut WaveformFrame) {
        if self.continuous && self.end_seconds > 0.0 {
            frame.finish_scan(self.end_seconds);
        }
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

#[derive(Clone, Debug)]
pub struct WaveformFrame {
    pub revision: u64,
    pub complete: bool,
    pub path: Option<PathBuf>,
    pub range: Option<PlaybackRange>,
    pub decoder_hint: DecoderHint,
    pub duration: Option<f64>,
    pub span_seconds: f64,
    pub max_peak: f32,
    /// `None` is not yet decoded; `Some(0.0)` is decoded silence.
    pub peaks: Vec<Option<f32>>,
    /// RMS of one contiguous decoded interval per bin. Disjoint previews retain
    /// peaks but enter RMS only when a later traversal connects their samples.
    pub(crate) rms: Vec<Option<f32>>,
    pub(crate) rms_counts: Vec<u64>,
    pub(crate) left_peaks: Vec<Option<f32>>,
    pub(crate) right_peaks: Vec<Option<f32>>,
    pub(crate) left_rms: Vec<Option<f32>>,
    pub(crate) right_rms: Vec<Option<f32>>,
    pub(crate) left_rms_counts: Vec<u64>,
    pub(crate) right_rms_counts: Vec<u64>,
    pub(crate) channel_weights: Vec<[f32; 2]>,
    pub(crate) rms_spans: Vec<Option<std::ops::Range<u64>>>,
    pub(crate) rms_prefixes: Vec<Option<RmsPrefix>>,
    pub(crate) suspended: bool,
    pub(crate) suspension_generation: u64,
    pub(crate) full_scan_cancelled: Weak<AtomicBool>,
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
            suspended: false,
            suspension_generation: 0,
            full_scan_cancelled: Weak::new(),
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
            || shared.decoder_hint != self.frame.decoder_hint
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
        shared.rms_spans[start..end].clone_from_slice(&self.frame.rms_spans[start..end]);
        shared.rms_spans.truncate(self.frame.rms_spans.len());
        shared.rms_prefixes[start..end].clone_from_slice(&self.frame.rms_prefixes[start..end]);
        shared.rms_prefixes.truncate(self.frame.rms_prefixes.len());
        shared.complete = self.frame.complete;
        shared.duration = self.frame.duration;
        shared.span_seconds = self.frame.span_seconds;
        shared.max_peak = self.frame.max_peak;
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

    pub(crate) fn suspension_generation(&self) -> u64 {
        self.suspension_generation
    }

    pub(crate) fn cancel_scan_on_suspend(&mut self, cancelled: &Arc<AtomicBool>) {
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
    }

    /// Still Mode owns this guard so late manual scans cannot restore results.
    pub(crate) fn set_still_mode(&mut self, suspended: bool) {
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

    /// The caller validates source identity and request generation before
    /// publishing a manual full scan. Preserve the shared version sequence.
    pub fn install_full(&mut self, mut frame: Self) {
        if self.suspended {
            return;
        }
        frame.revision = self.revision.wrapping_add(1);
        frame.suspended = self.suspended;
        frame.suspension_generation = self.suspension_generation;
        frame.complete = true;
        *self = frame;
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
        let intervals = [
            self.rms_prefixes[left].take(),
            self.bin_statistics(left),
            self.rms_prefixes[right].take(),
            self.bin_statistics(right),
        ];
        let (primary, prefix) = compact_statistics(intervals);
        for peaks in [&mut self.peaks, &mut self.left_peaks, &mut self.right_peaks] {
            peaks[index] = match (peaks[left], peaks[right]) {
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
    }

    /// Reopening the same source retains its timeline and results unless Still
    /// Mode discarded them. Metadata selection alone never allocates bins.
    pub(super) fn select_source(
        &mut self,
        path: &Path,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
        duration: Option<f64>,
    ) {
        if self.matches(path, range, decoder_hint) {
            return;
        }
        self.path = Some(path.to_owned());
        self.range = range;
        self.decoder_hint = decoder_hint;
        self.duration = duration.filter(|value| value.is_finite() && *value > 0.0);
        self.span_seconds = self.duration.unwrap_or(INITIAL_UNKNOWN_SPAN);
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
        self.select_source(path, range, decoder_hint, duration);
        self.prepare_bins(sample_rate);
    }

    pub(super) fn prepare_bins(&mut self, sample_rate: u32) {
        if self.suspended || !self.peaks.is_empty() {
            return;
        }
        let bins = self.duration.map_or(MAX_BINS, |duration| {
            ((duration * f64::from(sample_rate)).round().max(1.0) as usize).min(MAX_BINS)
        });
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
    }

    pub(super) fn finish_scan(&mut self, end_seconds: f64) {
        if self.suspended || self.complete {
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
                self.rms_spans.truncate(bins);
                self.rms_prefixes.truncate(bins);
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
        let mut changed = false;
        if self.duration.is_none() {
            // Doubling merges adjacent bins exactly, unlike arbitrary rescaling
            // which would misplace or lose old peaks. Empty bins stay unknown.
            while end_seconds > self.span_seconds {
                for index in 0..MAX_BINS / 2 {
                    self.compact_bin(index);
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
                self.rms_spans[MAX_BINS / 2..].fill(None);
                self.rms_prefixes[MAX_BINS / 2..].fill(None);
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
            let boundary = ((bin as u128 + 1) * u128::from(total_frames)).div_ceil(bins) as u64;
            let mut take = (boundary - frame).min((count - offset) as u64) as usize;
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
                if accumulate { combined_count.max(1) } else { 0 },
            );
            changed |= update_bin(
                &mut self.left_peaks,
                &mut self.left_rms,
                &mut self.left_rms_counts,
                bin,
                side_peaks[0],
                side_energy[0],
                if accumulate { side_counts[0] } else { 0 },
            );
            changed |= update_bin(
                &mut self.right_peaks,
                &mut self.right_rms,
                &mut self.right_rms_counts,
                bin,
                side_peaks[1],
                side_energy[1],
                if accumulate { side_counts[1] } else { 0 },
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
                let counts = [combined_count.max(1), side_counts[0], side_counts[1]];
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
    }

    #[test]
    fn still_mode_releases_all_bins_and_blocks_late_scans_across_source_changes() {
        let mut shared = frame(Some(2.0), 8_000);
        shared.set_channel_weights(&[[1.0, 0.0], [0.0, 1.0]]);
        shared.push(0.0, &[0.75; 1600], 8_000, 2);
        let mut pending = WaveformAccumulator::new(shared.clone());
        let manual = shared.clone();
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
            shared.install_full(manual.clone());
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
        assert!(shared.peaks[..800].iter().all(Option::is_none));
        assert!(shared.peaks.iter().flatten().all(|peak| *peak == 0.0));
        assert_eq!(shared.rms_counts.iter().sum::<u64>(), 160);
        assert!(shared.retained_capacity_bytes() <= retained);
        shared.install_full(manual);
        assert_eq!(shared.suspension_generation(), 1);
        shared.set_still_mode(true);
        assert_eq!(shared.suspension_generation(), 2);
        assert_eq!(shared.retained_capacity_bytes(), 0);
    }

    #[test]
    fn cached_seek_reopens_preserve_unique_counts_and_accept_new_pcm() {
        let mut shared = frame(Some(1.0), 3200);
        let mut accumulator = WaveformAccumulator::new(shared.clone());
        accumulator.push(0.0, &[1.0], 3200, 1);
        accumulator.publish(&mut shared);
        let revision = shared.revision;
        // Same RMS/peak, but newly represented PCM must still be published.
        accumulator.push(1.0 / 3200.0, &[1.0], 3200, 1);
        accumulator.publish(&mut shared);
        assert!(shared.revision > revision);
        assert_eq!(shared.rms_counts[0], 2);
        for _ in 0..100 {
            shared.select(
                Path::new("track.wav"),
                None,
                DecoderHint::Auto,
                Some(1.0),
                3200,
            );
            let mut reopened = WaveformAccumulator::new(shared.clone());
            let revision = shared.revision;
            reopened.push(0.0, &[1.0, 1.0], 3200, 1);
            reopened.publish(&mut shared);
            assert_eq!(shared.revision, revision);
            assert_eq!(shared.rms_counts[0], 2);
        }
        let mut reopened = WaveformAccumulator::new(shared.clone());
        reopened.push(2.0 / 3200.0, &[0.5, 0.0], 3200, 1);
        reopened.publish(&mut shared);
        assert_eq!(shared.rms_counts[..2], [2, 2]);
        assert_eq!(shared.peaks[..2], [Some(1.0), Some(0.5)]);
        assert!(!shared.complete);
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
        let mut frame = frame(Some(1.0), 3200);
        frame.push(0.0, &[1.0], 3200, 1);
        frame.push(1.0 / 3200.0, &[0.0], 3200, 1);
        let expected = (0.5_f32).sqrt();
        let revision = frame.revision;
        for _ in 0..100 {
            frame.push(0.0, &[1.0], 3200, 1);
            frame.push(1.0 / 3200.0, &[0.0], 3200, 1);
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
        let rate = 12800;
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
    fn middle_bin_cache_recovers_from_small_packets_across_reopens() {
        let rate = 12800;
        let samples: Vec<f32> = (0..rate).map(|index| (index % 8) as f32 / 8.0).collect();
        let mut shared = frame(Some(1.0), rate);
        let mut accumulator = WaveformAccumulator::new(shared.clone());
        accumulator.push(6.0 / f64::from(rate), &samples[6..7], rate, 1);
        accumulator.push(4.0 / f64::from(rate), &samples[4..5], rate, 1);
        accumulator.publish(&mut shared);
        assert_eq!(shared.rms_counts[0], 1);
        let mut coverage = WaveformCoverage::new(0.0);
        for start in (0..16).step_by(2) {
            // Reopening retains both displayed RMS and the earlier candidate;
            // no PCM is needed to recover candidate energy after publication.
            shared.select(
                Path::new("track.wav"),
                None,
                DecoderHint::Auto,
                Some(1.0),
                rate,
            );
            let mut reopened = WaveformAccumulator::new(shared.clone());
            let timestamp = start as f64 / f64::from(rate);
            reopened.push(timestamp, &samples[start..start + 2], rate, 1);
            reopened.publish(&mut shared);
            coverage.record(timestamp, 2, rate);
            if start < 4 {
                assert_eq!(shared.rms_counts[0], 1);
            }
        }
        let mut reopened = WaveformAccumulator::new(shared.clone());
        reopened.push(16.0 / f64::from(rate), &samples[16..], rate, 1);
        coverage.record(16.0 / f64::from(rate), samples.len() - 16, rate);
        reopened.finish(&coverage);
        reopened.publish(&mut shared);
        let mut expected = frame(Some(1.0), rate);
        expected.push(0.0, &samples, rate, 1);
        assert!(shared.complete);
        assert_statistics(&shared, &expected);
        assert!(shared.rms_counts.iter().all(|count| *count == 8));
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
            let mut coverage = WaveformCoverage::new(0.0);
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
        let mut coverage = WaveformCoverage::new(0.0);
        coverage.record(0.0, samples.len(), 100);
        coverage.finish(&mut actual);
        coverage.finish(&mut expected);
        assert!(actual.complete);
        assert_statistics(&actual, &expected);
    }

    #[test]
    fn a_continuous_complete_scan_has_exact_bin_counts_and_rms() {
        let samples: Vec<f32> = (0..3200).map(|index| (index % 2) as f32).collect();
        let mut actual = frame(Some(1.0), 3200);
        let mut coverage = WaveformCoverage::new(0.0);
        for start in (0..samples.len()).step_by(7) {
            let end = (start + 7).min(samples.len());
            let timestamp = start as f64 / 3200.0;
            actual.push(timestamp, &samples[start..end], 3200, 1);
            coverage.record(timestamp, end - start, 3200);
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
        live.select(
            Path::new("track.wav"),
            None,
            DecoderHint::Auto,
            Some(4.0),
            1,
        );
        assert_eq!(live.revision, revision + 1);
        live.finish_scan(4.0);
        assert_eq!(live.revision, revision + 1);
        assert_eq!(live.peaks, [Some(0.5), Some(0.0), Some(0.75), Some(0.25)]);
        live.select(
            Path::new("other.wav"),
            None,
            DecoderHint::Auto,
            Some(4.0),
            1,
        );
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
        frame.select(
            Path::new("track.wav"),
            None,
            DecoderHint::Auto,
            Some(99.0),
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
