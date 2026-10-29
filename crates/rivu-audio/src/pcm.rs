//! Immutable, budgeted interleaved PCM. Slices retain their backing allocation.

use super::Channel;
use anyhow::{Context, Result, bail};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

const CHUNK_FRAMES: usize = 4096;
static NEXT_STORAGE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct PcmBudget {
    limit: u64,
    used: AtomicU64,
}

impl PcmBudget {
    pub fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicU64::new(0),
        })
    }

    pub fn used_bytes(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }

    fn reserve(self: &Arc<Self>, bytes: u64) -> Result<Reservation> {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= self.limit)
            })
            .map_err(|_| {
                anyhow::anyhow!("PCM memory budget exceeded (limit {} bytes)", self.limit)
            })?;
        Ok(Reservation {
            budget: Arc::clone(self),
            bytes,
        })
    }
}

#[derive(Debug)]
struct Reservation {
    budget: Arc<PcmBudget>,
    bytes: u64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[derive(Debug)]
struct Chunk {
    samples: Box<[f32]>,
    _reservation: Reservation,
}

#[derive(Debug)]
struct Storage {
    id: u64,
    chunks: Vec<Chunk>,
    layout: Arc<[Channel]>,
    _directory: Reservation,
}

/// Cloning and slicing share storage; neither duplicates PCM nor releases its charge.
#[derive(Clone)]
pub struct PcmAudio {
    storage: Arc<Storage>,
    rate: u32,
    start: u64,
    frames: u64,
}

impl std::fmt::Debug for PcmAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PcmAudio")
            .field("sample_rate", &self.rate)
            .field("layout", &self.storage.layout)
            .field("start", &self.start)
            .field("frames", &self.frames)
            .field("retained_bytes", &self.retained_bytes())
            .finish()
    }
}

impl PcmAudio {
    pub(crate) fn identity(&self) -> super::SourceIdentity {
        super::SourceIdentity::Pcm {
            storage: self.storage.id,
            start: self.start,
            frames: self.frames,
        }
    }
    pub fn sample_rate(&self) -> u32 {
        self.rate
    }
    pub fn channels(&self) -> u16 {
        self.storage.layout.len() as u16
    }
    /// Speaker order of every interleaved frame, shared by clones and slices.
    pub fn layout(&self) -> &Arc<[Channel]> {
        &self.storage.layout
    }
    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn retained_bytes(&self) -> u64 {
        self.storage._directory.bytes
            + self
                .storage
                .chunks
                .iter()
                .map(|chunk| chunk._reservation.bytes)
                .sum::<u64>()
    }

    /// Copies complete frames into caller-owned storage, without allocating.
    pub fn read_frames(&self, start: u64, output: &mut [f32]) -> usize {
        let channels = self.storage.layout.len();
        let count = (output.len() / channels).min(self.frames.saturating_sub(start) as usize);
        if count == 0 {
            return 0;
        }
        let mut sample = (self.start + start) as usize * channels;
        let chunk_samples = CHUNK_FRAMES * channels;
        let mut written = 0;
        let count_samples = count * channels;
        while written < count_samples {
            let chunk = &self.storage.chunks[sample / chunk_samples].samples;
            let offset = sample % chunk_samples;
            let take = (count_samples - written).min(chunk.len() - offset);
            output[written..written + take].copy_from_slice(&chunk[offset..offset + take]);
            sample += take;
            written += take;
        }
        count
    }

    pub fn slice_frames(&self, start: u64, frames: u64) -> Result<Self> {
        if start
            .checked_add(frames)
            .is_none_or(|end| end > self.frames)
        {
            bail!("PCM slice exceeds its source frames");
        }
        Ok(Self {
            storage: Arc::clone(&self.storage),
            rate: self.rate,
            start: self.start + start,
            frames,
        })
    }
}

pub struct PcmBuilder {
    budget: Arc<PcmBudget>,
    chunks: Vec<Chunk>,
    directory: Reservation,
    rate: u32,
    layout: Arc<[Channel]>,
    frames: u64,
    filled: usize,
    failed: bool,
}

impl PcmBuilder {
    pub fn new(rate: u32, layout: Arc<[Channel]>, budget: Arc<PcmBudget>) -> Result<Self> {
        if rate == 0 || layout.is_empty() {
            bail!("PCM requires a nonzero sample rate and a nonempty speaker layout");
        }
        for (index, channel) in layout.iter().enumerate() {
            if layout[..index].contains(channel) {
                bail!("PCM has a duplicate speaker: {channel:?}");
            }
        }
        // Conservatively charge this small shared layout with each storage owner.
        let directory = budget.reserve(
            (std::mem::size_of::<Storage>()
                + 2 * std::mem::size_of::<usize>()
                + std::mem::size_of_val(layout.as_ref())) as u64,
        )?;
        Ok(Self {
            budget,
            chunks: Vec::new(),
            directory,
            rate,
            layout,
            frames: 0,
            filled: 0,
            failed: false,
        })
    }

