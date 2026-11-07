//! GUI-owned bounded waveform statistics. No decoder or transport resources are retained.
use rivu_format::decoder_hint::DecoderHint;
use rivu_protocol::audio::{PlaybackRange, SourceIdentity};
use std::path::{Path, PathBuf};

/// Display geometry and optional authoritative opened-content identity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveformIdentity {
    pub content: Option<SourceIdentity>,
    pub sample_rate: u32,
}

#[derive(Debug)]
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
    pub rms_spans: Vec<Option<std::ops::Range<u64>>>,
    /// Last PCM revision per bin, used to fence asynchronous full scans.
    pub bin_revisions: Vec<u64>,
    pub bin_epochs: Vec<u64>,
    pub identity: Option<WaveformIdentity>,
    pub source_generation: Option<u64>,
    /// GUI session cancellation boundary; never serialized or producer-owned.
    pub request_epoch: u64,
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
            rms_spans: Vec::new(),
            bin_revisions: Vec::new(),
            bin_epochs: Vec::new(),
            identity: None,
            source_generation: None,
            request_epoch: 0,
        }
    }
}

impl Clone for WaveformFrame {
    fn clone(&self) -> Self {
        let mut frame = Self::default();
        frame.clone_from(self);
        frame
    }

    fn clone_from(&mut self, source: &Self) {
        self.revision = source.revision;
        self.complete = source.complete;
        self.path.clone_from(&source.path);
        self.range = source.range;
        self.decoder_hint = source.decoder_hint;
        self.duration = source.duration;
        self.span_seconds = source.span_seconds;
        self.bin_frames = source.bin_frames;
        self.bin_count = source.bin_count;
        self.max_peak = source.max_peak;
        self.peaks.clone_from(&source.peaks);
        self.rms.clone_from(&source.rms);
        self.rms_counts.clone_from(&source.rms_counts);
        self.left_peaks.clone_from(&source.left_peaks);
        self.right_peaks.clone_from(&source.right_peaks);
        self.left_rms.clone_from(&source.left_rms);
        self.right_rms.clone_from(&source.right_rms);
        self.left_rms_counts.clone_from(&source.left_rms_counts);
        self.right_rms_counts.clone_from(&source.right_rms_counts);
        self.rms_spans.clone_from(&source.rms_spans);
        self.bin_revisions.clone_from(&source.bin_revisions);
        self.bin_epochs.clone_from(&source.bin_epochs);
        self.identity = source.identity;
        self.source_generation = source.source_generation;
        self.request_epoch = source.request_epoch;
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

    /// Heap storage retained by the bounded display snapshot.
    pub fn retained_capacity_bytes(&self) -> usize {
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
            + bytes(&self.rms_spans)
            + bytes(&self.bin_revisions)
            + bytes(&self.bin_epochs)
    }
}

const MAX_BINS: usize = 1024;

/// One session's rolling display. Only folded stereo and bounded statistics
/// enter this owner; PCM is consumed immediately and never retained here.
#[derive(Default)]
pub struct WaveformSummary {
    pub frame: WaveformFrame,
    energy: Vec<[f64; 3]>,
    end_seconds: Option<f64>,
    traversal: u64,
}

impl WaveformSummary {
    #[allow(clippy::too_many_arguments)]
    pub fn select(
        &mut self,
        path: &Path,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
        duration: Option<f64>,
        sample_rate: u32,
        generation: Option<u64>,
        identity: Option<SourceIdentity>,
    ) {
        let duration = duration.filter(|value| value.is_finite() && *value > 0.0);
        let same = self.frame.matches(path, range, decoder_hint)
            && self.frame.source_generation == generation
            && self.frame.identity.and_then(|identity| identity.content) == identity;
        if same && self.frame.identity.is_some() {
            return;
        }
        let revision = self.frame.revision.wrapping_add(1);
        let request_epoch = self.frame.request_epoch.wrapping_add(1);
        self.frame = WaveformFrame {
            revision,
            request_epoch,
            path: Some(path.to_owned()),
            range,
            decoder_hint,
            duration,
            span_seconds: duration.unwrap_or(60.0),
            source_generation: generation,
            identity: Some(WaveformIdentity {
                content: identity,
                sample_rate,
            }),
            ..WaveformFrame::default()
        };
        let bins = duration.map_or(MAX_BINS, |duration| {
            ((duration * f64::from(sample_rate)).round().max(1.0) as usize).min(MAX_BINS)
        });
        self.frame.bin_frames = (self.frame.span_seconds * f64::from(sample_rate))
            .round()
            .max(1.0) as u64 as u128;
        self.frame.bin_count = bins as u64;
        self.frame.peaks.resize(bins, None);
        self.frame.rms.resize(bins, None);
        self.frame.left_peaks.resize(bins, None);
        self.frame.right_peaks.resize(bins, None);
        self.frame.left_rms.resize(bins, None);
        self.frame.right_rms.resize(bins, None);
        self.frame.rms_counts.resize(bins, 0);
        self.frame.left_rms_counts.resize(bins, 0);
        self.frame.right_rms_counts.resize(bins, 0);
        self.frame.rms_spans.resize(bins, None);
        self.frame.bin_revisions.resize(bins, 0);
        self.frame.bin_epochs.resize(bins, 0);
        self.energy.clear();
        self.energy.resize(bins, [0.0; 3]);
        self.end_seconds = None;
        self.traversal = 1;
    }

