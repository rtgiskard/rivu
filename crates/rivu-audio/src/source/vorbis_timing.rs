//! Bounded startup compatibility for end-stamped Matroska Vorbis packets.
//!
//! This is convention detection, not gap repair: conflicting evidence defaults
//! to start stamps, and a selected convention never changes during the stream.

use super::{PCM_CHUNK_BYTES, pcm_chunk_frames, reserve_pcm};
use anyhow::Result;
use std::collections::VecDeque;

const PROBE_PACKETS: usize = 64;
const PROBE_SAMPLES: usize = PCM_CHUNK_BYTES / size_of::<f32>();

#[derive(Clone, Copy)]
enum Convention {
    Start,
    End,
}

struct BufferedPcm {
    start: f64,
    end_start: f64,
    offset: usize,
    end: usize,
}

#[derive(Default)]
struct Probe {
    previous: Option<(f64, u64)>,
    packets: usize,
    start_matches: u8,
    end_matches: u8,
    pcm: Vec<f32>,
    blocks: VecDeque<BufferedPcm>,
}

pub(super) struct VorbisTiming {
    convention: Option<Convention>,
    probe: Option<Probe>,
    tolerance: f64,
    rate: f64,
}

impl VorbisTiming {
    pub(super) fn new(tick: f64, rate: u32) -> Self {
        Self {
            convention: None,
            probe: Some(Probe::default()),
            tolerance: tick.max(0.5 / f64::from(rate)) + 1e-9,
            rate: f64::from(rate),
        }
    }

    /// FFmpeg does not return the zero-output overlap primer. A known stream
    /// origin supplies its timestamp anchor, not another PCM block or match.
    pub(super) fn with_origin(tick: f64, rate: u32, origin: Option<f64>) -> Self {
        let mut timing = Self::new(tick, rate);
        timing.probe.as_mut().unwrap().previous = origin.map(|origin| (origin, 0));
        timing
    }

    /// None requests buffering this packet, including the decision packet, so
    /// the entire retained prefix is released in its original sample order.
    pub(super) fn packet(
        &mut self,
        pts: f64,
        represented: u64,
        trim: f64,
        samples: usize,
    ) -> Option<f64> {
        if self.convention.is_none() {
            let probe = self.probe.as_mut().expect("active timing probe");
            if probe.packets == PROBE_PACKETS
                || samples > PROBE_SAMPLES.saturating_sub(probe.pcm.len())
            {
                self.finish();
            } else {
                probe.packets += 1;
                if let Some((previous_pts, previous_frames)) = probe.previous {
                    let delta = pts - previous_pts;
                    let start =
                        (delta - previous_frames as f64 / self.rate).abs() <= self.tolerance;
                    let end = (delta - represented as f64 / self.rate).abs() <= self.tolerance;
                    match (start, end) {
                        (true, false) => probe.start_matches += 1,
                        (false, true) => probe.end_matches += 1,
                        (false, false) => self.convention = Some(Convention::Start),
                        (true, true) => {}
                    }
                    if probe.start_matches != 0 && probe.end_matches != 0
                        || probe.start_matches >= 2
                    {
                        self.convention = Some(Convention::Start);
                    } else if probe.end_matches >= 2 {
                        self.convention = Some(Convention::End);
                    }
                }
                // At stream start the discarded Vorbis priming packet supplies
                // the zero-output anchor. Seek never starts another probe.
                probe.previous = Some((pts, represented));
                return None;
            }
        }
        Some(self.start(pts, represented, trim))
    }

    fn start(&self, pts: f64, represented: u64, trim: f64) -> f64 {
        match self.convention.expect("resolved timing convention") {
            Convention::Start => pts + trim,
            Convention::End => pts - represented as f64 / self.rate + trim,
        }
    }

    /// A converted fragment of a previously observed codec block is not a new
    /// timestamp transition and cannot contribute convention evidence.
    pub(super) fn continuation(
        &mut self,
        pts: f64,
        represented: u64,
        trim: f64,
        samples: usize,
    ) -> Option<f64> {
        if self.convention.is_none()
            && self
                .probe
                .as_ref()
                .is_some_and(|probe| samples > PROBE_SAMPLES.saturating_sub(probe.pcm.len()))
        {
            self.finish();
        }
        self.convention.map(|_| self.start(pts, represented, trim))
    }