    pub fn push(&mut self, samples: &[f32]) -> Result<()> {
        if self.failed {
            bail!("PCM builder failed a previous admission");
        }
        let channels = self.layout.len();
        if !samples.len().is_multiple_of(channels) {
            bail!("PCM contains an incomplete interleaved frame");
        }
        if samples.iter().any(|sample| !sample.is_finite()) {
            bail!("PCM contains a non-finite sample");
        }
        let next_frames = self
            .frames
            .checked_add((samples.len() / channels) as u64)
            .context("PCM frame count overflows")?;
        self.failed = true;
        let capacity = CHUNK_FRAMES * channels;
        let mut remaining = samples;
        while !remaining.is_empty() {
            if self.chunks.is_empty() || self.filled == capacity {
                if self.chunks.len() == self.chunks.capacity() {
                    let old = self.chunks.capacity();
                    let additional = old.max(4);
                    let mut reservation = self
                        .budget
                        .reserve((additional * std::mem::size_of::<Chunk>()) as u64)?;
                    self.chunks
                        .try_reserve_exact(additional)
                        .context("Allocating PCM chunk directory")?;
                    self.directory.bytes += reservation.bytes;
                    reservation.bytes = 0;
                }
                let reservation = self
                    .budget
                    .reserve((capacity * std::mem::size_of::<f32>()) as u64)?;
                self.chunks.push(Chunk {
                    samples: vec![0.0; capacity].into_boxed_slice(),
                    _reservation: reservation,
                });
                self.filled = 0;
            }
            let take = remaining.len().min(capacity - self.filled);
            self.chunks.last_mut().unwrap().samples[self.filled..self.filled + take]
                .copy_from_slice(&remaining[..take]);
            self.filled += take;
            remaining = &remaining[take..];
        }
        self.frames = next_frames;
        self.failed = false;
        Ok(())
    }