    pub fn clear(&mut self) {
        let revision = self.frame.revision.wrapping_add(1);
        let request_epoch = self.frame.request_epoch.wrapping_add(1);
        *self = Self::default();
        self.frame.revision = revision;
        self.frame.request_epoch = request_epoch;
    }

    /// Seek, capture and signal boundaries retain untouched bins, replacing a
    /// visited bin's peak AND energy/count together on the next traversal.
    pub fn begin_traversal(&mut self) {
        let next = self.traversal.wrapping_add(1);
        if next == 0 {
            self.frame.bin_epochs.fill(0);
        }
        self.traversal = next.max(1);
        self.frame.complete = false;
        self.end_seconds = None;
    }

    fn clear_bin(&mut self, bin: usize) {
        for values in [
            &mut self.frame.peaks,
            &mut self.frame.rms,
            &mut self.frame.left_peaks,
            &mut self.frame.right_peaks,
            &mut self.frame.left_rms,
            &mut self.frame.right_rms,
        ] {
            values[bin] = None;
        }
        self.frame.rms_counts[bin] = 0;
        self.frame.left_rms_counts[bin] = 0;
        self.frame.right_rms_counts[bin] = 0;
        self.frame.rms_spans[bin] = None;
        self.energy[bin] = [0.0; 3];
    }

