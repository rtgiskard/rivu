//! Cancellable offline decoding. This path never constructs an output device.

use super::{
    Channel, PlaybackRange, SourceIdentity,
    source::{DECODE_WORK_BUDGET, DecodeStep, Source, SourceSelection, check_cancelled},
};
use anyhow::{Context, Result, bail};
use rivu_format::decoder_hint::DecoderHint;
use rivu_utils::regular_file::open_regular_file;
use std::{
    fs::File,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};

/// Format and duration from the actual decoder retained for rendering.
#[derive(Clone, Debug)]
pub struct RenderFormat {
    pub source_identity: SourceIdentity,
    pub sample_rate: u32,
    pub layout: Arc<[Channel]>,
    pub duration: Option<f64>,
}

impl RenderFormat {
    pub fn channels(&self) -> u16 {
        self.layout.len() as u16
    }

    /// Admission estimate only; decoding still enforces its actual byte limit.
    pub fn estimated_frames(&self) -> Option<u64> {
        self.duration
            .filter(|duration| duration.is_finite() && *duration >= 0.0)
            .map(|duration| (duration * f64::from(self.sample_rate)).ceil() as u64)
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct CollectedPcm {
    pub samples: Vec<f32>,
    pub duration: Option<f64>,
    pub source_identity: Option<SourceIdentity>,
    pub spans: Vec<(f64, f64, usize)>,
}

#[cfg(test)]
impl CollectedPcm {
    pub(crate) fn emit(&mut self, event: super::RenderEvent<'_>) -> Result<()> {
        match event {
            super::RenderEvent::Start(format) => {
                self.source_identity = Some(format.source_identity)
            }
            super::RenderEvent::Pcm {
                media_start,
                media_step,
                samples,
            } => {
                self.spans.push((media_start, media_step, samples.len()));
                self.samples.extend_from_slice(samples);
            }
            super::RenderEvent::Complete { duration, .. } => self.duration = Some(duration),
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn collect_stream(
    snapshot: &super::AudioRenderSnapshot,
    buffer: u32,
    ffmpeg: bool,
    cancelled: &AtomicBool,
) -> Result<CollectedPcm> {
    let mut collected = CollectedPcm::default();
    super::render_stream(snapshot, buffer, ffmpeg, cancelled, |event| {
        collected.emit(event)
    })?;
    Ok(collected)
}
pub(super) fn render_stream(
    snapshot: &super::AudioRenderSnapshot,
    media_read_buffer_mib: u32,
    ffmpeg_enabled: bool,
    cancelled: &AtomicBool,
    mut emit: impl FnMut(super::RenderEvent<'_>) -> Result<()>,
) -> Result<()> {
    check_cancelled(Some(cancelled))?;
    let renderer = if let Some(pcm) = &snapshot.pcm {
        SourceRenderer::from_pcm(pcm.clone(), pcm.identity())?
    } else {
        SourceRenderer::open(
            &snapshot.path,
            snapshot.range,
            snapshot.decoder_hint,
            media_read_buffer_mib,
            ffmpeg_enabled,
            cancelled,
        )?
    };
    renderer.finish(cancelled)?;
    if snapshot
        .expected_identity
        .is_some_and(|expected| expected != renderer.format.source_identity)
    {
        bail!("Render source differs from the captured active source");
    }
    let context = snapshot
        .graph
        .as_ref()
        .map(super::graph::GraphSnapshot::source_context);
    let graph = if snapshot.signal == rivu_format::config::VisualizationSignal::After {
        snapshot
            .graph
            .as_ref()
            .map(super::graph::GraphSnapshot::prepare)
            .transpose()?
    } else {
        None
    };
    let SourceRenderer {
        mut source,
        identity_file,
        format,
    } = renderer;
    source.render_processed(snapshot, cancelled, graph, context, |event| {
        if matches!(&event, super::RenderEvent::Complete { .. }) {
            finish_source(identity_file.as_ref(), format.source_identity, cancelled)?;
        }
        emit(event)
    })
}

fn finish_source(
    file: Option<&File>,
    identity: SourceIdentity,
    cancelled: &AtomicBool,
) -> Result<()> {
    check_cancelled(Some(cancelled)).context("Audio render cancelled")?;
    if let Some(file) = file
        && crate::source_identity(file)? != identity
    {
        bail!("Audio render source changed while decoding");
    }
    Ok(())
}

/// Owns one opened decoder, so admission and rendering inspect the same source.
pub struct SourceRenderer {
    source: Source,
    identity_file: Option<File>,
    format: RenderFormat,
}

impl SourceRenderer {
    pub fn open(
        path: &Path,
        range: Option<PlaybackRange>,
        decoder_hint: DecoderHint,
        media_read_buffer_mib: u32,
        ffmpeg_enabled: bool,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        check_cancelled(Some(cancelled)).context("Audio render cancelled")?;
        let buffer_len = usize::try_from(media_read_buffer_mib)
            .context("Audio render media buffer size does not fit usize")?
            .checked_mul(super::BYTES_PER_MEBIBYTE)
            .context("Audio render media buffer size overflows")?;
        let file = open_regular_file(path)?;
        let identity_file = file
            .try_clone()
            .context("Retaining audio render source identity")?;
        let source = Source::open_file_with_cancel(
            path,
            file,
            buffer_len,
            decoder_hint,
            ffmpeg_enabled,
            false,
            SourceSelection { range, start: 0.0 },
            Some(cancelled),
        )?;
        let sample_rate = source.info().sample_rate;
        let channels =
            u16::try_from(source.layout().len()).context("Audio render channel count overflows")?;
        if channels == 0 || sample_rate == 0 {
            bail!("Audio render requires a nonempty, fixed-rate channel layout");
        }
        let format = RenderFormat {
            source_identity: source.identity(),
            sample_rate,
            layout: source.shared_layout(),
            duration: source.info().duration,
        };
        check_cancelled(Some(cancelled)).context("Audio render cancelled")?;
        Ok(Self {
            source,
            identity_file: Some(identity_file),
            format,
        })
    }

    pub fn format(&self) -> &RenderFormat {
        &self.format
    }

    /// Open retained PCM directly. The identity remains the admitted original source.
    pub fn from_pcm(pcm: super::PcmAudio, source_identity: SourceIdentity) -> Result<Self> {
        let format = RenderFormat {
            source_identity,
            sample_rate: pcm.sample_rate(),
            layout: Arc::clone(pcm.layout()),
            duration: Some(pcm.frames() as f64 / f64::from(pcm.sample_rate())),
        };
        Ok(Self {
            source: Source::from_pcm(pcm, 0.0)?,
            identity_file: None,
            format,
        })
    }

    /// Decode one format-checked block, borrowed until the next mutable operation.
    /// None means EOF, after cancellation and opened-source identity validation.
    pub fn next_block(&mut self, cancelled: &AtomicBool) -> Result<Option<&[f32]>> {
        loop {
            let mut work = DECODE_WORK_BUDGET;
            match self
                .source
                .next_frames_with_cancel(Some(cancelled), &mut work)?
            {
                DecodeStep::Frames(_) => break,
                DecodeStep::Yield => continue,
                DecodeStep::Eof => {
                    self.finish(cancelled)?;
                    return Ok(None);
                }
            }
        }
        if self.source.layout() != self.format.layout.as_ref()
            || self.source.info().sample_rate != self.format.sample_rate
        {
            bail!("Audio render source format changed within the track");
        }
        Ok(Some(self.source.decoded_samples()))
    }

    pub fn finish(&self, cancelled: &AtomicBool) -> Result<()> {
        finish_source(
            self.identity_file.as_ref(),
            self.format.source_identity,
            cancelled,
        )
    }

    pub fn render_pcm(
        mut self,
        budget: Arc<super::PcmBudget>,
        cancelled: &AtomicBool,
    ) -> Result<super::PcmAudio> {
        self.finish(cancelled)?;
        if let Some(pcm) = self.source.pcm() {
            return Ok(pcm.clone());
        }
        let mut builder = super::PcmBuilder::new(
            self.format.sample_rate,
            Arc::clone(&self.format.layout),
            budget,
        )?;
        while let Some(block) = self.next_block(cancelled)? {
            builder.push(block)?;
        }
        builder.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn float_source(directory: &Path) -> std::path::PathBuf {
        let path = directory.join("source.wav");
        let mut writer = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                channels: 2,
                sample_rate: 8_000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .unwrap();
        for frame in 0..800 {
            writer.write_sample(frame as f32 / 100.0).unwrap();
            writer.write_sample(-(frame as f32) / 200.0).unwrap();
        }
        writer.finalize().unwrap();
        path
    }

    #[test]
    fn render_pcm_preserves_selected_stereo_timeline_without_output_files() {
        let directory = tempfile::tempdir().unwrap();
        let source = float_source(directory.path());
        let budget = super::super::PcmBudget::new(128 * 1024);
        let pcm = SourceRenderer::open(
            &source,
            Some(PlaybackRange {
                start_seconds: 0.025,
                end_seconds: Some(0.075),
            }),
            DecoderHint::Native,
            2,
            false,
            &AtomicBool::new(false),
        )
        .unwrap()
        .render_pcm(Arc::clone(&budget), &AtomicBool::new(false))
        .unwrap();
        assert_eq!(
            (pcm.sample_rate(), pcm.channels(), pcm.frames()),
            (8_000, 2, 400)
        );
        let mut samples = vec![0.0; 800];
        assert_eq!(pcm.read_frames(0, &mut samples), 400);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        for (offset, pair) in samples.as_chunks::<2>().0.iter().enumerate() {
            assert_eq!(
                pair,
                &[
                    (offset + 200) as f32 / 100.0,
                    -((offset + 200) as f32) / 200.0
                ]
            );
        }
        let retained = budget.used_bytes();
        let selected = pcm.slice_frames(17, 23).unwrap();
        let identity = crate::source_identity(&File::open(&source).unwrap()).unwrap();
        let other_budget = super::super::PcmBudget::new(0);
        let alias = SourceRenderer::from_pcm(selected.clone(), identity)
            .unwrap()
            .render_pcm(Arc::clone(&other_budget), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(budget.used_bytes(), retained);
        assert_eq!(other_budget.used_bytes(), 0);
        assert_eq!(alias.identity(), selected.identity());
        assert!(
            SourceRenderer::from_pcm(selected.clone(), identity)
                .unwrap()
                .render_pcm(Arc::clone(&budget), &AtomicBool::new(true))
                .is_err()
        );
        drop(pcm);
        drop(selected);
        assert_eq!(budget.used_bytes(), retained);
        let mut selected_samples = [0.0; 46];
        assert_eq!(alias.read_frames(0, &mut selected_samples), 23);
        assert_eq!(&selected_samples, &samples[34..80]);
        drop(alias);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn render_pcm_budget_releases_failed_known_and_unknown_duration_decodes() {
        let directory = tempfile::tempdir().unwrap();
        let source = float_source(directory.path());
        let cancelled = AtomicBool::new(false);
        let renderer =
            SourceRenderer::open(&source, None, DecoderHint::Native, 1, false, &cancelled).unwrap();
        assert_eq!(renderer.format().channels(), 2);
        assert_eq!(renderer.format().sample_rate, 8000);
        let budget = super::super::PcmBudget::new(256);
        assert!(
            renderer
                .render_pcm(Arc::clone(&budget), &cancelled)
                .is_err()
        );
        assert_eq!(budget.used_bytes(), 0);

        let longer = crate::source::tests::constant_flac_file(0, 12);
        let renderer = SourceRenderer::open(
            longer.path(),
            None,
            DecoderHint::Native,
            1,
            false,
            &cancelled,
        )
        .unwrap();
        assert_eq!(renderer.format().duration, None);
        let budget = super::super::PcmBudget::new(40 * 1024);
        assert!(
            renderer
                .render_pcm(Arc::clone(&budget), &cancelled)
                .is_err()
        );
        assert_eq!(budget.used_bytes(), 0);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);

        let renderer =
            SourceRenderer::open(&source, None, DecoderHint::Native, 1, false, &cancelled).unwrap();
        assert_eq!(
            renderer
                .render_pcm(Arc::clone(&budget), &cancelled)
                .unwrap()
                .frames(),
            800
        );
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn admitted_renderer_keeps_its_decoder_when_source_path_is_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let original = float_source(directory.path());
        let source = directory.path().join("source-link.wav");
        std::os::unix::fs::symlink(&original, &source).unwrap();
        let cancelled = AtomicBool::new(false);
        let renderer =
            SourceRenderer::open(&source, None, DecoderHint::Native, 1, false, &cancelled).unwrap();
        let identity = renderer.format().source_identity;
        std::fs::remove_file(&source).unwrap();
        std::fs::write(&source, b"replacement is not audio").unwrap();
        assert_eq!(renderer.format().source_identity, identity);
        let pcm = renderer
            .render_pcm(super::super::PcmBudget::new(128 * 1024), &cancelled)
            .unwrap();
        assert_eq!(pcm.frames(), 800);
        let mut samples = [0.0; 2];
        assert_eq!(pcm.read_frames(799, &mut samples), 1);
        assert_eq!(samples, [7.99, -3.995]);
        assert_ne!(
            crate::source_identity(&File::open(source).unwrap()).unwrap(),
            identity
        );
    }

    #[test]
    fn cancellation_rejects_open_and_admitted_pcm_decode() {
        let directory = tempfile::tempdir().unwrap();
        let source = float_source(directory.path());
        let cancelled = AtomicBool::new(true);
        assert!(
            SourceRenderer::open(&source, None, DecoderHint::Native, 2, false, &cancelled).is_err()
        );
        cancelled.store(false, Ordering::Relaxed);
        let renderer =
            SourceRenderer::open(&source, None, DecoderHint::Native, 2, false, &cancelled).unwrap();
        let budget = super::super::PcmBudget::new(128 * 1024);
        cancelled.store(true, Ordering::Relaxed);
        assert!(
            renderer
                .render_pcm(Arc::clone(&budget), &cancelled)
                .is_err()
        );
        assert_eq!(budget.used_bytes(), 0);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn retained_pcm_decodes_after_original_disappears() {
        let directory = tempfile::tempdir().unwrap();
        let path = float_source(directory.path());
        let cancelled = AtomicBool::new(false);
        let budget = super::super::PcmBudget::new(128 * 1024);
        let renderer =
            SourceRenderer::open(&path, None, DecoderHint::Native, 1, false, &cancelled).unwrap();
        let identity = renderer.format().source_identity;
        let pcm = renderer
            .render_pcm(Arc::clone(&budget), &cancelled)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        let retained = budget.used_bytes();
        let renderer = SourceRenderer::from_pcm(pcm.clone(), identity).unwrap();
        assert_eq!(renderer.format().source_identity, identity);
        let decoded = renderer
            .render_pcm(Arc::clone(&budget), &cancelled)
            .unwrap();
        assert_eq!(
            (decoded.sample_rate(), decoded.channels(), decoded.frames()),
            (8000, 2, 800)
        );
        assert_eq!(budget.used_bytes(), retained);
        let mut samples = vec![0.0; 1600];
        assert_eq!(decoded.read_frames(0, &mut samples), 800);
        let mut expected = vec![0.0; samples.len()];
        assert_eq!(pcm.read_frames(0, &mut expected), 800);
        assert_eq!(samples, expected);
        drop(decoded);
        drop(pcm);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn block_decode_preserves_stereo_samples_and_rejects_cancelled_reads() {
        let directory = tempfile::tempdir().unwrap();
        let path = float_source(directory.path());
        let cancelled = AtomicBool::new(false);
        let mut renderer =
            SourceRenderer::open(&path, None, DecoderHint::Native, 1, false, &cancelled).unwrap();
        cancelled.store(true, Ordering::Relaxed);
        assert!(renderer.next_block(&cancelled).is_err());
        cancelled.store(false, Ordering::Relaxed);
        let mut samples = Vec::new();
        while let Some(block) = renderer.next_block(&cancelled).unwrap() {
            assert!(block.len().is_multiple_of(2));
            samples.extend_from_slice(block);
        }
        let expected: Vec<_> = (0..800)
            .flat_map(|frame| [frame as f32 / 100.0, -(frame as f32) / 200.0])
            .collect();
        assert_eq!(samples, expected);
        cancelled.store(true, Ordering::Relaxed);
        assert!(renderer.finish(&cancelled).is_err());
    }
}