    pub(super) fn buffer(
        &mut self,
        pts: f64,
        represented: u64,
        trim: f64,
        pcm: &[f32],
    ) -> Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        let probe = self.probe.as_mut().expect("retained timing prefix");
        let offset = probe.pcm.len();
        let required = offset + pcm.len();
        if required > probe.pcm.capacity() {
            let capacity = required
                .max(probe.pcm.capacity().saturating_mul(2))
                .min(PROBE_SAMPLES);
            probe.pcm.try_reserve_exact(capacity - offset)?;
        }
        probe.pcm.extend_from_slice(pcm);
        probe.blocks.push_back(BufferedPcm {
            start: pts + trim,
            end_start: pts - represented as f64 / self.rate + trim,
            offset,
            end: probe.pcm.len(),
        });
        Ok(())
    }

    pub(super) fn copy_ready(
        &mut self,
        output: &mut Vec<f32>,
        channels: usize,
    ) -> Result<Option<f64>> {
        let Some(convention) = self.convention else {
            return Ok(None);
        };
        let Some(probe) = self.probe.as_mut() else {
            return Ok(None);
        };
        let Some(block) = probe.blocks.front_mut() else {
            self.probe = None;
            return Ok(None);
        };
        let count = ((block.end - block.offset) / channels).min(pcm_chunk_frames(channels)?);
        let start = match convention {
            Convention::Start => block.start,
            Convention::End => block.end_start,
        };
        reserve_pcm(output, count, channels)?;
        let end = block.offset + count * channels;
        output.extend_from_slice(&probe.pcm[block.offset..end]);
        block.offset = end;
        let duration = count as f64 / self.rate;
        block.start += duration;
        block.end_start += duration;
        if end == block.end {
            probe.blocks.pop_front();
        }
        Ok(Some(start))
    }

    pub(super) fn ready(&self) -> bool {
        self.convention.is_some()
            && self
                .probe
                .as_ref()
                .is_some_and(|probe| !probe.blocks.is_empty())
    }

    pub(super) fn finish(&mut self) {
        self.convention.get_or_insert(Convention::Start);
    }

    pub(super) fn seek(&mut self) {
        self.finish();
        self.probe = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(timing: &mut VorbisTiming) -> Vec<(f64, Vec<f32>)> {
        let mut result = Vec::new();
        let mut pcm = Vec::new();
        while let Some(start) = timing.copy_ready(&mut pcm, 1).unwrap() {
            result.push((start, pcm.clone()));
        }
        result
    }

    #[test]
    fn a_real_origin_resolves_ffmpeg_end_stamps_by_the_second_output() {
        let mut timing = VorbisTiming::with_origin(0.001, 44_100, Some(0.0));
        assert!(timing.packet(0.013, 576, 0.0, 576).is_none());
        timing.buffer(0.013, 576, 0.0, &[0.25; 576]).unwrap();
        assert!(!timing.ready(), "one origin transition is insufficient");
        assert!(timing.packet(0.036, 1024, 0.0, 1024).is_none());
        timing.buffer(0.036, 1024, 0.0, &[0.5; 1024]).unwrap();
        assert!(
            timing.ready(),
            "two discriminating transitions resolve by 36 ms"
        );
        let blocks = drain(&mut timing);
        assert_eq!(blocks.len(), 2);
        assert!((blocks[0].0 - (0.013 - 576.0 / 44_100.0)).abs() < 1e-12);
        assert!((blocks[1].0 - (0.036 - 1024.0 / 44_100.0)).abs() < 1e-12);
        assert_eq!(blocks[0].1, [0.25; 576]);
        assert_eq!(blocks[1].1, [0.5; 1024]);
    }

    #[test]
    fn origin_and_pcm_fragments_never_manufacture_a_second_match() {
        let mut timing = VorbisTiming::with_origin(0.001, 48_000, Some(0.0));
        assert!(timing.packet(0.012, 576, 0.0, 128).is_none());
        timing.buffer(0.012, 576, 0.0, &[0.25; 128]).unwrap();
        assert!(
            timing
                .continuation(0.012, 576, 128.0 / 48_000.0, 448)
                .is_none()
        );
        timing
            .buffer(0.012, 576, 128.0 / 48_000.0, &[0.25; 448])
            .unwrap();
        assert!(!timing.ready());
        // An actual initial gap that mimics an endpoint cannot override the
        // following start-stamped transition.
        assert!(timing.packet(0.024, 1024, 0.0, 1024).is_none());
        timing.buffer(0.024, 1024, 0.0, &[0.5; 1024]).unwrap();
        let blocks = drain(&mut timing);
        assert_eq!(blocks[0].0, 0.012);
        assert!((blocks[1].0 - (0.012 + 128.0 / 48_000.0)).abs() < 1e-12);
        assert_eq!(blocks[2].0, 0.024);
    }

    #[test]
    fn an_initial_gap_cannot_alone_select_end_stamps() {
        let mut timing = VorbisTiming::new(0.001, 48_000);
        assert!(timing.packet(0.0, 0, 0.0, 0).is_none());
        assert!(timing.packet(0.012, 576, 0.0, 576).is_none());
        timing.buffer(0.012, 576, 0.0, &[0.25; 576]).unwrap();
        assert!(drain(&mut timing).is_empty());
        // A real initial 12 ms gap mimics an end stamp. The next switch
        // follows start stamps, so the original gap must remain visible.
        assert!(timing.packet(0.024, 1024, 0.0, 1024).is_none());
        timing.buffer(0.024, 1024, 0.0, &[0.5; 1024]).unwrap();
        assert_eq!(
            drain(&mut timing),
            vec![(0.012, vec![0.25; 576]), (0.024, vec![0.5; 1024])]
        );
    }

    #[test]
    fn ambiguous_prefix_releases_at_eof_or_either_storage_bound() {
        for packets in [1, PROBE_PACKETS] {
            let mut timing = VorbisTiming::new(1e-9, 48_000);
            for index in 0..packets {
                let pts = index as f64 * 128.0 / 48_000.0;
                assert!(timing.packet(pts, 128, 0.0, 128).is_none());
                timing.buffer(pts, 128, 0.0, &[0.25; 128]).unwrap();
            }
            if packets == PROBE_PACKETS {
                let pts = packets as f64 * 128.0 / 48_000.0;
                assert_eq!(timing.packet(pts, 128, 0.0, 128), Some(pts));
            } else {
                timing.finish();
            }
            let chunks = drain(&mut timing);
            assert_eq!(chunks.len(), packets);
            for (index, (start, pcm)) in chunks.iter().enumerate() {
                assert_eq!(*start, index as f64 * 128.0 / 48_000.0);
                assert_eq!(pcm, &[0.25; 128]);
            }
        }
        let mut timing = VorbisTiming::new(1e-9, 48_000);
        let pcm = vec![0.5; PROBE_SAMPLES];
        assert!(
            timing
                .packet(0.0, PROBE_SAMPLES as u64, 0.0, pcm.len())
                .is_none()
        );
        timing.buffer(0.0, PROBE_SAMPLES as u64, 0.0, &pcm).unwrap();
        let next = PROBE_SAMPLES as f64 / 48_000.0;
        assert_eq!(timing.packet(next, 1, 0.0, 1), Some(next));
        let chunks = drain(&mut timing);
        let flattened: Vec<_> = chunks
            .iter()
            .flat_map(|(_, pcm)| pcm.iter().copied())
            .collect();
        assert_eq!(flattened, pcm);
        let mut count = 0;
        for (start, pcm) in chunks {
            assert!((start - count as f64 / 48_000.0).abs() < 1e-9);
            count += pcm.len();
        }
    }

    #[test]
    fn fixed_end_stamps_keep_trim_and_later_discontinuities() {
        let mut timing = VorbisTiming::new(1e-9, 48_000);
        for (pts, count) in [(0.0, 0), (0.012, 576), (0.012 + 1024.0 / 48_000.0, 1024)] {
            assert!(timing.packet(pts, count, 0.0, 0).is_none());
        }
        // Use the represented duration, not the samples left after trimming.
        let start = timing.packet(0.2, 1024, 0.002, 100).unwrap();
        assert!((start - (0.2 - 1024.0 / 48_000.0 + 0.002)).abs() < 1e-12);
        timing.seek();
        let start = timing.packet(1.0, 576, 0.0, 576).unwrap();
        assert!((start - 0.988).abs() < 1e-12);
        // A shortened final endpoint stays an overlap; it does not change mode.
        let start = timing.packet(1.002, 1024, 0.0, 1024).unwrap();
        assert!((start - (1.002 - 1024.0 / 48_000.0)).abs() < 1e-12);
    }
}