    fn coarsen(&mut self) {
        let len = self.frame.peaks.len();
        let retained = len.div_ceil(2);
        for bin in 0..retained {
            let left = bin * 2;
            let right = left + 1;
            let epoch = self.frame.bin_epochs[left]
                .max(self.frame.bin_epochs.get(right).copied().unwrap_or(0));
            let include_left = self.frame.bin_epochs[left] == epoch;
            let include_right = right < len && self.frame.bin_epochs[right] == epoch;
            for values in [
                &mut self.frame.peaks,
                &mut self.frame.left_peaks,
                &mut self.frame.right_peaks,
            ] {
                values[bin] = match (
                    include_left.then_some(values[left]).flatten(),
                    include_right.then(|| values[right]).flatten(),
                ) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
            }
            let energies = std::array::from_fn(|side| {
                (if include_left {
                    self.energy[left][side]
                } else {
                    0.0
                }) + (if include_right {
                    self.energy[right][side]
                } else {
                    0.0
                })
            });
            self.energy[bin] = energies;
            for (side, (rms, counts)) in [
                (&mut self.frame.rms, &mut self.frame.rms_counts),
                (&mut self.frame.left_rms, &mut self.frame.left_rms_counts),
                (&mut self.frame.right_rms, &mut self.frame.right_rms_counts),
            ]
            .into_iter()
            .enumerate()
            {
                let count = (if include_left { counts[left] } else { 0 })
                    + (if include_right { counts[right] } else { 0 });
                counts[bin] = count;
                rms[bin] = (count != 0).then(|| (energies[side] / count as f64).sqrt() as f32);
            }
            self.frame.rms_spans[bin] = match (
                include_left
                    .then(|| self.frame.rms_spans[left].clone())
                    .flatten(),
                include_right
                    .then(|| self.frame.rms_spans[right].clone())
                    .flatten(),
            ) {
                (Some(a), Some(b)) => Some(a.start.min(b.start)..a.end.max(b.end)),
                (a, b) => a.or(b),
            };
            self.frame.bin_revisions[bin] = self.frame.bin_revisions[left]
                .max(self.frame.bin_revisions.get(right).copied().unwrap_or(0));
            self.frame.bin_epochs[bin] = epoch;
        }
        for bin in retained..len {
            self.clear_bin(bin);
            self.frame.bin_revisions[bin] = 0;
            self.frame.bin_epochs[bin] = 0;
        }
        self.frame.span_seconds *= 2.0;
        self.frame.bin_frames = self.frame.bin_frames.saturating_mul(2);
        self.frame.duration = None;
    }

    fn update_rms(&mut self, bin: usize) {
        for (side, (rms, counts)) in [
            (&mut self.frame.rms, &self.frame.rms_counts),
            (&mut self.frame.left_rms, &self.frame.left_rms_counts),
            (&mut self.frame.right_rms, &self.frame.right_rms_counts),
        ]
        .into_iter()
        .enumerate()
        {
            rms[bin] = Some((self.energy[bin][side] / counts[bin] as f64).sqrt() as f32);
        }
    }

    pub fn push_mapped(&mut self, start: f64, step: f64, samples: &[[f32; 2]]) {
        if samples.is_empty()
            || self.frame.peaks.is_empty()
            || !start.is_finite()
            || !step.is_finite()
            || step <= 0.0
        {
            return;
        }
        let end = start + step * samples.len() as f64;
        if !end.is_finite() || end <= 0.0 {
            return;
        }
        if self
            .end_seconds
            .is_some_and(|previous| (previous - start).abs() > step * 0.5)
        {
            self.begin_traversal();
        }
        while end > self.frame.span_seconds + step * 0.5 {
            self.coarsen();
        }
        let Some(identity) = self.frame.identity else {
            return;
        };
        let rate = identity.sample_rate;
        let revision = self.frame.revision.wrapping_add(1);
        let mut last_bin = None;
        for (offset, &[left, right]) in samples.iter().enumerate() {
            let seconds = start + offset as f64 * step;
            if seconds < 0.0 {
                continue;
            }
            let media_frame = (seconds * f64::from(rate)).round() as u64;
            let bin = (u128::from(media_frame) * u128::from(self.frame.bin_count)
                / self.frame.bin_frames) as usize;
            if bin >= self.frame.peaks.len() {
                continue;
            }
            // Positive media steps visit bins monotonically. Publish each bin's
            // final RMS once, not three square roots for every sample frame.
            if last_bin != Some(bin) {
                if let Some(previous) = last_bin {
                    self.update_rms(previous);
                }
                last_bin = Some(bin);
            }
            if self.frame.bin_epochs[bin] != self.traversal.max(1) {
                self.clear_bin(bin);
                self.frame.bin_epochs[bin] = self.traversal.max(1);
            }
            let left = if left.is_finite() { left } else { 0.0 };
            let right = if right.is_finite() { right } else { 0.0 };
            let peaks = [left.abs().max(right.abs()), left.abs(), right.abs()];
            let energy = [
                f64::from(left).powi(2) + f64::from(right).powi(2),
                f64::from(left).powi(2),
                f64::from(right).powi(2),
            ];
            for (side, (values, counts)) in [
                (&mut self.frame.peaks, &mut self.frame.rms_counts),
                (&mut self.frame.left_peaks, &mut self.frame.left_rms_counts),
                (
                    &mut self.frame.right_peaks,
                    &mut self.frame.right_rms_counts,
                ),
            ]
            .into_iter()
            .enumerate()
            {
                values[bin] = Some(values[bin].unwrap_or(0.0).max(peaks[side]));
                self.energy[bin][side] += energy[side];
                counts[bin] += if side == 0 { 2 } else { 1 };
            }
            let end_frame = ((seconds + step) * f64::from(rate)).round() as u64;
            self.frame.rms_spans[bin] = Some(match self.frame.rms_spans[bin].take() {
                Some(span) => span.start.min(media_frame)..span.end.max(end_frame),
                None => media_frame..end_frame,
            });
            self.frame.bin_revisions[bin] = revision;
        }
        if let Some(bin) = last_bin {
            self.update_rms(bin);
        }
        self.end_seconds = Some(end);
        self.frame.complete = false;
        self.frame.revision = revision;
        // Each combined bin is max(left, right); clearing and coarsening use
        // the same bins for all three arrays, so only combined peaks need a scan.
        self.frame.max_peak = self
            .frame
            .peaks
            .iter()
            .flatten()
            .copied()
            .fold(1.0, f32::max);
    }

