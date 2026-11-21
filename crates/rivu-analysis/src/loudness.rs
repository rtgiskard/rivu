//! Cumulative BS.1770 loudness from native, interleaved source PCM.
//!
//! The EBU R128 histogram mode retains a 400 ms filtered-audio ring and two
//! fixed 1,000-bin histograms, not a growing history of blocks or source PCM.
//! Results describe the source before playback gain, DSP, or output conversion.

use anyhow::{Context, Result, ensure};
use ebur128::{Channel, EbuR128, Mode};
use rivu_protocol::pcm::valid_layout;

/// Incremental analysis with memory independent of track duration.
#[derive(Debug)]
pub struct LoudnessAnalyzer {
    meter: EbuR128,
}

impl LoudnessAnalyzer {
    /// `layout` contains protocol `Start` speaker-mask bit indices in PCM order.
    /// Unknown, duplicate, and empty layouts are rejected rather than downmixed.
    pub fn new(sample_rate: u32, layout: &[u32]) -> Result<Self> {
        ensure!(valid_layout(layout), "Unsupported loudness channel layout");
        let mut meter = EbuR128::new(layout.len() as u32, sample_rate, Mode::I | Mode::HISTOGRAM)
            .context("Creating source loudness analyzer")?;
        for (index, &speaker) in layout.iter().enumerate() {
            // Native IDs are WAVE speaker-mask bit indices, not EbuR128 enum
            // discriminants. BS.1770-4 Annex 3 assigns 1.41 to side speakers
            // (M±90), but 1.0 to rear (M±135/M180) and elevated speakers.
            let channel = match speaker {
                0 => Channel::Left,
                1 => Channel::Right,
                2 => Channel::Center,
                3 => Channel::Unused,
                4 => Channel::Mp135,
                5 => Channel::Mm135,
                6 => Channel::MpSC,
                7 => Channel::MmSC,
                8 => Channel::Mp180,
                9 => Channel::Mp090,
                10 => Channel::Mm090,
                11 => Channel::Tp000,
                12 => Channel::Up030,
                13 => Channel::Up000,
                14 => Channel::Um030,
                15 => Channel::Up135,
                16 => Channel::Up180,
                17 => Channel::Um135,
                _ => unreachable!("validated native speaker index"),
            };
            meter.set_channel(index as u32, channel)?;
        }
        Ok(Self { meter })
    }

    /// Accept complete native-channel frames. Invalid input leaves the analyzer
    /// unchanged; finite over-full-scale samples are preserved without clipping.
    pub fn push(&mut self, samples: &[f32]) -> Result<()> {
        ensure!(
            samples.len().is_multiple_of(self.meter.channels() as usize),
            "Loudness PCM contains an incomplete frame"
        );
        // The upstream histogram accepts NaN energies as bins, so checking
        // only its final LUFS value cannot reliably reject non-finite input.
        ensure!(
            samples.iter().all(|sample| sample.is_finite()),
            "Loudness PCM contains a non-finite sample"
        );
        self.meter.add_frames_f32(samples)?;
        Ok(())
    }

    /// Read cumulative loudness without padding or consuming the measurement.
    /// Call at diagnostic publication cadence, not for every decoded packet.
    pub fn integrated_lufs(&self) -> Result<Option<f64>> {
        let lufs = self.meter.loudness_global()?;
        let integrated_lufs = if lufs == f64::NEG_INFINITY {
            None
        } else {
            ensure!(
                lufs.is_finite(),
                "Loudness analysis produced an invalid result"
            );
            Some(lufs)
        };
        Ok(integrated_lufs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    const RATE: u32 = 48_000;

    fn tone(amplitude: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|frame| (amplitude * (TAU * 997.0 * frame as f64 / f64::from(RATE)).sin()) as f32)
            .collect()
    }

    fn measure(layout: &[u32], samples: &[f32]) -> Option<f64> {
        let mut analyzer = LoudnessAnalyzer::new(RATE, layout).unwrap();
        analyzer.push(samples).unwrap();
        analyzer.integrated_lufs().unwrap()
    }

    fn assert_close(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{actual} differs from {expected} by more than {tolerance}"
        );
    }

    #[test]
    fn reference_tone_has_known_loudness() {
        // BS.1770-4 Annex 1: a full-scale 997 Hz single-channel sine is
        // -3.01 LKFS; reducing its amplitude to 0.1 subtracts 20 dB.
        let lufs = measure(&[2], &tone(0.1, RATE as usize * 3)).unwrap();
        // The library's histogram bins are 0.1 LU wide.
        assert_close(lufs, -23.01, 0.1);
    }