    pub fn finish(self) -> Result<PcmAudio> {
        if self.failed {
            bail!("Cannot publish PCM after failed admission");
        }
        Ok(PcmAudio {
            storage: Arc::new(Storage {
                id: NEXT_STORAGE_ID
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                    .map_err(|_| anyhow::anyhow!("PCM content identity exhausted"))?,
                chunks: self.chunks,
                layout: self.layout,
                _directory: self.directory,
            }),
            rate: self.rate,
            start: 0,
            frames: self.frames,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo() -> Arc<[Channel]> {
        Arc::from([Channel::FrontLeft, Channel::FrontRight])
    }

    fn mono() -> Arc<[Channel]> {
        Arc::from([Channel::FrontCenter])
    }

    #[test]
    fn speaker_layout_survives_slicing_memory_decode_and_render_format() {
        use Channel::*;
        for speakers in [
            vec![FrontLeft, FrontRight, FrontCenter, Lfe, SideLeft, SideRight],
            vec![SideRight, FrontCenter, FrontLeft, Lfe, SideLeft, FrontRight],
        ] {
            let layout: Arc<[Channel]> = speakers.into();
            let budget = PcmBudget::new(256 * 1024);
            let mut builder =
                PcmBuilder::new(48_000, Arc::clone(&layout), Arc::clone(&budget)).unwrap();
            let samples: Vec<_> = (0..3)
                .flat_map(|frame| {
                    layout
                        .iter()
                        .map(move |channel| (frame * 100 + channel.standard_index()) as f32)
                })
                .collect();
            builder.push(&samples).unwrap();
            let pcm = builder.finish().unwrap();
            let slice = pcm.slice_frames(1, 2).unwrap();
            let alias = slice.clone();
            assert!(Arc::ptr_eq(pcm.layout(), &layout));
            assert!(Arc::ptr_eq(alias.layout(), pcm.layout()));
            let retained = budget.used_bytes();
            let source = crate::source::Source::from_pcm(alias.clone(), 0.0).unwrap();
            assert_eq!(source.layout(), layout.as_ref());
            assert_eq!(source.info().channels, 6);
            let mut renderer =
                crate::SourceRenderer::from_pcm(alias.clone(), alias.identity()).unwrap();
            assert_eq!(renderer.format().channels(), 6);
            assert!(Arc::ptr_eq(&renderer.format().layout, &layout));
            let mut block = Vec::new();
            let cancel = std::sync::atomic::AtomicBool::new(false);
            assert!(renderer.read_block(&mut block, &cancel).unwrap());
            assert_eq!(block, samples[6..]);
            assert!(!renderer.read_block(&mut block, &cancel).unwrap());
            let rendered = crate::SourceRenderer::from_pcm(alias, slice.identity())
                .unwrap()
                .render_pcm(PcmBudget::new(0), &cancel)
                .unwrap();
            assert!(Arc::ptr_eq(rendered.layout(), &layout));
            assert_eq!(rendered.identity(), slice.identity());
            assert_eq!(budget.used_bytes(), retained);
            let device = [FrontLeft, FrontRight, FrontCenter, Lfe, SideLeft, SideRight];
            let mapping = crate::channels::mapping(rendered.layout(), &device).unwrap();
            for (speaker, index) in device.iter().zip(mapping) {
                assert_eq!(layout[index.unwrap()], *speaker);
            }
        }
    }

    #[test]
    fn aliases_and_cross_chunk_slices_retain_the_budget() {
        let budget = PcmBudget::new(128 * 1024);
        let mut builder = PcmBuilder::new(48000, stereo(), Arc::clone(&budget)).unwrap();
        let samples: Vec<_> = (0..10000).map(|i| i as f32).collect();
        builder.push(&samples[..200]).unwrap();
        builder.push(&samples[200..]).unwrap();
        let pcm = builder.finish().unwrap();
        let retained = budget.used_bytes();
        assert_eq!(pcm.retained_bytes(), retained);
        assert!(retained >= (samples.len() * std::mem::size_of::<f32>()) as u64);
        assert!(pcm.slice_frames(5000, 1).is_err());
        assert!(pcm.slice_frames(u64::MAX, 1).is_err());
        let slice = pcm.slice_frames(4088, 24).unwrap();
        let alias = slice.slice_frames(2, 20).unwrap();
        drop(pcm);
        drop(slice);
        assert_eq!(budget.used_bytes(), retained);
        let mut output = [f32::NAN; 43];
        assert_eq!(alias.read_frames(0, &mut output), 20);
        assert_eq!(&output[..40], &samples[8180..8220]);
        assert!(output[40..].iter().all(|sample| sample.is_nan()));
        assert_eq!(alias.read_frames(20, &mut output), 0);
        assert_eq!(alias.read_frames(u64::MAX, &mut output), 0);
        let mut partial = [f32::NAN; 3];
        assert_eq!(alias.read_frames(19, &mut partial), 1);
        assert_eq!(&partial[..2], &samples[8218..8220]);
        assert!(partial[2].is_nan());
        drop(alias);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn invalid_pcm_does_not_consume_capacity_or_poison_the_builder() {
        let budget = PcmBudget::new(128 * 1024);
        assert!(PcmBuilder::new(0, stereo(), Arc::clone(&budget)).is_err());
        assert!(PcmBuilder::new(8000, Arc::from([]), Arc::clone(&budget)).is_err());
        assert!(
            PcmBuilder::new(
                8000,
                Arc::from([Channel::FrontLeft, Channel::FrontLeft]),
                Arc::clone(&budget),
            )
            .is_err()
        );
        assert_eq!(budget.used_bytes(), 0);
        let mut builder = PcmBuilder::new(8000, stereo(), Arc::clone(&budget)).unwrap();
        let reserved = budget.used_bytes();
        assert!(builder.push(&[0.0]).is_err());
        assert!(builder.push(&[f32::NAN, 0.0]).is_err());
        assert!(builder.push(&[0.0, f32::INFINITY]).is_err());
        assert_eq!(budget.used_bytes(), reserved);
        builder.push(&[1.25, -2.5]).unwrap();
        let pcm = builder.finish().unwrap();
        let mut samples = [0.0; 2];
        assert_eq!(pcm.read_frames(0, &mut samples), 1);
        assert_eq!(samples, [1.25, -2.5]);
        drop(pcm);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn memory_source_seek_is_frame_exact_and_keeps_storage_alive() {
        use crate::source::{DecodeStep, Source};
        let budget = PcmBudget::new(128 * 1024);
        let mut builder = PcmBuilder::new(8000, stereo(), Arc::clone(&budget)).unwrap();
        let samples: Vec<_> = (0..10000).map(|i| i as f32).collect();
        builder.push(&samples).unwrap();
        let pcm = builder.finish().unwrap();
        let mut source = Source::from_pcm(pcm, 0.0).unwrap();
        assert!(source.poll_open().unwrap());
        for target in [4095_u64, 12, 5000, 0] {
            source.seek(target as f64 / 8000.0).unwrap();
            for invalid in [f64::NAN, f64::INFINITY, -1.0] {
                assert!(source.seek(invalid).is_err());
            }
            let mut no_work = 0;
            assert!(matches!(
                source.next_frames_with_budget(&mut no_work).unwrap(),
                DecodeStep::Yield
            ));
            let mut cursor = target;
            loop {
                let mut work = 1;
                match source.next_frames_with_budget(&mut work).unwrap() {
                    DecodeStep::Frames(span) => {
                        let count = (span.samples.len() / 2) as u64;
                        assert!(count > 0);
                        assert_eq!(
                            span.samples,
                            &samples[cursor as usize * 2..(cursor + count) as usize * 2]
                        );
                        assert_eq!(span.position, cursor - target);
                        assert_eq!(span.media_start, cursor as f64 / 8000.0);
                        assert_eq!(span.discontinuity, cursor == target);
                        assert_eq!(work, 0);
                        cursor += count;
                    }
                    DecodeStep::Eof => {
                        assert_eq!(cursor, 5000);
                        break;
                    }
                    DecodeStep::Yield => panic!("memory block must be available immediately"),
                }
            }
        }
        assert!(budget.used_bytes() > 0);
        drop(source);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn waveform_reads_pcm_without_opening_the_metadata_path() {
        let directory = tempfile::tempdir().unwrap();
        let budget = PcmBudget::new(128 * 1024);
        let mut builder = PcmBuilder::new(8000, mono(), Arc::clone(&budget)).unwrap();
        builder.push(&[0.25; 800]).unwrap();
        let mut snapshot = crate::WaveformScanSnapshot::original(
            directory.path().join("metadata-only.wav"),
            Some(crate::PlaybackRange {
                start_seconds: 100.0,
                end_seconds: Some(101.0),
            }),
            rivu_format::media::DecoderHint::Native,
            rivu_format::config::VisualizationSignal::Before,
            Arc::new(rivu_dsp::equalizer::config::EqualizerConfig::default()),
        );
        snapshot.pcm = Some(builder.finish().unwrap());
        let frame = crate::waveform(
            &snapshot,
            1,
            false,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(frame.path, Some(snapshot.path.clone()));
        assert!(frame.complete);
        assert_eq!(frame.duration, Some(0.1));
        assert_eq!(frame.span_seconds, 0.1);
        assert_eq!(frame.rms_counts.iter().sum::<u64>(), 800);
        for values in [
            &frame.peaks,
            &frame.rms,
            &frame.left_peaks,
            &frame.right_peaks,
            &frame.left_rms,
            &frame.right_rms,
        ] {
            assert_eq!(values.len(), 800);
            assert!(values.iter().all(|value| *value == Some(0.25)));
        }
        drop(snapshot);
        assert_eq!(budget.used_bytes(), 0);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn content_identity_distinguishes_slices_and_released_storage() {
        let budget = PcmBudget::new(128 * 1024);
        let old = {
            let mut builder = PcmBuilder::new(8000, mono(), Arc::clone(&budget)).unwrap();
            builder.push(&[0.25, 0.75]).unwrap();
            let pcm = builder.finish().unwrap();
            assert_eq!(pcm.identity(), pcm.clone().identity());
            assert_eq!(pcm.identity(), pcm.slice_frames(0, 2).unwrap().identity());
            assert_ne!(
                pcm.slice_frames(0, 1).unwrap().identity(),
                pcm.slice_frames(1, 1).unwrap().identity()
            );
            pcm.identity()
        };
        assert_eq!(budget.used_bytes(), 0);
        let mut builder = PcmBuilder::new(8000, mono(), budget).unwrap();
        builder.push(&[0.25, 0.75]).unwrap();
        assert_ne!(old, builder.finish().unwrap().identity());
    }

    #[test]
    fn failed_partial_push_cannot_publish_a_truncated_result() {
        let budget = PcmBudget::new(20 * 1024);
        let mut builder = PcmBuilder::new(8000, mono(), Arc::clone(&budget)).unwrap();
        assert!(builder.push(&[1.0; 5000]).is_err());
        assert!(builder.push(&[]).is_err());
        assert!(builder.finish().is_err());
        assert_eq!(budget.used_bytes(), 0);
    }
}