    pub fn finish(&mut self, duration: f64) {
        if duration.is_finite() && duration >= 0.0 {
            if duration > 0.0 && !self.frame.peaks.is_empty() && self.frame.span_seconds > 0.0 {
                let bins = ((duration / self.frame.span_seconds * self.frame.peaks.len() as f64)
                    .ceil() as usize)
                    .clamp(1, self.frame.peaks.len());
                self.frame.span_seconds *= bins as f64 / self.frame.peaks.len() as f64;
                for values in [
                    &mut self.frame.peaks,
                    &mut self.frame.rms,
                    &mut self.frame.left_peaks,
                    &mut self.frame.right_peaks,
                    &mut self.frame.left_rms,
                    &mut self.frame.right_rms,
                ] {
                    values.truncate(bins);
                }
                self.frame.rms_counts.truncate(bins);
                self.frame.left_rms_counts.truncate(bins);
                self.frame.right_rms_counts.truncate(bins);
                self.frame.rms_spans.truncate(bins);
                self.frame.bin_revisions.truncate(bins);
                self.frame.bin_epochs.truncate(bins);
                self.energy.truncate(bins);
            }
            self.frame.duration = Some(duration);
            self.frame.complete = true;
            self.frame.revision = self.frame.revision.wrapping_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(duration: Option<f64>, rate: u32) -> WaveformSummary {
        let mut summary = WaveformSummary::default();
        summary.select(
            Path::new("track.wav"),
            None,
            DecoderHint::Auto,
            duration,
            rate,
            Some(1),
            None,
        );
        summary
    }

    fn assert_peak_invariant(frame: &WaveformFrame) {
        assert_eq!(frame.peaks.len(), frame.left_peaks.len());
        assert_eq!(frame.peaks.len(), frame.right_peaks.len());
        for (bin, ((combined, left), right)) in frame
            .peaks
            .iter()
            .zip(&frame.left_peaks)
            .zip(&frame.right_peaks)
            .enumerate()
        {
            let expected = match (*left, *right) {
                (Some(left), Some(right)) => Some(left.max(right)),
                (left, right) => left.or(right),
            };
            assert_eq!(*combined, expected, "bin {bin}");
        }
    }

    #[test]
    fn eof_trims_unknown_tail_without_moving_decoded_bins() {
        let mut summary = summary(None, 100);
        summary.push_mapped(0.0, 0.01, &[[0.5; 2]; 100]);
        let width = (summary.frame.bin_frames, summary.frame.bin_count);
        let decoded = summary
            .frame
            .peaks
            .iter()
            .rposition(Option::is_some)
            .unwrap();
        summary.finish(1.0);
        assert_eq!(summary.frame.duration, Some(1.0));
        assert!(summary.frame.complete);
        assert!(summary.frame.peaks.len() < MAX_BINS);
        assert_eq!((summary.frame.bin_frames, summary.frame.bin_count), width);
        assert_eq!(summary.frame.peaks[decoded], Some(0.5));
        assert_eq!(summary.frame.left_rms_counts.iter().sum::<u64>(), 100);
        assert_peak_invariant(&summary.frame);
    }

    #[test]
    fn replay_replaces_peak_rms_and_count_together_preserving_unvisited_bins() {
        let mut summary = summary(Some(2.0), 1024);
        summary.push_mapped(0.0, 1.0 / 1024.0, &[[2.0, 1.0]; 4]);
        let old_tail = summary.frame.peaks[1];
        summary.begin_traversal();
        summary.push_mapped(0.0, 1.0 / 1024.0, &[[0.25, 0.125]]);
        assert_eq!(summary.frame.peaks[0], Some(0.25));
        assert_eq!(summary.frame.left_rms[0], Some(0.25));
        assert_eq!(summary.frame.right_rms[0], Some(0.125));
        assert_eq!(summary.frame.rms_counts[0], 2);
        assert_eq!(summary.frame.left_rms_counts[0], 1);
        assert_eq!(summary.frame.peaks[1], old_tail);
        assert_eq!(summary.frame.max_peak, 2.0);
        assert_peak_invariant(&summary.frame);

        // Replacing the last old maximum must lower normalization, including
        // when the new maximum switches from the left to the right channel.
        summary.push_mapped(2.0 / 1024.0, 1.0 / 1024.0, &[[0.125, 1.25]]);
        assert_eq!(summary.frame.max_peak, 1.25);
        assert_peak_invariant(&summary.frame);
        summary.begin_traversal();
        summary.push_mapped(2.0 / 1024.0, 1.0 / 1024.0, &[[0.125, 0.5]]);
        assert_eq!(summary.frame.max_peak, 1.0);
        assert_eq!(summary.frame.peaks[0], Some(0.25));
        assert_peak_invariant(&summary.frame);
    }

    #[test]
    fn media_step_places_resampled_pcm_in_selected_source_time() {
        let mut summary = summary(Some(8.0), 1);
        summary.push_mapped(2.0, 0.5, &[[0.5; 2], [0.75; 2], [0.25; 2]]);
        assert!(summary.frame.peaks[..2].iter().all(Option::is_none));
        assert_eq!(summary.frame.peaks[2], Some(0.5));
        assert_eq!(summary.frame.peaks[3], Some(0.75));
        assert_eq!(summary.frame.left_rms_counts[3], 2);
        assert_eq!(summary.frame.peaks[4], None);
    }

    #[test]
    fn packetization_keeps_exact_energy_counts_and_known_silence() {
        let samples: Vec<_> = (0..2048)
            .map(|index| [if index % 2 == 0 { 1.0 } else { 0.0 }; 2])
            .collect();
        let mut whole = summary(Some(1.0), 2048);
        whole.push_mapped(0.0, 1.0 / 2048.0, &samples);
        let mut packets = summary(Some(1.0), 2048);
        for (index, chunk) in samples.chunks(7).enumerate() {
            packets.push_mapped((index * 7) as f64 / 2048.0, 1.0 / 2048.0, chunk);
        }
        assert_eq!(packets.frame.peaks, whole.frame.peaks);
        assert_eq!(packets.frame.rms, whole.frame.rms);
        assert_eq!(packets.frame.rms_counts, vec![4; MAX_BINS]);
        assert_eq!(packets.frame.left_rms_counts, vec![2; MAX_BINS]);
        let mut silent = summary(Some(1.0), 2);
        silent.push_mapped(0.0, 0.5, &[[0.0; 2]]);
        assert_eq!(silent.frame.peaks, [Some(0.0), None]);
    }

    #[test]
    fn sparse_mapped_bins_finalize_rms_after_skipping_negative_start() {
        let mut summary = summary(Some(4.0), 4);
        summary.push_mapped(-1.0, 0.5, &[[0.25, 0.75]; 8]);
        for bin in 0..16 {
            if bin <= 10 && bin % 2 == 0 {
                assert_eq!(summary.frame.left_rms[bin], Some(0.25));
                assert_eq!(summary.frame.right_rms[bin], Some(0.75));
                assert!((summary.frame.rms[bin].unwrap() - 0.3125_f32.sqrt()).abs() < 1e-6);
                assert_eq!(summary.frame.rms_counts[bin], 2);
            } else {
                assert_eq!(summary.frame.rms[bin], None);
                assert_eq!(summary.frame.rms_counts[bin], 0);
            }
        }
    }

    #[test]
    fn unknown_duration_coarsens_bounded_storage_without_mixing_traversals() {
        let mut summary = summary(None, 100);
        summary.push_mapped(0.0, 0.01, &[[2.0; 2]]);
        summary.push_mapped(120.0, 0.01, &[[0.25; 2]]);
        assert_eq!(summary.frame.peaks.len(), MAX_BINS);
        assert_eq!(summary.frame.span_seconds, 240.0);
        assert_eq!(summary.frame.peaks[0], Some(2.0));
        assert_peak_invariant(&summary.frame);
        summary.push_mapped(0.0, 0.01, &[[0.125; 2]]);
        assert_eq!(summary.frame.peaks[0], Some(0.125));
        assert_eq!(summary.frame.left_rms_counts[0], 1);
        assert_eq!(summary.frame.left_rms[0], Some(0.125));
        assert_eq!(summary.frame.max_peak, 1.0);
        assert_peak_invariant(&summary.frame);
        assert!(summary.frame.retained_capacity_bytes() < 256 * 1024);
    }

    #[test]
    fn coarsening_discards_the_older_partner_peak_with_its_channel_statistics() {
        let mut summary = summary(Some(4.0), 1);
        summary.push_mapped(
            0.0,
            1.0,
            &[[4.0, 0.25], [0.5, 2.0], [1.0, 0.5], [0.25, 1.5]],
        );
        summary.begin_traversal();
        summary.push_mapped(1.0, 1.0, &[[0.25, 3.0]]);
        assert_eq!(summary.frame.max_peak, 4.0);
        summary.push_mapped(2.0, 1.0, &[[0.125, 0.5]; 3]);
        assert_eq!(summary.frame.span_seconds, 8.0);
        assert_eq!(summary.frame.peaks, [Some(3.0), Some(0.5), Some(0.5), None]);
        assert_eq!(summary.frame.left_peaks[0], Some(0.25));
        assert_eq!(summary.frame.right_peaks[0], Some(3.0));
        assert_eq!(summary.frame.left_rms_counts[0], 1);
        assert_eq!(summary.frame.max_peak, 3.0);
        assert_peak_invariant(&summary.frame);
    }

    #[test]
    fn authoritative_source_change_clears_even_when_path_and_duration_match() {
        let mut summary = summary(Some(2.0), 1);
        summary.push_mapped(0.0, 1.0, &[[0.75; 2]]);
        let retained = summary.frame.peaks.clone();
        summary.select(
            Path::new("track.wav"),
            None,
            DecoderHint::Auto,
            Some(2.0),
            1,
            Some(1),
            None,
        );
        assert_eq!(summary.frame.peaks, retained);
        summary.select(
            Path::new("track.wav"),
            None,
            DecoderHint::Auto,
            Some(2.0),
            1,
            Some(2),
            None,
        );
        assert_eq!(summary.frame.peaks, [None; 2]);
        assert_eq!(summary.frame.source_generation, Some(2));
        assert_peak_invariant(&summary.frame);
        summary.clear();
        assert!(summary.frame.peaks.is_empty());
        assert_eq!(summary.frame.max_peak, 1.0);
        assert_peak_invariant(&summary.frame);
    }
}