    #[test]
    fn source_gain_changes_loudness_by_the_same_decibels() {
        let quiet = measure(&[0], &tone(0.05, RATE as usize * 3));
        let loud = measure(
            &[0],
            &tone(0.05 * 10.0_f64.powf(6.0 / 20.0), RATE as usize * 3),
        );
        assert_close(loud.unwrap() - quiet.unwrap(), 6.0, 0.1);
    }

    #[test]
    fn silence_empty_and_short_inputs_have_no_integrated_loudness() {
        for samples in [vec![], vec![0.0; RATE as usize], tone(0.5, 19_199)] {
            assert_eq!(measure(&[2], &samples), None);
        }
        assert!(measure(&[2], &tone(0.5, 19_200)).is_some());
        assert_eq!(measure(&[2], &tone(1e-5, RATE as usize)), None);
    }

    #[test]
    fn lfe_is_excluded_from_loudness() {
        let samples = tone(0.1, RATE as usize);
        let center = measure(&[2], &samples);
        let with_lfe: Vec<_> = samples.iter().flat_map(|&sample| [sample, -1.25]).collect();
        assert_eq!(measure(&[2, 3], &with_lfe), center);
        assert_eq!(measure(&[3], &vec![-1.25; RATE as usize]), None);
    }

    #[test]
    fn speaker_positions_use_bs1770_weights_not_channel_order() {
        let samples = tone(0.1, RATE as usize);
        let reference = measure(&[2], &samples).unwrap();
        for speaker in 0..18 {
            if speaker == 3 {
                continue;
            }
            let expected_gain = if matches!(speaker, 9 | 10) {
                10.0 * 1.41_f64.log10()
            } else {
                0.0
            };
            assert_close(
                measure(&[speaker], &samples).unwrap() - reference,
                expected_gain,
                0.1,
            );
        }
        let reordered: Vec<_> = samples.iter().flat_map(|&s| [0.0, s, 0.0]).collect();
        assert_eq!(measure(&[3, 2, 9], &reordered), Some(reference));
    }

    #[test]
    fn arbitrary_frame_aligned_chunks_preserve_results() {
        let samples: Vec<_> = tone(0.1, RATE as usize * 2 + 79)
            .into_iter()
            .flat_map(|s| [s, -s * 0.3, 0.2])
            .collect();
        let expected = measure(&[0, 9, 3], &samples);
        for chunk_frames in [1, 17, 1024, 4799, 19_200] {
            let mut analyzer = LoudnessAnalyzer::new(RATE, &[0, 9, 3]).unwrap();
            analyzer.push(&[]).unwrap();
            for chunk in samples.chunks(chunk_frames * 3) {
                analyzer.push(chunk).unwrap();
            }
            assert_eq!(analyzer.integrated_lufs().unwrap(), expected);
        }
    }

    #[test]
    fn reading_loudness_preserves_subsequent_accumulation() {
        let first = tone(0.1, RATE as usize);
        let second = tone(0.2, RATE as usize);
        let mut analyzer = LoudnessAnalyzer::new(RATE, &[2]).unwrap();
        analyzer.push(&first).unwrap();
        assert_eq!(analyzer.integrated_lufs().unwrap(), measure(&[2], &first));
        analyzer.push(&second).unwrap();
        assert_eq!(
            analyzer.integrated_lufs().unwrap(),
            measure(&[2], &[first, second].concat())
        );
    }

    #[test]
    fn invalid_input_is_rejected_without_changing_measurements() {
        for layout in [vec![], vec![0, 0], vec![18], vec![u32::MAX]] {
            assert!(LoudnessAnalyzer::new(RATE, &layout).is_err());
        }
        assert!(LoudnessAnalyzer::new(0, &[2]).is_err());
        // A decoder that admits the valid prefix before rejecting its tail
        // would raise the later measurement relative to the clean control.
        let stereo =
            |mono: Vec<f32>| -> Vec<f32> { mono.into_iter().flat_map(|s| [s, s]).collect() };
        let loud_prefix = stereo(tone(0.9, RATE as usize));
        let accepted = stereo(tone(0.1, RATE as usize * 3));
        let mut analyzer = LoudnessAnalyzer::new(RATE, &[0, 1]).unwrap();
        assert!(
            analyzer
                .push(&[loud_prefix.as_slice(), &[0.5]].concat())
                .is_err()
        );
        for sample in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut rejected = loud_prefix.clone();
            *rejected.last_mut().unwrap() = sample;
            assert!(analyzer.push(&rejected).is_err());
        }
        analyzer.push(&accepted).unwrap();
        assert_eq!(
            analyzer.integrated_lufs().unwrap(),
            measure(&[0, 1], &accepted)
        );
    }
}
